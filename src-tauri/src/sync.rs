//! Phase 4 — Sync layer (06-data-flow.md §5, 02-architecture.md §3).
//!
//! Two independent, best-effort pipelines, each with its own flag and its own
//! adapter so one failing never affects the other:
//!
//! - **PostgreSQL sync** (one-way local → central, `synced` flag): companies,
//!   drivers, vehicles, users, trips. Runs automatically on connectivity with
//!   retry; IDs are client-side UUIDs so reconnects can never duplicate rows.
//! - **Google Sheets export** (`pushed_to_sheets` flag): logged trips only, at
//!   the configured frequency. OAuth/sheet selection are represented by the
//!   `integrations` row; the provider adapter is mockable for dev.
//!
//! Adapters are trait objects injected via `AppState`, so the real Rust
//! Postgres/Sheets drivers are swappable without touching the sync engine.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use postgres::types::ToSql;
use rusqlite::{Connection, params};
use rusqlite::types::ValueRef;
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, State};

use crate::db::{append_audit, now_iso, AppState};
use crate::models::{PgSyncStateView, SheetsStateView, SyncRunResult, SyncStatusView, TablePending};

const INTEGRATION_PERM: &str = "manage_integrations";

// ---------------------------------------------------------------------------
// Adapters — mock in dev, real drivers swappable behind the same traits
// ---------------------------------------------------------------------------

/// A PostgreSQL sink. The real implementation (Rust `postgres` crate, future
/// feature) will connect to the central DB; the mock records pushes and can be
/// toggled offline to simulate connectivity for the offline-first checklist.
pub trait PostgresAdapter: Send + Sync {
    fn label(&self) -> &str;
    fn connected(&self) -> bool;
    /// Push one or more rows for a table. Returns the ids PostgreSQL confirmed
    /// receiving (only those get `synced = 1` locally). Err = connectivity or
    /// server failure — rows stay pending and are retried on the next run.
    fn push_rows(&self, table: &str, rows: &[serde_json::Value]) -> Result<Vec<String>, String>;
    /// Dev-only connectivity simulation; no-op for real adapters.
    fn simulate_connectivity(&self, _online: bool) -> Result<(), String> {
        Err("connectivity simulation is only available on the mock adapter".to_string())
    }
    /// Whether a real target has been configured (mocks always report true).
    fn configured(&self) -> bool {
        true
    }
    /// Most recent failure detail, surfaced in the UI (None = no error).
    fn last_error(&self) -> Option<String> {
        None
    }
    /// Apply or clear this adapter's configuration. `None` disconnects.
    /// Err = the configuration is invalid or unreachable.
    fn configure(&self, _conn_string: Option<String>) -> Result<(), String> {
        Err("this adapter does not support runtime configuration".to_string())
    }
    /// Physically remove rows by id from the central side (hard delete).
    /// Rows that were never pushed simply aren't found — no-op is fine.
    fn delete_rows(&self, _table: &str, _ids: &[String]) -> Result<(), String> {
        Err("this adapter does not support deletes".to_string())
    }
    /// Run a read-only SQL query against the central mirror, returning rows as
    /// JSON objects keyed by column name. Used by the reporting repoint (Phase 5:
    /// reports read the permanent archive). Adapters that cannot answer (the mock)
    /// return an error so callers fall back to the local store.
    fn query_rows(&self, _sql: &str, _params: &[String]) -> Result<Vec<serde_json::Value>, String> {
        Err("central read queries are not supported by this adapter".to_string())
    }
    /// Add a missing column to a table in the central database.
    /// Used when a push fails due to a column not existing in the remote schema.
    fn add_missing_column(&self, _table: &str, _column_name: &str) -> Result<(), String> {
        Err("adding columns is not supported by this adapter".to_string())
    }
}

#[derive(Default)]
pub struct MockPostgres {
    online: AtomicBool,
    pushed: Mutex<Vec<(String, String, serde_json::Value)>>,
    configured_flag: AtomicBool,
    deleted: Mutex<Vec<(String, String)>>,
}

impl MockPostgres {
    pub fn new() -> Self {
        Self {
            online: AtomicBool::new(true),
            pushed: Mutex::new(Vec::new()),
            configured_flag: AtomicBool::new(true),
            deleted: Mutex::new(Vec::new()),
        }
    }
    /// Rows actually pushed, in order: (table, id, row).
    pub fn pushed(&self) -> Vec<(String, String, serde_json::Value)> {
        self.pushed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Rows deleted centrally: (table, id).
    pub fn deleted(&self) -> Vec<(String, String)> {
        self.deleted.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl PostgresAdapter for MockPostgres {
    fn label(&self) -> &str {
        "mock-postgres"
    }
    fn connected(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
    fn push_rows(&self, table: &str, rows: &[serde_json::Value]) -> Result<Vec<String>, String> {
        if !self.connected() {
            return Err("PostgreSQL unreachable (simulated offline)".to_string());
        }
        let mut acked = Vec::with_capacity(rows.len());
        let mut held = self.pushed.lock().unwrap_or_else(|e| e.into_inner());
        for row in rows {
            let id = row["id"].as_str().unwrap_or_default().to_string();
            held.push((table.to_string(), id.clone(), row.clone()));
            acked.push(id);
        }
        Ok(acked)
    }
    fn simulate_connectivity(&self, online: bool) -> Result<(), String> {
        self.online.store(online, Ordering::SeqCst);
        Ok(())
    }
    fn configured(&self) -> bool {
        self.configured_flag.load(Ordering::SeqCst)
    }
    fn configure(&self, conn_string: Option<String>) -> Result<(), String> {
        self.configured_flag.store(conn_string.is_some(), Ordering::SeqCst);
        Ok(())
    }
    fn delete_rows(&self, table: &str, ids: &[String]) -> Result<(), String> {
        let mut held = self.deleted.lock().unwrap_or_else(|e| e.into_inner());
        for id in ids {
            held.push((table.to_string(), id.clone()));
        }
        Ok(())
    }
}

/// Google Sheets sink. `push_trips` receives finalized logged trips; the mock
/// records them and can be toggled offline (e.g. revoked OAuth token) to prove
/// sheets failure never touches Postgres sync or local capture.
pub trait SheetsProvider: Send + Sync {
    fn label(&self) -> &str;
    fn connected(&self) -> bool;
    fn push_trips(&self, rows: &[serde_json::Value], mapping: &[crate::models::SheetColumnEntry]) -> Result<Vec<String>, String>;
    fn ensure_header_row(&self, _mapping: &[crate::models::SheetColumnEntry]) -> Result<(), String> { Ok(()) }
    fn push_new_rows(&self, rows: &[serde_json::Value], mapping: &[crate::models::SheetColumnEntry]) -> Result<Vec<String>, String> {
        self.push_trips(rows, mapping)
    }
    fn update_existing_rows(&self, _rows: &[serde_json::Value], _mapping: &[crate::models::SheetColumnEntry]) -> Result<(), String> { Ok(()) }
    fn set_sheet_mapping(&self, _mapping: &[crate::models::SheetColumnEntry]) {} // no-op for mocks
    fn simulate_connectivity(&self, _online: bool) -> Result<(), String> {
        Err("connectivity simulation is only available on the mock adapter".to_string())
    }
    /// Whether credentials + target have been configured (mocks always true).
    fn configured(&self) -> bool {
        true
    }
    /// Most recent failure detail, surfaced in the UI (None = no error).
    fn last_error(&self) -> Option<String> {
        None
    }
    /// Service-account email that owns the export (display only).
    fn service_account_email(&self) -> Option<String> {
        None
    }
    /// Apply or clear this provider's configuration. Returns the service
    /// account email on success; `None`/`None` disconnects.
    fn configure(&self, _json: Option<String>, _sheet_id: Option<String>) -> Result<String, String> {
        Err("this provider does not support runtime configuration".to_string())
    }
    /// Remove rows from the sheet: rows older than `cutoff_iso` (ISO datetime,
    /// None = no age limit) and/or rows whose trip id is in `excluded_ids`.
    /// The header row is always kept. Returns how many data rows were removed.
    fn prune(&self, _cutoff_iso: Option<&str>, _excluded_ids: &[String]) -> Result<usize, String> {
        Err("this provider does not support pruning".to_string())
    }
    /// Read all trip IDs (column 0) from the sheet data rows. Used for
    /// deduplication: prevents re-appending a trip that already exists in
    /// the sheet but lost its local `sheet_row` reference.
    fn read_existing_trip_ids(&self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
pub struct MockSheets {
    online: AtomicBool,
    pushed: Mutex<Vec<serde_json::Value>>,
    configured_flag: AtomicBool,
    email: Mutex<Option<String>>,
}

impl MockSheets {
    pub fn new() -> Self {
        Self {
            online: AtomicBool::new(true),
            pushed: Mutex::new(Vec::new()),
            configured_flag: AtomicBool::new(true),
            email: Mutex::new(None),
        }
    }
    pub fn pushed(&self) -> Vec<serde_json::Value> {
        self.pushed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl SheetsProvider for MockSheets {
    fn label(&self) -> &str {
        "mock-sheets"
    }
    fn connected(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
    fn push_trips(&self, rows: &[serde_json::Value], _mapping: &[crate::models::SheetColumnEntry]) -> Result<Vec<String>, String> {
        if !self.connected() {
            return Err("Google Sheets unreachable (simulated revoked/offline)".to_string());
        }
        let mut acked = Vec::with_capacity(rows.len());
        let mut held = self.pushed.lock().unwrap_or_else(|e| e.into_inner());
        for row in rows {
            let id = row["id"].as_str().unwrap_or_default().to_string();
            held.push(row.clone());
            acked.push(id);
        }
        Ok(acked)
    }
    fn simulate_connectivity(&self, online: bool) -> Result<(), String> {
        self.online.store(online, Ordering::SeqCst);
        Ok(())
    }
    fn configured(&self) -> bool {
        self.configured_flag.load(Ordering::SeqCst)
    }
    fn service_account_email(&self) -> Option<String> {
        self.email.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn configure(&self, json: Option<String>, sheet_id: Option<String>) -> Result<String, String> {
        let (Some(j), Some(_)) = (json, sheet_id) else {
            self.configured_flag.store(false, Ordering::SeqCst);
            *self.email.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Ok("disconnected".to_string());
        };
        self.configured_flag.store(true, Ordering::SeqCst);
        let email = serde_json::from_str::<serde_json::Value>(&j)
            .ok()
            .and_then(|v| v["client_email"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "mock-sheets@local".to_string());
        *self.email.lock().unwrap_or_else(|e| e.into_inner()) = Some(email.clone());
        Ok(email)
    }
    fn prune(&self, _cutoff_iso: Option<&str>, _excluded_ids: &[String]) -> Result<usize, String> {
        Ok(0)
    }
}

pub fn mock_postgres() -> Arc<dyn PostgresAdapter> {
    Arc::new(MockPostgres::new())
}

pub fn mock_sheets() -> Arc<dyn SheetsProvider> {
    Arc::new(MockSheets::new())
}

// ---------------------------------------------------------------------------
// Postgres sync engine
// ---------------------------------------------------------------------------

/// Syncable tables (all carry the `synced` flag). Order matters: reference data
/// first, trips last, so central never receives a trip before its vehicle.
pub const PG_SYNC_TABLES: &[(&str, &str)] = &[
    ("companies", "Companies"),
    ("drivers", "Drivers"),
    ("vehicles", "Vehicles"),
    ("users", "Users"),
    ("trips", "Trips"),
];

fn rows_where_not_synced(conn: &Connection, table: &str) -> Result<Vec<serde_json::Value>, String> {
    // Trips must have BOTH entry AND exit before pushing to Supabase.
    // A trip without exit is still in-progress locally.
    let where_clause = if table == "trips" {
        "synced = 0 AND entry_time IS NOT NULL AND exit_time IS NOT NULL"
    } else {
        "synced = 0"
    };
    let sql = format!("SELECT * FROM {table} WHERE {where_clause} ORDER BY created_at ASC");
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("{table} scan failed: {e}"))?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([]).map_err(|e| format!("{table} scan failed: {e}"))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| format!("{table} scan failed: {e}"))? {
        let mut obj = serde_json::Map::new();
        for (i, name) in names.iter().enumerate() {
            match row.get_ref(i).map_err(|e| format!("{table} read failed: {e}"))? {
                ValueRef::Null => {
                    obj.insert(name.clone(), serde_json::Value::Null);
                }
                ValueRef::Integer(n) => {
                    obj.insert(name.clone(), serde_json::json!(n));
                }
                ValueRef::Real(f) => {
                    obj.insert(name.clone(), serde_json::json!(f));
                }
                ValueRef::Text(t) => {
                    obj.insert(name.clone(), serde_json::Value::String(String::from_utf8_lossy(t).into_owned()));
                }
                ValueRef::Blob(_) => {}
            }
        }
        out.push(serde_json::Value::Object(obj));
    }
    Ok(out)
}

/// One full Postgres sync pass: push every unsynced row, in table order, and
/// flip `synced = 1` only for ids the central side confirmed. Idempotent — a
/// row that fails to ack simply stays pending for the next pass (02 §3).
pub fn run_pg_sync_impl(conn: &Connection, pg: &dyn PostgresAdapter) -> Result<SyncRunResult, String> {
    let mut tables = Vec::new();
    let mut total_pushed = 0i64;
    for (name, display) in PG_SYNC_TABLES {
        let pending = pending_for_table(conn, name)?;
        let mut acked = 0i64;
        if pending > 0 && pg.configured() {
            let rows = rows_where_not_synced(conn, name)?;
            if !rows.is_empty() {
                // Push failure is non-fatal — skip this table, rows stay pending for next cycle.
                if let Ok(ids) = pg.push_rows(name, &rows) {
                    for id in &ids {
                        conn.execute(&format!("UPDATE {name} SET synced = 1 WHERE id = ?1"), params![id])
                            .map_err(|e| format!("{name} flag flip failed: {e}"))?;
                    }
                    acked = ids.len() as i64;
                }
            }
        }
        tables.push(TablePending {
            table: name.to_string(),
            display: display.to_string(),
            pending,
        });
        total_pushed += acked;
    }
    set_setting(conn, "pg_last_synced_at", &now_iso())?;
    Ok(SyncRunResult {
        pushed: total_pushed,
        tables,
        last_run_at: Some(now_iso()),
    })
}

pub fn pending_for_table(conn: &Connection, table: &str) -> Result<i64, String> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE synced = 0"), [], |r| r.get(0))
        .map_err(|e| format!("{table} count failed: {e}"))
}

/// Record deleted IDs that need to be synced to central DB.
/// Called when a record is hard-deleted locally.
pub fn record_deleted_ids(conn: &Connection, table: &str, ids: &[String]) -> Result<(), String> {
    let now = now_iso();
    crate::log::log(&format!("[sync] record_deleted_ids: table={} ids={:?}", table, ids));
    for id in ids {
        conn.execute(
            "INSERT OR IGNORE INTO pending_deletes (table_name, row_id, deleted_at) VALUES (?1, ?2, ?3)",
            params![table, id, now],
        )
        .map_err(|e| format!("record delete failed: {e}"))?;
    }
    crate::log::log(&format!("[sync] record_deleted_ids: recorded {} deletes for {}", ids.len(), table));
    Ok(())
}

/// Get all pending deletes for a specific table.
pub fn get_pending_deletes_for_table(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT row_id FROM pending_deletes WHERE table_name = ?1 ORDER BY deleted_at ASC")
        .map_err(|e| format!("pending deletes query failed: {e}"))?;
    let ids = stmt
        .query_map(params![table], |r| r.get(0))
        .map_err(|e| format!("pending deletes query failed: {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(ids)
}

/// Get all pending deletes across all sync tables.
pub fn get_all_pending_deletes(conn: &Connection) -> Result<Vec<(String, String)>, String> {
    let mut stmt = conn
        .prepare("SELECT table_name, row_id FROM pending_deletes ORDER BY deleted_at ASC")
        .map_err(|e| format!("pending deletes query failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| format!("pending deletes query failed: {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Remove IDs from the pending_deletes table after successful central deletion.
pub fn clear_pending_deletes(conn: &Connection, table: &str, ids: &[String]) -> Result<(), String> {
    for id in ids {
        conn.execute(
            "DELETE FROM pending_deletes WHERE table_name = ?1 AND row_id = ?2",
            params![table, id],
        )
        .map_err(|e| format!("clear pending delete failed: {e}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Split-phase helpers for lock-free sync (called from spawn_sync_poller).
// Each phase is either DB-only (needs lock) or network-only (no lock).
// ---------------------------------------------------------------------------

/// Phase 1 (lock): collect all unsynced rows from a table.
pub fn collect_unsynced_rows(conn: &Connection, table: &str) -> Result<Vec<serde_json::Value>, String> {
    rows_where_not_synced(conn, table)
}

/// Phase 2 (NO lock): push rows to central DB via network.
pub fn push_rows_to_central(pg: &dyn PostgresAdapter, table: &str, rows: &[serde_json::Value]) -> Result<Vec<String>, String> {
    if rows.is_empty() {
        return Ok(vec![]);
    }
    if !pg.configured() {
        return Ok(vec![]);
    }
    pg.push_rows(table, rows).map_err(|e| format!("{table} push failed: {e}"))
}

/// Phase 3 (lock): mark rows as synced after successful push.
pub fn mark_rows_synced(conn: &Connection, table: &str, ids: &[String]) -> Result<(), String> {
    for id in ids {
        conn.execute(&format!("UPDATE {table} SET synced = 1 WHERE id = ?1"), params![id])
            .map_err(|e| format!("{table} flag flip failed: {e}"))?;
    }
    Ok(())
}

/// Phase 1 (NO lock): fetch rows from central DB via network.
pub fn fetch_central_rows(pg: &dyn PostgresAdapter, table: &str, since: &str) -> Result<Vec<serde_json::Value>, String> {
    let sql = format!(
        "SELECT * FROM {} WHERE updated_at > $1 ORDER BY updated_at ASC",
        pg_quote_ident(table)
    );
    pg.query_rows(&sql, &[since.to_string()]).map_err(|e| format!("{table} pull failed: {e}"))
}

/// Phase 2 (lock): upsert central rows into local DB.
pub fn upsert_central_rows(conn: &Connection, table: &str, central_rows: &[serde_json::Value]) -> Result<i64, String> {
    if central_rows.is_empty() {
        return Ok(0);
    }
    let local_timestamps = batch_load_local_timestamps(conn, table)?;
    let mut pulled = 0i64;
    for row in central_rows {
        let Some(obj) = row.as_object() else { continue };
        let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() { continue }
        let central_updated = obj.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(ref local_ts) = local_timestamps.get(id) {
            if local_ts.as_str() >= central_updated {
                continue;
            }
        }
        upsert_row_from_central(conn, table, obj)?;
        pulled += 1;
    }
    Ok(pulled)
}

/// Fetch archive trips from central DB within a date range. Used by the
/// "Load from Archive" feature to pull historical trips for offline reporting.
pub fn fetch_archive_trips(pg: &dyn PostgresAdapter, from: &str, to: &str) -> Result<Vec<serde_json::Value>, String> {
    let from_extended = crate::reporting::extend_bare_date_to_end_of_day(from);
    let sql = "SELECT * FROM trips WHERE time_in >= $1 AND time_in <= $2 ORDER BY time_in ASC".to_string();
    pg.query_rows(&sql, &[from_extended, to.to_string()])
        .map_err(|e| format!("archive trips pull failed: {e}"))
}

/// Load archive trips into local SQLite. Fetches from PostgreSQL within the
/// given date range and upserts into local storage. Returns the count of
/// trips loaded.
pub fn load_archive_trips_impl(conn: &Connection, pg: &dyn PostgresAdapter, from: &str, to: &str) -> Result<i64, String> {
    let rows = fetch_archive_trips(pg, from, to)?;
    upsert_central_rows(conn, "trips", &rows)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DateRangePreset {
    pub label: String,
    pub from: String,
    pub to: String,
}

fn today_end_of_day() -> String {
    chrono::Local::now().format("%Y-%m-%dT23:59:59Z").to_string()
}

fn days_ago(days: i64) -> String {
    (chrono::Local::now() - chrono::Duration::days(days)).format("%Y-%m-%dT00:00:00Z").to_string()
}

fn months_ago(months: i64) -> String {
    let dt = chrono::Local::now() - chrono::Duration::days(months * 30);
    dt.format("%Y-%m-%dT00:00:00Z").to_string()
}

/// Returns the available date range presets for the "Load from Archive" selector.
#[tauri::command]
pub fn get_date_range_presets() -> Vec<DateRangePreset> {
    let to = today_end_of_day();
    vec![
        DateRangePreset { label: "Last 30 days".to_string(), from: days_ago(30), to: to.clone() },
        DateRangePreset { label: "Last 90 days".to_string(), from: days_ago(90), to: to.clone() },
        DateRangePreset { label: "Last 6 months".to_string(), from: months_ago(6), to: to.clone() },
        DateRangePreset { label: "Last year".to_string(), from: months_ago(12), to: to.clone() },
    ]
}

/// Pull reference data (companies, vehicles, drivers) from central DB.
/// Uses last-edit-wins-by-timestamp: if the central row is newer than the
/// local row, the central version wins. Local edits that haven't been pushed
/// yet are preserved (they'll push on next sync).
pub const REFERENCE_TABLES: &[&str] = &["companies", "drivers", "vehicles"];

pub fn pull_reference_data(conn: &Connection, pg: &dyn PostgresAdapter) -> Result<PullResult, String> {
    if !pg.connected() {
        return Ok(PullResult { pulled: 0, tables: vec![] });
    }
    let last_pull = get_setting(conn, "pg_last_pulled_at").unwrap_or_default();
    let mut total_pulled = 0i64;
    let mut tables = Vec::new();

    for &table in REFERENCE_TABLES {
        // Query central for rows newer than our last pull
        let sql = format!(
            "SELECT * FROM {} WHERE updated_at > $1 ORDER BY updated_at ASC",
            pg_quote_ident(table)
        );
        let central_rows = pg.query_rows(&sql, &[last_pull.clone()])
            .map_err(|e| format!("{table} pull failed: {e}"))?;

        if central_rows.is_empty() {
            tables.push(TablePending { table: table.to_string(), display: table.to_string(), pending: 0 });
            continue;
        }

        // Batch-load all local timestamps in one query instead of per-row
        let local_timestamps = batch_load_local_timestamps(conn, table)?;

        let mut pulled = 0i64;
        for row in &central_rows {
            let Some(obj) = row.as_object() else { continue };
            let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() { continue }
            let central_updated = obj.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");

            // Check local timestamp from batch (O(1) lookup instead of per-row query)
            if let Some(ref local_ts) = local_timestamps.get(id) {
                if local_ts.as_str() >= central_updated {
                    continue; // local is same age or newer — skip
                }
            }

            // Upsert from central (central is newer)
            upsert_row_from_central(conn, table, obj)?;
            pulled += 1;
        }
        tables.push(TablePending { table: table.to_string(), display: table.to_string(), pending: 0 });
        total_pulled += pulled;
    }

    set_setting(conn, "pg_last_pulled_at", &now_iso())?;
    Ok(PullResult { pulled: total_pulled, tables })
}

/// Load all (id, updated_at) pairs for a table in one query.
fn batch_load_local_timestamps(conn: &Connection, table: &str) -> Result<std::collections::HashMap<String, String>, String> {
    let sql = format!("SELECT id, updated_at FROM {} WHERE updated_at IS NOT NULL", pg_quote_ident(table));
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("{table} timestamp query failed: {e}"))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| format!("{table} timestamp query failed: {e}"))?;
    let mut map = std::collections::HashMap::new();
    for row in rows {
        if let Ok((id, ts)) = row {
            map.insert(id, ts);
        }
    }
    Ok(map)
}

fn upsert_row_from_central(conn: &Connection, table: &str, row: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let keys: Vec<&str> = row.keys().map(|s| s.as_str()).collect();
    if keys.is_empty() { return Ok(()) }

    // Build column list and placeholders
    let columns: Vec<String> = keys.iter().map(|k| pg_quote_ident(k)).collect();
    let placeholders: Vec<String> = (1..=keys.len()).map(|i| format!("?{i}")).collect();
    let update_clause: Vec<String> = keys.iter()
        .filter(|k| **k != "id")
        .map(|k| format!("{col} = excluded.{col}", col = pg_quote_ident(k)))
        .collect();

    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})
         ON CONFLICT (id) DO UPDATE SET {}",
        pg_quote_ident(table),
        columns.join(", "),
        placeholders.join(", "),
        update_clause.join(", ")
    );

    let params: Vec<Box<dyn rusqlite::types::ToSql>> = keys.iter()
        .map(|k| {
            let val = row.get(*k).cloned().unwrap_or(serde_json::Value::Null);
            match val {
                serde_json::Value::String(s) => Box::new(s) as Box<dyn rusqlite::types::ToSql>,
                serde_json::Value::Number(n) => {
                    // Store all numbers as text to avoid i64/f64 type mismatch
                    Box::new(n.to_string()) as Box<dyn rusqlite::types::ToSql>
                }
                serde_json::Value::Bool(b) => Box::new(b as i32),
                serde_json::Value::Null => Box::new(rusqlite::types::Value::Null),
                _ => Box::new(val.to_string()),
            }
        })
        .collect();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    conn.execute(&sql, param_refs.as_slice())
        .map_err(|e| format!("{table} upsert failed for id={}: {e}", row.get("id").unwrap_or(&serde_json::Value::Null)))?;
    Ok(())
}

#[derive(Default)]
pub struct PullResult {
    pub pulled: i64,
    pub tables: Vec<TablePending>,
}

pub fn pg_sync_state_impl(conn: &Connection, pg: &dyn PostgresAdapter) -> Result<PgSyncStateView, String> {
    let mut tables = Vec::new();
    for (name, display) in PG_SYNC_TABLES {
        tables.push(TablePending {
            table: name.to_string(),
            display: display.to_string(),
            pending: pending_for_table(conn, name)?,
        });
    }
    Ok(PgSyncStateView {
        connected: pg.connected(),
        adapter: pg.label().to_string(),
        tables,
        last_synced_at: get_setting(conn, "pg_last_synced_at"),
        configured: pg.configured(),
        last_error: pg.last_error(),
        trip_retention_days: get_setting(conn, "trip_retention_days").and_then(|s| s.parse().ok()),
    })
}

fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map(|_| ())
    .map_err(|e| format!("app_settings write failed: {e}"))
}

fn get_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM app_settings WHERE key = ?1", params![key], |r| r.get(0)).ok()
}

// ---------------------------------------------------------------------------
// Google Sheets integration + sync
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Configurable sheet column mapping
// ---------------------------------------------------------------------------

/// All available field keys that can be mapped to sheet columns.
pub const AVAILABLE_SHEET_FIELDS: &[(&str, &str)] = &[
    ("id", "Trip ID"),
    ("plate", "Plate"),
    ("entry_time", "Entry time"),
    ("exit_time", "Exit time"),
    ("trip_status", "Trip status"),
    ("company", "Company"),
    ("driver", "Driver"),
    ("capacity_at_trip", "Capacity"),
    ("capacity_unit", "Unit"),
    ("officer_name", "Officer"),
    ("receipt_no", "Receipt no"),
    ("confidence_score", "Confidence"),
    ("capture_method", "Capture method"),
    ("is_discharge_trip", "Discharge trip"),
    ("created_at", "Created at"),
    ("model_version", "Model version"),
    ("ocr_engine", "OCR engine"),
];

/// The default column mapping when Google Sheets is first connected.
pub fn default_sheet_mapping() -> Vec<crate::models::SheetColumnEntry> {
    use crate::models::SheetColumnEntry;
    let enabled_keys = [
        "id", "plate", "entry_time", "exit_time",
        "company", "driver", "capacity_at_trip", "capacity_unit", "officer_name",
    ];
    AVAILABLE_SHEET_FIELDS
        .iter()
        .map(|(key, label)| SheetColumnEntry {
            field_key: key.to_string(),
            header: label.to_string(),
            enabled: enabled_keys.contains(key),
        })
        .collect()
}

/// Read the current sheet column mapping (from settings, or defaults)
/// and merge any custom parent fields from field_definitions so they
/// appear as available sheet columns.
pub fn read_sheet_mapping(conn: &Connection) -> Vec<crate::models::SheetColumnEntry> {
    use crate::models::SheetColumnEntry;
    let mut mapping: Vec<SheetColumnEntry> = get_setting(conn, "sheet_column_mapping")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(default_sheet_mapping);
    // Collect existing field_keys so we don't duplicate built-in entries.
    let existing: std::collections::HashSet<String> = mapping.iter().map(|e| e.field_key.clone()).collect();
    // Add custom parent fields from field_definitions that aren't already present.
    let custom_fields = list_custom_sheet_fields(conn);
    for (key, label) in custom_fields {
        if !existing.contains(&key) {
            mapping.push(SheetColumnEntry {
                field_key: key,
                header: label,
                enabled: false, // disabled by default — user must opt in
            });
        }
    }
    mapping
}

/// Query field_definitions for custom parent fields that can be exported
/// as sheet columns. These are fields whose entity_type has a parent in
/// the trip query (e.g. vehicle custom fields stored in extra_fields).
fn list_custom_sheet_fields(conn: &Connection) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for entity_type in &["vehicle", "company", "driver"] {
        let Ok(mut stmt) = conn.prepare(
            "SELECT field_key, field_label FROM field_definitions
             WHERE entity_type = ?1 AND is_standard = 0 AND is_hidden = 0
             ORDER BY sort_order, field_label",
        ) else {
            continue;
        };
        let Ok(rows) = stmt.query_map(params![entity_type], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) else {
            continue;
        };
        let collected: Vec<(String, String)> = rows.flatten().collect();
        for (key, label) in collected {
            result.push((
                format!("{entity_type}_extra_{}", key),
                format!("{label} ({entity_type})"),
            ));
        }
    }
    result
}

#[tauri::command]
pub fn get_sheet_column_mapping(state: State<AppState>) -> Result<Vec<crate::models::SheetColumnEntry>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    Ok(read_sheet_mapping(&conn))
}

#[tauri::command]
pub fn set_sheet_column_mapping(
    state: State<AppState>,
    actor_id: String,
    mapping: Vec<crate::models::SheetColumnEntry>,
) -> Result<Vec<crate::models::SheetColumnEntry>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, "manage_integrations")?;
    let json = serde_json::to_string(&mapping).map_err(|e| format!("serialize failed: {e}"))?;
    set_setting(&conn, "sheet_column_mapping", &json)
        .map_err(|e| format!("save mapping failed: {e}"))?;
    append_audit(&conn, &actor_id, "updated_sheet_column_mapping", None, None)?;
    drop(conn);
    // Update the in-memory mapping on the sheets provider so the next sync uses it immediately.
    state.sheets.set_sheet_mapping(&mapping);
    Ok(mapping)
}

pub fn sheets_state_impl(conn: &Connection, sheets: &dyn SheetsProvider) -> Result<SheetsStateView, String> {
    let (connected, target_sheet_id, shared_group, sync_frequency, last_synced_at, status) = conn
        .query_row(
            "SELECT status, target_sheet_id, shared_group, sync_frequency, last_synced_at, status
             FROM integrations WHERE type = 'google_sheets' ORDER BY created_at DESC LIMIT 1",
            [],
            |r| {
                let status: String = r.get(0)?;
                Ok((
                    status == "connected",
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    status,
                ))
            },
        )
        .unwrap_or_else(|_| {
            // Not yet connected: report a clean disconnected state rather than an error.
            (
                false,
                None,
                None,
                "realtime".to_string(),
                None,
                "disconnected".to_string(),
            )
        });
    Ok(SheetsStateView {
        connected: sheets.connected() && connected,
        adapter: sheets.label().to_string(),
        pending: pending_sheets_trips(conn)?,
        target_sheet_id,
        shared_group,
        frequency: sync_frequency,
        last_synced_at,
        status,
        configured: sheets.configured(),
        service_account_email: sheets.service_account_email(),
        last_error: sheets.last_error(),
        retention_days: get_setting(conn, "sheets_retention_days").and_then(|s| s.parse().ok()),
    })
}

/// Only trips that are sheet-eligible are counted: auto-detected plates that
/// matched the reference DB push themselves (08-anpr-integration.md §9), while
/// manual entries push only after the officer classifies them as discharge
/// (`is_discharge_trip = 1`); non-discharge and unclassified manual entries
/// stay local and never reach the sheet.
fn pending_sheets_trips(conn: &Connection) -> Result<i64, String> {
    conn.query_row(
        "SELECT COUNT(*) FROM trips
         WHERE status = 'logged'
           AND (capture_method = 'auto' OR is_discharge_trip = 1)
           AND (
             (sheet_row IS NULL)
             OR (sheet_row IS NOT NULL AND sheet_exit_pushed = 0 AND exit_time IS NOT NULL)
           )",
        [],
        |r| r.get(0),
    )
    .map_err(|e| format!("sheets pending count failed: {e}"))
}

fn sheet_trip_rows_filtered(conn: &Connection, has_sheet_row: bool) -> Result<Vec<serde_json::Value>, String> {
    let row_filter = if has_sheet_row {
        // Exit updates: trip is already in the sheet but exit hasn't been pushed yet.
        "t.sheet_row IS NOT NULL AND t.sheet_exit_pushed = 0"
    } else {
        "t.sheet_row IS NULL"
    };
    let sql = format!(
        "SELECT t.id, COALESCE(v.plate_number, json_extract(t.resolution_notes, '$.plate'), '') AS plate,
                COALESCE(t.entry_time, t.time_in) AS entry_time, t.exit_time, t.trip_status,
                t.capacity_at_trip, t.capacity_unit, t.receipt_no, t.confidence_score,
                t.capture_method, t.is_discharge_trip, t.created_at,
                COALESCE(c.name, '') AS company, COALESCE(d.name, '') AS driver,
                u.name AS officer_name, t.model_version, t.ocr_engine, t.status,
                t.sheet_row,
                v.extra_fields AS vehicles_extra,
                c.extra_fields AS companies_extra,
                d.extra_fields AS drivers_extra
         FROM trips t
         LEFT JOIN vehicles v ON v.id = t.vehicle_id
         LEFT JOIN companies c ON c.id = t.company_id
         LEFT JOIN drivers d ON d.id = t.driver_id
         LEFT JOIN users u ON u.id = t.officer_id
         WHERE t.status = 'logged'
           AND (t.capture_method = 'auto' OR t.is_discharge_trip = 1)
           AND {row_filter}
         ORDER BY t.created_at ASC",
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("sheet rows failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "plate": r.get::<_, String>(1)?,
                "entry_time": r.get::<_, String>(2)?,
                "exit_time": r.get::<_, Option<String>>(3)?,
                "trip_status": r.get::<_, Option<String>>(4)?,
                "capacity_at_trip": r.get::<_, Option<f64>>(5)?,
                "capacity_unit": r.get::<_, String>(6)?,
                "receipt_no": r.get::<_, Option<String>>(7)?,
                "confidence_score": r.get::<_, Option<f64>>(8)?,
                "capture_method": r.get::<_, String>(9)?,
                "is_discharge_trip": r.get::<_, Option<bool>>(10)?,
                "created_at": r.get::<_, String>(11)?,
                "company": r.get::<_, String>(12)?,
                "driver": r.get::<_, String>(13)?,
                "officer_name": r.get::<_, Option<String>>(14)?,
                "model_version": r.get::<_, Option<String>>(15)?,
                "ocr_engine": r.get::<_, Option<String>>(16)?,
                "status": r.get::<_, String>(17)?,
                "sheet_row": r.get::<_, Option<i64>>(18)?,
                "vehicles_extra": parse_extra_field_json(r.get::<_, Option<String>>(19)?),
                "companies_extra": parse_extra_field_json(r.get::<_, Option<String>>(20)?),
                "drivers_extra": parse_extra_field_json(r.get::<_, Option<String>>(21)?),
            }))
        })
        .map_err(|e| format!("sheet rows failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("sheet rows failed: {e}"))
}

/// Parse a JSON string from the extra_fields column into a serde_json::Value.
fn parse_extra_field_json(raw: Option<String>) -> serde_json::Value {
    match raw.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
        Some(v) => v,
        None => json!({}),
    }
}

/// Data collected from SQLite during the read phase of a Sheets sync cycle.
/// All fields are plain owned types so the lock can be dropped before network.
pub struct SheetsSyncData {
    pub pending: i64,
    pub mapping: Vec<crate::models::SheetColumnEntry>,
    pub new_rows: Vec<serde_json::Value>,
    pub update_rows: Vec<serde_json::Value>,
}

/// Phase 1 — Read all data needed for Sheets sync from SQLite.
/// Returns an owned struct so the DB lock can be dropped before HTTP calls.
pub fn prepare_sheets_data(
    conn: &Connection,
    _sheets: &dyn SheetsProvider,
) -> Result<SheetsSyncData, String> {
    let pending = pending_sheets_trips(conn)?;
    let mapping = read_sheet_mapping(conn);
    let new_rows = sheet_trip_rows_filtered(conn, false)?;
    let update_rows = sheet_trip_rows_filtered(conn, true)?;
    Ok(SheetsSyncData { pending, mapping, new_rows, update_rows })
}

/// Dedup: remove any new_rows whose trip ID already exists in the sheet.
/// This prevents the infinite duplication loop when sheet_row is lost.
/// Must be called OUTSIDE the sync_db lock (it does network I/O).
pub fn dedup_sheets_rows(sheets: &dyn SheetsProvider, data: &mut SheetsSyncData) {
    if data.new_rows.is_empty() { return; }
    match sheets.read_existing_trip_ids() {
        Ok(existing) if !existing.is_empty() => {
            let before = data.new_rows.len();
            data.new_rows.retain(|r| {
                r.get("id")
                    .and_then(|v| v.as_str())
                    .map(|id| !existing.contains(&id.to_string()))
                    .unwrap_or(true)
            });
            let deduped = before - data.new_rows.len();
            if deduped > 0 {
                crate::log::log(&format!(
                    "[sheets] dedup: removed {deduped} rows that already exist in the sheet"
                ));
            }
        }
        _ => {} // Can't read sheet (not configured / network error) — skip dedup.
    }
}

/// Phase 3 — Write back the results of a Sheets sync cycle to SQLite.
/// Updates pushed flags and last_synced_at timestamp.
pub fn finalize_sheets_results(
    conn: &Connection,
    new_rows: &[serde_json::Value],
    update_rows: &[serde_json::Value],
    acked_ids: &[String],
) -> Result<i64, String> {
    let mut pushed = 0i64;
    // Mark new rows as pushed.
    for entry in acked_ids {
        let (id, sheet_row) = if let Some((id_str, row_str)) = entry.split_once(':') {
            (id_str.to_string(), row_str.parse::<i64>().unwrap_or(0))
        } else {
            (entry.clone(), 0)
        };
        // ALWAYS set pushed_to_sheets=1 — even if sheet_row wasn't parsed.
        // Without this, trips with sheet_row=0 get re-appended every sync cycle
        // causing infinite duplication.
        if sheet_row > 0 {
            conn.execute(
                "UPDATE trips SET pushed_to_sheets = 1, sheet_row = ?1 WHERE id = ?2",
                params![sheet_row, id],
            )
            .map_err(|e| format!("sheets flag flip failed: {e}"))?;
        } else {
            conn.execute("UPDATE trips SET pushed_to_sheets = 1 WHERE id = ?1", params![id])
                .map_err(|e| format!("sheets flag flip failed: {e}"))?;
        }
    }
    pushed += acked_ids.len() as i64;
    // Mark exit-updated rows — these were pushed via update_existing_rows
    // but acked_ids only contains new-row acks. Mark them directly.
    for row in update_rows {
        if let Some(id) = row.get("id").and_then(|v| v.as_str()) {
            conn.execute(
                "UPDATE trips SET pushed_to_sheets = 1, sheet_exit_pushed = 1 WHERE id = ?1",
                params![id],
            )
            .map_err(|e| format!("sheets exit-update flag flip failed: {e}"))?;
        }
    }
    pushed += update_rows.len() as i64;
    if pushed > 0 {
        conn.execute(
            "UPDATE integrations SET last_synced_at = ?1, updated_at = ?1 WHERE type = 'google_sheets'",
            params![now_iso()],
        )
        .map_err(|e| format!("sheets last-synced update failed: {e}"))?;
    }
    Ok(pushed)
}

/// Phase 2 — Execute all network calls for Sheets sync (no DB lock held).
/// Returns the acked IDs from push_new_rows.
pub fn execute_sheets_network(
    sheets: &dyn SheetsProvider,
    mapping: &[crate::models::SheetColumnEntry],
    new_rows: &[serde_json::Value],
    update_rows: &[serde_json::Value],
) -> Result<Vec<String>, String> {
    // Always ensure headers are written before any data push.
    sheets.ensure_header_row(mapping)?;
    // 1) Append new trips.
    let mut acked_ids = Vec::new();
    if !new_rows.is_empty() {
        let acked = sheets.push_new_rows(new_rows, mapping)?;
        acked_ids.extend(acked);
    }
    // 2) Update existing rows (exit matches).
    if !update_rows.is_empty() {
        sheets.update_existing_rows(update_rows, mapping)?;
    }
    Ok(acked_ids)
}

/// Push all logged-but-unsynced trips to the sheet and flip `pushed_to_sheets`
/// only for confirmed rows. Completely independent of the Postgres pipeline.
pub fn run_sheets_sync_impl(conn: &Connection, sheets: &dyn SheetsProvider) -> Result<SyncRunResult, String> {
    let mut data = prepare_sheets_data(conn, sheets)?;
    dedup_sheets_rows(sheets, &mut data);
    let pending = data.pending;
    let mut pushed = 0i64;
    if pending > 0 && sheets.configured() {
        // Network failure is non-fatal — rows stay pending for next cycle.
        if let Ok(acked_ids) = execute_sheets_network(sheets, &data.mapping, &data.new_rows, &data.update_rows) {
            pushed = finalize_sheets_results(conn, &data.new_rows, &data.update_rows, &acked_ids)?;
        }
    }
    Ok(SyncRunResult {
        pushed,
        tables: vec![TablePending {
            table: "trips".to_string(),
            display: "Trips".to_string(),
            pending,
        }],
        last_run_at: Some(now_iso()),
    })
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn sync_status(state: State<AppState>) -> Result<SyncStatusView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let pg = pg_sync_state_impl(&conn, &*state.pg)?;
    let sheets = sheets_state_impl(&conn, &*state.sheets)?;
    Ok(SyncStatusView {
        online: pg.connected || sheets.connected,
        pg,
        sheets,
    })
}

/// Manual Postgres sync trigger (automatic sync already runs in the background;
/// this exists for diagnostics and the admin status panel). Gated like other
/// integration controls.
#[tauri::command]
pub fn sync_now_pg<R: tauri::Runtime>(state: State<AppState>, actor_id: String, handle: AppHandle<R>) -> Result<String, String> {
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }

    let pg = state.pg.clone();
    let db = state.sync_db.clone();
    let actor = actor_id.clone();

    // Early checks: give the user instant honest feedback instead of spawning
    // a thread that silently fails on every table.
    if !pg.configured() {
        let _ = handle.emit("pg-sync-done", json!({ "pushed": 0, "error": "PostgreSQL is not configured. Enter a connection string first." }));
        return Ok("not configured".to_string());
    }

    crate::log::log(&format!("[sync] manual sync triggered by {actor_id} — pg.configured={} pg.connected={}",
        pg.configured(), pg.connected(),
    ));
    // Record health event synchronously so the dashboard reflects the outcome
    // immediately (background thread may not have finished when the caller checks).
    if pg.configured() && !pg.connected() {
        if let Ok(conn) = state.db.lock() {
            let _ = crate::monitor::record_health_event(&conn, "sync", "offline",
                Some("PostgreSQL is unreachable — pending rows will sync when connectivity returns"));
        }
    } else if pg.configured() && pg.connected() {
        if let Ok(conn) = state.db.lock() {
            let _ = crate::monitor::record_health_event(&conn, "sync", "ok", None);
        }
    }
    std::thread::spawn(move || {
        // Phase 1: collect unsynced rows
        let (rows_by_table, pending_counts) = {
            let Ok(conn) = db.lock() else {
                let _ = handle.emit("pg-sync-done", json!({ "pushed": 0, "error": "database busy" }));
                return;
            };
            let mut rows_by_table: std::collections::HashMap<String, Vec<serde_json::Value>> = std::collections::HashMap::new();
            let mut pending_counts = Vec::new();
            for (name, display) in PG_SYNC_TABLES {
                let pending = match pending_for_table(&conn, name) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if pending > 0 && pg.configured() {
                    if let Ok(rows) = rows_where_not_synced(&conn, name) {
                        if !rows.is_empty() {
                            rows_by_table.insert(name.to_string(), rows);
                        }
                    }
                }
                pending_counts.push(TablePending { table: name.to_string(), display: display.to_string(), pending });
            }
            (rows_by_table, pending_counts)
        };

        // Phase 2: push to Postgres in deterministic table order (no lock held)
        let mut total_pushed = 0i64;
        let mut pushed_ids_by_table: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        let mut errors: Vec<String> = Vec::new();
        for (table_name, _) in PG_SYNC_TABLES {
            if let Some(rows) = rows_by_table.get(*table_name) {
                if pg.configured() && !rows.is_empty() {
                    match pg.push_rows(table_name, rows) {
                        Ok(ids) => {
                            total_pushed += ids.len() as i64;
                            pushed_ids_by_table.insert(table_name.to_string(), ids);
                        }
                        Err(e) => {
                            errors.push(e.clone());
                            crate::log::log(&format!("[sync] manual push {table_name}: {e}"));
                        }
                    }
                }
            }
        }

        // Phase 3: mark synced + emit result
        {
            let Ok(conn) = db.lock() else {
                let _ = handle.emit("pg-sync-done", json!({ "pushed": total_pushed, "error": "database busy" }));
                return;
            };
            for (table_name, _) in PG_SYNC_TABLES {
                if let Some(ids) = pushed_ids_by_table.get(*table_name) {
                    for id in ids {
                        let _ = conn.execute(&format!("UPDATE {table_name} SET synced = 1 WHERE id = ?1"), params![id]);
                    }
                }
            }
            // Only update last_synced_at when rows were actually pushed —
            // otherwise the UI misleadingly shows a recent sync time.
            if total_pushed > 0 {
                let _ = set_setting(&conn, "pg_last_synced_at", &now_iso());
            }
            let _ = append_audit(&conn, &actor, "manual_postgres_sync", None, Some(json!({ "pushed": total_pushed })));
            // Record sync health event so the System Monitor reflects the outcome.
            if errors.is_empty() {
                if total_pushed > 0 {
                    let _ = crate::monitor::record_health_event(&conn, "sync", "ok", None);
                }
            } else {
                let _ = crate::monitor::record_health_event(&conn, "sync", "offline",
                    Some(&format!("PostgreSQL push failed: {}", errors.join("; "))));
            }

            // Phase 4: process pending deletes (local deletions → central)
            match get_all_pending_deletes(&conn) {
                Ok(pending_deletes) if !pending_deletes.is_empty() => {
                    crate::log::log(&format!("[sync] manual sync: processing {} pending deletes", pending_deletes.len()));
                    // Group by table
                    let mut by_table: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
                    for (table, id) in pending_deletes {
                        by_table.entry(table).or_default().push(id);
                    }
                    for (table, ids) in by_table {
                        match pg.delete_rows(&table, &ids) {
                            Ok(()) => {
                                crate::log::log(&format!("[sync] manual sync delete_rows {table}: deleted {} from central", ids.len()));
                                let _ = clear_pending_deletes(&conn, &table, &ids);
                            }
                            Err(e) => {
                                crate::log::log(&format!("[sync] manual sync delete_rows {table}: failed: {e}"));
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let error_msg = if errors.is_empty() { None } else { Some(errors.join("; ")) };
        let _ = handle.emit("pg-sync-done", json!({ "pushed": total_pushed, "error": error_msg }));
    });
    Ok("syncing".to_string())
}

/// Load trips from the PostgreSQL archive into local SQLite for offline reporting.
/// Takes a date range (from/to) and non-blocking: spawns a thread, emits events,
/// and returns "loading" immediately. Emits "archive-loaded" on completion with the
/// count of trips loaded, or "archive-load-error" on failure.
#[tauri::command]
pub fn load_archive_trips<R: tauri::Runtime>(
    state: State<AppState>,
    actor_id: String,
    from: String,
    to: String,
    handle: AppHandle<R>,
) -> Result<String, String> {
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }

    // Validate date range: max 1 year
    let from_dt = chrono::DateTime::parse_from_rfc3339(&from)
        .or_else(|_| chrono::NaiveDate::parse_from_str(&from, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(chrono::FixedOffset::east_opt(0).unwrap()).unwrap()))
        .map_err(|_| "Invalid 'from' date format".to_string())?;
    let to_dt = chrono::DateTime::parse_from_rfc3339(&to)
        .or_else(|_| chrono::NaiveDate::parse_from_str(&to, "%Y-%m-%d").map(|d| d.and_hms_opt(23, 59, 59).unwrap().and_local_timezone(chrono::FixedOffset::east_opt(0).unwrap()).unwrap()))
        .map_err(|_| "Invalid 'to' date format".to_string())?;
    let span_days = (to_dt.signed_duration_since(&from_dt)).num_days();
    if span_days < 0 {
        return Err("'from' date must be before 'to' date.".to_string());
    }
    if span_days > 365 {
        return Err("Maximum date range is 1 year. Please select a smaller range.".to_string());
    }

    let pg = state.pg.clone();
    let db = state.sync_db.clone();

    if !pg.configured() {
        let _ = handle.emit("archive-load-error", json!({ "error": "PostgreSQL is not configured" }));
        return Err("PostgreSQL is not configured".to_string());
    }

    // Check if we already have trips in this date range cached locally
    {
        let Ok(conn) = db.lock() else {
            return Err("database busy".to_string());
        };
        let from_extended = crate::reporting::extend_bare_date_to_end_of_day(&from);
        let local_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM trips WHERE time_in >= ?1 AND time_in <= ?2",
                rusqlite::params![from_extended, to],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if local_count > 0 {
            crate::log::log(&format!("[archive] {} trips already cached locally for {} to {}, skipping pull", local_count, from, to));
            let _ = handle.emit("archive-loaded", json!({ "loaded": 0, "from": from, "to": to, "cached": true }));
            return Ok("cached".to_string());
        }
    }

    crate::log::log(&format!("[archive] load requested by {actor_id}: {} to {}", from, to));

    std::thread::spawn(move || {
        let loaded = {
            let Ok(conn) = db.lock() else {
                let _ = handle.emit("archive-load-error", json!({ "error": "database busy" }));
                return;
            };
            match load_archive_trips_impl(&conn, &*pg, &from, &to) {
                Ok(count) => count,
                Err(e) => {
                    crate::log::log(&format!("[archive] load failed: {e}"));
                    let _ = handle.emit("archive-load-error", json!({ "error": e }));
                    return;
                }
            }
        };

        // Record audit entry
        if let Ok(conn) = db.lock() {
            let _ = append_audit(&conn, &actor_id, "loaded_archive_trips", None, Some(json!({
                "from": from,
                "to": to,
                "loaded": loaded
            })));
        }

        crate::log::log(&format!("[archive] loaded {} trips from {} to {}", loaded, from, to));
        let _ = handle.emit("archive-loaded", json!({ "loaded": loaded, "from": from, "to": to }));
    });

    Ok("loading".to_string())
}

#[tauri::command]
pub fn connect_google_sheets(
    state: State<AppState>,
    actor_id: String,
    target_sheet_id: Option<String>,
    shared_group: Option<String>,
    sync_frequency: String,
) -> Result<SheetsStateView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    let now = now_iso();
    let id = conn
        .query_row(
            "SELECT id FROM integrations WHERE type = 'google_sheets' ORDER BY created_at DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok();
    match id {
        Some(id) => {
            conn.execute(
                "UPDATE integrations SET connected_by = ?1, target_sheet_id = ?2, shared_group = ?3,
                        sync_frequency = ?4, status = 'connected', last_synced_at = NULL, updated_at = ?5
                 WHERE id = ?6",
                params![actor_id, target_sheet_id, shared_group, sync_frequency, now, id],
            )
            .map_err(|e| format!("integration update failed: {e}"))?;
        }
        None => {
            conn.execute(
                "INSERT INTO integrations (id, type, connected_by, target_sheet_id, shared_group,
                        sync_frequency, status, created_at, updated_at)
                 VALUES (?1, 'google_sheets', ?2, ?3, ?4, ?5, 'connected', ?6, ?6)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    actor_id,
                    target_sheet_id,
                    shared_group,
                    sync_frequency,
                    now
                ],
            )
            .map_err(|e| format!("integration create failed: {e}"))?;
        }
    }
    append_audit(
        &conn,
        &actor_id,
        "connected_google_sheets",
        None,
        Some(json!({ "target_sheet_id": target_sheet_id, "shared_group": shared_group, "sync_frequency": sync_frequency })),
    )?;
    sheets_state_impl(&conn, &*state.sheets)
}

#[tauri::command]
pub fn disconnect_google_sheets(state: State<AppState>, actor_id: String) -> Result<SheetsStateView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    conn.execute(
        "UPDATE integrations SET status = 'disconnected', updated_at = ?1 WHERE type = 'google_sheets'",
        params![now_iso()],
    )
    .map_err(|e| format!("integration disconnect failed: {e}"))?;
    conn.execute(
        "DELETE FROM app_settings WHERE key IN ('sheets_service_account_json', 'sheets_target_sheet_id')",
        [],
    )
    .map_err(|e| format!("settings clear failed: {e}"))?;
    state.sheets.configure(None, None)?;
    append_audit(&conn, &actor_id, "disconnected_google_sheets", None, None)?;
    sheets_state_impl(&conn, &*state.sheets)
}

/// Admin-set retention window for the sheet: rows older than N days are pruned
/// on the background loop. `None` disables pruning entirely.
#[tauri::command]
pub fn set_sheets_retention(state: State<AppState>, actor_id: String, days: Option<i64>) -> Result<SheetsStateView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    match days {
        Some(d) if d >= 1 => {
            set_setting(&conn, "sheets_retention_days", &d.to_string())?;
        }
        Some(_) => return Err("Retention must be at least 1 day, or empty to disable pruning.".to_string()),
        None => {
            conn.execute("DELETE FROM app_settings WHERE key = 'sheets_retention_days'", [])
                .map_err(|e| format!("settings clear failed: {e}"))?;
        }
    }
    append_audit(&conn, &actor_id, "set_sheets_retention", None, Some(json!({ "days": days })))?;
    sheets_state_impl(&conn, &*state.sheets)
}

/// Immediately remove every exported row from the sheet (headers stay). Old
/// trips are not re-exported — only new trips append afterwards.
#[tauri::command]
pub fn clear_exported_trips(state: State<AppState>, actor_id: String) -> Result<SheetsStateView, String> {
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }
    if !state.sheets.configured() {
        return Err("Google Sheets is not configured.".to_string());
    }
    // Network call — prune ALL data rows (NO lock held).
    // Pass None as cutoff to remove everything regardless of age.
    let removed = state.sheets.prune(None, &[])?;
    // Do NOT reset pushed_to_sheets — trips stay marked as "exported" so
    // the sync poller doesn't re-push them immediately after clearing.
    // Only clear sheet_row so trips know they have no row reference.
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE trips SET sheet_row = NULL, sheet_exit_pushed = 0",
            [],
        ).map_err(|e| format!("clear sheet flags failed: {e}"))?;
        append_audit(&conn, &actor_id, "cleared_sheet_exports", None, Some(json!({ "rows_removed": removed })))?;
        sheets_state_impl(&conn, &*state.sheets)
    }
}

/// Admin-set retention window for the daily trip entries: entries older than N
/// days are deleted from local storage AND the Postgres archive (bulk, on the
/// background loop). The registry (companies/vehicles/drivers/users) is never
/// touched, and this is fully separate from the Google Sheet retention.
#[tauri::command]
pub fn set_trip_retention(state: State<AppState>, actor_id: String, days: Option<i64>) -> Result<PgSyncStateView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    match days {
        Some(d) if d >= 1 => {
            set_setting(&conn, "trip_retention_days", &d.to_string())?;
        }
        Some(_) => return Err("Retention must be at least 1 day, or empty to disable.".to_string()),
        None => {
            conn.execute("DELETE FROM app_settings WHERE key = 'trip_retention_days'", [])
                .map_err(|e| format!("settings clear failed: {e}"))?;
        }
    }
    append_audit(&conn, &actor_id, "set_trip_retention", None, Some(json!({ "days": days })))?;
    pg_sync_state_impl(&conn, &*state.pg)
}

#[tauri::command]
pub fn set_google_sheets_frequency(
    state: State<AppState>,
    actor_id: String,
    sync_frequency: String,
) -> Result<SheetsStateView, String> {
    if sync_frequency != "realtime" && sync_frequency != "every_15_min" {
        return Err("Sync frequency must be realtime or every_15_min.".to_string());
    }
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    conn.execute(
        "UPDATE integrations SET sync_frequency = ?1, updated_at = ?2 WHERE type = 'google_sheets'",
        params![sync_frequency, now_iso()],
    )
    .map_err(|e| format!("integration frequency update failed: {e}"))?;
    append_audit(&conn, &actor_id, "set_google_sheets_frequency", None, Some(json!({ "sync_frequency": sync_frequency })))?;
    sheets_state_impl(&conn, &*state.sheets)
}

#[tauri::command]
pub fn sync_now_sheets<R: tauri::Runtime>(state: State<AppState>, actor_id: String, handle: AppHandle<R>) -> Result<String, String> {
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }

    let sheets = state.sheets.clone();
    let db = state.sync_db.clone();
    let actor = actor_id.clone();
    std::thread::spawn(move || {
        // Phase 1: prepare data (read-only, under lock)
        let mut data = {
            let Ok(conn) = db.lock() else {
                let _ = handle.emit("sheets-sync-error", json!({ "error": "database busy" }));
                return;
            };
            match prepare_sheets_data(&conn, &*sheets) {
                Ok(d) => d,
                Err(e) => {
                    let _ = handle.emit("sheets-sync-error", json!({ "error": e }));
                    return;
                }
            }
        };
        // Phase 1.5: dedup via network (no lock held)
        dedup_sheets_rows(&*sheets, &mut data);
        let pending = data.pending;

        // Phase 2: network push (no lock held)
        let acked_ids = if pending > 0 && sheets.configured() {
            match execute_sheets_network(&*sheets, &data.mapping, &data.new_rows, &data.update_rows) {
                Ok(ids) => ids,
                Err(e) => {
                    let _ = handle.emit("sheets-sync-error", json!({ "error": e }));
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let pushed = acked_ids.len() as i64;

        // Phase 3: finalize + emit result
        {
            let Ok(conn) = db.lock() else { return; };
            if !acked_ids.is_empty() {
                let _ = finalize_sheets_results(&conn, &data.new_rows, &data.update_rows, &acked_ids);
            }
            let _ = append_audit(&conn, &actor, "manual_sheets_sync", None, Some(json!({ "pushed": pushed })));
        }

        let _ = handle.emit("sheets-sync-done", json!({ "pushed": pushed }));
    });
    Ok("syncing".to_string())
}

/// Dev/test-only connectivity toggle for the mock adapters (02 §7 simulated
/// sync, 06-data-flow.md testing checklist). Real adapters reject this.
#[tauri::command]
pub fn simulate_connectivity(state: State<AppState>, postgres_online: bool, sheets_online: bool) -> Result<(), String> {
    state.pg.simulate_connectivity(postgres_online)?;
    state.sheets.simulate_connectivity(sheets_online)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Config sync — pull PG/Sheets configuration from PostgreSQL to local SQLite
// ---------------------------------------------------------------------------

/// Pull company config from PostgreSQL and save to local SQLite.
/// Called on login and periodically to keep config in sync across all PCs.
pub fn pull_company_config(state: &AppState, company_id: &str) -> Result<(), String> {
    if !state.pg.configured() || !state.pg.connected() {
        return Ok(()); // Not connected, skip
    }
    
    let rows = state.pg.query_rows(
        &format!("SELECT * FROM company_config WHERE company_id = '{}'", pg_literal_string(company_id)),
        &[],
    ).map_err(|e| format!("Failed to query company_config: {e}"))?;
    
    if let Some(row) = rows.first() {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        
        // Save PG connection string
        if let Some(pg_str) = row.get("pg_connection_string").and_then(|v| v.as_str()) {
            let _ = crate::db::set_setting(&conn, "pg_connection_string", pg_str);
        }
        
        // Save Sheets ID
        if let Some(sheets_id) = row.get("sheets_id").and_then(|v| v.as_str()) {
            let _ = crate::db::set_setting(&conn, "sheets_id", sheets_id);
        }
        
        // Save other config
        if let Some(freq) = row.get("sheets_frequency").and_then(|v| v.as_str()) {
            let _ = crate::db::set_setting(&conn, "sheets_frequency", freq);
        }
        
        if let Some(enabled) = row.get("anpr_enabled").and_then(|v| v.as_bool()) {
            let _ = crate::db::set_setting(&conn, "anpr_enabled", if enabled { "true" } else { "false" });
        }
        
        // Save to company_config table
        let _ = conn.execute(
            "INSERT INTO company_config (company_id, pg_connection_string, sheets_id, sheets_frequency, anpr_enabled, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(company_id) DO UPDATE SET 
                 pg_connection_string = excluded.pg_connection_string,
                 sheets_id = excluded.sheets_id,
                 sheets_frequency = excluded.sheets_frequency,
                 anpr_enabled = excluded.anpr_enabled,
                 updated_at = excluded.updated_at",
            rusqlite::params![
                company_id,
                row.get("pg_connection_string").and_then(|v| v.as_str()).unwrap_or(""),
                row.get("sheets_id").and_then(|v| v.as_str()).unwrap_or(""),
                row.get("sheets_frequency").and_then(|v| v.as_str()).unwrap_or("realtime"),
                row.get("anpr_enabled").and_then(|v| v.as_bool()).unwrap_or(false),
                crate::db::now_iso(),
            ],
        );
    }
    
    Ok(())
}

/// Push company config from local SQLite to PostgreSQL.
/// Called when admin updates config on any PC.
pub fn push_company_config(state: &AppState, company_id: &str) -> Result<(), String> {
    if !state.pg.configured() || !state.pg.connected() {
        return Ok(()); // Not connected, skip
    }
    
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    
    // Read local config
    let pg_conn_str = crate::db::get_setting(&conn, "pg_connection_string").unwrap_or_default();
    let sheets_id = crate::db::get_setting(&conn, "sheets_id").unwrap_or_default();
    let sheets_freq = crate::db::get_setting(&conn, "sheets_frequency").unwrap_or_else(|| "realtime".to_string());
    let anpr_enabled = crate::db::get_setting(&conn, "anpr_enabled").unwrap_or_else(|| "false".to_string()) == "true";
    
    // Push to PostgreSQL
    let sql = format!(
        "INSERT INTO company_config (company_id, pg_connection_string, sheets_id, sheets_frequency, anpr_enabled, updated_at)
         VALUES ('{}', '{}', '{}', '{}', {}, '{}')
         ON CONFLICT (company_id) DO UPDATE SET 
             pg_connection_string = EXCLUDED.pg_connection_string,
             sheets_id = EXCLUDED.sheets_id,
             sheets_frequency = EXCLUDED.sheets_frequency,
             anpr_enabled = EXCLUDED.anpr_enabled,
             updated_at = EXCLUDED.updated_at",
        pg_literal_string(company_id),
        pg_literal_string(&pg_conn_str),
        pg_literal_string(&sheets_id),
        pg_literal_string(&sheets_freq),
        anpr_enabled,
        pg_literal_string(&crate::db::now_iso()),
    );
    
    drop(conn);
    state.pg.query_rows(&sql, &[]).map_err(|e| format!("Failed to push config: {e}"))?;
    
    Ok(())
}

/// Raw version for use in background threads (takes individual parameters).
pub fn pull_company_config_raw(pg: &Arc<dyn PostgresAdapter>, db: &Arc<Mutex<Connection>>, company_id: &str) -> Result<(), String> {
    if !pg.configured() || !pg.connected() {
        return Ok(()); // Not connected, skip
    }
    
    let rows = pg.query_rows(
        &format!("SELECT * FROM company_config WHERE company_id = '{}'", pg_literal_string(company_id)),
        &[],
    ).map_err(|e| format!("Failed to query company_config: {e}"))?;
    
    if let Some(row) = rows.first() {
        let conn = db.lock().map_err(|e| e.to_string())?;
        
        // Save PG connection string
        if let Some(pg_str) = row.get("pg_connection_string").and_then(|v| v.as_str()) {
            let _ = crate::db::set_setting(&conn, "pg_connection_string", pg_str);
        }
        
        // Save Sheets ID
        if let Some(sheets_id) = row.get("sheets_id").and_then(|v| v.as_str()) {
            let _ = crate::db::set_setting(&conn, "sheets_id", sheets_id);
        }
        
        // Save other config
        if let Some(freq) = row.get("sheets_frequency").and_then(|v| v.as_str()) {
            let _ = crate::db::set_setting(&conn, "sheets_frequency", freq);
        }
        
        if let Some(enabled) = row.get("anpr_enabled").and_then(|v| v.as_bool()) {
            let _ = crate::db::set_setting(&conn, "anpr_enabled", if enabled { "true" } else { "false" });
        }
        
        // Save to company_config table
        let _ = conn.execute(
            "INSERT INTO company_config (company_id, pg_connection_string, sheets_id, sheets_frequency, anpr_enabled, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(company_id) DO UPDATE SET 
                 pg_connection_string = excluded.pg_connection_string,
                 sheets_id = excluded.sheets_id,
                 sheets_frequency = excluded.sheets_frequency,
                 anpr_enabled = excluded.anpr_enabled,
                 updated_at = excluded.updated_at",
            rusqlite::params![
                company_id,
                row.get("pg_connection_string").and_then(|v| v.as_str()).unwrap_or(""),
                row.get("sheets_id").and_then(|v| v.as_str()).unwrap_or(""),
                row.get("sheets_frequency").and_then(|v| v.as_str()).unwrap_or("realtime"),
                row.get("anpr_enabled").and_then(|v| v.as_bool()).unwrap_or(false),
                crate::db::now_iso(),
            ],
        );
    }
    
    Ok(())
}

// ---------------------------------------------------------------------------
// Real PostgreSQL adapter — Phase 4 (02 §3, 06-data-flow.md §5)
// ---------------------------------------------------------------------------

/// Central schema columns, mirrored from the SQLite source (01-database-schema.md).
/// The engine pushes the same table names with client-side UUID ids; Postgres
/// types are chosen so numbers/booleans stay queryable for Phase 5 reporting.
fn pg_column_type(table: &str, col: &str) -> &'static str {
    match table {
        "companies" | "drivers" => match col {
            "synced" => "INTEGER",
            _ => "TEXT",
        },
        "vehicles" => match col {
            "registered_capacity" => "DOUBLE PRECISION",
            "synced" => "INTEGER",
            _ => "TEXT",
        },
        "users" => match col {
            "synced" => "INTEGER",
            _ => "TEXT",
        },
        "trips" => match col {
            "capacity_at_trip" | "confidence_score" => "DOUBLE PRECISION",
            "pushed_to_sheets" | "synced" | "archived" | "is_discharge_trip" => "INTEGER",
            _ => "TEXT",
        },
        _ => "TEXT",
    }
}

fn pg_quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Get column names for a table from local SQLite.
fn sqlite_columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    let pragma_sql = format!("PRAGMA table_info(\"{}\")", table);
    let mut stmt = conn.prepare(&pragma_sql)
        .map_err(|e| format!("PRAGMA failed for {table}: {e}"))?;
    let cols: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| format!("PRAGMA read failed for {table}: {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(cols)
}

/// Escape a string for use in PostgreSQL SQL literals (single-quote escaping).
pub fn pg_literal_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn base_columns(table: &str) -> Vec<&'static str> {
    match table {
        "companies" | "drivers" => {
            vec!["id", "name", "status", "extra_fields", "created_at", "updated_at", "synced"]
        }
        "vehicles" => vec![
            "id", "plate_number", "company_id", "registered_capacity", "default_driver_id",
            "status", "extra_fields", "created_at", "updated_at", "synced",
        ],
        "users" => vec![
            "id", "name", "auth_type", "credential_hash", "status", "revoked_by", "revoked_at",
            "profile_photo_ref", "phone_number", "theme_mode", "theme_accent", "language_preference",
            "created_at", "updated_at", "synced",
        ],
        "trips" => vec![
            "id", "vehicle_id", "driver_id", "company_id", "capacity_at_trip", "capacity_unit", "time_in",
            "receipt_no", "officer_id", "capture_method", "confidence_score", "photo_refs",
            "status", "resolution_notes", "pushed_to_sheets", "created_at", "updated_at", "synced",
            "is_discharge_trip", "model_version", "ocr_engine", "entry_time", "exit_time", "trip_status",
            "entry_photo_refs", "exit_photo_refs", "sheet_row", "sheet_exit_pushed",
        ],
        _ => vec!["id"],
    }
}

/// Convert a JSON cell into a Postgres parameter typed per the column schema.
/// Unknown columns are stored as TEXT (stringified) so schema drift never
/// blocks a push.
/// Render an error plus its full cause chain (the postgres client hides the
/// inner "wrong type" detail behind its serialization wrapper).
fn error_chain(e: &(dyn std::error::Error + Send + Sync)) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        msg.push_str(&format!(" -> {s}"));
        src = s.source();
    }
    msg
}

/// Convert a JSON value to a SQL literal for embedding in simple queries.
/// This avoids the extended query protocol that PgBouncer may not support.
fn pg_literal(v: &serde_json::Value, ty: &str) -> String {
    match v {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(b) => if *b { "TRUE".to_string() } else { "FALSE".to_string() },
        serde_json::Value::Number(n) => {
            match ty {
                "INTEGER" => n.as_i64().map(|x| x.to_string()).unwrap_or_else(|| "NULL".to_string()),
                "DOUBLE PRECISION" => n.as_f64().map(|x| format!("{x}")).unwrap_or_else(|| "NULL".to_string()),
                _ => n.as_f64().map(|x| format!("{x}")).unwrap_or_else(|| "NULL".to_string()),
            }
        }
        serde_json::Value::String(s) => {
            // PostgreSQL only requires single-quote escaping (doubling).
            // Backslashes and double-quotes are literal inside '...' with
            // standard_conforming_strings=on (the default since PG 9.1).
            let escaped = s.replace('\'', "''");
            format!("'{escaped}'")
        }
        other => {
            let s = other.to_string().replace('\'', "''");
            format!("'{s}'")
        }
    }
}

fn to_pg_param(v: &serde_json::Value, ty: &str) -> Box<dyn ToSql + Sync> {
    match ty {
        // i32 accepts INT2/INT4; the synced-style flags are 0/1 so the smaller
        // width is both correct and type-safe (i64 only accepts BIGINT/INT8).
        "INTEGER" => match v {
            serde_json::Value::Null => Box::new(None::<i32>),
            serde_json::Value::Number(n) => Box::new(n.as_i64().and_then(|x| i32::try_from(x).ok())),
            serde_json::Value::Bool(b) => Box::new(Some(if *b { 1i32 } else { 0i32 })),
            serde_json::Value::String(s) => Box::new(s.parse::<i32>().ok()),
            _ => Box::new(None::<i32>),
        },
        "DOUBLE PRECISION" => match v {
            serde_json::Value::Null => Box::new(None::<f64>),
            serde_json::Value::Number(n) => Box::new(n.as_f64()),
            serde_json::Value::String(s) => Box::new(s.parse::<f64>().ok()),
            _ => Box::new(None::<f64>),
        },
        _ => match v {
            serde_json::Value::Null => Box::new(None::<String>),
            serde_json::Value::String(s) => Box::new(Some(s.clone())),
            other => Box::new(Some(other.to_string())),
        },
    }
}

/// Connect to `cs`; if the database does not exist yet, create it first via
/// the maintenance `postgres` database and reconnect. This makes setup as
/// simple as "paste the connection string".
fn make_tls_connector() -> Result<postgres_native_tls::MakeTlsConnector, String> {
    // Accept all TLS certificates — required for Supabase and other managed
    // Postgres providers that use their own CA. The connection string itself
    // contains authentication, so certificate verification is not the primary
    // security control here.
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| format!("TLS builder failed: {e}"))?;
    Ok(postgres_native_tls::MakeTlsConnector::new(connector))
}

fn connect_with_create(cs: &str) -> Result<postgres::Client, String> {
    let mut config: postgres::Config = cs.parse().map_err(|e| format!("invalid connection string: {e}"))?;
    config.connect_timeout(std::time::Duration::from_secs(3));
    config.keepalives(true);
    config.keepalives_idle(std::time::Duration::from_secs(10));
    config.keepalives_interval(std::time::Duration::from_secs(3));
    let tls = make_tls_connector()?;

    /// Connect, verify with SELECT 1, return client.
    /// IMPORTANT: Do NOT use SET statement_timeout here.
    /// PgBouncer in transaction-mode strips session settings between
    /// statements, so the SET is wasted and adds a round-trip that can
    /// fail. Dead-connection detection is handled by TCP keepalive
    /// (idle=10s, interval=3s) at the OS level.
    let make_client = |cfg: &mut postgres::Config| -> Result<postgres::Client, String> {
        let mut c = cfg.connect(tls.clone()).map_err(|e| {
            let is_missing_db = e.as_db_error().map(|d| d.message().contains("does not exist")).unwrap_or(false);
            if is_missing_db { "__MISSING_DB__".to_string() } else { format!("cannot connect to PostgreSQL: {e}") }
        })?;
        // Verify the connection actually works for queries, not just
        // the TLS handshake. Some PgBouncer setups accept the handshake
        // but kill the connection on the first real query.
        c.execute("SELECT 1", &[])
            .map_err(|e| format!("connection verified but SELECT 1 failed: {e}"))?;
        Ok(c)
    };

    match make_client(&mut config) {
        Ok(c) => Ok(c),
        Err(e) if e == "__MISSING_DB__" => {
            let dbname = config
                .get_dbname()
                .ok_or("connection string must include a database name")?
                .to_string();
            config.dbname("postgres");
            let mut admin = config
                .connect(tls.clone())
                .map_err(|ae| format!("cannot connect to the 'postgres' maintenance database: {ae}"))?;
            admin
                .batch_execute(&format!("CREATE DATABASE {}", pg_quote_ident(&dbname)))
                .map_err(|ce| format!("cannot create database '{dbname}': {ce}"))?;
            drop(admin);
            config.dbname(&dbname);
            make_client(&mut config)
        }
        Err(e) => Err(e),
    }
}

/// Mirror the source tables centrally (CREATE TABLE IF NOT EXISTS) plus any
/// extra columns a row may carry that aren't in the base schema yet. Every
/// table gets a PRIMARY KEY on `id` (and an idempotent unique index, so a
/// table that already exists without the constraint is still upsert-safe).
fn ensure_schema_for(client: &mut postgres::Client) -> Result<(), String> {
    // Build ONE big batch_execute with all DDL statements.
    // PgBouncer in transaction-mode treats batch_execute as a single
    // protocol message, so all statements run in one round-trip.
    // This avoids the per-table connection drops that killed the old
    // approach of running SELECT + CREATE TABLE per table.
    let mut sql = String::new();
    for &(table, _) in PG_SYNC_TABLES {
        let defs: Vec<String> = base_columns(table)
            .iter()
            .map(|c| {
                if *c == "id" {
                    format!("{} {} PRIMARY KEY", pg_quote_ident(c), pg_column_type(table, c))
                } else {
                    format!("{} {}", pg_quote_ident(c), pg_column_type(table, c))
                }
            })
            .collect();
        sql.push_str(&format!(
            "CREATE TABLE IF NOT EXISTS {} ({}); ",
            pg_quote_ident(table),
            defs.join(", ")
        ));
        sql.push_str(&format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS {} ON {}(\"id\"); ",
            pg_quote_ident(&format!("{table}_id_key")),
            pg_quote_ident(table)
        ));
    }
    crate::log::log(&format!("[sync] ensure_schema_for: sending {} DDL statements in one batch", PG_SYNC_TABLES.len() * 2));
    match client.batch_execute(&sql) {
        Ok(()) => {
            crate::log::log("[sync] ensure_schema_for: DDL batch OK");
            Ok(())
        }
        Err(e) => {
            // Even if some DDL failed, the tables that already exist
            // are fine. Only fail if ALL tables are missing (the push
            // will fail anyway).
            crate::log::log(&format!("[sync] ensure_schema_for: DDL batch partial: {e}"));
            Err(format!("ensure_schema_for: {e}"))
        }
    }
}/// Upsert rows by UUID id. Returns the ids confirmed written; every row is
/// idempotent (ON CONFLICT DO UPDATE) so a mid-batch failure is safe to retry.
///
/// IMPORTANT: No transactions are used here. PgBouncer in transaction-mode
/// pooling causes COMMIT to hang when the backend is reassigned between the
/// INSERT and the COMMIT. Since each INSERT is idempotent (ON CONFLICT DO
/// UPDATE), a partial failure is safe — the next sync cycle retries the
/// remaining rows. This eliminates the primary cause of worker hangs.
fn push_rows_impl(
    client: &mut postgres::Client,
    table: &str,
    rows: &[serde_json::Value],
) -> Result<Vec<String>, String> {
    crate::log::log(&format!("[sync] push_rows_impl: table={table} rows={}", rows.len()));
    // Skip ALTER TABLE phase entirely. Over PgBouncer, DDL statements
    // hang for 40+ seconds and block all subsequent pushes. Dynamic
    // columns that dont exist yet will cause the INSERT to fail with
    // "column does not exist", which is non-fatal - the error is logged
    // and the next sync cycle retries. Missing columns can be added
    // manually via Supabase SQL Editor if needed.
    let base = base_columns(table);
    // Phase 2: batch upsert — multi-row INSERT for speed.
    // Collect the union of ALL column names across all rows, then
    // insert in batches. Each batch is ONE round-trip instead of N.
    // Rows with missing columns get NULL — ON CONFLICT DO UPDATE
    // only touches columns present in this INSERT, so the next batch
    // handles remaining columns. All rows are idempotent.
    //
    // Over an unstable connection: 113 rows → 1 batch instead of 3.
    // For bulk data: 10,000 rows → 20 batches instead of 200 (10x speedup).
    // PostgreSQL handles multi-row INSERTs up to ~1000 rows efficiently.
    // Each batch is idempotent (ON CONFLICT DO UPDATE) so partial failures are safe.
    const BATCH_SIZE: usize = 500;

    // 1. Collect all column names (union across all rows), preserving
    //    a stable order: id first, then base columns, then dynamic.
    let mut all_cols: Vec<String> = Vec::new();
    {
        let mut seen = std::collections::HashSet::new();
        // id first
        all_cols.push("id".to_string());
        seen.insert("id".to_string());
        // base columns
        for c in &base {
            if seen.insert(c.to_string()) {
                all_cols.push(c.to_string());
            }
        }
        // dynamic columns
        for row in rows {
            if let Some(obj) = row.as_object() {
                for key in obj.keys() {
                    if seen.insert(key.clone()) {
                        all_cols.push(key.clone());
                    }
                }
            }
        }
    }
    let col_count = all_cols.len();
    let quoted_cols: Vec<String> = all_cols.iter().map(|c| pg_quote_ident(c)).collect();
    let update_set: String = all_cols.iter()
        .filter(|c| c.as_str() != "id")
        .map(|c| format!("{} = EXCLUDED.{}", pg_quote_ident(c), pg_quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let col_list = quoted_cols.join(", ");

    // 2. Process rows in batches.
    let mut all_acked: Vec<String> = Vec::new();
    let mut error_count = 0u32;
    let total_rows = rows.len();
    for batch_start in (0..total_rows).step_by(BATCH_SIZE) {
        let batch_end = (batch_start + BATCH_SIZE).min(total_rows);
        let batch = &rows[batch_start..batch_end];
        let batch_len = batch.len();

        // Build placeholders: ($1, $2, ...$C), ($C+1, ..., $(2*C)), ...
        let mut placeholders = Vec::with_capacity(batch_len);
        let mut flat_params: Vec<Box<dyn ToSql + Sync>> = Vec::with_capacity(batch_len * col_count);
        for row in batch {
            let obj = row.as_object();
            let start = flat_params.len() + 1; // 1-indexed
            let end = start + col_count;
            placeholders.push(
                (start..end).map(|i| format!("${i}")).collect::<Vec<_>>().join(", ")
            );
            for col_name in &all_cols {
                let val = obj.and_then(|o| o.get(col_name.as_str())).unwrap_or(&serde_json::Value::Null);
                flat_params.push(to_pg_param(val, pg_column_type(table, col_name)));
            }
        }
        let values_clause = placeholders.iter().map(|p| format!("({p})")).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "INSERT INTO {} ({}) VALUES {values_clause} ON CONFLICT (\"id\") DO UPDATE SET {update_set}",
            pg_quote_ident(table), col_list
        );
        let param_refs: Vec<&(dyn ToSql + Sync)> = flat_params.iter().map(|b| b.as_ref()).collect();

        // Use batch_execute (simple query protocol) instead of
        // execute (extended query protocol). PgBouncer in transaction-mode
        // may not fully support the extended protocol for DML, causing
        // the connection to hang and get killed by the server.
        // Embed values directly as SQL literals.
        if batch_start == 0 {
            crate::log::log(&format!("[sync] push_rows_impl: {table} batch: {} cols, {} rows, simple protocol",
                col_count, batch_len));
        }
        // Build VALUES clause with literal values instead of $N params.
        let mut value_rows: Vec<String> = Vec::with_capacity(batch_len);
        for row in batch {
            let obj = row.as_object();
            let vals: Vec<String> = all_cols.iter().map(|col_name| {
                let v = obj.and_then(|o| o.get(col_name.as_str())).unwrap_or(&serde_json::Value::Null);
                pg_literal(v, pg_column_type(table, col_name))
            }).collect();
            value_rows.push(format!("({})", vals.join(", ")));
        }
        let simple_sql = format!(
            "INSERT INTO {} ({}) VALUES {} ON CONFLICT (\"id\") DO UPDATE SET {}",
            pg_quote_ident(table), col_list,
            value_rows.join(", "), update_set
        );

        match client.batch_execute(&simple_sql) {
            Ok(_) => {
                for row in batch {
                    if let Some(id) = row.get("id").and_then(|v| v.as_str()) {
                        all_acked.push(id.to_string());
                    }
                }
            }
            Err(e) => {
                error_count += 1;
                let batch_info = format!("rows {}-{}", batch_start, batch_end - 1);
                if error_count <= 3 {
                    crate::log::log(&format!("[sync] push_rows_impl: {table} batch {batch_info} failed: {}", error_chain(&e)));
                    // Log first 200 chars of SQL for debugging.
                    if simple_sql.len() > 200 {
                        crate::log::log(&format!("[sync] push_rows_impl: {table} SQL (truncated): {}...", &simple_sql[..200]));
                    }
                }
                // Fallback: retry with smaller batches (50) instead of
                // individual INSERTs which would be N network round-trips.
                let sub_batch_size = 50;
                for sub_start in (0..batch.len()).step_by(sub_batch_size) {
                    let sub_end = (sub_start + sub_batch_size).min(batch.len());
                    let sub = &batch[sub_start..sub_end];
                    let sub_cols: Vec<&String> = sub[0].as_object().map(|o| o.keys().collect()).unwrap_or_default();
                    let sub_col_list: String = sub_cols.iter().map(|c| pg_quote_ident(c)).collect::<Vec<_>>().join(", ");
                    let sub_rows_sql: Vec<String> = sub.iter().filter_map(|r| {
                        let obj = r.as_object()?;
                        let vals: Vec<String> = sub_cols.iter().map(|c| {
                            let v = obj.get(*c).unwrap_or(&serde_json::Value::Null);
                            pg_literal(v, pg_column_type(table, c))
                        }).collect();
                        Some(format!("({})", vals.join(", ")))
                    }).collect();
                    if sub_rows_sql.is_empty() { continue; }
                    let sub_update_set: String = sub_cols.iter().filter(|c| c.as_str() != "id")
                        .map(|c| format!("{} = EXCLUDED.{}", pg_quote_ident(c), pg_quote_ident(c)))
                        .collect::<Vec<_>>().join(", ");
                    let sub_sql = format!(
                        "INSERT INTO {} ({}) VALUES {} ON CONFLICT (\"id\") DO UPDATE SET {}",
                        pg_quote_ident(table), sub_col_list,
                        sub_rows_sql.join(", "), sub_update_set
                    );
                    if client.batch_execute(&sub_sql).is_ok() {
                        for r in sub {
                            if let Some(id) = r.get("id").and_then(|v| v.as_str()) {
                                all_acked.push(id.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    if error_count > 0 {
        crate::log::log(&format!("[sync] push_rows_impl: {table} {}/{} rows succeeded ({} batch errors, retried individually)", all_acked.len(), total_rows, error_count));
    }
    crate::log::log(&format!("[sync] push_rows_impl: {table} DONE — {} ids acked ({} batches, {} errors)", all_acked.len(), (total_rows + BATCH_SIZE - 1) / BATCH_SIZE, error_count));
    Ok(all_acked)
}

// ---------------------------------------------------------------------------
// Background Postgres worker — all network I/O runs here, never on
// Tauri command threads. Callers send a command + reply channel, then
// recv_timeout so they NEVER block for more than WORKER_TIMEOUT.
// ---------------------------------------------------------------------------

/// Maximum time (seconds) a Tauri command thread will wait for a Postgres
/// operation. If the worker is busy (sync push running), the caller gets an
/// error and falls back to local SQLite — the UI stays responsive.
const WORKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

enum PgCommand {
    IsConfigured(Sender<bool>),
    IsConnected(Sender<bool>),
    LastError(Sender<Option<String>>),
    Configure(Option<String>, Sender<Result<(), String>>),
    PushRows(String, Vec<serde_json::Value>, Sender<Result<Vec<String>, String>>),
    QueryRows(String, Vec<String>, Sender<Result<Vec<serde_json::Value>, String>>),
    DeleteRows(String, Vec<String>, Sender<Result<(), String>>),
    Stop,
}

struct PostgresWorker {
    client: Option<postgres::Client>,
    cfg: Option<String>,
    last_err: Option<String>,
    schema_validated: bool,
    last_connect_fail: i64,
    connect_backoff: i64,
    /// Epoch seconds of the last successful health-check ping. Used to
    /// rate-limit pings so we don't add latency to every command.
    last_health_check: i64,
    /// Epoch seconds of the last successful command completion. Used to
    /// keep the is_connected flag true during brief connection blips —
    /// if we were connected 5 seconds ago and the link just hiccuped,
    /// the UI should still show "Online" while we reconnect.
    last_successful_contact: i64,
}

impl PostgresWorker {
    fn new() -> Self {
        Self {
            client: None,
            cfg: None,
            last_err: None,
            schema_validated: false,
            last_connect_fail: 0,
            connect_backoff: 0,
            last_health_check: 0,
            last_successful_contact: 0,
        }
    }

    fn ensure_client(&mut self) -> Result<(), String> {
        let cs = match self.cfg.as_ref() {
            Some(cs) => cs.clone(),
            None => return Err("PostgreSQL is not configured".to_string()),
        };
        if self.client.as_ref().is_some_and(|c| !c.is_closed()) {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        if self.connect_backoff > 0 && now - self.last_connect_fail < self.connect_backoff {
            return Err(format!("backed off (retry in {}s)", self.connect_backoff - (now - self.last_connect_fail)));
        }
        crate::log::log("[sync] ensure_client: connecting…");
        match connect_with_create(&cs) {
            Ok(c) => {
                crate::log::log("[sync] ensure_client: connection SUCCESS");
                self.client = Some(c);
                self.last_err = None;
                self.connect_backoff = 0;
                // Do NOT reset schema_validated here. The schema only
                // needs validation once per configure() call (user changed
                // connection string). Resetting on every reconnect caused
                // ensure_schema_for to run on EVERY push attempt — over
                // an unstable connection, this created an infinite loop
                // of: connect → schema fail → reconnect → schema fail → …
                Ok(())
            }
            Err(e) => {
                crate::log::log(&format!("[sync] ensure_client: connection FAILED: {e}"));
                self.last_err = Some(e.clone());
                // No backoff — retry on next signal immediately.
                let prev = self.connect_backoff;
                self.connect_backoff = 0;
                self.last_connect_fail = now;
                Err(e)
            }
        }
    }

    /// Lightweight connection health check, rate-limited to once every 30s.
    /// Pings the server with `SELECT 1`. If the ping fails or the connection
    /// is dead, drops the client so `ensure_client` will reconnect on the
    /// next command. This prevents the worker from silently hanging on a
    /// half-open TCP connection after a server crash or network blip.
    ///
    /// Also detects "idle in transaction" stuck connections — a COMMIT that
    /// never completed (common with PgBouncer transaction-mode pooling).
    /// If found, drops the client to force a clean reconnect.
    fn check_health(&mut self) {
        // Non-blocking check: just test if the OS has flagged the connection
        // as dead. TCP keepalive (idle=10s, interval=3s) handles dead-connection
        // detection at the OS level. This method does NOT do a network ping
        // (simple_query can hang on a half-open socket, making the health
        // check itself a source of worker hangs).
        let healthy = match self.client.as_ref() {
            Some(c) if !c.is_closed() => true,
            _ => false,
        };
        if !healthy && self.client.is_some() {
            crate::log::log("[sync] pg health check: connection dead — dropping, will reconnect");
            self.client = None;
            // Do NOT reset schema_validated here — schema doesn't change
            // when the connection drops. Resetting causes ensure_schema_for
            // to re-run on every push, which hangs over PgBouncer.
            self.last_err = Some("connection lost, reconnecting".to_string());
        }
    }

    fn handle(&mut self, cmd: PgCommand) {
        match cmd {
            PgCommand::IsConfigured(tx) => {
                let _ = tx.send(self.cfg.is_some());
            }
            PgCommand::IsConnected(tx) => {
                let connected = self.client.as_ref().is_some_and(|c| !c.is_closed());
                let _ = tx.send(connected);
            }
            PgCommand::LastError(tx) => {
                let _ = tx.send(self.last_err.clone());
            }
            PgCommand::Configure(conn_string, tx) => {
                match &conn_string {
                    Some(cs) => {
                        self.cfg = Some(cs.clone());
                        self.client = None;
                        self.last_err = None;
                        self.connect_backoff = 0;
                        self.schema_validated = false;
                        // Try to connect now so is_connected becomes true
                        // immediately. ensure_client has a 6s timeout so it
                        // won't block the worker forever.
                        let result = self.ensure_client().map(|_| ());
                        let _ = tx.send(result);
                    }
                    None => {
                        self.cfg = None;
                        self.client = None;
                        self.last_err = None;
                        let _ = tx.send(Ok(()));
                    }
                }
            }
            PgCommand::PushRows(table, rows, tx) => {
                crate::log::log(&format!("[sync] worker: PushRows {table} ({} rows)", rows.len()));
                // Retry loop: up to 3 attempts with reconnection between
                // each. Over an unstable connection, the first attempt may
                // push some rows before the connection drops. The second
                // attempt reconnects and pushes the remaining rows. Because
                // every INSERT is idempotent (ON CONFLICT DO UPDATE),
                // re-pushing already-pushed rows is safe and fast.
                const MAX_ATTEMPTS: u32 = 1;
                let mut all_acked: Vec<String> = Vec::new();
                let mut last_err = String::new();
                for attempt in 1..=MAX_ATTEMPTS {
                    if attempt > 1 {
                        crate::log::log(&format!("[sync] worker: PushRows {table} — retry attempt {attempt}/{MAX_ATTEMPTS}, reconnecting"));
                        self.client = None;
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                    let result = (|| -> Result<Vec<String>, String> {
                        self.ensure_client()?;
                        let client = self.client.as_mut().ok_or("no client")?;
                        // Skip ensure_schema_for — over PgBouncer, the DDL
                        // queries hang and block all pushes indefinitely.
                        // Instead, do lazy DDL: if INSERT fails because the
                        // table doesn't exist, create it and retry.
                        match push_rows_impl(client, &table, &rows) {
                            Ok(ids) => {
                                if ids.is_empty() && !rows.is_empty() {
                                    // Non-empty rows but empty result → INSERT failed, trigger schema creation
                                    match ensure_schema_for(client) {
                                        Ok(()) => {
                                            self.schema_validated = true;
                                            push_rows_impl(client, &table, &rows)
                                        }
                                        Err(e) => Err(e),
                                    }
                                } else {
                                    Ok(ids)
                                }
                            }
                            Err(e) if e.contains("does not exist") || e.contains("relation") || e.contains("all INSERTs failed") => {
                                crate::log::log(&format!("[sync] worker: PushRows {table} — table missing, creating"));
                                match ensure_schema_for(client) {
                                    Ok(()) => {
                                        self.schema_validated = true;
                                        push_rows_impl(client, &table, &rows)
                                    }
                                    Err(schema_err) => Err(schema_err),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    })();
                    match result {
                        Ok(ids) => {
                            all_acked = ids;
                            if !all_acked.is_empty() {
                                crate::log::log(&format!("[sync] worker: PushRows {table} — attempt {attempt} pushed {}/{} rows", all_acked.len(), rows.len()));
                                // Got results — if all rows pushed, done.
                                // If partial, retry remaining on next attempt.
                                if all_acked.len() >= rows.len() {
                                    break;
                                }
                                last_err = format!("partial: {}/{} pushed", all_acked.len(), rows.len());
                            } else if rows.is_empty() {
                                // Empty input → empty output is success, not error
                                break;
                            } else {
                                last_err = "push returned 0 rows".to_string();
                            }
                        }
                        Err(e) => {
                            last_err = e;
                            crate::log::log(&format!("[sync] worker: PushRows {table} — attempt {attempt} failed: {last_err}"));
                            self.client = None;
                        }
                    }
                }
                // Deduplicate acked ids (from overlapping retries)
                all_acked.sort();
                all_acked.dedup();
                let result = if all_acked.is_empty() && !last_err.is_empty() {
                    Err(last_err)
                } else {
                    Ok(all_acked)
                };
                match &result {
                    Ok(_) => { self.last_err = None; }
                    Err(e) => {
                        self.last_err = Some(e.clone());
                        self.client = None;
                    }
                }
                let _ = tx.send(result);
            }
            PgCommand::QueryRows(sql, params, tx) => {
                let result = (|| -> Result<Vec<serde_json::Value>, String> {
                    self.ensure_client()?;
                    let client = self.client.as_mut().ok_or("no client")?;
                    // Run the query directly WITHOUT a transaction.
                    // Previous code wrapped in a transaction + SET LOCAL statement_timeout,
                    // but PgBouncer in transaction-mode pooling causes COMMIT to hang
                    // (the backend gets reassigned before COMMIT arrives). This leaves
                    // the connection stuck in "idle in transaction" and blocks ALL
                    // subsequent commands including pushes.
                    //
                    // The session-level statement_timeout (30s) set in connect_with_create
                    // already protects against slow queries. No transaction needed for reads.
                    let param_refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
                    let rows = client.query(sql.as_str(), &param_refs)
                        .map_err(|e| format!("query failed: {}", error_chain(&e)))?;
                    let result: Vec<serde_json::Value> = rows.iter().map(|row| {
                        let mut obj = serde_json::Map::new();
                        for (i, col) in row.columns().iter().enumerate() {
                            obj.insert(col.name().to_string(), pg_cell_to_json(row, i));
                        }
                        serde_json::Value::Object(obj)
                    }).collect();
                    Ok(result)
                })();
                let _ = tx.send(result);
            }
            PgCommand::DeleteRows(table, ids, tx) => {
                let result = (|| -> Result<(), String> {
                    self.ensure_client()?;
                    let client = self.client.as_mut().ok_or("no client")?;
                    let sql = format!("DELETE FROM {} WHERE id = $1", pg_quote_ident(&table));
                    for id in &ids {
                        let param: &(dyn ToSql + Sync) = id;
                        client.execute(&sql, std::slice::from_ref(&param))
                            .map_err(|e| format!("delete failed: {}", error_chain(&e)))?;
                    }
                    Ok(())
                })();
                let _ = tx.send(result);
            }
            PgCommand::Stop => {}
        }
    }
}

fn spawn_pg_worker(
    shared_err: std::sync::Arc<Mutex<Option<String>>>,
    is_connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    is_busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
    busy_since: std::sync::Arc<Mutex<Option<std::time::Instant>>>,
) -> Sender<PgCommand> {
    let (tx, rx) = std::sync::mpsc::channel::<PgCommand>();
    let worker_tx = tx.clone();
    std::thread::Builder::new()
        .name("pg-worker".into())
        .spawn(move || {
            let mut worker = PostgresWorker::new();
            while let Ok(cmd) = rx.recv() {
                let is_stop = matches!(cmd, PgCommand::Stop);
                is_busy.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut bs) = busy_since.lock() { *bs = Some(std::time::Instant::now()); }
                worker.check_health();
                let cmd_start = std::time::Instant::now();
                worker.handle(cmd);
                let elapsed = cmd_start.elapsed().as_secs();
                if elapsed > 20 {
                    crate::log::log(&format!("[sync] worker: command took {elapsed}s — possible hang, resetting connection"));
                    worker.client = None;
                    // Do NOT reset schema_validated — schema only changes
                    // on explicit configure(). Resetting here causes the
                    // ensure_schema_for loop that blocks all pushes.
                }
                is_busy.store(false, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut bs) = busy_since.lock() { *bs = None; }
                // Push the worker's last_err into the shared cache so
                // RealPostgres::last_error() can read it without sending
                // a command to this thread (which blocks up to WORKER_TIMEOUT).
                if let Ok(mut e) = shared_err.lock() {
                    *e = worker.last_err.clone();
                }
                // Update is_connected with a 30s grace period. If the
                // client is alive right now, record it. If it's dead but
                // we were connected within the last 30s, keep reporting
                // connected — this prevents the badge from flickering
                // "Offline" during brief network blips while we reconnect.
                let now_ts = chrono::Utc::now().timestamp();
                let client_alive = worker.client.as_ref().is_some_and(|c| !c.is_closed());
                if client_alive {
                    worker.last_successful_contact = now_ts;
                }
                let connected = client_alive
                    || (now_ts - worker.last_successful_contact < 30);
                is_connected.store(connected, std::sync::atomic::Ordering::Relaxed);
                if is_stop { break; }
            }
        })
        .expect("failed to spawn pg-worker thread");
    worker_tx
}

/// A non-blocking PostgreSQL adapter. All network I/O runs on a dedicated
/// background thread (spawned via `spawn_pg_worker`). Tauri command threads
/// send commands via a channel and recv with a timeout — they NEVER block
/// on Supabase network latency.
pub struct RealPostgres {
    tx: Sender<PgCommand>,
    is_connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    is_configured: std::sync::atomic::AtomicBool,
    /// True while the worker is processing a command. Prevents the poller
    /// from queuing duplicate push commands that build up a backlog.
    is_busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Shared last-error cache — the worker writes here after every
    /// operation; `last_error()` reads here (never sends a command).
    /// This prevents sync_status (called on EVERY page mount) from
    /// blocking up to 5 seconds when the worker is busy.
    cached_last_err: Arc<Mutex<Option<String>>>,
    /// Timestamp when the worker started its current command. Used by
    /// push_rows() to detect a stuck worker (>60s) and force-clear is_busy.
    busy_since: Arc<Mutex<Option<std::time::Instant>>>,
}

impl RealPostgres {
    pub fn new() -> Self {
        let shared_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let is_connected: Arc<std::sync::atomic::AtomicBool> = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let is_busy: Arc<std::sync::atomic::AtomicBool> = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let busy_since: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
        let tx = spawn_pg_worker(shared_err.clone(), is_connected.clone(), is_busy.clone(), busy_since.clone());
        Self {
            tx,
            is_connected,
            is_configured: std::sync::atomic::AtomicBool::new(false),
            is_busy,
            cached_last_err: shared_err,
            busy_since,
        }
    }

    /// Restore a previously saved connection string on startup.
    /// Waits for the worker to attempt the connection (up to 20s) so
    /// `is_connected` reflects the real state immediately. If the worker
    /// is busy or the connection fails, the adapter is still marked
    /// configured so the background poller retries on the next cycle.
    pub fn restore(&self, conn_string: String) {
        self.is_configured.store(true, std::sync::atomic::Ordering::Relaxed);
        // Use configure() which waits for the worker response (up to 15s)
        // so is_connected gets properly set on startup.
        match self.configure(Some(conn_string)) {
            Ok(()) => crate::log::log("[sync] restore: connected on startup"),
            Err(e) => crate::log::log(&format!("[sync] restore: startup connect deferred — {e}")),
        }
    }

    /// Send a command to the worker with a custom timeout.
    fn send_timeout<T>(&self, cmd: PgCommand, rx: std::sync::mpsc::Receiver<T>, timeout: std::time::Duration) -> Option<T> {
        match self.tx.send(cmd) {
            Ok(()) => rx.recv_timeout(timeout).ok(),
            Err(_) => None, // worker died
        }
    }

    /// Send a command to the worker and wait up to WORKER_TIMEOUT.
    /// Returns None if the worker is busy (sync push running) — callers
    /// fall back to local data instead of blocking.
    fn send<T>(&self, cmd: PgCommand, rx: std::sync::mpsc::Receiver<T>) -> Option<T> {
        self.send_timeout(cmd, rx, WORKER_TIMEOUT)
    }
}

impl PostgresAdapter for RealPostgres {
    fn label(&self) -> &str {
        "postgres"
    }
    fn configured(&self) -> bool {
        self.is_configured.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn last_error(&self) -> Option<String> {
        // Pure Mutex read — NEVER sends a command to the worker.
        // The worker updates cached_last_err after every push/query/configure.
        // This ensures sync_status (called by every page) never blocks.
        let cached = self.cached_last_err.lock().ok().and_then(|e| e.clone());
        if cached.is_some() {
            return cached;
        }
        // Unconfigured adapters should explain themselves so the UI
        // shows "Not configured" instead of a blank badge.
        if !self.configured() {
            return Some("PostgreSQL is not configured".to_string());
        }
        None
    }
    fn connected(&self) -> bool {
        // Pure atomic check — NEVER sends a command to the worker.
        // The worker updates this flag after every push/query/configure.
        // This ensures sync_status (called by every page) never blocks.
        self.is_connected.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn push_rows(&self, table: &str, rows: &[serde_json::Value]) -> Result<Vec<String>, String> {
        // If the worker is busy, check if it's been stuck for >60s.
        // This happens when a QueryRows blocks on a dead connection and
        // the worker thread never returns. Force-clear is_busy so we can
        // retry (the stuck worker will eventually fail and reset itself).
        if self.is_busy.load(std::sync::atomic::Ordering::Relaxed) {
            let stuck = self.busy_since.lock()
                .ok()
                .and_then(|bs| *bs)
                .map(|t| t.elapsed().as_secs() > 60)
                .unwrap_or(false);
            if stuck {
                crate::log::log(&format!("[sync] {table}: worker stuck >60s, force-clearing is_busy"));
                self.is_busy.store(false, std::sync::atomic::Ordering::SeqCst);
            } else {
                crate::log::log(&format!("[sync] {table}: push deferred — worker busy"));
                return Err(format!("{table} push deferred — worker busy (will retry next cycle)"));
            }
        }
        // Mark as busy now (only if not already set by the force-clear above).
        if !self.is_busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
            if let Ok(mut bs) = self.busy_since.lock() { *bs = Some(std::time::Instant::now()); }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        // Give push_rows up to 15 seconds. This runs on dedicated background
        // worker threads, so it NEVER blocks the UI or main thread.
        let result = match self.send_timeout(PgCommand::PushRows(table.to_string(), rows.to_vec(), tx), rx, std::time::Duration::from_secs(15)) {
            Some(result) => {
                // NOTE: Do NOT set is_connected here — let only the worker thread
                // manage is_connected to avoid races between caller and worker.
                result
            }
            None => {
                // Worker timed out — clear is_busy so next cycle can retry.
                self.is_busy.store(false, std::sync::atomic::Ordering::SeqCst);
                if let Ok(mut bs) = self.busy_since.lock() { *bs = None; }
                Err(format!("{table} push timed out — retrying next cycle"))
            }
        };
        // Always clear is_busy.
        self.is_busy.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut bs) = self.busy_since.lock() { *bs = None; }
        result
    }
    fn configure(&self, conn_string: Option<String>) -> Result<(), String> {
        // Do NOT set is_configured or is_connected here — if the worker is busy,
        // the send_timeout below returns None and the flags stay stale.
        // Only update on a confirmed result from the worker.
        let (tx, rx) = std::sync::mpsc::channel();
        match self.send_timeout(PgCommand::Configure(conn_string.clone(), tx), rx, std::time::Duration::from_secs(15)) {
            Some(result) => {
                self.is_configured.store(conn_string.is_some() && result.is_ok(), std::sync::atomic::Ordering::Relaxed);
                self.is_connected.store(result.is_ok(), std::sync::atomic::Ordering::Relaxed);
                result
            }
            None => {
                // Worker busy — don't touch is_connected or is_configured.
                // The worker will process Configure eventually and update them.
                // The UI's 5s refresh will pick up the correct state.
                Err("Postgres worker busy — retrying in background".to_string())
            }
        }
    }
    fn delete_rows(&self, table: &str, ids: &[String]) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        match self.send_timeout(PgCommand::DeleteRows(table.to_string(), ids.to_vec(), tx), rx, std::time::Duration::from_secs(15)) {
            Some(result) => result,
            None => {
                // Worker timed out — clear is_busy so the next push can proceed.
                self.is_busy.store(false, std::sync::atomic::Ordering::SeqCst);
                if let Ok(mut bs) = self.busy_since.lock() { *bs = None; }
                Err(format!("{table} delete deferred — worker busy"))
            }
        }
    }
    fn query_rows(&self, sql: &str, params: &[String]) -> Result<Vec<serde_json::Value>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        match self.send_timeout(PgCommand::QueryRows(sql.to_string(), params.to_vec(), tx), rx, std::time::Duration::from_secs(15)) {
            Some(result) => result,
            None => {
                // Worker timed out — clear is_busy so the next push can proceed.
                self.is_busy.store(false, std::sync::atomic::Ordering::SeqCst);
                if let Ok(mut bs) = self.busy_since.lock() { *bs = None; }
                Err("PostgreSQL query timed out — falling back to local data".to_string())
            }
        }
    }
}

/// Convert one central row cell into JSON, keyed by the column's Postgres type.
/// Central columns are TEXT / INTEGER / DOUBLE PRECISION only (see
/// `pg_column_type`), so the numeric/string branches cover every real case;
/// the fallback tries the string read so unknown column types degrade to text
/// instead of erroring.
fn pg_cell_to_json(row: &postgres::Row, i: usize) -> serde_json::Value {
    use postgres::types::Type;
    match *row.columns()[i].type_() {
        Type::BOOL => row
            .try_get::<_, Option<bool>>(i)
            .ok()
            .flatten()
            .map(serde_json::Value::Bool)
            .unwrap_or(serde_json::Value::Null),
        Type::INT2 | Type::INT4 | Type::INT8 => row
            .try_get::<_, Option<i64>>(i)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v))
            .unwrap_or(serde_json::Value::Null),
        Type::FLOAT4 | Type::FLOAT8 | Type::NUMERIC => row
            .try_get::<_, Option<f64>>(i)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v))
            .unwrap_or(serde_json::Value::Null),
        _ => row
            .try_get::<_, Option<String>>(i)
            .ok()
            .flatten()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
    }
}

// ---------------------------------------------------------------------------
// Real Google Sheets adapter — Phase 4 (05-ui-screens.md §6f)
// ---------------------------------------------------------------------------

/// Column order for the exported sheet; keep in sync with `sheet_trip_rows`.
/// The first column carries the trip id so the sheet can be pruned precisely
/// (soft/hard-deleted rows) and invoice-makers can cross-reference a trip.

fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'!') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn sa_email(service_account_json: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(service_account_json).map_err(|e| format!("invalid service account JSON: {e}"))?;
    parsed["client_email"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or("service account JSON is missing client_email".to_string())
}

/// Get the current server time from Google to avoid JWT failures when the
/// local clock is skewed (which Google strictly rejects).
fn google_server_time(client: &reqwest::blocking::Client) -> i64 {
    // Hit any lightweight Google endpoint and parse the `Date` header.
    // We intentionally ignore errors — the caller falls back to local time.
    if let Ok(resp) = client.get("https://www.googleapis.com/").send() {
        if let Some(date_str) = resp.headers().get("date") {
            if let Ok(date_val) = date_str.to_str() {
                // HTTP Date format: "Thu, 20 Aug 2026 02:16:28 GMT"
                if let Ok(tm) = chrono::DateTime::parse_from_str(date_val, "%a, %d %b %Y %H:%M:%S %z") {
                    return tm.timestamp();
                }
            }
        }
    }
    chrono::Utc::now().timestamp()
}

/// Exchange the service account's signed JWT for an access token (RFC 7523).
fn fetch_token(client: &reqwest::blocking::Client, service_account_json: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(service_account_json).map_err(|e| format!("invalid service account JSON: {e}"))?;
    let email = parsed["client_email"]
        .as_str()
        .ok_or("service account JSON is missing client_email")?
        .to_string();
    let key = parsed["private_key"]
        .as_str()
        .ok_or("service account JSON is missing private_key")?
        .to_string();
    let token_uri = parsed["token_uri"]
        .as_str()
        .unwrap_or("https://oauth2.googleapis.com/token")
        .to_string();
    // Use Google's server time to avoid clock skew issues.
    let now = google_server_time(client);
    let claims = json!({
        "iss": email,
        "scope": "https://www.googleapis.com/auth/spreadsheets",
        "aud": token_uri,
        "iat": now,
        "exp": now + 3600,
    });
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    // Trim whitespace/newlines that may sneak in when the JSON is pasted.
    let key_clean = key.trim().replace('\r', "");
    let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(key_clean.as_bytes())
        .map_err(|e| format!("invalid private key: {e}"))?;
    let jwt = jsonwebtoken::encode(&header, &claims, &encoding_key)
        .map_err(|e| format!("cannot sign JWT: {e}"))?;
    crate::log::log(&format!("[sheets-auth] JWT signed OK, email={email}, aud={token_uri}"));
    // Reuse the client created above for google_server_time.
    // The JWT is base64url (no characters needing form encoding).
    let body = format!("grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion={jwt}");
    let resp = client
        .post(&token_uri)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = resp.status();
    let resp_text = resp.text().unwrap_or_default();
    if !status.is_success() {
        // Parse Google's error message for a helpful hint
        if let Ok(err_json) = serde_json::from_str::<serde_json::Value>(&resp_text) {
            let msg = err_json.get("error_description").or(err_json.get("error"))
                .and_then(|v| v.as_str()).unwrap_or(&resp_text);
            return Err(format!("Google token error ({status}): {msg}"));
        }
        return Err(format!("Google token error ({status}): {resp_text}"));
    }
    let resp_json: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| format!("token response unreadable: {e} — raw: {resp_text}"))?;
    resp_json["access_token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or("token response missing access_token".to_string())
}

/// Verify the service account can read the sheet and return its first tab name.
fn sheet_meta(client: &reqwest::blocking::Client, token: &str, sheet_id: &str) -> Result<String, String> {
    let url = format!(
        "https://sheets.googleapis.com/v4/spreadsheets/{sheet_id}?fields=sheets.properties.title"
    );
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .map_err(|e| format!("cannot reach Google Sheets: {e}"))?
        .error_for_status()
        .map_err(|e| {
            format!("Google Sheets rejected access to this sheet — share it with the service account email: {e}")
        })?;
    let j: serde_json::Value = resp.json().map_err(|e| format!("sheet metadata unreadable: {e}"))?;
    j["sheets"][0]["properties"]["title"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or("the spreadsheet has no first tab".to_string())
}

/// Always overwrite the header row in the sheet to match the current mapping.
/// This runs on every sync cycle so that reordered/renamed/disabled columns
/// and newly added custom fields are reflected immediately.
fn ensure_headers(client: &reqwest::blocking::Client, token: &str, sheet_id: &str, tab: &str, mapping: &[crate::models::SheetColumnEntry]) -> Result<(), String> {
    let range = format!("{tab}!A1");
    let headers: Vec<serde_json::Value> = mapping.iter().filter(|e| e.enabled).map(|e| json!(e.header)).collect();
    if headers.is_empty() {
        return Ok(());
    }
    // Always write headers — this keeps the sheet in sync with the mapping.
    let body = json!({ "range": range, "majorDimension": "ROWS", "values": vec![headers] });
    let put_url = format!(
        "https://sheets.googleapis.com/v4/spreadsheets/{sheet_id}/values/{}?valueInputOption=RAW",
        urlenc(&range)
    );
    client
        .put(&put_url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .map_err(|e| format!("cannot write sheet header: {e}"))?
        .error_for_status()
        .map_err(|e| format!("sheet header write rejected: {e}"))?;
    Ok(())
}

fn fmt_opt_num(v: Option<&serde_json::Value>) -> serde_json::Value {
    match v {
        Some(serde_json::Value::Number(n)) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 => json!((f as i64).to_string()),
            Some(f) => json!(f.to_string()),
            None => json!(""),
        },
        _ => json!(""),
    }
}

fn opt_str(v: Option<&serde_json::Value>) -> serde_json::Value {
    match v {
        Some(serde_json::Value::String(s)) => json!(s),
        _ => json!(""),
    }
}

/// Map a field_key to the corresponding cell value in a trip row.
fn field_key_to_value(row: &serde_json::Value, field_key: &str) -> serde_json::Value {
    match field_key {
        "id" => opt_str(row.get("id")),
        "plate" => opt_str(row.get("plate")),
        "entry_time" => opt_str(row.get("entry_time")),
        "exit_time" => opt_str(row.get("exit_time")),
        "trip_status" => opt_str(row.get("trip_status")),
        "capacity_at_trip" => fmt_opt_num(row.get("capacity_at_trip")),
        "capacity_unit" => opt_str(row.get("capacity_unit")),
        "receipt_no" => opt_str(row.get("receipt_no")),
        "confidence_score" => fmt_opt_num(row.get("confidence_score")),
        "capture_method" => opt_str(row.get("capture_method")),
        "is_discharge_trip" => match row.get("is_discharge_trip") {
            Some(serde_json::Value::Bool(true)) => json!("Yes"),
            Some(serde_json::Value::Bool(false)) => json!("No"),
            _ => json!(""),
        },
        "created_at" => opt_str(row.get("created_at")),
        "company" => opt_str(row.get("company")),
        "driver" => opt_str(row.get("driver")),
        "officer_name" => opt_str(row.get("officer_name")),
        "model_version" => opt_str(row.get("model_version")),
        "ocr_engine" => opt_str(row.get("ocr_engine")),
        "status" => opt_str(row.get("status")),
        // Custom parent fields: vehicle_extra_<key>, company_extra_<key>, driver_extra_<key>
        k if k.starts_with("vehicle_extra_") || k.starts_with("company_extra_") || k.starts_with("driver_extra_") => {
            extract_extra_field(row, k)
        }
        _ => json!(""),
    }
}

/// Extract a custom field value from the extra_fields JSON blob stored on
/// a row. The field_key format is `<entity>_extra_<field_key>`.
fn extract_extra_field(row: &serde_json::Value, field_key: &str) -> serde_json::Value {
    let (entity, raw_key) = if let Some(rest) = field_key.strip_prefix("vehicle_extra_") {
        ("vehicles_extra", rest)
    } else if let Some(rest) = field_key.strip_prefix("company_extra_") {
        ("companies_extra", rest)
    } else if let Some(rest) = field_key.strip_prefix("driver_extra_") {
        ("drivers_extra", rest)
    } else {
        return json!("");
    };
    match row.get(entity) {
        Some(serde_json::Value::Object(map)) => match map.get(raw_key) {
            Some(serde_json::Value::String(s)) => json!(s),
            Some(serde_json::Value::Number(n)) => json!(n.to_string()),
            Some(serde_json::Value::Bool(b)) => json!(if *b { "Yes" } else { "No" }),
            Some(other) => json!(other.to_string()),
            None => json!(""),
        },
        _ => json!(""),
    }
}

#[derive(Clone)]
struct SheetsCreds {
    client_email: String,
    json: String,
    sheet_id: String,
    /// First tab name; None until validated (network). Restored credentials
    /// start None and are validated lazily so app launch never blocks on
    /// Google.
    first_sheet: Option<String>,
}

/// Service-account Google Sheets export: signed-JWT auth (no browser popup),
/// appending rows to the first tab of the configured spreadsheet.
pub struct RealSheets {
    creds: Mutex<Option<SheetsCreds>>,
    token: Mutex<Option<(String, i64)>>,
    last_fail: Mutex<Option<i64>>,
    last_err: Mutex<Option<String>>,
    mapping: Mutex<Vec<crate::models::SheetColumnEntry>>,
    /// Cached connected state — updated after each sync cycle, NOT on every
    /// connected() call. This prevents Google Sheets network calls from
    /// blocking the Tauri command thread when sync_status is called by
    /// every page on mount.
    cached_connected: std::sync::atomic::AtomicBool,
    /// Reusable HTTP client — avoids TCP+TLS handshake on every API call.
    client: reqwest::blocking::Client,
    /// Hash of the last-written header row — skip the PUT call if unchanged.
    cached_header_hash: Mutex<Option<u64>>,
}

impl RealSheets {
    pub fn new() -> Self {
        Self {
            creds: Mutex::new(None),
            token: Mutex::new(None),
            last_fail: Mutex::new(None),
            last_err: Mutex::new(None),
            mapping: Mutex::new(default_sheet_mapping()),
            cached_connected: std::sync::atomic::AtomicBool::new(false),
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("HTTP client"),
            cached_header_hash: Mutex::new(None),
        }
    }
    pub fn set_mapping(&self, m: Vec<crate::models::SheetColumnEntry>) {
        *self.mapping.lock().unwrap_or_else(|e| e.into_inner()) = m;
    }

    fn set_error(&self, e: String) {
        *self.last_err.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
    }

    /// Cached access token; refreshes only when expired. Errors are cached for
    /// 60s so an offline machine doesn't hammer the token endpoint.
    /// Restore previously saved credentials WITHOUT any network (startup path).
    /// The sheet is validated lazily on first real use.
    pub fn restore(&self, json: String, sheet_id: String) -> Result<String, String> {
        let email = sa_email(&json)?;
        *self.creds.lock().unwrap_or_else(|e| e.into_inner()) = Some(SheetsCreds {
            client_email: email.clone(),
            json,
            sheet_id,
            first_sheet: None,
        });
        *self.last_err.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // Set cached_connected so sheets_due() can return true on first poller cycle.
        // ensure_validated() slow path will confirm/override this on first use.
        self.cached_connected.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(email)
    }

    /// Resolve the first tab name (network) exactly once; everything that
    /// touches the sheet calls this first.
    ///
    /// IMPORTANT: clones creds before any network call so the Mutex is never
    /// held during I/O. This prevents sync_status (called from the frontend)
    /// from blocking on a long Google Sheets token refresh.
    fn ensure_validated(&self) -> Result<String, String> {
        // Fast path: already validated — just read the cached tab name.
        {
            let creds = self.creds.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = creds.as_ref() {
                if let Some(first) = c.first_sheet.as_ref() {
                    return Ok(first.clone());
                }
            } else {
                return Err("Google Sheets is not configured".to_string());
            }
        } // Mutex dropped — all network calls below are lock-free.

        // Clone creds for network calls (Mutex not held).
        let (json, sheet_id) = {
            let creds = self.creds.lock().unwrap_or_else(|e| e.into_inner());
            let c = creds.as_ref().ok_or("Google Sheets is not configured")?;
            (c.json.clone(), c.sheet_id.clone())
        };
        let token = fetch_token(&self.client, &json).map_err(|e| {
            self.set_error(e.clone());
            self.cached_connected.store(false, std::sync::atomic::Ordering::Relaxed);
            e
        })?;
        let first = sheet_meta(&self.client, &token, &sheet_id).map_err(|e| {
            self.set_error(e.clone());
            self.cached_connected.store(false, std::sync::atomic::Ordering::Relaxed);
            e
        })?;
        let mapping = self.mapping.lock().unwrap_or_else(|e| e.into_inner()).clone();
        ensure_headers(&self.client, &token, &sheet_id, &first, &mapping).map_err(|e| {
            self.set_error(e.clone());
            self.cached_connected.store(false, std::sync::atomic::Ordering::Relaxed);
            e
        })?;
        let server_now = google_server_time(&self.client);
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some((token, server_now + 3600 - 60));
        // Write back the validated tab name under Mutex (fast write).
        if let Some(c) = self.creds.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            c.first_sheet = Some(first.clone());
        }
        *self.last_err.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // Cache the connected state so connected() never needs network.
        self.cached_connected.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(first)
    }

    fn access_token(&self) -> Result<String, String> {
        let creds = self
            .creds
            .lock()
            .unwrap()
            .clone()
            .ok_or("Google Sheets is not configured")?;
        let now = chrono::Utc::now().timestamp();
        {
            let tok = self.token.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((t, exp)) = tok.as_ref() {
                if *exp > now {
                    return Ok(t.clone());
                }
            }
        }
        if let Some(last) = *self.last_fail.lock().unwrap_or_else(|e| e.into_inner()) {
            if now - last < 60 {
                return Err("Google Sheets is unreachable (recent attempt failed)".to_string());
            }
        }
        match fetch_token(&self.client, &creds.json) {
            Ok(t) => {
                // Use Google server time for expiry so the cached token isn't
                // prematurely rejected when the local clock is skewed.
                let server_now = google_server_time(&self.client);
                *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some((t.clone(), server_now + 3600 - 60));
                Ok(t)
            }
            Err(e) => {
                *self.last_fail.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
                self.set_error(e.clone());
                Err(e)
            }
        }
    }
}

impl SheetsProvider for RealSheets {
    fn label(&self) -> &str {
        "google-sheets"
    }
    fn set_sheet_mapping(&self, mapping: &[crate::models::SheetColumnEntry]) {
        *self.mapping.lock().unwrap_or_else(|e| e.into_inner()) = mapping.to_vec();
    }
    fn configured(&self) -> bool {
        self.creds.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
    fn last_error(&self) -> Option<String> {
        self.last_err.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn service_account_email(&self) -> Option<String> {
        self.creds.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|c| c.client_email.clone())
    }
    fn connected(&self) -> bool {
        // Pure atomic check — NEVER makes network calls on every invocation.
        // The flag is updated by ensure_validated() after each sync cycle.
        self.cached_connected.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn push_trips(&self, rows: &[serde_json::Value], mapping: &[crate::models::SheetColumnEntry]) -> Result<Vec<String>, String> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let creds = self.creds.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or("Google Sheets is not configured")?;
        let first_sheet = self.ensure_validated()?;
        let token = self.access_token().map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        // One batched request for the whole batch — NOT one request per row.
        // Per-row requests held the DB lock for minutes on a full backlog;
        // this is what kept the app "lugging" after launch.
        // Use column mapping: only enabled fields, in mapping order, with custom headers.
        let enabled_keys: Vec<&str> = mapping.iter().filter(|e| e.enabled).map(|e| e.field_key.as_str()).collect();
        let values: Vec<Vec<serde_json::Value>> = rows
            .iter()
            .map(|row| enabled_keys.iter().map(|k| field_key_to_value(row, k)).collect())
            .collect();
        let range = format!("{first_sheet}!A1");
        let url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{}/values/{}:append?valueInputOption=RAW",
            creds.sheet_id,
            urlenc(&range)
        );
        let body = json!({ "range": range, "majorDimension": "ROWS", "values": values });
        self.client
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .map_err(|e| format!("sheet append failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("sheet append rejected (share the sheet with the service account): {e}"))?;
        let acked: Vec<String> = rows
            .iter()
            .map(|r| r["id"].as_str().unwrap_or_default().to_string())
            .collect();
        *self.last_err.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(acked)
    }
    fn ensure_header_row(&self, mapping: &[crate::models::SheetColumnEntry]) -> Result<(), String> {
        // Skip the API call if headers haven't changed since last write.
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for e in mapping.iter().filter(|e| e.enabled) {
                e.header.hash(&mut h);
            }
            h.finish()
        };
        {
            let cached = self.cached_header_hash.lock().unwrap_or_else(|e| e.into_inner());
            if *cached == Some(hash) {
                return Ok(());
            }
        }
        let creds = self.creds.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or("Google Sheets is not configured")?;
        let first_sheet = self.ensure_validated()?;
        let token = self.access_token().map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        ensure_headers(&self.client, &token, &creds.sheet_id, &first_sheet, mapping).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        *self.cached_header_hash.lock().unwrap_or_else(|e| e.into_inner()) = Some(hash);
        Ok(())
    }
    fn push_new_rows(&self, rows: &[serde_json::Value], mapping: &[crate::models::SheetColumnEntry]) -> Result<Vec<String>, String> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let creds = self.creds.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or("Google Sheets is not configured")?;
        let first_sheet = self.ensure_validated()?;
        let token = self.access_token().map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let enabled_keys: Vec<&str> = mapping.iter().filter(|e| e.enabled).map(|e| e.field_key.as_str()).collect();
        let values: Vec<Vec<serde_json::Value>> = rows
            .iter()
            .map(|row| {
                enabled_keys.iter().map(|k| {
                    let v = field_key_to_value(row, k);
                    // Ensure every cell is a string (Google RAW mode expects strings).
                    match v {
                        serde_json::Value::Null => json!(""),
                        serde_json::Value::String(_) => v,
                        other => json!(other.to_string()),
                    }
                }).collect()
            })
            .collect();
        let range = format!("{first_sheet}!A1");
        let url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{}/values/{}:append?valueInputOption=RAW&includeValuesInResponse=true",
            creds.sheet_id,
            urlenc(&range)
        );
        let body = json!({ "range": range, "majorDimension": "ROWS", "values": values });
        let resp = self.client
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .map_err(|e| format!("sheet append failed: {e}"))?;
        let status = resp.status();
        let resp_text = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(format!("sheet append rejected ({status}): {resp_text}"));
        }
        let j: serde_json::Value = serde_json::from_str(&resp_text)
            .map_err(|e| format!("sheet append response unreadable: {e} — raw: {resp_text}"))?;
        let updated_range = j["updates"]["updatedRange"].as_str().unwrap_or("");
        let start_row = updated_range.split('!').last()
            .and_then(|r| r.split(':').next())
            .and_then(|r| r.chars().skip_while(|c| c.is_alphabetic()).collect::<String>().parse::<i64>().ok())
            .unwrap_or(0);
        let acked: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let id = r["id"].as_str().unwrap_or_default().to_string();
                // Encode sheet_row into the ack string as "id:row" so caller can store it.
                if start_row > 0 {
                    format!("{}:{}", id, start_row + i as i64)
                } else {
                    id
                }
            })
            .collect();
        *self.last_err.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(acked)
    }
    fn update_existing_rows(&self, rows: &[serde_json::Value], mapping: &[crate::models::SheetColumnEntry]) -> Result<(), String> {
        if rows.is_empty() {
            return Ok(());
        }
        let creds = self.creds.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or("Google Sheets is not configured")?;
        let first_sheet = self.ensure_validated()?;
        let token = self.access_token().map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let enabled_keys: Vec<&str> = mapping.iter().filter(|e| e.enabled).map(|e| e.field_key.as_str()).collect();
        let col_count = enabled_keys.len();
        let end_col = (b'A' + (col_count as u8).saturating_sub(1).min(25)) as char;

        let mut data_entries = Vec::new();
        for row in rows {
            let sheet_row = row.get("sheet_row").and_then(|v| v.as_i64()).unwrap_or(0);
            if sheet_row <= 0 { continue; }
            let values: Vec<serde_json::Value> = enabled_keys.iter().map(|k| {
                let v = field_key_to_value(row, k);
                match v {
                    serde_json::Value::Null => json!(""),
                    serde_json::Value::String(_) => v,
                    other => json!(other.to_string()),
                }
            }).collect();
            let range = format!("{first_sheet}!A{sheet_row}:{end_col}{sheet_row}");
            data_entries.push(json!({
                "range": range,
                "majorDimension": "ROWS",
                "values": [values]
            }));
        }

        if !data_entries.is_empty() {
            // Batch update in chunks of 50 in a single network round-trip per chunk
            for chunk in data_entries.chunks(50) {
                let url = format!(
                    "https://sheets.googleapis.com/v4/spreadsheets/{}/values:batchUpdate",
                    creds.sheet_id
                );
                let body = json!({
                    "valueInputOption": "RAW",
                    "data": chunk
                });
                self.client
                    .post(&url)
                    .bearer_auth(&token)
                    .json(&body)
                    .send()
                    .map_err(|e| format!("sheet batch update failed: {e}"))?
                    .error_for_status()
                    .map_err(|e| format!("sheet batch update rejected: {e}"))?;
            }
        }
        *self.last_err.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }
    fn configure(&self, json: Option<String>, sheet_id: Option<String>) -> Result<String, String> {
        let (Some(j), Some(sid)) = (json, sheet_id) else {
            *self.creds.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *self.token.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *self.last_fail.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *self.last_err.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.cached_connected.store(false, std::sync::atomic::Ordering::Relaxed);
            return Ok("disconnected".to_string());
        };
        let email = sa_email(&j).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let token = fetch_token(&self.client, &j).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let first_sheet = sheet_meta(&self.client, &token, &sid).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let mapping = self.mapping.lock().unwrap_or_else(|e| e.into_inner()).clone();
        ensure_headers(&self.client, &token, &sid, &first_sheet, &mapping).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        *self.creds.lock().unwrap_or_else(|e| e.into_inner()) = Some(SheetsCreds {
            client_email: email.clone(),
            json: j,
            sheet_id: sid,
            first_sheet: Some(first_sheet),
        });
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some((token, chrono::Utc::now().timestamp() + 3600 - 60));
        *self.last_err.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // Set cached_connected so sheets_due() returns true on next poller cycle.
        self.cached_connected.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(email)
    }
    fn prune(&self, cutoff_iso: Option<&str>, excluded_ids: &[String]) -> Result<usize, String> {
        let creds = self.creds.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or("Google Sheets is not configured")?;
        let first_sheet = self.ensure_validated()?;
        let token = self.access_token().map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let range = format!("{first_sheet}!A1:Z");
        let url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{}/values/{}?majorDimension=ROWS",
            creds.sheet_id,
            urlenc(&range)
        );
        let resp = self.client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .map_err(|e| format!("sheet read failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("sheet read rejected: {e}"))?;
        let j: serde_json::Value = resp.json().map_err(|e| format!("sheet read unreadable: {e}"))?;
        let Some(rows) = j["values"].as_array() else {
            return Ok(0);
        };
        // Row 0 is the header; data rows follow. Columns: Trip ID (0) … Created at (9).
        let total = rows.len().saturating_sub(1);
        if total == 0 {
            return Ok(0);
        }
        let cell = |row: &serde_json::Value, idx: usize| -> String {
            row.get(idx).and_then(|v| v.as_str()).unwrap_or("").to_string()
        };
        let cutoff = cutoff_iso.and_then(|c| {
            chrono::DateTime::parse_from_rfc3339(c)
                .ok()
                .map(|t| t.with_timezone(&chrono::Utc))
        });
        let keep: Vec<&serde_json::Value> = rows
            .iter()
            .skip(1)
            .filter(|row| {
                if excluded_ids.iter().any(|e| e == &cell(row, 0)) {
                    return false;
                }
                match &cutoff {
                    // No cutoff → clear everything (used by clear_exported_trips)
                    None => false,
                    // With cutoff → keep only rows newer than the cutoff
                    Some(cut) => {
                        if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&cell(row, 9)) {
                            if ts.with_timezone(&chrono::Utc) < *cut {
                                return false;
                            }
                        }
                        true
                    }
                }
            })
            .collect();
        let removed = total - keep.len();
        if removed == 0 {
            return Ok(0);
        }
        // Rewrite: clear every data row, then write back what stays.
        let data_range = format!("{first_sheet}!A2:Z");
        let clear_url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{}/values/{}:clear",
            creds.sheet_id,
            urlenc(&data_range)
        );
        self.client
            .post(&clear_url)
            .bearer_auth(&token)
            .json(&json!({}))
            .send()
            .map_err(|e| format!("sheet clear failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("sheet clear rejected: {e}"))?;
        if !keep.is_empty() {
            let keep_vals: Vec<serde_json::Value> = keep.into_iter().cloned().collect();
            let body = json!({ "range": data_range, "majorDimension": "ROWS", "values": keep_vals });
            let put_url = format!(
                "https://sheets.googleapis.com/v4/spreadsheets/{}/values/{}?valueInputOption=RAW",
                creds.sheet_id,
                urlenc(&data_range)
            );
            self.client
                .put(&put_url)
                .bearer_auth(&token)
                .json(&body)
                .send()
                .map_err(|e| format!("sheet rewrite failed: {e}"))?
                .error_for_status()
                .map_err(|e| format!("sheet rewrite rejected: {e}"))?;
        }
        Ok(removed)
    }
    fn read_existing_trip_ids(&self) -> Result<Vec<String>, String> {
        let creds = self.creds.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or("Google Sheets is not configured")?;
        let first_sheet = self.ensure_validated()?;
        let token = self.access_token().map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let range = format!("{first_sheet}!A1:A");
        let url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{}/values/{}?majorDimension=ROWS",
            creds.sheet_id,
            urlenc(&range)
        );
        let resp = self.client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .map_err(|e| format!("sheet read failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("sheet read rejected: {e}"))?;
        let j: serde_json::Value = resp.json().map_err(|e| format!("sheet read unreadable: {e}"))?;
        let Some(rows) = j["values"].as_array() else {
            return Ok(Vec::new());
        };
        // Skip header row (index 0), collect all trip IDs from column A.
        Ok(rows
            .iter()
            .skip(1)
            .filter_map(|row| row.get(0)?.as_str().map(String::from))
            .filter(|id| !id.is_empty())
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Startup wiring + configuration commands
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// REST-based PostgreSQL adapter using Supabase REST API (PostgREST)
// This is faster and more resilient than PgBouncer for unstable connections
// ---------------------------------------------------------------------------

/// Introspect local SQLite schema and generate CREATE TABLE statements for Supabase.
/// This dynamically creates tables that match each company's actual data structure -
/// no hardcoded schema, so different companies with different fields are supported.
/// DEPRECATED: Tables are pre-created by running SUPABASE_SETUP.sql. Kept for future use.
#[allow(dead_code)]
fn generate_schema_from_local(conn: &Connection) -> Result<String, String> {
    let mut sql = String::new();

    // Get all tables from local SQLite
    let mut tables_stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(|e| format!("Failed to query tables: {e}"))?;

    let table_names: Vec<String> = tables_stmt
        .query_map([], |r| r.get(0))
        .map_err(|e| format!("Failed to read table names: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    for table_name in table_names {
        // Get columns for this table
        let pragma_sql = format!("PRAGMA table_info(\"{}\")", table_name);
        let mut col_stmt = conn.prepare(&pragma_sql)
            .map_err(|e| format!("Failed to query columns for {table_name}: {e}"))?;

        let columns: Vec<(String, String, bool, Option<String>)> = col_stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,  // name
                    r.get::<_, String>(2)?,  // type
                    r.get::<_, bool>(3)?,    // notnull
                    r.get::<_, Option<String>>(4)?,  // dflt_value
                ))
            })
            .map_err(|e| format!("Failed to read columns: {e}"))?
            .filter_map(|r| r.ok())
            .collect();

        if columns.is_empty() {
            continue;
        }

        // Map SQLite types to PostgreSQL types
        let mut col_defs = Vec::new();
        for (name, sql_type, notnull, default) in &columns {
            let pg_type = match sql_type.to_uppercase().as_str() {
                "INTEGER" => "BIGINT",
                "REAL" => "DOUBLE PRECISION",
                "TEXT" => "TEXT",
                "BLOB" => "BYTEA",
                "BOOLEAN" => "BOOLEAN",
                "DATETIME" | "TIMESTAMP" => "TIMESTAMPTZ",
                _ => "TEXT", // Default to TEXT for unknown types
            };

            let mut col_def = format!("    \"{}\" {}", name, pg_type);
            if *notnull && name != "id" {
                col_def.push_str(" NOT NULL");
            }
            if let Some(def) = default {
                col_def.push_str(&format!(" DEFAULT {}", def));
            }
            col_defs.push(col_def);
        }

        // PRIMARY KEY on id column
        if columns.iter().any(|(n, _, _, _)| n == "id") {
            col_defs.push("    PRIMARY KEY (\"id\")".to_string());
        }

        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS public.\"{}\" (\n{}\n);\n\n",
            table_name,
            col_defs.join(",\n")
        );
        sql.push_str(&create_sql);
    }

    // Grant access
    sql.push_str("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO anon;\n");
    sql.push_str("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO authenticated;\n");

    Ok(sql)
}

/// REST adapter config: "REST|URL|SERVICE_ROLE_KEY"
/// - URL: https://[ref].supabase.co/rest/v1
/// - SERVICE_ROLE_KEY: JWT for REST API calls (full admin access, bypasses RLS)
struct RestConfig {
    url: String,
    service_role_key: String,
    project_ref: String,
    pat: Option<String>,
}

impl RestConfig {
    fn parse(conn_string: &str) -> Result<Self, String> {
        let prefix = "REST|";
        if !conn_string.starts_with(prefix) {
            return Err("Invalid REST config format: must start with 'REST|'. Expected: REST|https://...|[service-role-key]".to_string());
        }
        let without_prefix = &conn_string[prefix.len()..];

        // Format: URL|service_role_key
        // service_role_key is a JWT (contains dots, no pipes)
        let parts: Vec<&str> = without_prefix.split('|').collect();
        if parts.len() != 2 {
            return Err(format!("Invalid REST config format: expected 2 parts after REST|, got {}. Expected: REST|https://...|[service-role-key]", parts.len()));
        }

        let url = parts[0].trim_end_matches('/').to_string();
        let service_role_key = parts[1].to_string();

        // Extract project ref from URL: https://[project-ref].supabase.co/rest/v1
        let project_ref = if let Some(start) = url.find("://") {
            let after_proto = &url[start + 3..];
            if let Some(dot_pos) = after_proto.find(".supabase.co") {
                after_proto[..dot_pos].to_string()
            } else {
                return Err("Could not extract project reference from URL".to_string());
            }
        } else {
            return Err("Invalid URL format".to_string());
        };

        if url.is_empty() {
            return Err("URL is empty".to_string());
        }
        if service_role_key.is_empty() {
            return Err("Service role key is empty".to_string());
        }

        Ok(Self {
            url,
            service_role_key,
            project_ref,
            pat: None,
        })
    }
}

struct RestPostgres {
    client: reqwest::blocking::Client,
    config: std::sync::Mutex<Option<RestConfig>>,
    is_connected: std::sync::atomic::AtomicBool,
    cached_last_err: Arc<Mutex<Option<String>>>,
}

impl RestPostgres {
    fn new() -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("failed to build HTTP client"),
            config: std::sync::Mutex::new(None),
            is_connected: std::sync::atomic::AtomicBool::new(false),
            cached_last_err: Arc::new(Mutex::new(None)),
        }
    }

    fn set_error(&self, err: &str) {
        if let Ok(mut e) = self.cached_last_err.lock() {
            *e = Some(err.to_string());
        }
    }

    fn clear_error(&self) {
        if let Ok(mut e) = self.cached_last_err.lock() {
            *e = None;
        }
    }

    /// Push rows using Supabase REST API with automatic retries
    /// Dynamically discovers remote columns and only sends those that match
    fn push_rows_impl(&self, table: &str, rows: &[serde_json::Value]) -> Result<Vec<String>, String> {
        let config = self.config.lock().map_err(|e| e.to_string())?;
        let config = config.as_ref().ok_or("REST not configured")?;
        self.clear_error();

        // Send all columns from the rows — PostgREST ignores extra columns
        // that don't exist in the remote schema (returns 400), which is
        // retried by the caller. No need to discover remote columns here.
        let filtered = rows.to_vec();

        crate::log::log(&format!("[sync] push_rows {}: {} rows, {} columns after filtering", table, filtered.len(), if filtered.is_empty() { 0 } else { filtered[0].as_object().map(|m| m.len()).unwrap_or(0) }));

        let mut all_acked = Vec::new();
        let batch_size = 100;

        for batch_start in (0..filtered.len()).step_by(batch_size) {
            let batch_end = (batch_start + batch_size).min(filtered.len());
            let batch = &filtered[batch_start..batch_end];

            let mut retries = 0;
            let max_retries = 3;

            loop {
                let result = self.do_push_batch(&config.url, table, batch, &config.service_role_key);

                match result {
                    Ok(ids) => {
                        all_acked.extend(ids);
                        break;
                    }
                    Err(e) => {
                        // Check if this is a column-not-found error (PGRST204)
                        if e.contains("PGRST204") || e.contains("Could not find the") {
                            // Extract column name from error
                            if let Some(start) = e.find("Could not find the '") {
                                let after = &e[start + 18..];
                                if let Some(end) = after.find("'") {
                                    let col_name = &after[..end];
                                    crate::log::log(&format!("[sync] REST {table}: unknown column '{}' - check local vs remote schema mismatch", col_name));
                                }
                            }
                            // Don't retry on column errors - the schema mismatch needs to be fixed manually
                            self.set_error(&e);
                            return Err(format!("{table} column mismatch: {e}"));
                        }
                        if retries < max_retries {
                            retries += 1;
                            let delay = std::time::Duration::from_millis(100 * 2_u64.pow(retries as u32));
                            std::thread::sleep(delay);
                            crate::log::log(&format!("[sync] REST {table} batch retry {}/{}: {}", retries, max_retries, e));
                        } else {
                            self.set_error(&e);
                            return Err(format!("{table} REST push failed after {} retries: {e}", max_retries));
                        }
                    }
                }
            }
        }

        Ok(all_acked)
    }

    fn do_push_batch(&self, base_url: &str, table: &str, rows: &[serde_json::Value], service_role_key: &str) -> Result<Vec<String>, String> {
        let url = format!("{}/{}", base_url, table);

        let mut request = self.client.post(&url);
        request = request
            .header("apikey", service_role_key)
            .header("Authorization", format!("Bearer {}", service_role_key))
            .header("Content-Type", "application/json")
            // Upsert behavior: on duplicate id, merge (update) instead of error
            .header("Prefer", "return=minimal,resolution=merge-duplicates");

        // Build the actual payload with all rows
        let payload = rows.to_vec();
        request = request.json(&payload);

        let response = request.send().map_err(|e| format!("HTTP request failed: {}", e))?;

        let status = response.status();
        if status.is_success() {
            // Extract IDs from the input rows — all are considered acked
            let ids: Vec<String> = rows.iter()
                .filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(String::from))
                .collect();
            Ok(ids)
        } else {
            // Read the body for the error message
            let text = response.text().unwrap_or_default();
            Err(format!("REST API error {}: {}", status, text))
        }
    }

    /// Delete rows using REST API
    fn delete_rows_impl(&self, table: &str, ids: &[String]) -> Result<(), String> {
        let config = self.config.lock().map_err(|e| e.to_string())?;
        let config = config.as_ref().ok_or("REST not configured")?;
        self.clear_error();

        crate::log::log(&format!("[sync] delete_rows {table}: attempting to delete {} rows from central", ids.len()));
        for id in ids {
            let url = format!("{}/{}/{}", config.url, table, id);
            crate::log::log(&format!("[sync] delete_rows {table}/{id}: DELETE {}", url));
            let response = self.client.delete(&url)
                .header("apikey", &config.service_role_key)
                .header("Authorization", format!("Bearer {}", config.service_role_key))
                .send()
                .map_err(|e| format!("HTTP delete failed: {}", e))?;

            crate::log::log(&format!("[sync] delete_rows {table}/{id}: status {}", response.status()));
            if !response.status().is_success() && response.status().as_u16() != 404 {
                return Err(format!("REST delete failed: {}", response.status()));
            }
        }
        Ok(())
    }

    /// Discover columns for a table using PostgREST (no PAT needed)
    /// Uses a SELECT with limit 0 and Prefer: return=minimal to get headers
    fn discover_remote_columns(&self, table: &str) -> std::collections::HashSet<String> {
        let config = match self.config.lock() {
            Ok(c) => c,
            Err(_) => return std::collections::HashSet::new(),
        };
        let config = match config.as_ref() {
            Some(c) => c,
            None => return std::collections::HashSet::new(),
        };

        // Use PostgREST with Prefer: return=minimal + limit=0 to get column info in headers
        // Actually simpler: just try to insert with one row and see what columns are accepted
        // But that modifies data. Instead, use the REST API's ability to handle extra columns
        //
        // Better approach: query the table with limit 0 to trigger schema check
        let url = format!("{}/{}?limit=0", config.url, table);

        if let Ok(resp) = self.client.get(&url)
            .header("apikey", &config.service_role_key)
            .header("Authorization", format!("Bearer {}", config.service_role_key))
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(5))
            .send()
        {
            let status = resp.status();
            if status.is_success() || status.as_u16() == 400 {
                // Parse columns from error message if available
                let body = resp.text().unwrap_or_default();
                crate::log::log(&format!("[sync] discover_remote_columns {}: status={}, body={}", table, status, &body[..body.len().min(500)]));
            }
        }

        // Fallback: return empty set. Caller must NOT filter when this is empty.
        crate::log::log(&format!("[sync] discover_remote_columns {}: could not fetch remote schema, allowing all columns", table));
        std::collections::HashSet::new()
    }

    /// Get columns for a table from Supabase via Management API
    /// Falls back to allowing all columns if schema query fails
    fn get_remote_columns(&self, table: &str) -> std::collections::HashSet<String> {
        // Try to get PAT from settings if available
        let pat = self.get_pat_from_settings().ok();

        if let Some(ref pat) = pat {
            let config = match self.config.lock() {
                Ok(c) => c,
                Err(_) => return std::collections::HashSet::new(),
            };
            let config = match config.as_ref() {
                Some(c) => c,
                None => return std::collections::HashSet::new(),
            };

            let url = format!("https://api.supabase.com/v1/projects/{}/database/query", config.project_ref);
            let sql = format!(
                "SELECT column_name FROM information_schema.columns WHERE table_name = '{}' AND table_schema = 'public'",
                table
            );

            if let Ok(resp) = self.client.post(&url)
                .header("Authorization", format!("Bearer {}", pat))
                .header("Content-Type", "application/json")
                .body(serde_json::json!({ "query": sql }).to_string())
                .timeout(std::time::Duration::from_secs(10))
                .send()
            {
                if resp.status().is_success() {
                    if let Ok(body) = resp.text() {
                        if let Ok(cols) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
                            let mut result = std::collections::HashSet::new();
                            for col in cols {
                                if let Some(name) = col.get("column_name").and_then(|v| v.as_str()) {
                                    result.insert(name.to_string());
                                }
                            }
                            if !result.is_empty() {
                                crate::log::log(&format!("[sync] Discovered {} columns for table {}", result.len(), table));
                                return result;
                            }
                        }
                    }
                }
            }
        }

        crate::log::log(&format!("[sync] Could not discover columns for table {} - allowing all", table));
        std::collections::HashSet::new()
    }

    fn get_pat_from_settings(&self) -> Result<String, String> {
        // PAT should be stored in app settings - this is set when user provides it during connect
        // For now, return empty - the schema discovery will be skipped
        Err("PAT not available in REST adapter".to_string())
    }

    /// Check connection health - hits a generic endpoint that doesn't require specific tables
    fn check_connection(&self) -> bool {
        if let Ok(config) = self.config.lock() {
            if let Some(config) = config.as_ref() {
                // Use the root REST endpoint - returns OpenAPI spec if API key is valid
                // This works even if no tables exist yet
                let url = format!("{}/", config.url);
                match self.client.get(&url)
                    .header("apikey", &config.service_role_key)
                    .header("Authorization", format!("Bearer {}", config.service_role_key))
                    .timeout(std::time::Duration::from_secs(5))
                    .send()
                {
                    Ok(resp) => {
                        let status = resp.status();
                        // 200 = OK, 404 = endpoint not found but key valid
                        // Any response means the API key is valid
                        status.is_success() || status == 404
                    },
                    Err(e) => {
                        crate::log::log(&format!("[sync] REST check_connection failed: {}", e));
                        false
                    }
                }
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Execute a SQL statement via Supabase SQL API.
    /// Used for DDL (CREATE TABLE, ALTER TABLE) and complex SELECT queries.
    fn exec_sql(&self, sql: &str) -> Result<serde_json::Value, String> {
        let config = self.config.lock().map_err(|e| e.to_string())?;
        let config = config.as_ref().ok_or("REST not configured")?;
        // SQL API is at the project root, NOT at /rest/v1/sql
        let sql_url = format!("https://{}.supabase.co/sql", config.project_ref);
        crate::log::log(&format!("[sync] exec_sql: POST {}", sql_url));
        crate::log::log(&format!("[sync] exec_sql: sql length = {}", sql.len()));

        let resp = self.client.post(&sql_url)
            .header("apikey", &config.service_role_key)
            .header("Authorization", format!("Bearer {}", config.service_role_key))
            .header("Content-Type", "application/json")
            .body(sql.to_string())
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .map_err(|e| format!("SQL API request failed: {e}"))?;

        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        let text_for_log = if text.len() > 500 { format!("{}...", &text[..500]) } else { text.clone() };
        crate::log::log(&format!("[sync] exec_sql: status = {}, body = {}", status, text_for_log));
        if !status.is_success() {
            return Err(format!("SQL API error {status}: {text}"));
        }
        serde_json::from_str(&text).map_err(|e| format!("SQL API JSON parse error: {e}: {text}"))
    }

    /// Run a SELECT query via SQL API, returning rows as JSON objects.
    fn query_rows_sql_api(&self, sql: &str) -> Result<Vec<serde_json::Value>, String> {
        let val = self.exec_sql(sql)?;
        // SQL API returns rows as an array of objects
        match val {
            serde_json::Value::Array(arr) => Ok(arr),
            serde_json::Value::Object(_) => Ok(vec![val]),
            _ => Ok(vec![]),
        }
    }

    /// Auto-create and auto-alter tables in Supabase to match local SQLite schema.
    /// Runs CREATE TABLE IF NOT EXISTS for each sync table, then adds any missing
    /// columns via ALTER TABLE ADD COLUMN IF NOT EXISTS.
    pub fn ensure_schema(&self, conn: &Connection) -> Result<(), String> {
        let config = self.config.lock().map_err(|e| e.to_string())?;
        let config = config.as_ref().ok_or("REST not configured")?;
        let _ = config; // we just need to verify it's configured

        // Build one big SQL statement with all DDL
        let mut sql = String::new();

        for &(table, _) in PG_SYNC_TABLES {
            sql.push_str(&self.generate_create_table_sql(table));
            // Add missing columns from local SQLite
            if let Ok(local_cols) = sqlite_columns(conn, table) {
                let base: std::collections::HashSet<String> = base_columns(table).into_iter().map(String::from).collect();
                for col in &local_cols {
                    if !base.contains(col.as_str()) {
                        let pg_type = pg_column_type(table, col);
                        sql.push_str(&format!(
                            "ALTER TABLE public.\"{}\" ADD COLUMN IF NOT EXISTS \"{}\" {};\n",
                            table, col, pg_type
                        ));
                    }
                }
            }
        }

        // Grant access
        sql.push_str("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO anon;\n");
        sql.push_str("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO authenticated;\n");
        sql.push_str("GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO anon;\n");
        sql.push_str("GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO authenticated;\n");

        drop(config); // release lock before network call
        crate::log::log(&format!("[sync] ensure_schema: sending DDL via SQL API ({} bytes)", sql.len()));
        match self.exec_sql(&sql) {
            Ok(_) => {
                crate::log::log("[sync] ensure_schema: DDL OK");
                Ok(())
            }
            Err(e) => {
                crate::log::log(&format!("[sync] ensure_schema: DDL partial: {e}"));
                Ok(())
            }
        }
    }

    /// Create a single table if it doesn't exist in Supabase.
    fn create_table_if_missing(&self, table: &str) -> Result<(), String> {
        let sql = self.generate_create_table_sql(table);
        match self.exec_sql(&sql) {
            Ok(_) => {
                crate::log::log(&format!("[sync] create_table_if_missing: {table} OK, ensuring notify function exists"));
                // First ensure the notify function exists
                let create_fn_sql = r#"
                    CREATE OR REPLACE FUNCTION public.notify_pgrst_cache_needs_refresh()
                    RETURNS void LANGUAGE plpgsql SECURITY DEFINER AS $$
                    BEGIN
                      NOTIFY pgrst, 'reload schema cache';
                    END;
                    $$;
                    GRANT EXECUTE ON FUNCTION public.notify_pgrst_cache_needs_refresh() TO anon, authenticated;
                "#;
                if let Ok(config) = self.config.lock() {
                    if let Some(cfg) = config.as_ref() {
                        // Create the function first
                        let fn_url = format!("https://{}.supabase.co/sql", cfg.project_ref);
                        let _ = self.client.post(&fn_url)
                            .header("apikey", &cfg.service_role_key)
                            .header("Authorization", format!("Bearer {}", cfg.service_role_key))
                            .header("Content-Type", "application/json")
                            .body(create_fn_sql.to_string())
                            .send();
                        
                        // Then call it to reload schema cache
                        let rpc_url = format!("https://{}.supabase.co/rest/v1/rpc/notify_pgrst_cache_needs_refresh", cfg.project_ref);
                        let _ = self.client.post(&rpc_url)
                            .header("apikey", &cfg.service_role_key)
                            .header("Authorization", format!("Bearer {}", cfg.service_role_key))
                            .header("Content-Type", "application/json")
                            .send();
                        crate::log::log(&format!("[sync] create_table_if_missing: {table} schema cache refresh notified"));
                    }
                }
                Ok(())
            }
            Err(e) => {
                crate::log::log(&format!("[sync] create_table_if_missing: {table} failed: {e}"));
                Ok(()) // Non-fatal: we'll retry on next push
            }
        }
    }

    /// Generate CREATE TABLE IF NOT EXISTS SQL for a single table.
    fn generate_create_table_sql(&self, table: &str) -> String {
        let defs: Vec<String> = base_columns(table)
            .iter()
            .map(|c| {
                if *c == "id" {
                    format!("\"{}\" {} PRIMARY KEY", c, pg_column_type(table, c))
                } else {
                    format!("\"{}\" {}", c, pg_column_type(table, c))
                }
            })
            .collect();
        format!(
            "CREATE TABLE IF NOT EXISTS public.\"{}\" ({});\n",
            table,
            defs.join(", ")
        )
    }

    /// Create all sync tables in Supabase if they don't exist.
    /// Uses the EXACT schema from the working SUPABASE_SETUP.sql script.
    fn create_all_tables(&self) -> Result<(), String> {
        let sql = r#"
-- Companies
CREATE TABLE IF NOT EXISTS public.companies (
    synced INTEGER DEFAULT 0,
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    extra_fields TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Drivers
CREATE TABLE IF NOT EXISTS public.drivers (
    synced INTEGER DEFAULT 0,
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    extra_fields TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Vehicles
CREATE TABLE IF NOT EXISTS public.vehicles (
    synced INTEGER DEFAULT 0,
    id TEXT PRIMARY KEY,
    plate_number TEXT NOT NULL,
    company_id TEXT REFERENCES public.companies(id),
    registered_capacity REAL,
    default_driver_id TEXT REFERENCES public.drivers(id),
    status TEXT NOT NULL DEFAULT 'active',
    extra_fields TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_vehicles_plate ON public.vehicles(plate_number);

-- Users
CREATE TABLE IF NOT EXISTS public.users (
    synced INTEGER DEFAULT 0,
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    auth_type TEXT NOT NULL,
    credential_hash TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    revoked_by TEXT REFERENCES public.users(id),
    revoked_at TEXT,
    profile_photo_ref TEXT,
    phone_number TEXT,
    theme_mode TEXT DEFAULT 'light',
    theme_accent TEXT,
    language_preference TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Trips
CREATE TABLE IF NOT EXISTS public.trips (
    id TEXT PRIMARY KEY,
    vehicle_id TEXT REFERENCES public.vehicles(id),
    driver_id TEXT REFERENCES public.drivers(id),
    company_id TEXT,
    capacity_at_trip REAL,
    time_in TEXT NOT NULL,
    receipt_no TEXT,
    officer_id TEXT REFERENCES public.users(id),
    capture_method TEXT NOT NULL DEFAULT 'auto',
    confidence_score REAL,
    photo_refs TEXT,
    status TEXT NOT NULL DEFAULT 'logged',
    resolution_notes TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    synced INTEGER DEFAULT 0,
    pushed_to_sheets INTEGER DEFAULT 0,
    sheet_row INTEGER,
    sheet_exit_pushed INTEGER DEFAULT 0,
    is_discharge_trip INTEGER,
    model_version TEXT,
    ocr_engine TEXT,
    archived INTEGER DEFAULT 0,
    exit_time TEXT,
    exit_photo_refs TEXT
);
CREATE INDEX IF NOT EXISTS idx_trips_time_in ON public.trips(time_in);
CREATE INDEX IF NOT EXISTS idx_trips_status ON public.trips(status);

-- Permissions
CREATE TABLE IF NOT EXISTS public.permissions (
    id TEXT PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    min_auth_level TEXT NOT NULL,
    description TEXT
);

-- User Permissions
CREATE TABLE IF NOT EXISTS public.user_permissions (
    user_id TEXT REFERENCES public.users(id),
    permission_id TEXT REFERENCES public.permissions(id),
    granted_by TEXT REFERENCES public.users(id),
    granted_at TEXT NOT NULL,
    PRIMARY KEY (user_id, permission_id)
);

-- Role Presets
CREATE TABLE IF NOT EXISTS public.role_presets (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    permission_ids TEXT NOT NULL
);

-- Audit Log
CREATE TABLE IF NOT EXISTS public.audit_log (
    id TEXT PRIMARY KEY,
    actor_id TEXT REFERENCES public.users(id),
    action TEXT NOT NULL,
    target_id TEXT,
    details TEXT,
    created_at TEXT NOT NULL
);

-- Integrations
CREATE TABLE IF NOT EXISTS public.integrations (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    connected_by TEXT REFERENCES public.users(id),
    target_sheet_id TEXT,
    shared_group TEXT,
    sync_frequency TEXT DEFAULT 'realtime',
    status TEXT NOT NULL DEFAULT 'active',
    last_synced_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- App Settings
CREATE TABLE IF NOT EXISTS public.app_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ANPR Config
CREATE TABLE IF NOT EXISTS public.anpr_config (
    id TEXT PRIMARY KEY,
    camera_id TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    min_confidence REAL DEFAULT 0.7,
    cooldown_seconds INTEGER DEFAULT 30,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Field Definitions
CREATE TABLE IF NOT EXISTS public.field_definitions (
    id TEXT PRIMARY KEY,
    entity_type TEXT NOT NULL,
    field_key TEXT NOT NULL,
    display_label TEXT NOT NULL,
    field_type TEXT NOT NULL,
    required INTEGER NOT NULL DEFAULT 0,
    options TEXT,
    sort_order INTEGER NOT NULL DEFAULT 0,
    is_standard INTEGER DEFAULT 0,
    is_hidden INTEGER DEFAULT 0,
    created_at TEXT NOT NULL
);

-- Grant access
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO anon;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO authenticated;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO anon;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO authenticated;
"#;
        crate::log::log(&format!("[sync] create_all_tables: executing {} bytes of SQL", sql.len()));
        match self.exec_sql(sql) {
            Ok(result) => {
                crate::log::log(&format!("[sync] create_all_tables: OK, result = {:?}", result));
                Ok(())
            }
            Err(e) => {
                crate::log::log(&format!("[sync] create_all_tables FAILED: {}", e));
                Err(e)
            }
        }
    }
}

impl PostgresAdapter for RestPostgres {
    fn label(&self) -> &str {
        "rest-postgres"
    }

    fn configured(&self) -> bool {
        self.config.lock().ok().map(|c| c.is_some()).unwrap_or(false)
    }

    fn connected(&self) -> bool {
        // For REST adapter, connected means configured - each HTTP request is independent
        // so previous failures don't permanently mark us offline
        self.config.lock().ok().map(|c| c.is_some()).unwrap_or(false)
    }

    fn last_error(&self) -> Option<String> {
        self.cached_last_err.lock().ok().and_then(|e| e.clone())
    }

    fn configure(&self, conn_string: Option<String>) -> Result<(), String> {
        match conn_string {
            Some(cs) => {
                let config = RestConfig::parse(&cs)?;
                if let Ok(mut c) = self.config.lock() {
                    *c = Some(config);
                }
                // Test connection
                if self.check_connection() {
                    self.clear_error();
                    // Auto-create tables on configure
                    crate::log::log("[sync] configure: connected, creating tables...");
                    if let Err(e) = self.create_all_tables() {
                        crate::log::log(&format!("[sync] configure: create_all_tables failed: {}", e));
                    }
                    Ok(())
                } else {
                    Err("REST connection test failed - check URL and API key".to_string())
                }
            }
            None => {
                if let Ok(mut c) = self.config.lock() {
                    *c = None;
                }
                self.clear_error();
                Ok(())
            }
        }
    }

    fn push_rows(&self, table: &str, rows: &[serde_json::Value]) -> Result<Vec<String>, String> {
        if !self.configured() {
            return Err("REST adapter not configured".to_string());
        }
        crate::log::log(&format!("[sync] push_rows {table}: pushing {} rows", rows.len()));
        match self.push_rows_impl(table, rows) {
            Ok(ids) => Ok(ids),
            Err(e) if e.contains("PGRST205") || e.contains("does not exist") || e.contains("not find the table") => {
                crate::log::log(&format!("[sync] REST push {table}: table missing ({}), auto-creating", e));
                if let Err(e) = self.create_table_if_missing(table) {
                    crate::log::log(&format!("[sync] REST push {table}: create_table_if_missing failed: {}", e));
                }
                self.push_rows_impl(table, rows)
            }
            Err(e) => {
                // Column mismatches (PGRST204) — filter out unknown columns iteratively
                if e.contains("PGRST204") || e.contains("column mismatch") || e.contains("schema cache") || e.contains("Could not find the") {
                    crate::log::log(&format!("[sync] REST push {table}: column mismatch ({e}), filtering unknown columns"));
                    
                    let mut current_rows = rows.to_vec();
                    let mut filtered_count = 0;
                    let mut max_iterations = 30;
                    let mut last_error = e.clone();
                    
                    // Loop: keep filtering unknown columns until push succeeds or we run out of columns
                    while max_iterations > 0 {
                        max_iterations -= 1;
                        
                        // Try to push with current columns
                        match self.push_rows_impl(table, &current_rows) {
                            Ok(ids) => {
                                crate::log::log(&format!("[sync] REST push {table}: success after filtering {} columns", filtered_count));
                                return Ok(ids);
                            }
                            Err(e2) if e2.contains("PGRST204") || e2.contains("Could not find the") => {
                                // Extract the unknown column name
                                if let Some(col_name) = extract_column_name_from_error(&e2) {
                                    crate::log::log(&format!("[sync] REST push {table}: filtering out column '{}'", col_name));
                                    filtered_count += 1;
                                    // Filter out the problematic column
                                    current_rows = current_rows.iter().map(|row| {
                                        let mut new_row = serde_json::Map::new();
                                        if let Some(obj) = row.as_object() {
                                            for (k, v) in obj {
                                                if k != &col_name {
                                                    new_row.insert(k.clone(), v.clone());
                                                }
                                            }
                                        }
                                        serde_json::Value::Object(new_row)
                                    }).collect();
                                    last_error = e2;
                                    continue;
                                }
                                // Could not extract column name, give up
                                crate::log::log(&format!("[sync] REST push {table}: could not extract column name from error, giving up"));
                                break;
                            }
                            Err(e2) => {
                                // Different error, propagate it
                                last_error = e2;
                                break;
                            }
                        }
                    }
                    
                    if filtered_count > 0 {
                        crate::log::log(&format!("[sync] REST push {table}: filtered {} columns but push still failed: {}", filtered_count, last_error));
                    } else {
                        crate::log::log(&format!("[sync] REST push {table}: column mismatch could not be fixed: {}", last_error));
                    }
                }
                Err(e)
            }
        }
    }

    fn delete_rows(&self, table: &str, ids: &[String]) -> Result<(), String> {
        if !self.configured() {
            return Err("REST adapter not configured".to_string());
        }
        self.delete_rows_impl(table, ids)
    }

    fn query_rows(&self, sql: &str, _params: &[String]) -> Result<Vec<serde_json::Value>, String> {
        if !self.configured() {
            return Err("REST adapter not configured".to_string());
        }
        self.query_rows_sql_api(sql)
    }

    /// Add a missing column to a table in Supabase.
    fn add_missing_column(&self, table: &str, column_name: &str) -> Result<(), String> {
        let pg_type = pg_column_type(table, column_name);
        let sql = format!(
            "ALTER TABLE public.\"{}\" ADD COLUMN IF NOT EXISTS \"{}\" {}",
            table, column_name, pg_type
        );
        crate::log::log(&format!("[sync] add_missing_column: {}", sql));
        self.exec_sql(&sql).map(|_| ())
    }
}

/// Extract column name from PostgREST error message like:
/// "Could not find the 'column_name' column of 'table' in the schema cache"
/// or from JSON: {"message":"Could not find the 'col' column of..."}
fn extract_column_name_from_error(error: &str) -> Option<String> {
    // Pattern 1: Look for "the '" and capture until next "'"
    // This handles both "Could not find the 'X'" and other formats
    if let Some(the_pos) = error.find("the '") {
        let after_the = &error[the_pos + 5..]; // Skip "the '"
        if let Some(quote_end) = after_the.find('\'') {
            let col = after_the[..quote_end].to_string();
            // Validate: must be non-empty and contain only valid identifier chars
            if !col.is_empty() && col.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                crate::log::log(&format!("[sync] extract_column: found '{}'", col));
                return Some(col);
            }
        }
    }
    
    // Pattern 2: Try direct search for "Could not find the '"
    if let Some(start) = error.find("Could not find the '") {
        let after = &error[start + 18..]; // Skip "Could not find the '"
        if let Some(end) = after.find('\'') {
            let col = after[..end].to_string();
            if !col.is_empty() && col.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                crate::log::log(&format!("[sync] extract_column (direct): found '{}'", col));
                return Some(col);
            }
        }
    }
    
    // Pattern 3: Look for column_name in quotes anywhere in the string
    // Match pattern: 'word' where word looks like a column name
    let mut last_valid_col = None;
    let mut search_start = 0;
    while let Some(quote_pos) = error[search_start..].find('\'') {
        let actual_pos = search_start + quote_pos;
        let after_quote = &error[actual_pos + 1..];
        if let Some(end_quote) = after_quote.find('\'') {
            let candidate = after_quote[..end_quote].to_string();
            // Check if it looks like a column name (not too long, valid chars)
            if candidate.len() < 64 && candidate.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                // Prefer columns that appear after "the" or "column"
                if candidate.len() > 0 && !candidate.starts_with("PGRST") && candidate != "null" {
                    last_valid_col = Some(candidate);
                    if error[..actual_pos].trim().ends_with("the") || error[..actual_pos].trim().ends_with("column") {
                        crate::log::log(&format!("[sync] extract_column (search): found '{}'", last_valid_col.as_ref().unwrap()));
                        return last_valid_col;
                    }
                }
            }
        }
        search_start = actual_pos + 1;
    }
    
    if let Some(col) = last_valid_col {
        crate::log::log(&format!("[sync] extract_column (best-effort): found '{}'", col));
        return Some(col);
    }
    
    crate::log::log(&format!("[sync] extract_column: could not find column name in error"));
    None
}

/// Build the app's Postgres adapter, restoring any saved connection string
/// without blocking startup on the network (first use connects lazily).
/// Automatically detects REST| prefix to use REST adapter instead of PgBouncer.
pub fn real_postgres(conn: &Connection) -> Arc<dyn PostgresAdapter> {
    let cs = get_setting(conn, "pg_connection_string");

    if let Some(ref cs) = cs {
        if cs.starts_with("REST|") {
            // Use REST adapter for Supabase REST API
            let pg = RestPostgres::new();
            // Store the config but do NOT connect synchronously — avoid blocking startup.
            // Connection will be tested lazily on first push/query.
            if let Ok(config) = RestConfig::parse(cs) {
                if let Ok(mut c) = pg.config.lock() {
                    *c = Some(config);
                }
                crate::log::log("[sync] REST adapter configured (lazy connect on first use)");
            }
            return Arc::new(pg);
        }
    }

    // Default: use PgBouncer adapter
    let pg = RealPostgres::new();
    if let Some(cs) = cs {
        pg.restore(cs);
    }
    Arc::new(pg)
}

/// Build the app's Sheets provider, restoring saved credentials and validating
/// them (failures surface in the Sync panel via `last_error`).
pub fn real_sheets(conn: &Connection) -> Arc<dyn SheetsProvider> {
    let sheets = RealSheets::new();
    // Load saved column mapping (or keep defaults if not set yet).
    sheets.set_sheet_mapping(&read_sheet_mapping(conn));
    if let (Some(j), Some(sid)) = (
        get_setting(conn, "sheets_service_account_json"),
        get_setting(conn, "sheets_target_sheet_id"),
    ) {
        // Restore WITHOUT network — validation happens lazily on first use so
        // app launch never blocks on Google (startup lag fix).
        let _ = sheets.restore(j, sid);
    }
    Arc::new(sheets)
}

/// Mask credentials in audit detail (never store the password part).
fn sanitize_conn_string(cs: &str) -> String {
    let mut out = cs.to_string();
    if let Some(at) = cs.rfind('@') {
        if let Some(sep) = cs.find("://") {
            let start = sep + 3;
            let authority = &cs[start..at];
            if !authority.contains('[') {
                if let Some(colon) = authority.find(':') {
                    out.replace_range(start + colon + 1..at, "***");
                }
            }
        }
    }
    out
}

#[tauri::command]
pub fn configure_postgres<R: tauri::Runtime>(state: State<AppState>, actor_id: String, connection_string: String, handle: AppHandle<R>) -> Result<String, String> {
    if connection_string.trim().is_empty() {
        return Err("Connection string cannot be empty.".to_string());
    }
    // Permission check (fast)
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;

        let is_rest = connection_string.starts_with("REST|");
        let current_is_rest = state.pg.label() == "rest-postgres";
        let needs_adapter_switch = is_rest != current_is_rest;

        // Save the connection string first (so next restart uses correct adapter)
        set_setting(&conn, "pg_connection_string", &connection_string);
        drop(conn);

        // Create new adapter if type changed
        let pg: Arc<dyn PostgresAdapter> = if needs_adapter_switch {
            if is_rest {
                Arc::new(RestPostgres::new()) as Arc<dyn PostgresAdapter>
            } else {
                Arc::new(RealPostgres::new()) as Arc<dyn PostgresAdapter>
            }
        } else {
            state.pg.clone()
        };

        let db = state.db.clone();
        let pg_for_thread = pg.clone();
        std::thread::spawn(move || {
            // Configure the adapter (REST connection test happens inside)
            match pg_for_thread.configure(Some(connection_string.clone())) {
                Ok(()) => {
                    let emit_payload = if let Ok(conn) = db.lock() {
                        let _ = append_audit(&conn, &actor_id, "configured_postgres", None, Some(json!({ "connection_string": sanitize_conn_string(&connection_string) })));
                        let view = pg_sync_state_impl(&conn, &*pg_for_thread).ok();
                        drop(conn);
                        view
                    } else {
                        None
                    };
                    if let Some(view) = emit_payload {
                        let _ = handle.emit("pg-configured", view);
                    }
                }
                Err(e) => {
                    let _ = handle.emit("pg-config-error", json!({ "error": e }));
                }
            }
        });
        Ok("testing".to_string())
    }
}

/// Setup tables via the Supabase pooler connection (using the database password).
/// DEPRECATED: Tables must be pre-created by running SUPABASE_SETUP.sql in the
/// Supabase SQL Editor once. This function is kept for reference but not called
/// from the normal flow.
#[allow(dead_code)]
fn setup_tables_via_pooler(conn_string: &str, schema_sql: &str) -> Result<(), String> {
    // Parsing 3-part format — no database password. Return error.
    let _ = RestConfig::parse(conn_string)?;
    Err("Auto table creation is disabled. Run SUPABASE_SETUP.sql in Supabase SQL Editor first.".to_string())
}

#[tauri::command]
pub fn disconnect_postgres<R: tauri::Runtime>(state: State<AppState>, actor_id: String, handle: AppHandle<R>) -> Result<String, String> {
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }
    let pg = state.pg.clone();
    let db = state.db.clone();
    std::thread::spawn(move || {
        let _ = pg.configure(None);
        // Release db BEFORE emit to avoid deadlock.
        let emit_payload = if let Ok(conn) = db.lock() {
            let _ = conn.execute("DELETE FROM app_settings WHERE key = 'pg_connection_string'", []);
            let _ = append_audit(&conn, &actor_id, "disconnected_postgres", None, None);
            let view = pg_sync_state_impl(&conn, &*pg).ok();
            drop(conn);
            view
        } else {
            None
        };
        if let Some(view) = emit_payload {
            let _ = handle.emit("pg-disconnected", view);
        }
    });
    Ok("disconnecting".to_string())
}

/// Create tables in Supabase using the Personal Access Token via Management API.
#[tauri::command]
pub fn create_postgres_tables<R: tauri::Runtime>(
    state: State<AppState>,
    actor_id: String,
    pat: String,
    handle: AppHandle<R>,
) -> Result<String, String> {
    if pat.trim().is_empty() {
        return Err("Personal Access Token is required to create tables.".to_string());
    }
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }
    // Get the project ref from the saved connection string
    let project_ref = {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        get_setting(&conn, "pg_connection_string")
            .and_then(|cs| {
                if cs.starts_with("REST|") {
                    let parts: Vec<&str> = cs[5..].split('|').collect();
                    if !parts.is_empty() {
                        // Extract ref from URL like https://xxx.supabase.co/rest/v1
                        let url = parts[0];
                        if let Some(start) = url.find("://") {
                            let after_proto = &url[start + 3..];
                            if let Some(dot_pos) = after_proto.find(".supabase.co") {
                                return Some(after_proto[..dot_pos].to_string());
                            }
                        }
                    }
                }
                None
            })
            .ok_or("No REST connection configured")?
    };

    let db = state.db.clone();
    let service_role_key = {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        get_setting(&conn, "pg_connection_string")
            .and_then(|cs| {
                if cs.starts_with("REST|") {
                    let parts: Vec<&str> = cs[5..].split('|').collect();
                    if parts.len() >= 3 {
                        return Some(parts[2].to_string());
                    }
                }
                None
            })
    };
    std::thread::spawn(move || {
        if let Ok(conn) = db.lock() {
            let _ = set_setting(&conn, "supabase_pat", &pat);
        }

        let sql = include_str!("../../docs/SUPABASE_SETUP.sql");
        let url = format!("https://api.supabase.com/v1/projects/{}/database/query", project_ref);

        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                crate::log::log(&format!("[sync] create_postgres_tables: HTTP client failed: {}", e));
                let _ = handle.emit("pg-tables-error", json!({ "error": format!("HTTP client failed: {}", e) }));
                return;
            }
        };

        crate::log::log(&format!("[sync] create_postgres_tables: POST {}", url));

        let resp = match client.post(&url)
            .header("Authorization", format!("Bearer {}", pat))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "query": sql }).to_string())
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                crate::log::log(&format!("[sync] create_postgres_tables: Management API request failed: {}", e));
                let _ = handle.emit("pg-tables-error", json!({ "error": format!("Management API request failed: {}", e) }));
                return;
            }
        };

        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        crate::log::log(&format!("[sync] create_postgres_tables: status = {}, body = {}", status, if text.len() > 300 { format!("{}...", &text[..300]) } else { text.clone() }));

        if status.is_success() {
            crate::log::log("[sync] create_postgres_tables: tables created, ensuring notify function exists...");

            // First ensure the notify function exists in public schema
            let create_fn_sql = r#"
                CREATE OR REPLACE FUNCTION public.notify_pgrst_cache_needs_refresh()
                RETURNS void LANGUAGE plpgsql SECURITY DEFINER AS $$
                BEGIN
                  NOTIFY pgrst, 'reload schema cache';
                END;
                $$;
                GRANT EXECUTE ON FUNCTION public.notify_pgrst_cache_needs_refresh() TO anon, authenticated;
            "#;
            let fn_resp = client.post(&url)
                .header("Authorization", format!("Bearer {}", pat))
                .header("Content-Type", "application/json")
                .body(serde_json::json!({ "query": create_fn_sql }).to_string())
                .send();
            crate::log::log(&format!("[sync] create_postgres_tables: notify fn creation: {:?}", fn_resp.map(|r| r.status())));

            // Then call it to reload schema cache
            if let Some(ref svc_key) = service_role_key {
                let rpc_url = format!("https://{}.supabase.co/rest/v1/rpc/notify_pgrst_cache_needs_refresh", project_ref);
                let _ = client.post(&rpc_url)
                    .header("Authorization", format!("Bearer {}", svc_key))
                    .header("apikey", svc_key)
                    .header("Content-Type", "application/json")
                    .send();
                crate::log::log("[sync] create_postgres_tables: schema cache refresh notified");
            }

            crate::log::log("[sync] create_postgres_tables: SUCCESS");
            let _ = handle.emit("pg-tables-created", ());
        } else {
            let _ = handle.emit("pg-tables-error", json!({ "error": format!("Failed to create tables: {}", text) }));
            crate::log::log(&format!("[sync] create_postgres_tables: FAILED"));
        }
    });
    Ok("creating".to_string())
}

#[tauri::command]
pub fn configure_google_sheets<R: tauri::Runtime>(
    state: State<AppState>,
    actor_id: String,
    service_account_json: String,
    target_sheet_id: String,
    shared_group: Option<String>,
    sync_frequency: String,
    handle: AppHandle<R>,
) -> Result<String, String> {
    if sync_frequency != "realtime" && sync_frequency != "every_15_min" {
        return Err("Sync frequency must be realtime or every_15_min.".to_string());
    }
    if service_account_json.trim().is_empty() || target_sheet_id.trim().is_empty() {
        return Err("Service account JSON and target sheet ID are required.".to_string());
    }
    // Phase 1: Permission check (fast)
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }
    // Phase 2: Network call on background thread — frontend receives "testing" instantly
    let sheets = state.sheets.clone();
    let db = state.db.clone();
    std::thread::spawn(move || {
        match sheets.configure(Some(service_account_json.clone()), Some(target_sheet_id.clone())) {
            Ok(email) => {
                // Persist + audit (fast) — release db BEFORE emit to avoid deadlock
                let emit_payload = if let Ok(conn) = db.lock() {
                    let now = now_iso();
                    let id = conn
                        .query_row(
                            "SELECT id FROM integrations WHERE type = 'google_sheets' ORDER BY created_at DESC LIMIT 1",
                            [],
                            |r| r.get::<_, String>(0),
                        )
                        .ok();
                    match id {
                        Some(id) => {
                            let _ = conn.execute(
                                "UPDATE integrations SET connected_by = ?1, target_sheet_id = ?2, shared_group = ?3,
                                        sync_frequency = ?4, status = 'connected', last_synced_at = NULL, updated_at = ?5
                                 WHERE id = ?6",
                                params![actor_id, target_sheet_id, shared_group, sync_frequency, now, id],
                            );
                        }
                        None => {
                            let _ = conn.execute(
                                "INSERT INTO integrations (id, type, connected_by, target_sheet_id, shared_group,
                                        sync_frequency, status, created_at, updated_at)
                                 VALUES (?1, 'google_sheets', ?2, ?3, ?4, ?5, 'connected', ?6, ?6)",
                                params![
                                    uuid::Uuid::new_v4().to_string(),
                                    actor_id,
                                    target_sheet_id,
                                    shared_group,
                                    sync_frequency,
                                    now
                                ],
                            );
                        }
                    }
                    let _ = set_setting(&conn, "sheets_service_account_json", &service_account_json);
                    let _ = set_setting(&conn, "sheets_target_sheet_id", &target_sheet_id);
                    let _ = append_audit(
                        &conn,
                        &actor_id,
                        "configured_google_sheets",
                        None,
                        Some(json!({
                            "service_account_email": email,
                            "target_sheet_id": target_sheet_id,
                            "shared_group": shared_group,
                            "sync_frequency": sync_frequency,
                        })),
                    );
                    let view = sheets_state_impl(&conn, &*sheets).ok();
                    drop(conn);
                    view
                } else {
                    None
                };
                if let Some(view) = emit_payload {
                    let _ = handle.emit("sheets-configured", view);
                }
            }
            Err(e) => {
                let _ = handle.emit("sheets-config-error", json!({ "error": e }));
            }
        }
    });
    Ok("testing".to_string())
}

// ---------------------------------------------------------------------------
// Background sync (06-data-flow.md §5: retry loop, no manual send required)
// ---------------------------------------------------------------------------

/// Sheet pruning reads + possibly rewrites the spreadsheet; cap it at once a
/// minute so the 10s loop never pounds the Sheets API.
static LAST_SHEET_PRUNE: AtomicI64 = AtomicI64::new(0);

/// Trip retention (local + Postgres) runs at most once a minute as well.
static LAST_TRIP_RETENTION: AtomicI64 = AtomicI64::new(0);

/// Delete daily trip entries older than the admin-set window from local
/// storage and the Postgres archive. Only confirmed (synced) logged trips age
/// out — unsynced rows are never lost, and the reference registry is never
/// touched. The sheet has its own, separate retention.
pub fn run_trip_retention(conn: &Connection, pg: &dyn PostgresAdapter) -> Result<(), String> {
    let Some(days) = get_setting(conn, "trip_retention_days").and_then(|s| s.parse::<i64>().ok()) else {
        return Ok(());
    };
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(days))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM trips WHERE status = 'logged' AND synced = 1 AND time_in < ?1")
        .map_err(|e| format!("retention scan failed: {e}"))?
        .query_map(params![cutoff], |r| r.get::<_, String>(0))
        .map_err(|e| format!("retention scan failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("retention scan failed: {e}"))?;
    if ids.is_empty() {
        return Ok(());
    }
    let n = conn
        .execute(
            "DELETE FROM trips WHERE status = 'logged' AND synced = 1 AND time_in < ?1",
            params![cutoff],
        )
        .map_err(|e| format!("retention delete failed: {e}"))?;
    let _ = pg.delete_rows("trips", &ids);
    let _ = append_audit(
        conn,
        "system",
        "retention_deleted_trips",
        None,
        Some(json!({ "cutoff": cutoff, "removed": n })),
    );
    Ok(())
}

pub fn sheets_due(conn: &Connection, sheets: &dyn SheetsProvider) -> bool {
    let Ok(st) = sheets_state_impl(conn, sheets) else {
        return false;
    };
    if !st.connected {
        return false;
    }
    match st.frequency.as_str() {
        "realtime" => true,
        _ => match &st.last_synced_at {
            None => true,
            Some(iso) => match chrono::DateTime::parse_from_rfc3339(iso) {
                Ok(t) => {
                    let elapsed = chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc)).num_seconds();
                    elapsed >= 15 * 60
                }
                Err(_) => true,
            },
        },
    }
}

/// One background pass over both targets. Every step is best-effort and
/// independently failable; health events are deduped by record_health_event so
/// the 10s loop never spams System Monitor.
pub fn run_background_sync(conn: &Connection, pg: &dyn PostgresAdapter, sheets: &dyn SheetsProvider) {
    if pg.configured() {
        // Push local changes to central
        if let Err(e) = run_pg_sync_impl(conn, pg) {
            let _ = crate::monitor::record_health_event(conn, "sync", "degraded", Some(&format!("PostgreSQL sync failed: {e}")));
        }
        // Pull reference data from central (companies, vehicles, drivers)
        // Uses last-edit-wins-by-timestamp for conflict resolution
        if let Err(e) = pull_reference_data(conn, pg) {
            let _ = crate::monitor::record_health_event(conn, "sync", "degraded", Some(&format!("PostgreSQL pull failed: {e}")));
        }
    }
    if sheets.configured() && sheets_due(conn, sheets) {
        if let Err(e) = run_sheets_sync_impl(conn, sheets) {
            let _ = crate::monitor::record_health_event(conn, "sync", "degraded", Some(&format!("Sheets sync failed: {e}")));
        }
    }
    // Sheet pruning — retention window (admin-set days) + trips that were
    // soft/hard deleted from the app but already exported. Pruning only ever
    // touches the sheet; Postgres and local data are unaffected.
    if sheets.configured() && sheets.connected() {
        let now_ts = chrono::Utc::now().timestamp();
        if now_ts - LAST_SHEET_PRUNE.load(Ordering::Relaxed) >= 60 {
            LAST_SHEET_PRUNE.store(now_ts, Ordering::Relaxed);
            let retention = get_setting(conn, "sheets_retention_days").and_then(|s| s.parse::<i64>().ok());
            let cutoff = retention.map(|days| {
                (chrono::Utc::now() - chrono::Duration::days(days))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            });
            let excluded: Vec<String> = conn
                .prepare("SELECT id FROM trips WHERE archived = 1 AND pushed_to_sheets = 1")
                .map(|mut stmt| {
                    stmt.query_map([], |r| r.get::<_, String>(0))
                        .map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            if cutoff.is_some() || !excluded.is_empty() {
                if let Err(e) = sheets.prune(cutoff.as_deref(), &excluded) {
                    let _ = crate::monitor::record_health_event(
                        conn,
                        "sync",
                        "degraded",
                        Some(&format!("Sheet pruning failed: {e}")),
                    );
                }
            }
        }
    }
    // Daily-entry retention (local + Postgres), at most once a minute.
    let now_ts2 = chrono::Utc::now().timestamp();
    if now_ts2 - LAST_TRIP_RETENTION.load(Ordering::Relaxed) >= 60 {
        LAST_TRIP_RETENTION.store(now_ts2, Ordering::Relaxed);
        if let Err(e) = run_trip_retention(conn, pg) {
            let _ = crate::monitor::record_health_event(
                conn,
                "sync",
                "degraded",
                Some(&format!("Trip retention failed: {e}")),
            );
        }
    }
    let pg_pending: i64 = pg_sync_state_impl(conn, pg)
        .map(|s| s.tables.iter().map(|t| t.pending).sum())
        .unwrap_or(0);
    let sheets_pending = sheets_state_impl(conn, sheets).map(|s| s.pending).unwrap_or(0);
    let total = pg_pending + sheets_pending;
    if total == 0 {
        let _ = crate::monitor::record_health_event(conn, "sync", "ok", None);
    } else {
        let status = if pg.connected() || sheets.connected() { "degraded" } else { "offline" };
        let detail = format!("{total} record(s) awaiting sync");
        let _ = crate::monitor::record_health_event(conn, "sync", status, Some(&detail));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_postgres_unconfigured_is_offline() {
        let pg = RealPostgres::new();
        assert_eq!(pg.label(), "postgres");
        assert!(!pg.configured());
        assert!(!pg.connected());
        assert!(pg.last_error().is_some(), "unconfigured should explain itself");
        // Disconnecting a configured adapter is a no-op success path.
        assert!(pg.configure(None).is_ok());
    }

    #[test]
    fn real_postgres_rejects_garbage_connection_string() {
        let pg = RealPostgres::new();
        let err = pg.configure(Some("definitely not a connection string".to_string()));
        assert!(err.is_err());
        assert!(!pg.configured());
    }

    #[test]
    fn sanitize_conn_string_masks_passwords() {
        assert_eq!(
            sanitize_conn_string("postgresql://u:pw@h:5432/db"),
            "postgresql://u:***@h:5432/db"
        );
        assert_eq!(
            sanitize_conn_string("postgresql://u@h:5432/db"),
            "postgresql://u@h:5432/db"
        );
    }

    #[test]
    fn sheets_token_rejects_garbage_key_without_panicking() {
        // Regression: jsonwebtoken 11 requires an explicit crypto provider
        // feature; without it any JWT operation panics and takes the app down.
        // A garbage key must surface as a clean error instead.
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let err = fetch_token(
            &client,
            r#"{ "client_email": "a@b.iam.gserviceaccount.com", "private_key": "not a key" }"#,
        );
        assert!(err.is_err(), "expected a clean error, got: {err:?}");
    }

    #[test]
    fn field_key_to_value_maps_row_to_columns() {
        let row = json!({
            "id": "x",
            "plate": "KDG 123A",
            "entry_time": "2026-01-01T10:00:00Z",
            "capacity_at_trip": 40.0,
            "capacity_unit": "t",
            "receipt_no": null,
            "confidence_score": 0.97,
            "capture_method": "auto",
            "is_discharge_trip": true,
            "created_at": "2026-01-01T10:00:00Z",
            "company": "Acme",
            "driver": "Jane",
        });
        assert_eq!(field_key_to_value(&row, "plate"), json!("KDG 123A"));
        assert_eq!(field_key_to_value(&row, "capacity_at_trip"), json!("40"));
        assert_eq!(field_key_to_value(&row, "confidence_score"), json!("0.97"));
        assert_eq!(field_key_to_value(&row, "receipt_no"), json!(""));
        assert_eq!(field_key_to_value(&row, "is_discharge_trip"), json!("Yes"));
        assert_eq!(field_key_to_value(&row, "driver"), json!("Jane"));
    }
}
