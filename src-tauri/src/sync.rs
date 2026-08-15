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
use std::sync::{Arc, Mutex};

use postgres::types::ToSql;
use rusqlite::{Connection, params};
use rusqlite::types::ValueRef;
use serde_json::json;
use tauri::State;

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
        self.pushed.lock().unwrap().clone()
    }
    /// Rows deleted centrally: (table, id).
    pub fn deleted(&self) -> Vec<(String, String)> {
        self.deleted.lock().unwrap().clone()
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
        let mut held = self.pushed.lock().unwrap();
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
        let mut held = self.deleted.lock().unwrap();
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
    fn push_trips(&self, rows: &[serde_json::Value]) -> Result<Vec<String>, String>;
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
        self.pushed.lock().unwrap().clone()
    }
}

impl SheetsProvider for MockSheets {
    fn label(&self) -> &str {
        "mock-sheets"
    }
    fn connected(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
    fn push_trips(&self, rows: &[serde_json::Value]) -> Result<Vec<String>, String> {
        if !self.connected() {
            return Err("Google Sheets unreachable (simulated revoked/offline)".to_string());
        }
        let mut acked = Vec::with_capacity(rows.len());
        let mut held = self.pushed.lock().unwrap();
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
        self.email.lock().unwrap().clone()
    }
    fn configure(&self, json: Option<String>, sheet_id: Option<String>) -> Result<String, String> {
        let (Some(j), Some(_)) = (json, sheet_id) else {
            self.configured_flag.store(false, Ordering::SeqCst);
            *self.email.lock().unwrap() = None;
            return Ok("disconnected".to_string());
        };
        self.configured_flag.store(true, Ordering::SeqCst);
        let email = serde_json::from_str::<serde_json::Value>(&j)
            .ok()
            .and_then(|v| v["client_email"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "mock-sheets@local".to_string());
        *self.email.lock().unwrap() = Some(email.clone());
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
const PG_SYNC_TABLES: &[(&str, &str)] = &[
    ("companies", "Companies"),
    ("drivers", "Drivers"),
    ("vehicles", "Vehicles"),
    ("users", "Users"),
    ("trips", "Trips"),
];

fn rows_where_not_synced(conn: &Connection, table: &str) -> Result<Vec<serde_json::Value>, String> {
    let sql = format!("SELECT * FROM {table} WHERE synced = 0 ORDER BY created_at ASC");
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
        if pending > 0 && pg.connected() {
            let rows = rows_where_not_synced(conn, name)?;
            if !rows.is_empty() {
                let ids = pg.push_rows(name, &rows).map_err(|e| format!("{name} push failed: {e}"))?;
                for id in ids {
                    conn.execute(&format!("UPDATE {name} SET synced = 1 WHERE id = ?1"), params![id])
                        .map_err(|e| format!("{name} flag flip failed: {e}"))?;
                }
                acked = rows.len() as i64;
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

fn pending_for_table(conn: &Connection, table: &str) -> Result<i64, String> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE synced = 0"), [], |r| r.get(0))
        .map_err(|e| format!("{table} count failed: {e}"))
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

fn pending_sheets_trips(conn: &Connection) -> Result<i64, String> {
    conn.query_row(
        "SELECT COUNT(*) FROM trips WHERE status = 'logged' AND pushed_to_sheets = 0",
        [],
        |r| r.get(0),
    )
    .map_err(|e| format!("sheets pending count failed: {e}"))
}

fn sheet_trip_rows(conn: &Connection) -> Result<Vec<serde_json::Value>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT t.id, COALESCE(v.plate_number, json_extract(t.resolution_notes, '$.plate'), '') AS plate,
                    t.time_in, t.capacity_at_trip, t.capacity_unit, t.receipt_no, t.confidence_score,
                    t.capture_method, t.is_discharge_trip, t.created_at,
                    COALESCE(c.name, '') AS company, COALESCE(d.name, '') AS driver
             FROM trips t
             LEFT JOIN vehicles v ON v.id = t.vehicle_id
             LEFT JOIN companies c ON c.id = t.company_id
             LEFT JOIN drivers d ON d.id = t.driver_id
             WHERE t.status = 'logged' AND t.pushed_to_sheets = 0
             ORDER BY t.created_at ASC",
        )
        .map_err(|e| format!("sheet rows failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "plate": r.get::<_, String>(1)?,
                "time_in": r.get::<_, String>(2)?,
                "capacity_at_trip": r.get::<_, Option<f64>>(3)?,
                "capacity_unit": r.get::<_, String>(4)?,
                "receipt_no": r.get::<_, Option<String>>(5)?,
                "confidence_score": r.get::<_, Option<f64>>(6)?,
                "capture_method": r.get::<_, String>(7)?,
                "is_discharge_trip": r.get::<_, Option<bool>>(8)?,
                "created_at": r.get::<_, String>(9)?,
                "company": r.get::<_, String>(10)?,
                "driver": r.get::<_, String>(11)?,
            }))
        })
        .map_err(|e| format!("sheet rows failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("sheet rows failed: {e}"))
}

/// Push all logged-but-unsynced trips to the sheet and flip `pushed_to_sheets`
/// only for confirmed rows. Completely independent of the Postgres pipeline.
pub fn run_sheets_sync_impl(conn: &Connection, sheets: &dyn SheetsProvider) -> Result<SyncRunResult, String> {
    let pending = pending_sheets_trips(conn)?;
    let mut pushed = 0i64;
    if pending > 0 && sheets.connected() {
        let rows = sheet_trip_rows(conn)?;
        if !rows.is_empty() {
            let ids = sheets.push_trips(&rows).map_err(|e| format!("sheets push failed: {e}"))?;
            for id in ids {
                conn.execute("UPDATE trips SET pushed_to_sheets = 1 WHERE id = ?1", params![id])
                    .map_err(|e| format!("sheets flag flip failed: {e}"))?;
            }
            pushed = rows.len() as i64;
            conn.execute(
                "UPDATE integrations SET last_synced_at = ?1, updated_at = ?1 WHERE type = 'google_sheets'",
                params![now_iso()],
            )
            .map_err(|e| format!("sheets last-synced update failed: {e}"))?;
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
        online: pg.connected,
        pg,
        sheets,
    })
}

/// Manual Postgres sync trigger (automatic sync already runs in the background;
/// this exists for diagnostics and the admin status panel). Gated like other
/// integration controls.
#[tauri::command]
pub fn sync_now_pg(state: State<AppState>, actor_id: String) -> Result<SyncRunResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    let result = run_pg_sync_impl(&conn, &*state.pg)?;
    append_audit(&conn, &actor_id, "manual_postgres_sync", None, Some(json!({ "pushed": result.pushed })))?;
    // `result.tables` records the pre-run pending counts; recompute post-run so
    // the health signal reflects what still waits for connectivity.
    let pending_total: i64 = pg_sync_state_impl(&conn, &*state.pg)?.tables.iter().map(|t| t.pending).sum();
    if pending_total > 0 {
        let detail = format!("{pending_total} record(s) still awaiting central sync");
        if state.pg.connected() {
            let _ = crate::monitor::record_health_event(&conn, "sync", "degraded", Some(&detail));
        } else {
            let _ = crate::monitor::record_health_event(&conn, "sync", "offline", Some(&detail));
        }
    } else {
        let _ = crate::monitor::record_health_event(&conn, "sync", "ok", None);
    }
    Ok(result)
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
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    if !state.sheets.configured() {
        return Err("Google Sheets is not configured.".to_string());
    }
    let removed = state.sheets.prune(Some(&now_iso()), &[])?;
    append_audit(&conn, &actor_id, "cleared_sheet_exports", None, Some(json!({ "rows_removed": removed })))?;
    sheets_state_impl(&conn, &*state.sheets)
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
pub fn sync_now_sheets(state: State<AppState>, actor_id: String) -> Result<SyncRunResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    let result = run_sheets_sync_impl(&conn, &*state.sheets)?;
    append_audit(&conn, &actor_id, "manual_sheets_sync", None, Some(json!({ "pushed": result.pushed })))?;
    let st = sheets_state_impl(&conn, &*state.sheets)?;
    if st.pending > 0 {
        let detail = format!("{} logged trip(s) awaiting sheet export", st.pending);
        if st.connected {
            let _ = crate::monitor::record_health_event(&conn, "sync", "degraded", Some(&detail));
        } else {
            let _ = crate::monitor::record_health_event(&conn, "sync", "offline", Some(&detail));
        }
    } else {
        let _ = crate::monitor::record_health_event(&conn, "sync", "ok", None);
    }
    Ok(result)
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
            "pushed_to_sheets" | "synced" => "INTEGER",
            _ => "TEXT",
        },
        _ => "TEXT",
    }
}

fn pg_quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
            "id", "vehicle_id", "driver_id", "company_id", "capacity_at_trip", "time_in",
            "receipt_no", "officer_id", "capture_method", "confidence_score", "photo_refs",
            "status", "resolution_notes", "pushed_to_sheets", "created_at", "updated_at", "synced",
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
fn connect_with_create(cs: &str) -> Result<postgres::Client, String> {
    let mut config: postgres::Config = cs.parse().map_err(|e| format!("invalid connection string: {e}"))?;
    config.connect_timeout(std::time::Duration::from_secs(6));
    match config.connect(postgres::NoTls) {
        Ok(c) => Ok(c),
        Err(e) => {
            let is_missing_db = e.as_db_error().map(|d| d.message().contains("does not exist")).unwrap_or(false);
            if !is_missing_db {
                return Err(format!("cannot connect to PostgreSQL: {e}"));
            }
            let dbname = config
                .get_dbname()
                .ok_or("connection string must include a database name")?
                .to_string();
            config.dbname("postgres");
            let mut admin = config
                .connect(postgres::NoTls)
                .map_err(|ae| format!("cannot connect to the 'postgres' maintenance database: {ae}"))?;
            admin
                .batch_execute(&format!("CREATE DATABASE {}", pg_quote_ident(&dbname)))
                .map_err(|ce| format!("cannot create database '{dbname}': {ce}"))?;
            drop(admin);
            config.dbname(&dbname);
            config
                .connect(postgres::NoTls)
                .map_err(|e2| format!("cannot connect after creating database: {e2}"))
        }
    }
}

/// Mirror the source tables centrally (CREATE TABLE IF NOT EXISTS) plus any
/// extra columns a row may carry that aren't in the base schema yet. Every
/// table gets a PRIMARY KEY on `id` (and an idempotent unique index, so a
/// table that already exists without the constraint is still upsert-safe).
fn ensure_schema_for(client: &mut postgres::Client) -> Result<(), String> {
    for (table, _display) in PG_SYNC_TABLES {
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
        client
            .batch_execute(&format!(
                "CREATE TABLE IF NOT EXISTS {} ({})",
                pg_quote_ident(table),
                defs.join(", ")
            ))
            .map_err(|e| format!("central schema for {table} failed: {e}"))?;
        client
            .batch_execute(&format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {} ON {}(\"id\")",
                pg_quote_ident(&format!("{table}_id_key")),
                pg_quote_ident(table)
            ))
            .map_err(|e| format!("central id index for {table} failed: {e}"))?;
    }
    Ok(())
}

/// Upsert rows by UUID id. Returns the ids confirmed written; every row is
/// idempotent (ON CONFLICT DO UPDATE) so a mid-batch failure is safe to retry.
fn push_rows_impl(
    client: &mut postgres::Client,
    table: &str,
    rows: &[serde_json::Value],
) -> Result<Vec<String>, String> {
    for row in rows {
        let Some(obj) = row.as_object() else { continue };
        for key in obj.keys() {
            if key != "id" && !base_columns(table).contains(&key.as_str()) {
                client
                    .batch_execute(&format!(
                        "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} TEXT",
                        pg_quote_ident(table),
                        pg_quote_ident(key)
                    ))
                    .map_err(|e| format!("central column add for {table}.{key} failed: {e}"))?;
            }
        }
    }
    let mut acked = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(obj) = row.as_object() else { continue };
        let cols: Vec<&String> = obj.keys().collect();
        let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
        let params: Vec<Box<dyn ToSql + Sync>> = cols
            .iter()
            .map(|c| to_pg_param(obj.get(*c).unwrap_or(&serde_json::Value::Null), pg_column_type(table, c)))
            .collect();
        let param_refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT (\"id\") DO UPDATE SET {}",
            pg_quote_ident(table),
            cols.iter().map(|c| pg_quote_ident(c)).collect::<Vec<_>>().join(", "),
            placeholders.join(", "),
            cols.iter()
                .filter(|c| c.as_str() != "id")
                .map(|c| format!("{} = EXCLUDED.{}", pg_quote_ident(c), pg_quote_ident(c)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        client
            .execute(&sql, &param_refs)
            .map_err(|e| format!("central upsert into {table} failed: {}", error_chain(&e)))?;
        acked.push(obj["id"].as_str().unwrap_or_default().to_string());
    }
    Ok(acked)
}

/// A native synchronous PostgreSQL sink. Configured with a standard connection
/// string (`postgresql://user@host:5432/dbname`); on first configuration it
/// creates the database and mirrors the schema, so setup is paste-and-go.
pub struct RealPostgres {
    cfg: Mutex<Option<String>>,
    client: Mutex<Option<postgres::Client>>,
    last_err: Mutex<Option<String>>,
}

impl RealPostgres {
    pub fn new() -> Self {
        Self {
            cfg: Mutex::new(None),
            client: Mutex::new(None),
            last_err: Mutex::new(None),
        }
    }

    /// Restore a previously saved connection string without touching the
    /// network (startup path); the first real use connects lazily.
    pub fn restore(&self, conn_string: String) {
        *self.cfg.lock().unwrap() = Some(conn_string);
        *self.client.lock().unwrap() = None;
        *self.last_err.lock().unwrap() = None;
    }

    fn set_error(&self, e: String) {
        *self.last_err.lock().unwrap() = Some(e);
    }

    fn ensure_client(&self) -> Result<(), String> {
        let cfg = self.cfg.lock().unwrap();
        let Some(cs) = cfg.as_ref() else {
            let msg = "PostgreSQL is not configured".to_string();
            self.set_error(msg.clone());
            return Err(msg);
        };
        let mut client = self.client.lock().unwrap();
        if client.as_ref().is_none_or(|c| c.is_closed()) {
            match connect_with_create(cs) {
                Ok(c) => {
                    *client = Some(c);
                    *self.last_err.lock().unwrap() = None;
                }
                Err(e) => {
                    self.set_error(e.clone());
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

impl PostgresAdapter for RealPostgres {
    fn label(&self) -> &str {
        "postgres"
    }
    fn configured(&self) -> bool {
        self.cfg.lock().unwrap().is_some()
    }
    fn last_error(&self) -> Option<String> {
        self.last_err.lock().unwrap().clone()
    }
    fn connected(&self) -> bool {
        self.ensure_client().is_ok()
    }
    fn push_rows(&self, table: &str, rows: &[serde_json::Value]) -> Result<Vec<String>, String> {
        self.ensure_client()?;
        let mut client = self.client.lock().unwrap();
        let client_ref = client.as_mut().ok_or("PostgreSQL client unavailable")?;
        ensure_schema_for(client_ref)?;
        match push_rows_impl(client_ref, table, rows) {
            Ok(ids) => {
                *self.last_err.lock().unwrap() = None;
                Ok(ids)
            }
            Err(e) => {
                self.set_error(e.clone());
                Err(e)
            }
        }
    }
    fn configure(&self, conn_string: Option<String>) -> Result<(), String> {
        let mut cfg = self.cfg.lock().unwrap();
        let mut client = self.client.lock().unwrap();
        *client = None;
        *self.last_err.lock().unwrap() = None;
        match &conn_string {
            Some(cs) => {
                let mut c = connect_with_create(cs).map_err(|e| {
                    self.set_error(e.clone());
                    e
                })?;
                ensure_schema_for(&mut c).map_err(|e| {
                    self.set_error(e.clone());
                    e
                })?;
                *cfg = Some(cs.clone());
                *client = Some(c);
            }
            None => {
                *cfg = None;
            }
        }
        Ok(())
    }
    fn delete_rows(&self, table: &str, ids: &[String]) -> Result<(), String> {
        if ids.is_empty() {
            return Ok(());
        }
        self.ensure_client()?;
        let mut client = self.client.lock().unwrap();
        let client_ref = client.as_mut().ok_or("PostgreSQL client unavailable")?;
        let sql = format!("DELETE FROM {} WHERE id = $1", pg_quote_ident(table));
        for id in ids {
            let param: &(dyn ToSql + Sync) = id;
            client_ref
                .execute(&sql, std::slice::from_ref(&param))
                .map_err(|e| format!("central delete from {table} failed: {}", error_chain(&e)))?;
        }
        Ok(())
    }
    fn query_rows(&self, sql: &str, params: &[String]) -> Result<Vec<serde_json::Value>, String> {
        self.ensure_client()?;
        let mut client = self.client.lock().unwrap();
        let client_ref = client.as_mut().ok_or("PostgreSQL client unavailable")?;
        let param_refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let rows = client_ref
            .query(sql, &param_refs)
            .map_err(|e| format!("central report query failed: {}", error_chain(&e)))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut obj = serde_json::Map::new();
            for (i, col) in row.columns().iter().enumerate() {
                obj.insert(col.name().to_string(), pg_cell_to_json(&row, i));
            }
            out.push(serde_json::Value::Object(obj));
        }
        *self.last_err.lock().unwrap() = None;
        Ok(out)
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
const SHEET_HEADERS: &[&str] = &[
    "Trip ID",
    "Plate",
    "Time in",
    "Capacity",
    "Unit",
    "Receipt no",
    "Confidence",
    "Capture method",
    "Discharge trip",
    "Created at",
    "Company",
    "Driver",
];

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

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP client failed: {e}"))
}

fn sa_email(service_account_json: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(service_account_json).map_err(|e| format!("invalid service account JSON: {e}"))?;
    parsed["client_email"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or("service account JSON is missing client_email".to_string())
}

/// Exchange the service account's signed JWT for an access token (RFC 7523).
fn fetch_token(service_account_json: &str) -> Result<String, String> {
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
    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "iss": email,
        "scope": "https://www.googleapis.com/auth/spreadsheets",
        "aud": token_uri,
        "iat": now,
        "exp": now + 3600,
    });
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(key.as_bytes())
        .map_err(|e| format!("invalid private key: {e}"))?;
    let jwt = jsonwebtoken::encode(&header, &claims, &encoding_key)
        .map_err(|e| format!("cannot sign JWT: {e}"))?;
    let client = http_client()?;
    // The JWT is base64url (no characters needing form encoding).
    let body = format!("grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion={jwt}");
    let resp = client
        .post(&token_uri)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .map_err(|e| format!("token request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("token request rejected: {e}"))?;
    let resp_json: serde_json::Value = resp.json().map_err(|e| format!("token response unreadable: {e}"))?;
    resp_json["access_token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or("token response missing access_token".to_string())
}

/// Verify the service account can read the sheet and return its first tab name.
fn sheet_meta(token: &str, sheet_id: &str) -> Result<String, String> {
    let url = format!(
        "https://sheets.googleapis.com/v4/spreadsheets/{sheet_id}?fields=sheets.properties.title"
    );
    let client = http_client()?;
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

/// Write the header row once, when the sheet's A1 cell is empty.
fn ensure_headers(token: &str, sheet_id: &str, tab: &str) -> Result<(), String> {
    let client = http_client()?;
    let range = format!("{tab}!A1");
    let get_url = format!(
        "https://sheets.googleapis.com/v4/spreadsheets/{sheet_id}/values/{}?majorDimension=ROWS",
        urlenc(&range)
    );
    let resp = client
        .get(&get_url)
        .bearer_auth(token)
        .send()
        .map_err(|e| format!("cannot read sheet header: {e}"))?
        .error_for_status()
        .map_err(|e| format!("sheet header read rejected: {e}"))?;
    let j: serde_json::Value = resp.json().map_err(|e| format!("sheet header unreadable: {e}"))?;
    let a1_empty = j["values"][0][0].as_str().map(|s| s.is_empty()).unwrap_or(true);
    if !a1_empty {
        return Ok(());
    }
    let headers: Vec<serde_json::Value> = SHEET_HEADERS.iter().map(|h| json!(h)).collect();
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

fn sheet_cell(row: &serde_json::Value, header: &str) -> serde_json::Value {
    match header {
        "Trip ID" => opt_str(row.get("id")),
        "Plate" => opt_str(row.get("plate")),
        "Time in" => opt_str(row.get("time_in")),
        "Capacity" => fmt_opt_num(row.get("capacity_at_trip")),
        "Unit" => opt_str(row.get("capacity_unit")),
        "Receipt no" => opt_str(row.get("receipt_no")),
        "Confidence" => fmt_opt_num(row.get("confidence_score")),
        "Capture method" => opt_str(row.get("capture_method")),
        "Discharge trip" => match row.get("is_discharge_trip") {
            Some(serde_json::Value::Bool(true)) => json!("Yes"),
            Some(serde_json::Value::Bool(false)) => json!("No"),
            _ => json!(""),
        },
        "Created at" => opt_str(row.get("created_at")),
        "Company" => opt_str(row.get("company")),
        "Driver" => opt_str(row.get("driver")),
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
}

impl RealSheets {
    pub fn new() -> Self {
        Self {
            creds: Mutex::new(None),
            token: Mutex::new(None),
            last_fail: Mutex::new(None),
            last_err: Mutex::new(None),
        }
    }

    fn set_error(&self, e: String) {
        *self.last_err.lock().unwrap() = Some(e);
    }

    /// Cached access token; refreshes only when expired. Errors are cached for
    /// 60s so an offline machine doesn't hammer the token endpoint.
    /// Restore previously saved credentials WITHOUT any network (startup path).
    /// The sheet is validated lazily on first real use.
    pub fn restore(&self, json: String, sheet_id: String) -> Result<String, String> {
        let email = sa_email(&json)?;
        *self.creds.lock().unwrap() = Some(SheetsCreds {
            client_email: email.clone(),
            json,
            sheet_id,
            first_sheet: None,
        });
        *self.last_err.lock().unwrap() = None;
        Ok(email)
    }

    /// Resolve the first tab name (network) exactly once; everything that
    /// touches the sheet calls this first.
    fn ensure_validated(&self) -> Result<String, String> {
        let mut creds = self.creds.lock().unwrap();
        let Some(c) = creds.as_mut() else {
            return Err("Google Sheets is not configured".to_string());
        };
        if let Some(first) = c.first_sheet.as_ref() {
            return Ok(first.clone());
        }
        let token = fetch_token(&c.json).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let first = sheet_meta(&token, &c.sheet_id).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        ensure_headers(&token, &c.sheet_id, &first).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let now = chrono::Utc::now().timestamp();
        *self.token.lock().unwrap() = Some((token, now + 3600 - 60));
        c.first_sheet = Some(first.clone());
        *self.last_err.lock().unwrap() = None;
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
            let tok = self.token.lock().unwrap();
            if let Some((t, exp)) = tok.as_ref() {
                if *exp > now {
                    return Ok(t.clone());
                }
            }
        }
        if let Some(last) = *self.last_fail.lock().unwrap() {
            if now - last < 60 {
                return Err("Google Sheets is unreachable (recent attempt failed)".to_string());
            }
        }
        match fetch_token(&creds.json) {
            Ok(t) => {
                *self.token.lock().unwrap() = Some((t.clone(), now + 3600 - 60));
                Ok(t)
            }
            Err(e) => {
                *self.last_fail.lock().unwrap() = Some(now);
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
    fn configured(&self) -> bool {
        self.creds.lock().unwrap().is_some()
    }
    fn last_error(&self) -> Option<String> {
        self.last_err.lock().unwrap().clone()
    }
    fn service_account_email(&self) -> Option<String> {
        self.creds.lock().unwrap().as_ref().map(|c| c.client_email.clone())
    }
    fn connected(&self) -> bool {
        if self.creds.lock().unwrap().is_none() {
            return false;
        }
        self.ensure_validated().is_ok()
    }
    fn push_trips(&self, rows: &[serde_json::Value]) -> Result<Vec<String>, String> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let creds = self.creds.lock().unwrap().clone().ok_or("Google Sheets is not configured")?;
        let first_sheet = self.ensure_validated()?;
        let token = self.access_token().map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let client = http_client()?;
        // One batched request for the whole batch — NOT one request per row.
        // Per-row requests held the DB lock for minutes on a full backlog;
        // this is what kept the app "lugging" after launch.
        let values: Vec<Vec<serde_json::Value>> = rows
            .iter()
            .map(|row| SHEET_HEADERS.iter().map(|h| sheet_cell(row, h)).collect())
            .collect();
        let range = format!("{first_sheet}!A1");
        let url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{}/values/{}:append?valueInputOption=RAW",
            creds.sheet_id,
            urlenc(&range)
        );
        let body = json!({ "range": range, "majorDimension": "ROWS", "values": values });
        client
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
        *self.last_err.lock().unwrap() = None;
        Ok(acked)
    }
    fn configure(&self, json: Option<String>, sheet_id: Option<String>) -> Result<String, String> {
        let (Some(j), Some(sid)) = (json, sheet_id) else {
            *self.creds.lock().unwrap() = None;
            *self.token.lock().unwrap() = None;
            *self.last_fail.lock().unwrap() = None;
            *self.last_err.lock().unwrap() = None;
            return Ok("disconnected".to_string());
        };
        let email = sa_email(&j).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let token = fetch_token(&j).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let first_sheet = sheet_meta(&token, &sid).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        ensure_headers(&token, &sid, &first_sheet).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        *self.creds.lock().unwrap() = Some(SheetsCreds {
            client_email: email.clone(),
            json: j,
            sheet_id: sid,
            first_sheet: Some(first_sheet),
        });
        *self.token.lock().unwrap() = Some((token, chrono::Utc::now().timestamp() + 3600 - 60));
        *self.last_err.lock().unwrap() = None;
        Ok(email)
    }
    fn prune(&self, cutoff_iso: Option<&str>, excluded_ids: &[String]) -> Result<usize, String> {
        let creds = self.creds.lock().unwrap().clone().ok_or("Google Sheets is not configured")?;
        let first_sheet = self.ensure_validated()?;
        let token = self.access_token().map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let client = http_client()?;
        let range = format!("{first_sheet}!A1:Z");
        let url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{}/values/{}?majorDimension=ROWS",
            creds.sheet_id,
            urlenc(&range)
        );
        let resp = client
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
                if let Some(cut) = &cutoff {
                    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&cell(row, 9)) {
                        if ts.with_timezone(&chrono::Utc) < *cut {
                            return false;
                        }
                    }
                }
                true
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
        client
            .post(&clear_url)
            .bearer_auth(&token)
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
            client
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
}

// ---------------------------------------------------------------------------
// Startup wiring + configuration commands
// ---------------------------------------------------------------------------

/// Build the app's Postgres adapter, restoring any saved connection string
/// without blocking startup on the network (first use connects lazily).
pub fn real_postgres(conn: &Connection) -> Arc<dyn PostgresAdapter> {
    let pg = RealPostgres::new();
    if let Some(cs) = get_setting(conn, "pg_connection_string") {
        pg.restore(cs);
    }
    Arc::new(pg)
}

/// Build the app's Sheets provider, restoring saved credentials and validating
/// them (failures surface in the Sync panel via `last_error`).
pub fn real_sheets(conn: &Connection) -> Arc<dyn SheetsProvider> {
    let sheets = RealSheets::new();
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
pub fn configure_postgres(state: State<AppState>, actor_id: String, connection_string: String) -> Result<PgSyncStateView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    if connection_string.trim().is_empty() {
        return Err("Connection string cannot be empty.".to_string());
    }
    state
        .pg
        .configure(Some(connection_string.clone()))
        .map_err(|e| format!("PostgreSQL configuration failed: {e}"))?;
    set_setting(&conn, "pg_connection_string", &connection_string)?;
    append_audit(
        &conn,
        &actor_id,
        "configured_postgres",
        None,
        Some(json!({ "connection_string": sanitize_conn_string(&connection_string) })),
    )?;
    pg_sync_state_impl(&conn, &*state.pg)
}

#[tauri::command]
pub fn disconnect_postgres(state: State<AppState>, actor_id: String) -> Result<PgSyncStateView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    state.pg.configure(None)?;
    conn.execute("DELETE FROM app_settings WHERE key = 'pg_connection_string'", [])
        .map_err(|e| format!("settings clear failed: {e}"))?;
    append_audit(&conn, &actor_id, "disconnected_postgres", None, None)?;
    pg_sync_state_impl(&conn, &*state.pg)
}

#[tauri::command]
pub fn configure_google_sheets(
    state: State<AppState>,
    actor_id: String,
    service_account_json: String,
    target_sheet_id: String,
    shared_group: Option<String>,
    sync_frequency: String,
) -> Result<SheetsStateView, String> {
    if sync_frequency != "realtime" && sync_frequency != "every_15_min" {
        return Err("Sync frequency must be realtime or every_15_min.".to_string());
    }
    if service_account_json.trim().is_empty() || target_sheet_id.trim().is_empty() {
        return Err("Service account JSON and target sheet ID are required.".to_string());
    }
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    let email = state
        .sheets
        .configure(Some(service_account_json.clone()), Some(target_sheet_id.clone()))
        .map_err(|e| format!("Google Sheets configuration failed: {e}"))?;
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
    set_setting(&conn, "sheets_service_account_json", &service_account_json)?;
    set_setting(&conn, "sheets_target_sheet_id", &target_sheet_id)?;
    append_audit(
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
    )?;
    sheets_state_impl(&conn, &*state.sheets)
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

fn sheets_due(conn: &Connection, sheets: &dyn SheetsProvider) -> bool {
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
        if let Err(e) = run_pg_sync_impl(conn, pg) {
            let _ = crate::monitor::record_health_event(conn, "sync", "degraded", Some(&format!("PostgreSQL sync failed: {e}")));
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
        let err = fetch_token(
            r#"{ "client_email": "a@b.iam.gserviceaccount.com", "private_key": "not a key" }"#,
        );
        assert!(err.is_err(), "expected a clean error, got: {err:?}");
    }

    #[test]
    fn sheet_cell_maps_row_to_columns() {
        let row = json!({
            "id": "x",
            "plate": "KDG 123A",
            "time_in": "2026-01-01T10:00:00Z",
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
        assert_eq!(sheet_cell(&row, "Plate"), json!("KDG 123A"));
        assert_eq!(sheet_cell(&row, "Capacity"), json!("40"));
        assert_eq!(sheet_cell(&row, "Confidence"), json!("0.97"));
        assert_eq!(sheet_cell(&row, "Receipt no"), json!(""));
        assert_eq!(sheet_cell(&row, "Discharge trip"), json!("Yes"));
        assert_eq!(sheet_cell(&row, "Driver"), json!("Jane"));
    }
}
