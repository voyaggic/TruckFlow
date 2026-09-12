//! Phase 5 — Reporting & oversight (05-ui-screens.md §5, §6c, §6g).
//!
//! Strictly read-only: every command in this module only selects. Zero monetary
//! data exists anywhere in the schema, and no write/delete path is reachable
//! from here by construction — the module holds no INSERT/UPDATE/DELETE calls.
//!
//! Data source: the permanent PostgreSQL archive (06-data-flow.md Step 6 —
//! "Queries PostgreSQL only, never SQLite"). Every command below first asks the
//! central adapter; when the archive is configured but unreachable, or the
//! adapter cannot answer (mock), the query falls back to the local working
//! buffer so reports never break — the response carries `data_source` so the UI
//! shows where the numbers came from. The mock PG adapter cannot answer
//! aggregate queries, which exercises the fallback path in tests.

use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use tauri::State;

use crate::db::AppState;
use crate::models::{
    AuditEntry, AuditFilters, CompanyTripCount, DailyTripCount, OfficerActivityView,
    PriorPeriodComparison, ReportDashboard, ReportExportRow, ReportFilters, ReportSummary,
    TripView, VehicleTripRow,
};
use crate::sync::PostgresAdapter;

const REPORT_PERM: &str = "view_reporting_dashboard";
const AUDIT_PERM: &str = "view_audit_log";

/// Logged trips are the reporting universe — queued/declined work is not a
/// completed trip and is excluded everywhere below. Archived (soft-deleted)
/// trips are hidden from reporting too; they stay in the Postgres archive.
const STATUS_CLAUSE: &str = "t.status = 'logged' AND t.archived = 0";

// ---------------------------------------------------------------------------
// Shared filter plumbing
// ---------------------------------------------------------------------------

fn parse_dt(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d);
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d
            .and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc().fixed_offset());
    }
    None
}

/// Compose the shared WHERE clause (status + date range + company) plus its
/// bound parameters. Parameters are appended in construction order.
///
/// **Critical date-handling fix:** `time_in` is stored as a full RFC3339
/// timestamp (e.g. `2025-08-15T14:30:00+03:00`). When the UI sends a bare
/// `YYYY-MM-DD` upper bound, the string comparison
/// `time_in <= '2025-08-15'` silently excludes every trip on that day
/// because `'2025-08-15T...' > '2025-08-15'` lexicographically. To fix this,
/// any bare-date `to` value is extended to `YYYY-MM-DDT23:59:59Z` so the
/// entire calendar day is covered.
pub fn extend_bare_date_to_end_of_day(s: &str) -> String {
    // Bare YYYY-MM-DD → append end-of-day time
    if s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        format!("{s}T23:59:59Z")
    } else {
        s.to_string()
    }
}

fn build_where(filters: &ReportFilters) -> (String, Vec<SqlValue>) {
    let mut parts: Vec<String> = vec![STATUS_CLAUSE.to_string()];
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(from) = filters.from.as_deref() {
        parts.push("t.time_in >= ?".to_string());
        params.push(SqlValue::Text(from.to_string()));
    }
    if let Some(to) = filters.to.as_deref() {
        let extended = extend_bare_date_to_end_of_day(to);
        parts.push("t.time_in <= ?".to_string());
        params.push(SqlValue::Text(extended));
    }
    if let Some(cid) = filters.company_id.as_deref() {
        parts.push("t.company_id = ?".to_string());
        params.push(SqlValue::Text(cid.to_string()));
    }
    (parts.join(" AND "), params)
}

/// Trips in the window (respecting the same filters minus the range, for
/// prior-period work).
fn count_logged(conn: &Connection, where_sql: &str, params: &[SqlValue]) -> Result<i64, String> {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM trips t WHERE {where_sql}"),
        rusqlite::params_from_iter(params.iter()),
        |r| r.get(0),
    )
    .map_err(|e| format!("trip count failed: {e}"))
}

// ---------------------------------------------------------------------------
// Query surface
// ---------------------------------------------------------------------------

pub fn report_summary(conn: &Connection, filters: &ReportFilters) -> Result<ReportSummary, String> {
    let (where_sql, params) = build_where(filters);

    let total_trips = count_logged(conn, &where_sql, &params)?;

    let active_companies: i64 = conn
        .query_row(
            &format!("SELECT COUNT(DISTINCT t.company_id) FROM trips t WHERE {where_sql} AND t.company_id IS NOT NULL"),
            rusqlite::params_from_iter(params.iter()),
            |r| r.get(0),
        )
        .map_err(|e| format!("active companies count failed: {e}"))?;

    let days = match (filters.from.as_deref().and_then(parse_dt), filters.to.as_deref().and_then(parse_dt)) {
        (Some(from), Some(to)) => (to.date_naive() - from.date_naive()).num_days() + 1,
        (Some(from), None) => (chrono::Utc::now().date_naive() - from.date_naive()).num_days() + 1,
        (None, Some(to)) => (to.date_naive() - chrono::Utc::now().date_naive()).abs().num_days() + 1,
        (None, None) => 1,
    }
    .max(1);
    let avg_trips_per_day = total_trips as f64 / days as f64;

    let prior_period = prior_period_comparison(conn, filters, total_trips)?;

    Ok(ReportSummary {
        total_trips,
        active_companies,
        avg_trips_per_day,
        prior_period,
    })
}

/// Compare the selected window against the immediately preceding window of the
/// same length (05 §5 "comparison to a prior period").
fn prior_period_comparison(
    conn: &Connection,
    filters: &ReportFilters,
    current: i64,
) -> Result<PriorPeriodComparison, String> {
    let Some(from) = filters.from.as_deref().and_then(parse_dt) else {
        // No bounded start → no prior period exists.
        return Ok(PriorPeriodComparison { prior_trips: 0, delta_trips: current, delta_percent: None });
    };
    let to = filters.to.as_deref().and_then(parse_dt).unwrap_or_else(|| chrono::Utc::now().fixed_offset());
    // Inclusive calendar-day length of the selected window, e.g. Aug 8–12 = 5.
    let len_days = (to.date_naive() - from.date_naive()).num_days().max(0) + 1;
    let prior_to = from - chrono::Duration::seconds(1);
    let prior_from = prior_to - chrono::Duration::days(len_days) + chrono::Duration::seconds(1);

    let prior_filters = ReportFilters {
        from: Some(prior_from.to_rfc3339()),
        to: Some(prior_to.to_rfc3339()),
        company_id: filters.company_id.clone(),
    };
    let (where_sql, params) = build_where(&prior_filters);
    let prior_trips = count_logged(conn, &where_sql, &params)?;

    let delta_trips = current - prior_trips;
    let delta_percent = if prior_trips > 0 {
        Some((delta_trips as f64 / prior_trips as f64) * 100.0)
    } else {
        None
    };
    Ok(PriorPeriodComparison { prior_trips, delta_trips, delta_percent })
}

pub fn trips_over_time(conn: &Connection, filters: &ReportFilters) -> Result<Vec<DailyTripCount>, String> {
    let (where_sql, params) = build_where(filters);
    let sql = format!(
        "SELECT date(t.time_in) AS day, COUNT(*) FROM trips t
         WHERE {where_sql} GROUP BY day ORDER BY day"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("trips-over-time failed: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(DailyTripCount { date: r.get(0)?, count: r.get(1)? })
        })
        .map_err(|e| format!("trips-over-time failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("trips-over-time read failed: {e}"))
}

pub fn top_companies(conn: &Connection, filters: &ReportFilters, limit: i64) -> Result<Vec<CompanyTripCount>, String> {
    let (where_sql, params) = build_where(filters);
    let sql = format!(
        "SELECT t.company_id, COALESCE(NULLIF(c.name, ''), 'Unknown'), COUNT(*)
         FROM trips t LEFT JOIN companies c ON c.id = t.company_id
         WHERE {where_sql}
         GROUP BY t.company_id ORDER BY COUNT(*) DESC, 2 LIMIT ?"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("top companies failed: {e}"))?;
    let mut all: Vec<SqlValue> = params;
    all.push(SqlValue::Integer(limit));
    let rows = stmt
        .query_map(rusqlite::params_from_iter(all.iter()), |r| {
            Ok(CompanyTripCount { company_id: r.get(0)?, company_name: r.get(1)?, count: r.get(2)? })
        })
        .map_err(|e| format!("top companies failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("top companies read failed: {e}"))
}

pub fn trips_by_vehicle(conn: &Connection, filters: &ReportFilters, limit: i64) -> Result<Vec<VehicleTripRow>, String> {
    let (where_sql, params) = build_where(filters);
    let sql = format!(
        "SELECT COALESCE(v.plate_number, json_extract(t.resolution_notes, '$.plate'), 'Unknown'),
                c.name, COUNT(*), COALESCE(SUM(t.capacity_at_trip), 0)
         FROM trips t
         LEFT JOIN vehicles v ON v.id = t.vehicle_id
         LEFT JOIN companies c ON c.id = t.company_id
         WHERE {where_sql}
         GROUP BY t.vehicle_id, COALESCE(v.plate_number, json_extract(t.resolution_notes, '$.plate'), 'Unknown'), c.name
         ORDER BY COUNT(*) DESC LIMIT ?"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("trips-by-vehicle failed: {e}"))?;
    let mut all: Vec<SqlValue> = params;
    all.push(SqlValue::Integer(limit));
    let rows = stmt
        .query_map(rusqlite::params_from_iter(all.iter()), |r| {
            Ok(VehicleTripRow {
                plate_number: r.get(0)?,
                company_name: r.get(1)?,
                trip_count: r.get(2)?,
                total_capacity: r.get(3)?,
            })
        })
        .map_err(|e| format!("trips-by-vehicle failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("trips-by-vehicle read failed: {e}"))
}

/// Full trip records behind the aggregates (05 §5 drill-down, required).
pub fn report_trips(conn: &Connection, filters: &ReportFilters, limit: i64) -> Result<Vec<crate::models::TripView>, String> {
    let (where_sql, params) = build_where(filters);
    let sql = format!(
        "{select} WHERE {where_sql} ORDER BY t.time_in DESC LIMIT ?",
        select = crate::capture::TRIP_SELECT
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("report drill-down failed: {e}"))?;
    let mut all: Vec<SqlValue> = params;
    all.push(SqlValue::Integer(limit));
    let rows = stmt
        .query_map(rusqlite::params_from_iter(all.iter()), crate::capture::read_trip)
        .map_err(|e| format!("report drill-down failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("report drill-down read failed: {e}"))
}

/// Flat rows for the Excel / CSV export (05 §5 "Export buttons").
pub fn report_export_rows(conn: &Connection, filters: &ReportFilters) -> Result<Vec<ReportExportRow>, String> {
    let (where_sql, params) = build_where(filters);
    let sql = format!(
        "SELECT t.id,
            COALESCE(v.plate_number, json_extract(t.resolution_notes, '$.plate'), ''),
            t.time_in, COALESCE(c.name, ''), COALESCE(d.name, ''),
            t.capacity_at_trip, t.capacity_unit, t.receipt_no, t.capture_method, t.confidence_score
         FROM trips t
         LEFT JOIN vehicles v ON v.id = t.vehicle_id
         LEFT JOIN companies c ON c.id = t.company_id
         LEFT JOIN drivers d ON d.id = t.driver_id
         WHERE {where_sql}
         ORDER BY t.time_in DESC"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("export rows failed: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(ReportExportRow {
                id: r.get(0)?,
                plate: r.get(1)?,
                time_in: r.get(2)?,
                company: r.get(3)?,
                driver: r.get(4)?,
                capacity_at_trip: r.get(5)?,
                capacity_unit: r.get(6)?,
                receipt_no: r.get(7)?,
                capture_method: r.get(8)?,
                confidence_score: r.get(9)?,
            })
        })
        .map_err(|e| format!("export rows failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("export rows read failed: {e}"))
}

// ---------------------------------------------------------------------------
// Central (PostgreSQL) query surface — Phase 5 repoint (06-data-flow.md Step 6)
// ---------------------------------------------------------------------------
// The central mirror stores the same tables/columns as the local store (typed
// in `sync.rs`), so every query below mirrors its SQLite twin but speaks
// Postgres: `$n` placeholders, `->>` JSON access, and `archived` as TEXT
// ('0'/'1' — the column is auto-added as TEXT on first push).

/// Central-dialect WHERE clause. `archived` arrives as TEXT '0'/'1' (or NULL
/// before the column was ever pushed), so the archive filter compares text.
/// Same bare-date extension as `build_where` above.
fn build_where_pg(filters: &ReportFilters) -> (String, Vec<String>) {
    let mut parts: Vec<String> = vec!["t.status = 'logged' AND COALESCE(t.archived, '0') = '0'".to_string()];
    let mut params: Vec<String> = Vec::new();
    if let Some(from) = filters.from.as_deref() {
        parts.push("t.time_in >= $".to_string() + &(params.len() + 1).to_string());
        params.push(from.to_string());
    }
    if let Some(to) = filters.to.as_deref() {
        let extended = extend_bare_date_to_end_of_day(to);
        parts.push("t.time_in <= $".to_string() + &(params.len() + 1).to_string());
        params.push(extended);
    }
    if let Some(cid) = filters.company_id.as_deref() {
        parts.push("t.company_id = $".to_string() + &(params.len() + 1).to_string());
        params.push(cid.to_string());
    }
    (parts.join(" AND "), params)
}

fn pg_count_logged(pg: &dyn PostgresAdapter, where_sql: &str, params: &[String]) -> Result<i64, String> {
    let rows = pg.query_rows(&format!("SELECT COUNT(*) AS n FROM trips t WHERE {where_sql}"), params)?;
    rows.first()
        .and_then(|r| r["n"].as_i64())
        .ok_or_else(|| "central trip count missing".to_string())
}

pub fn pg_report_summary(pg: &dyn PostgresAdapter, filters: &ReportFilters) -> Result<ReportSummary, String> {
    let (where_sql, params) = build_where_pg(filters);

    let total_trips = pg_count_logged(pg, &where_sql, &params)?;

    let active_companies: i64 = {
        let rows = pg.query_rows(
            &format!("SELECT COUNT(DISTINCT t.company_id) AS n FROM trips t WHERE {where_sql} AND t.company_id IS NOT NULL"),
            &params,
        )?;
        rows.first()
            .and_then(|r| r["n"].as_i64())
            .ok_or_else(|| "central active-companies count missing".to_string())?
    };

    let days = match (filters.from.as_deref().and_then(parse_dt), filters.to.as_deref().and_then(parse_dt)) {
        (Some(from), Some(to)) => (to.date_naive() - from.date_naive()).num_days() + 1,
        (Some(from), None) => (chrono::Utc::now().date_naive() - from.date_naive()).num_days() + 1,
        (None, Some(to)) => (to.date_naive() - chrono::Utc::now().date_naive()).abs().num_days() + 1,
        (None, None) => 1,
    }
    .max(1);
    let avg_trips_per_day = total_trips as f64 / days as f64;

    let prior_period = pg_prior_period_comparison(pg, filters, total_trips)?;

    Ok(ReportSummary {
        total_trips,
        active_companies,
        avg_trips_per_day,
        prior_period,
    })
}

fn pg_prior_period_comparison(
    pg: &dyn PostgresAdapter,
    filters: &ReportFilters,
    current: i64,
) -> Result<PriorPeriodComparison, String> {
    let Some(from) = filters.from.as_deref().and_then(parse_dt) else {
        return Ok(PriorPeriodComparison { prior_trips: 0, delta_trips: current, delta_percent: None });
    };
    let to = filters.to.as_deref().and_then(parse_dt).unwrap_or_else(|| chrono::Utc::now().fixed_offset());
    let len_days = (to.date_naive() - from.date_naive()).num_days().max(0) + 1;
    let prior_to = from - chrono::Duration::seconds(1);
    let prior_from = prior_to - chrono::Duration::days(len_days) + chrono::Duration::seconds(1);

    let prior_filters = ReportFilters {
        from: Some(prior_from.to_rfc3339()),
        to: Some(prior_to.to_rfc3339()),
        company_id: filters.company_id.clone(),
    };
    let (where_sql, params) = build_where_pg(&prior_filters);
    let prior_trips = pg_count_logged(pg, &where_sql, &params)?;

    let delta_trips = current - prior_trips;
    let delta_percent = if prior_trips > 0 {
        Some((delta_trips as f64 / prior_trips as f64) * 100.0)
    } else {
        None
    };
    Ok(PriorPeriodComparison { prior_trips, delta_trips, delta_percent })
}

pub fn pg_trips_over_time(pg: &dyn PostgresAdapter, filters: &ReportFilters) -> Result<Vec<DailyTripCount>, String> {
    let (where_sql, params) = build_where_pg(filters);
    let sql = format!(
        "SELECT to_char(t.time_in::date, 'YYYY-MM-DD') AS day, COUNT(*) AS n FROM trips t
         WHERE {where_sql} GROUP BY 1 ORDER BY 1"
    );
    let rows = pg.query_rows(&sql, &params)?;
    rows.iter()
        .map(|r| {
            Ok(DailyTripCount {
                date: r["day"].as_str().unwrap_or("").to_string(),
                count: r["n"].as_i64().unwrap_or(0),
            })
        })
        .collect()
}

pub fn pg_top_companies(pg: &dyn PostgresAdapter, filters: &ReportFilters, limit: i64) -> Result<Vec<CompanyTripCount>, String> {
    let (where_sql, params) = build_where_pg(filters);
    let limit = limit.clamp(1, 5000);
    let sql = format!(
        "SELECT t.company_id AS company_id, COALESCE(NULLIF(c.name, ''), 'Unknown') AS name, COUNT(*) AS n
         FROM trips t LEFT JOIN companies c ON c.id = t.company_id
         WHERE {where_sql}
         GROUP BY t.company_id, COALESCE(NULLIF(c.name, ''), 'Unknown') ORDER BY n DESC, 2 LIMIT {limit}"
    );
    let rows = pg.query_rows(&sql, &params)?;
    rows.iter()
        .map(|r| {
            Ok(CompanyTripCount {
                company_id: r["company_id"].as_str().map(String::from),
                company_name: r["name"].as_str().unwrap_or("Unknown").to_string(),
                count: r["n"].as_i64().unwrap_or(0),
            })
        })
        .collect()
}

pub fn pg_trips_by_vehicle(pg: &dyn PostgresAdapter, filters: &ReportFilters, limit: i64) -> Result<Vec<VehicleTripRow>, String> {
    let (where_sql, params) = build_where_pg(filters);
    let limit = limit.clamp(1, 5000);
    let sql = format!(
        "SELECT COALESCE(v.plate_number, t.resolution_notes::jsonb ->> 'plate', 'Unknown') AS plate,
                c.name AS company_name, COUNT(*) AS n, COALESCE(SUM(t.capacity_at_trip), 0) AS cap
         FROM trips t
         LEFT JOIN vehicles v ON v.id = t.vehicle_id
         LEFT JOIN companies c ON c.id = t.company_id
         WHERE {where_sql}
         GROUP BY t.vehicle_id, COALESCE(v.plate_number, t.resolution_notes::jsonb ->> 'plate', 'Unknown'), c.name
         ORDER BY n DESC LIMIT {limit}"
    );
    let rows = pg.query_rows(&sql, &params)?;
    rows.iter()
        .map(|r| {
            Ok(VehicleTripRow {
                plate_number: r["plate"].as_str().unwrap_or("Unknown").to_string(),
                company_name: r["company_name"].as_str().map(String::from),
                trip_count: r["n"].as_i64().unwrap_or(0),
                total_capacity: r["cap"].as_f64().unwrap_or(0.0),
            })
        })
        .collect()
}

/// Central drill-down: the same TripView rows as `report_trips` but read from
/// the archive. `resolution_notes` is a JSON string centrally, so the plate
/// fallback uses `->>`.
pub fn pg_report_trips(pg: &dyn PostgresAdapter, filters: &ReportFilters, limit: i64) -> Result<Vec<TripView>, String> {
    let (where_sql, params) = build_where_pg(filters);
    let limit = limit.clamp(1, 5000);
    let sql = format!(
        "SELECT t.id,
            COALESCE(v.plate_number, t.resolution_notes::jsonb ->> 'plate', '') AS plate,
            t.company_id AS company_id, c.name AS company_name,
            t.driver_id AS driver_id, d.name AS driver_name,
            t.capacity_at_trip AS capacity_at_trip, t.capacity_unit AS capacity_unit,
            t.time_in AS time_in, t.receipt_no AS receipt_no,
            t.officer_id AS officer_id, u.name AS officer_name,
            t.capture_method AS capture_method, t.confidence_score AS confidence_score,
            t.photo_refs AS photo_refs, t.status AS status, t.resolution_notes AS resolution_notes,
            t.vehicle_id AS vehicle_id, t.is_discharge_trip AS is_discharge_trip,
            t.model_version AS model_version, t.ocr_engine AS ocr_engine
         FROM trips t
         LEFT JOIN vehicles v ON v.id = t.vehicle_id
         LEFT JOIN companies c ON c.id = t.company_id
         LEFT JOIN drivers d ON d.id = t.driver_id
         LEFT JOIN users u ON u.id = t.officer_id
         WHERE {where_sql}
         ORDER BY t.time_in DESC LIMIT {limit}"
    );
    let rows = pg.query_rows(&sql, &params)?;
    rows.iter().map(|r| central_trip_view(r)).collect()
}

/// Map one central trip row (JSON keyed by column name) into a `TripView`.
/// Mirrors `crate::capture::read_trip` for the archive's column layout.
fn central_trip_view(r: &serde_json::Value) -> Result<TripView, String> {
    let photo_refs: Option<String> = r["photo_refs"].as_str().map(String::from);
    let photo_count = photo_refs
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(s).ok())
        .map(|a| a.len())
        .unwrap_or(0);
    let resolution: Option<String> = r["resolution_notes"].as_str().map(String::from);
    let (reason, candidates) = match resolution.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()) {
        Some(serde_json::Value::Object(map)) => {
            let reason = map.get("reason").and_then(|v| v.as_str()).map(String::from);
            let candidates = map
                .get("candidates")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            (reason, candidates)
        }
        _ => (None, vec![]),
    };
    let discharge = match r["is_discharge_trip"].as_str() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    };
    let entry_time = r["entry_time"].as_str().unwrap_or(r["time_in"].as_str().unwrap_or(""));
    Ok(TripView {
        id: r["id"].as_str().unwrap_or("").to_string(),
        vehicle_id: r["vehicle_id"].as_str().map(String::from),
        plate_number: r["plate"].as_str().unwrap_or("").to_string(),
        company_id: r["company_id"].as_str().map(String::from),
        company_name: r["company_name"].as_str().map(String::from),
        driver_id: r["driver_id"].as_str().map(String::from),
        driver_name: r["driver_name"].as_str().map(String::from),
        capacity_at_trip: r["capacity_at_trip"].as_f64(),
        capacity_unit: r["capacity_unit"].as_str().unwrap_or("").to_string(),
        entry_time: entry_time.to_string(),
        exit_time: r["exit_time"].as_str().map(String::from),
        trip_status: r["trip_status"].as_str().unwrap_or("complete").to_string(),
        time_in: entry_time.to_string(),
        receipt_no: r["receipt_no"].as_str().map(String::from),
        officer_id: r["officer_id"].as_str().map(String::from),
        officer_name: r["officer_name"].as_str().map(String::from),
        capture_method: r["capture_method"].as_str().unwrap_or("").to_string(),
        confidence_score: r["confidence_score"].as_f64(),
        entry_photo_count: photo_count,
        exit_photo_count: 0,
        photo_count,
        status: r["status"].as_str().unwrap_or("").to_string(),
        reason,
        candidates,
        is_discharge_trip: discharge,
        model_version: r["model_version"].as_str().map(String::from),
        ocr_engine: r["ocr_engine"].as_str().map(String::from),
    })
}

/// Central export rows (Excel / CSV) — mirrors `report_export_rows`.
pub fn pg_report_export_rows(pg: &dyn PostgresAdapter, filters: &ReportFilters) -> Result<Vec<ReportExportRow>, String> {
    let (where_sql, params) = build_where_pg(filters);
    let sql = format!(
        "SELECT t.id AS id,
            COALESCE(v.plate_number, t.resolution_notes::jsonb ->> 'plate', '') AS plate,
            t.time_in AS time_in, COALESCE(c.name, '') AS company, COALESCE(d.name, '') AS driver,
            t.capacity_at_trip AS capacity_at_trip, t.capacity_unit AS capacity_unit,
            t.receipt_no AS receipt_no, t.capture_method AS capture_method,
            t.confidence_score AS confidence_score
         FROM trips t
         LEFT JOIN vehicles v ON v.id = t.vehicle_id
         LEFT JOIN companies c ON c.id = t.company_id
         LEFT JOIN drivers d ON d.id = t.driver_id
         WHERE {where_sql}
         ORDER BY t.time_in DESC"
    );
    let rows = pg.query_rows(&sql, &params)?;
    rows.iter()
        .map(|r| {
            Ok(ReportExportRow {
                id: r["id"].as_str().unwrap_or("").to_string(),
                plate: r["plate"].as_str().unwrap_or("").to_string(),
                time_in: r["time_in"].as_str().unwrap_or("").to_string(),
                company: r["company"].as_str().unwrap_or("").to_string(),
                driver: r["driver"].as_str().unwrap_or("").to_string(),
                capacity_at_trip: r["capacity_at_trip"].as_f64(),
                capacity_unit: r["capacity_unit"].as_str().unwrap_or("").to_string(),
                receipt_no: r["receipt_no"].as_str().map(String::from),
                capture_method: r["capture_method"].as_str().unwrap_or("").to_string(),
                confidence_score: r["confidence_score"].as_f64(),
            })
        })
        .collect()
}

/// Run a central report query; returns `Ok(None)` when the adapter cannot
/// answer (mock, or not configured) so callers fall back to the local store.
fn try_central<T>(pg: &dyn PostgresAdapter, f: impl FnOnce(&dyn PostgresAdapter) -> Result<T, String>) -> Result<Option<T>, String> {
    if !pg.configured() {
        return Ok(None);
    }
    match f(pg) {
        Ok(v) => Ok(Some(v)),
        Err(_) => Ok(None), // unreachable archive → local fallback, never break reports
    }
}

// ---------------------------------------------------------------------------
// Audit log (§6g) + officer oversight (§6c)
// ---------------------------------------------------------------------------

pub fn list_audit(conn: &Connection, filters: &AuditFilters, limit: i64) -> Result<Vec<AuditEntry>, String> {
    let mut parts: Vec<String> = vec!["1 = 1".to_string()];
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(from) = filters.from.as_deref() {
        parts.push("a.timestamp >= ?".to_string());
        params.push(SqlValue::Text(from.to_string()));
    }
    if let Some(to) = filters.to.as_deref() {
        parts.push("a.timestamp <= ?".to_string());
        params.push(SqlValue::Text(to.to_string()));
    }
    if let Some(actor) = filters.actor_id.as_deref() {
        parts.push("a.actor_id = ?".to_string());
        params.push(SqlValue::Text(actor.to_string()));
    }
    if let Some(action) = filters.action.as_deref() {
        parts.push("a.action = ?".to_string());
        params.push(SqlValue::Text(action.to_string()));
    }
    let where_sql = parts.join(" AND ");
    let sql = format!(
        "SELECT a.id, a.actor_id, u.name, a.action, a.target_id, a.details, a.timestamp
         FROM audit_log a LEFT JOIN users u ON u.id = a.actor_id
         WHERE {where_sql}
         ORDER BY a.timestamp DESC LIMIT ?"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("audit list failed: {e}"))?;
    let mut all: Vec<SqlValue> = params;
    all.push(SqlValue::Integer(limit));
    let rows = stmt
        .query_map(rusqlite::params_from_iter(all.iter()), |r| {
            let details: Option<String> = r.get(5)?;
            Ok(AuditEntry {
                id: r.get(0)?,
                actor_id: r.get(1)?,
                actor_name: r.get(2)?,
                action: r.get(3)?,
                target_id: r.get(4)?,
                details: details
                    .and_then(|s| serde_json::from_str(&s).ok()),
                timestamp: r.get(6)?,
            })
        })
        .map_err(|e| format!("audit list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("audit list read failed: {e}"))
}

pub fn list_audit_actions(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT action FROM audit_log ORDER BY action")
        .map_err(|e| format!("audit actions failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("audit actions failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("audit actions read failed: {e}"))
}

/// Aggregate per-officer activity over a period — historical/aggregate only,
/// never access to a live session (05 §6c).
pub fn list_officer_activity(
    conn: &Connection,
    from: Option<String>,
    to: Option<String>,
) -> Result<Vec<OfficerActivityView>, String> {
    // Trips logged per officer in the window (+ last activity timestamp).
    let mut trip_parts: Vec<String> = vec!["t.status = 'logged' AND t.officer_id IS NOT NULL".to_string()];
    let mut trip_params: Vec<SqlValue> = Vec::new();
    if let Some(f) = from.as_deref() {
        trip_parts.push("t.time_in >= ?".to_string());
        trip_params.push(SqlValue::Text(f.to_string()));
    }
    if let Some(t) = to.as_deref() {
        trip_parts.push("t.time_in <= ?".to_string());
        trip_params.push(SqlValue::Text(t.to_string()));
    }
    let trip_where = trip_parts.join(" AND ");

    let mut trips: Vec<(String, i64, Option<String>)> = {
        let sql = format!(
            "SELECT t.officer_id, COUNT(*), MAX(t.time_in) FROM trips t
             WHERE {trip_where} AND t.archived = 0 GROUP BY t.officer_id"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("officer trips failed: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(trip_params.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?))
            })
            .map_err(|e| format!("officer trips failed: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("officer trips read failed: {e}"))?
    };

    // Queue items resolved per officer in the window (audit-recorded actions).
    let mut audit_parts: Vec<String> =
        vec!["a.actor_id IS NOT NULL AND a.action IN ('resolved_queue_confirm','approved_trip')".to_string()];
    let mut audit_params: Vec<SqlValue> = Vec::new();
    if let Some(f) = from.as_deref() {
        audit_parts.push("a.timestamp >= ?".to_string());
        audit_params.push(SqlValue::Text(f.to_string()));
    }
    if let Some(t) = to.as_deref() {
        audit_parts.push("a.timestamp <= ?".to_string());
        audit_params.push(SqlValue::Text(t.to_string()));
    }
    let audit_where = audit_parts.join(" AND ");

    let mut resolved: Vec<(String, i64)> = {
        let sql = format!(
            "SELECT a.actor_id, COUNT(*) FROM audit_log a WHERE {audit_where} GROUP BY a.actor_id"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("officer audit failed: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(audit_params.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(|e| format!("officer audit failed: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("officer audit read failed: {e}"))?
    };
    resolved.sort_by(|a, b| a.0.cmp(&b.0));

    // Officer display names come from the users table.
    let mut names: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT id, name FROM users ORDER BY name")
            .map_err(|e| format!("officer names failed: {e}"))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| format!("officer names failed: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("officer names read failed: {e}"))?
    };
    names.sort_by(|a, b| a.0.cmp(&b.0));

    trips.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::new();
    let mut ti = 0usize;
    let mut ri = 0usize;
    for (uid, uname) in names {
        let mut trips_logged = 0i64;
        let mut last_active = None;
        while ti < trips.len() && trips[ti].0 <= uid {
            if trips[ti].0 == uid {
                trips_logged = trips[ti].1;
                last_active = trips[ti].2.clone();
            }
            ti += 1;
        }
        let mut queue_resolved = 0i64;
        while ri < resolved.len() && resolved[ri].0 <= uid {
            if resolved[ri].0 == uid {
                queue_resolved = resolved[ri].1;
            }
            ri += 1;
        }
        if trips_logged > 0 || queue_resolved > 0 {
            out.push(OfficerActivityView {
                officer_id: uid,
                officer_name: uname,
                trips_logged,
                queue_resolved,
                last_active_at: last_active,
            });
        }
    }
    out.sort_by(|a, b| b.trips_logged.cmp(&a.trips_logged).then_with(|| a.officer_name.cmp(&b.officer_name)));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Commands (read-only, permission-gated)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn report_dashboard(state: State<AppState>, actor_id: String, filters: ReportFilters) -> Result<ReportDashboard, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, REPORT_PERM)?;
    Ok(ReportDashboard {
        summary: report_summary(&conn, &filters)?,
        trips_over_time: trips_over_time(&conn, &filters)?,
        top_companies: top_companies(&conn, &filters, 10)?,
        trips_by_vehicle: trips_by_vehicle(&conn, &filters, 100)?,
        data_source: "local".to_string(),
    })
}

/// Central-archive dashboard; `None` means the adapter could not answer and
/// the caller falls back to the local store. Never propagates archive errors.
fn central_dashboard(pg: &dyn PostgresAdapter, filters: &ReportFilters) -> Result<Option<ReportDashboard>, String> {
    try_central(pg, |pg| {
        Ok(ReportDashboard {
            summary: pg_report_summary(pg, filters)?,
            trips_over_time: pg_trips_over_time(pg, filters)?,
            top_companies: pg_top_companies(pg, filters, 10)?,
            trips_by_vehicle: pg_trips_by_vehicle(pg, filters, 100)?,
            data_source: "postgres".to_string(),
        })
    })
}

#[tauri::command]
pub fn report_trips_drill(
    state: State<AppState>,
    actor_id: String,
    filters: ReportFilters,
    limit: i64,
) -> Result<Vec<crate::models::TripView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, REPORT_PERM)?;
    let limit = limit.clamp(1, 2000);
    report_trips(&conn, &filters, limit)
}

#[tauri::command]
pub fn report_export(state: State<AppState>, actor_id: String, filters: ReportFilters) -> Result<Vec<ReportExportRow>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, REPORT_PERM)?;
    report_export_rows(&conn, &filters)
}

/// Write the current filter result to a CSV file (Excel-compatible) and return
/// its absolute path (05 §5 "Excel export"). If `target_path` is provided the
/// file is written there (from the frontend's native Save-As dialog);
/// otherwise a timestamped file is created in the app's exports folder.
#[tauri::command]
pub fn report_export_csv(
    state: State<AppState>,
    actor_id: String,
    filters: ReportFilters,
    target_path: Option<String>,
) -> Result<String, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, REPORT_PERM)?;
    let rows = report_export_rows(&conn, &filters)?;
    drop(conn);

    // Build CSV and write to disk (no lock held)
    let path = if let Some(tp) = target_path.as_deref() {
        std::path::PathBuf::from(tp)
    } else {
        let dir = state.frames_dir.parent().unwrap_or(std::path::Path::new(".")).join("exports");
        std::fs::create_dir_all(&dir).map_err(|e| format!("export dir create failed: {e}"))?;
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        dir.join(format!("truckflow-report-{ts}.csv"))
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("export dir create failed: {e}"))?;
    }

    let mut csv = String::from("Trip ID,Plate,Time In,Company,Driver,Capacity,Unit,Receipt,Capture Method,Confidence\n");
    for r in &rows {
        let cell = |v: &str| -> String {
            if v.contains([',', '"', '\n']) { format!("\"{}\"", v.replace('"', "\"\"")) } else { v.to_string() }
        };
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{}\n",
            cell(&r.id),
            cell(&r.plate),
            cell(&r.time_in),
            cell(&r.company),
            cell(&r.driver),
            r.capacity_at_trip.map(|v| v.to_string()).unwrap_or_default(),
            cell(&r.capacity_unit),
            cell(r.receipt_no.as_deref().unwrap_or("")),
            cell(&r.capture_method),
            r.confidence_score.map(|v| v.to_string()).unwrap_or_default(),
        ));
    }
    std::fs::write(&path, csv).map_err(|e| format!("export write failed: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Write the current filter result to a real `.xlsx` workbook (rust_xlsxwriter)
/// and return its absolute path. If `target_path` is provided the file is
/// written there (from the frontend's native Save-As dialog).
#[tauri::command]
pub fn report_export_xlsx(
    state: State<AppState>,
    actor_id: String,
    filters: ReportFilters,
    target_path: Option<String>,
) -> Result<String, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, REPORT_PERM)?;
    let rows = report_export_rows(&conn, &filters)?;
    drop(conn);

    let path = if let Some(tp) = target_path.as_deref() {
        std::path::PathBuf::from(tp)
    } else {
        let dir = state.frames_dir.parent().unwrap_or(std::path::Path::new(".")).join("exports");
        std::fs::create_dir_all(&dir).map_err(|e| format!("export dir create failed: {e}"))?;
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        dir.join(format!("truckflow-report-{ts}.xlsx"))
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("export dir create failed: {e}"))?;
    }

    let mut workbook = rust_xlsxwriter::Workbook::new();
    let worksheet = workbook.add_worksheet();
    let headers = [
        "Trip ID", "Plate", "Time In", "Company", "Driver", "Capacity", "Unit",
        "Receipt", "Capture Method", "Confidence",
    ];
    for (i, h) in headers.iter().enumerate() {
        let _ = worksheet.write_string(0, i as u16, *h);
    }
    for (ri, r) in rows.iter().enumerate() {
        let row = ri as u32 + 1;
        let _ = worksheet.write_string(row, 0, &r.id);
        let _ = worksheet.write_string(row, 1, &r.plate);
        let _ = worksheet.write_string(row, 2, &r.time_in);
        let _ = worksheet.write_string(row, 3, &r.company);
        let _ = worksheet.write_string(row, 4, &r.driver);
        match r.capacity_at_trip {
            Some(v) => { let _ = worksheet.write_number(row, 5, v); }
            None => { let _ = worksheet.write_string(row, 5, ""); }
        }
        let _ = worksheet.write_string(row, 6, &r.capacity_unit);
        let _ = worksheet.write_string(row, 7, r.receipt_no.as_deref().unwrap_or(""));
        let _ = worksheet.write_string(row, 8, &r.capture_method);
        match r.confidence_score {
            Some(v) => { let _ = worksheet.write_number(row, 9, v); }
            None => { let _ = worksheet.write_string(row, 9, ""); }
        }
    }
    workbook
        .save(&path)
        .map_err(|e| format!("xlsx save failed: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn list_audit_log(state: State<AppState>, actor_id: String, filters: AuditFilters) -> Result<Vec<AuditEntry>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, AUDIT_PERM)?;
    list_audit(&conn, &filters, 500)
}

#[tauri::command]
pub fn list_audit_actions_command(state: State<AppState>, actor_id: String) -> Result<Vec<String>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, AUDIT_PERM)?;
    list_audit_actions(&conn)
}

#[tauri::command]
pub fn officer_activity(
    state: State<AppState>,
    actor_id: String,
    from: Option<String>,
    to: Option<String>,
) -> Result<Vec<OfficerActivityView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, AUDIT_PERM)?;
    list_officer_activity(&conn, from, to)
}

/// Batch-delete audit entries so the log stays manageable (it can pile up).
/// Gated on `manage_users`: deleting audit history is an admin power, not a
/// read-only privilege.
#[tauri::command]
pub fn delete_audit_entries(
    state: State<AppState>,
    actor_id: String,
    entry_ids: Vec<String>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    let mut deleted: usize = 0;
    for id in &entry_ids {
        deleted += conn
            .execute("DELETE FROM audit_log WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| format!("audit delete failed: {e}"))?;
    }
    crate::db::append_audit(
        &conn,
        &actor_id,
        "deleted_audit_entries",
        None,
        Some(serde_json::json!({ "count": entry_ids.len() })),
    )?;
    Ok(deleted as i64)
}
