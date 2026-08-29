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
        if pending > 0 && pg.configured() {
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

pub fn pending_for_table(conn: &Connection, table: &str) -> Result<i64, String> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE synced = 0"), [], |r| r.get(0))
        .map_err(|e| format!("{table} count failed: {e}"))
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
         WHERE status = 'logged' AND pushed_to_sheets = 0
           AND (capture_method = 'auto' OR is_discharge_trip = 1)",
        [],
        |r| r.get(0),
    )
    .map_err(|e| format!("sheets pending count failed: {e}"))
}

fn sheet_trip_rows_filtered(conn: &Connection, has_sheet_row: bool) -> Result<Vec<serde_json::Value>, String> {
    let row_filter = if has_sheet_row { "t.sheet_row IS NOT NULL" } else { "t.sheet_row IS NULL" };
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
         WHERE t.status = 'logged' AND t.pushed_to_sheets = 0
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
///
/// Deduplication: reads the existing trip IDs from the sheet (column 0) and
/// filters them out of `new_rows` so a trip that was already appended but
/// somehow lost its `sheet_row` reference is never duplicated.
pub fn prepare_sheets_data(
    conn: &Connection,
    sheets: &dyn SheetsProvider,
) -> Result<SheetsSyncData, String> {
    let pending = pending_sheets_trips(conn)?;
    let mapping = read_sheet_mapping(conn);
    let mut new_rows = sheet_trip_rows_filtered(conn, false)?;
    let update_rows = sheet_trip_rows_filtered(conn, true)?;
    // Dedup: remove any new_rows whose trip ID already exists in the sheet.
    // This prevents the infinite duplication loop when sheet_row is lost.
    if !new_rows.is_empty() {
        match sheets.read_existing_trip_ids() {
            Ok(existing) if !existing.is_empty() => {
                let before = new_rows.len();
                new_rows.retain(|r| {
                    r.get("id")
                        .and_then(|v| v.as_str())
                        .map(|id| !existing.contains(&id.to_string()))
                        .unwrap_or(true)
                });
                let deduped = before - new_rows.len();
                if deduped > 0 {
                    crate::log::log(&format!(
                        "[sheets] dedup: removed {deduped} rows that already exist in the sheet"
                    ));
                }
            }
            _ => {} // Can't read sheet (not configured / network error) — skip dedup.
        }
    }
    Ok(SheetsSyncData { pending, mapping, new_rows, update_rows })
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
    // Mark exit-updated rows.
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
    let data = prepare_sheets_data(conn, sheets)?;
    let pending = data.pending;
    let mut pushed = 0i64;
    if pending > 0 && sheets.configured() {
        let acked_ids = execute_sheets_network(sheets, &data.mapping, &data.new_rows, &data.update_rows)?;
        pushed = finalize_sheets_results(conn, &data.new_rows, &data.update_rows, &acked_ids)?;
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
pub fn sync_now_pg(state: State<AppState>, actor_id: String, handle: AppHandle) -> Result<String, String> {
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }

    let pg = state.pg.clone();
    let db = state.sync_db.clone();
    let actor = actor_id.clone();
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
        }

        let error_msg = if errors.is_empty() { None } else { Some(errors.join("; ")) };
        let _ = handle.emit("pg-sync-done", json!({ "pushed": total_pushed, "error": error_msg }));
    });
    Ok("syncing".to_string())
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
pub fn sync_now_sheets(state: State<AppState>, actor_id: String, handle: AppHandle) -> Result<String, String> {
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }

    let sheets = state.sheets.clone();
    let db = state.db.clone();
    let actor = actor_id.clone();
    std::thread::spawn(move || {
        // Phase 1: prepare data
        let data = {
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
    config.connect_timeout(std::time::Duration::from_secs(15));
    // TCP keepalive: detect dead connections at the OS level so the worker
    // doesn't hang forever on a half-open socket. Without this, a silently
    // dropped connection (server crash, network outage) causes every push
    // and pull to block indefinitely.
    config.keepalives(true);
    config.keepalives_idle(std::time::Duration::from_secs(30));
    config.keepalives_interval(std::time::Duration::from_secs(5));
    let tls = make_tls_connector()?;
    let make_client = |cfg: &mut postgres::Config| -> Result<postgres::Client, String> {
        let mut c = cfg.connect(tls.clone()).map_err(|e| {
            let is_missing_db = e.as_db_error().map(|d| d.message().contains("does not exist")).unwrap_or(false);
            if is_missing_db { "__MISSING_DB__".to_string() } else { format!("cannot connect to PostgreSQL: {e}") }
        })?;
        // Set a statement timeout so individual queries can't hang forever.
        // This prevents the client Mutex from being held indefinitely when
        // Supabase / PgBouncer drops a backend connection silently.
        c.execute("SET statement_timeout = '30000'", &[])
            .map_err(|e| format!("cannot set statement_timeout: {e}"))?;
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
}/// Upsert rows by UUID id. Returns the ids confirmed written; every row is
/// idempotent (ON CONFLICT DO UPDATE) so a mid-batch failure is safe to retry.
///
/// Batching strategy:
///   1. Collect ALL unique dynamic column names across all rows first, then
///      run ALTER TABLE once per unique column (instead of per row × column).
///   2. Wrap all INSERTs in a single transaction and execute them via
///      batch_execute — one network round-trip instead of N.
/// Maximum rows per transaction chunk. 327 rows in a single transaction
/// exceeds Supabase/PgBouncer's idle-in-transaction timeout and causes
/// infinite retry loops. Chunks of 50 keep each commit under 5 seconds.
const PG_CHUNK_SIZE: usize = 50;

fn push_rows_impl(
    client: &mut postgres::Client,
    table: &str,
    rows: &[serde_json::Value],
) -> Result<Vec<String>, String> {
    // Phase 1: deduplicate ALTER TABLE — one round-trip per unique dynamic column.
    let mut seen_dynamic: std::collections::HashSet<String> = std::collections::HashSet::new();
    let base = base_columns(table);
    for row in rows {
        let Some(obj) = row.as_object() else { continue };
        for key in obj.keys() {
            if key != "id" && !base.contains(&key.as_str()) && seen_dynamic.insert(key.clone()) {
                client
                    .batch_execute(&format!(
                        "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} TEXT",
                        pg_quote_ident(table), pg_quote_ident(key)
                    ))
                    .map_err(|e| format!("central column add for {table}.{key} failed: {e}"))?;
            }
        }
    }
    // Phase 2: upsert in chunks — each chunk is its own transaction so a
    // timeout on chunk 3 doesn't lose the work from chunks 1-2.
    let mut all_acked: Vec<String> = Vec::new();
    for chunk in rows.chunks(PG_CHUNK_SIZE) {
        let mut tx = client
            .transaction()
            .map_err(|e| format!("central tx begin for {table} failed: {e}"))?;
        {
            tx.execute("SET LOCAL statement_timeout = '30000'", &[])
                .map_err(|e| format!("central statement_timeout set failed: {e}"))?;
            for row in chunk {
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
                tx.execute(&sql, &param_refs)
                    .map_err(|e| format!("central upsert into {table} failed: {}", error_chain(&e)))?;
            }
        }
        tx.commit()
            .map_err(|e| format!("central tx commit for {table} failed: {e}"))?;
        // Mark this chunk as acked — the caller can mark them synced
        // immediately so a crash only loses the current chunk, not all rows.
        all_acked.extend(chunk.iter().filter_map(|r| r.as_object()?.get("id")?.as_str().map(String::from)));
    }
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
        match connect_with_create(&cs) {
            Ok(c) => {
                self.client = Some(c);
                self.last_err = None;
                self.connect_backoff = 0;
                self.schema_validated = false;
                Ok(())
            }
            Err(e) => {
                self.last_err = Some(e.clone());
                let prev = self.connect_backoff;
                self.connect_backoff = (prev.max(10) * 2).min(120);
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
    fn check_health(&mut self) {
        let now = chrono::Utc::now().timestamp();
        if now - self.last_health_check < 30 {
            return; // rate-limited
        }
        self.last_health_check = now;
        let healthy = match self.client.as_mut() {
            Some(c) if !c.is_closed() => c.simple_query("SELECT 1").is_ok(),
            _ => false,
        };
        if !healthy {
            crate::log::log("[sync] pg health check failed — dropping connection, will reconnect");
            self.client = None;
            self.schema_validated = false;
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
                let result = (|| -> Result<Vec<String>, String> {
                    self.ensure_client()?;
                    let client = self.client.as_mut().ok_or("no client")?;
                    if !self.schema_validated {
                        ensure_schema_for(client)?;
                        self.schema_validated = true;
                    }
                    push_rows_impl(client, &table, &rows)
                })();
                match &result {
                    Ok(_) => { self.last_err = None; }
                    Err(e) => {
                        self.last_err = Some(e.clone());
                        self.client = None;
                        self.schema_validated = false;
                    }
                }
                let _ = tx.send(result);
            }
            PgCommand::QueryRows(sql, params, tx) => {
                let result = (|| -> Result<Vec<serde_json::Value>, String> {
                    self.ensure_client()?;
                    let client = self.client.as_mut().ok_or("no client")?;
                    let mut pg_tx = client.transaction()
                        .map_err(|e| format!("query tx begin failed: {}", error_chain(&e)))?;
                    pg_tx.execute("SET LOCAL statement_timeout = '10000'", &[])
                        .map_err(|e| format!("query timeout set failed: {}", error_chain(&e)))?;
                    let param_refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
                    let rows = pg_tx.query(sql.as_str(), &param_refs)
                        .map_err(|e| format!("query failed: {}", error_chain(&e)))?;
                    let result: Vec<serde_json::Value> = rows.iter().map(|row| {
                        let mut obj = serde_json::Map::new();
                        for (i, col) in row.columns().iter().enumerate() {
                            obj.insert(col.name().to_string(), pg_cell_to_json(row, i));
                        }
                        serde_json::Value::Object(obj)
                    }).collect();
                    pg_tx.commit()
                        .map_err(|e| format!("query tx commit failed: {}", error_chain(&e)))?;
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
                // Proactively detect dead connections before processing each
                // command. Without this, a half-open TCP socket causes every
                // push/pull to block indefinitely.
                worker.check_health();
                worker.handle(cmd);
                is_busy.store(false, std::sync::atomic::Ordering::Relaxed);
                // Push the worker's last_err into the shared cache so
                // RealPostgres::last_error() can read it without sending
                // a command to this thread (which blocks up to WORKER_TIMEOUT).
                if let Ok(mut e) = shared_err.lock() {
                    *e = worker.last_err.clone();
                }
                // Update is_connected after EVERY command. This is critical:
                // when the poller times out waiting for a long push, it used
                // to set is_connected=false, which permanently skipped future
                // pushes. Now the worker restores the flag after completion.
                let connected = worker.client.as_ref().is_some_and(|c| !c.is_closed());
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
}

impl RealPostgres {
    pub fn new() -> Self {
        let shared_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let is_connected: Arc<std::sync::atomic::AtomicBool> = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let is_busy: Arc<std::sync::atomic::AtomicBool> = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tx = spawn_pg_worker(shared_err.clone(), is_connected.clone(), is_busy.clone());
        Self {
            tx,
            is_connected,
            is_configured: std::sync::atomic::AtomicBool::new(false),
            is_busy,
            cached_last_err: shared_err,
        }
    }

    /// Restore a previously saved connection string without touching the
    /// network (startup path); the first real use connects lazily.
    pub fn restore(&self, conn_string: String) {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = self.tx.send(PgCommand::Configure(Some(conn_string), tx));
        // Don't wait for response — the worker connects lazily.
        // Just mark as configured so callers don't skip.
        self.is_configured.store(true, std::sync::atomic::Ordering::Relaxed);
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
        self.cached_last_err.lock().ok().and_then(|e| e.clone())
    }
    fn connected(&self) -> bool {
        // Pure atomic check — NEVER sends a command to the worker.
        // The worker updates this flag after every push/query/configure.
        // This ensures sync_status (called by every page) never blocks.
        self.is_connected.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn push_rows(&self, table: &str, rows: &[serde_json::Value]) -> Result<Vec<String>, String> {
        // Don't queue a push if the worker is still processing the previous one.
        // Without this, every 10s poller cycle would queue another push,
        // building up a backlog of identical pushes.
        if self.is_busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(format!("{table} push deferred — worker busy (will retry next cycle)"));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        // Give push_rows up to 120 seconds for large batches over remote TLS.
        // This runs on dedicated background worker threads (spawn_sync_poller and sync_now_pg),
        // so it NEVER blocks the UI or main thread.
        let result = match self.send_timeout(PgCommand::PushRows(table.to_string(), rows.to_vec(), tx), rx, std::time::Duration::from_secs(120)) {
            Some(result) => {
                // NOTE: Do NOT set is_connected here — let only the worker thread
                // manage is_connected to avoid races between caller and worker.
                result
            }
            None => {
                // Worker timed out — clear is_busy so next cycle can retry.
                self.is_busy.store(false, std::sync::atomic::Ordering::SeqCst);
                Err(format!("{table} push timed out — retrying next cycle"))
            }
        };
        // Always clear is_busy.
        self.is_busy.store(false, std::sync::atomic::Ordering::SeqCst);
        result
    }
    fn configure(&self, conn_string: Option<String>) -> Result<(), String> {
        self.is_configured.store(conn_string.is_some(), std::sync::atomic::Ordering::Relaxed);
        self.is_connected.store(false, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        match self.send_timeout(PgCommand::Configure(conn_string, tx), rx, std::time::Duration::from_secs(15)) {
            Some(result) => {
                self.is_connected.store(result.is_ok(), std::sync::atomic::Ordering::Relaxed);
                result
            }
            None => Err("Postgres worker busy".to_string()),
        }
    }
    fn delete_rows(&self, table: &str, ids: &[String]) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        match self.send_timeout(PgCommand::DeleteRows(table.to_string(), ids.to_vec(), tx), rx, std::time::Duration::from_secs(15)) {
            Some(result) => result,
            None => Err(format!("{table} delete deferred — worker busy")),
        }
    }
    fn query_rows(&self, sql: &str, params: &[String]) -> Result<Vec<serde_json::Value>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        match self.send_timeout(PgCommand::QueryRows(sql.to_string(), params.to_vec(), tx), rx, std::time::Duration::from_secs(15)) {
            Some(result) => result,
            None => Err("PostgreSQL query timed out — falling back to local data".to_string()),
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
    let client = http_client()?;
    // Use Google's server time to avoid clock skew issues.
    let now = google_server_time(&client);
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

/// Always overwrite the header row in the sheet to match the current mapping.
/// This runs on every sync cycle so that reordered/renamed/disabled columns
/// and newly added custom fields are reflected immediately.
fn ensure_headers(token: &str, sheet_id: &str, tab: &str, mapping: &[crate::models::SheetColumnEntry]) -> Result<(), String> {
    let client = http_client()?;
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
        let token = fetch_token(&json).map_err(|e| {
            self.set_error(e.clone());
            self.cached_connected.store(false, std::sync::atomic::Ordering::Relaxed);
            e
        })?;
        let first = sheet_meta(&token, &sheet_id).map_err(|e| {
            self.set_error(e.clone());
            self.cached_connected.store(false, std::sync::atomic::Ordering::Relaxed);
            e
        })?;
        let mapping = self.mapping.lock().unwrap_or_else(|e| e.into_inner()).clone();
        ensure_headers(&token, &sheet_id, &first, &mapping).map_err(|e| {
            self.set_error(e.clone());
            self.cached_connected.store(false, std::sync::atomic::Ordering::Relaxed);
            e
        })?;
        let server_now = google_server_time(&http_client().map_err(|e| {
            self.cached_connected.store(false, std::sync::atomic::Ordering::Relaxed);
            e
        })?);
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
        match fetch_token(&creds.json) {
            Ok(t) => {
                // Use Google server time for expiry so the cached token isn't
                // prematurely rejected when the local clock is skewed.
                let client = http_client()?;
                let server_now = google_server_time(&client);
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
        let client = http_client()?;
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
        *self.last_err.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(acked)
    }
    fn ensure_header_row(&self, mapping: &[crate::models::SheetColumnEntry]) -> Result<(), String> {
        let creds = self.creds.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or("Google Sheets is not configured")?;
        let first_sheet = self.ensure_validated()?;
        let token = self.access_token().map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        ensure_headers(&token, &creds.sheet_id, &first_sheet, mapping).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
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
        let client = http_client()?;
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
        let resp = client
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
        let client = http_client()?;
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
                client
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
        let token = fetch_token(&j).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let first_sheet = sheet_meta(&token, &sid).map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let mapping = self.mapping.lock().unwrap_or_else(|e| e.into_inner()).clone();
        ensure_headers(&token, &sid, &first_sheet, &mapping).map_err(|e| {
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
        client
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
    fn read_existing_trip_ids(&self) -> Result<Vec<String>, String> {
        let creds = self.creds.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or("Google Sheets is not configured")?;
        let first_sheet = self.ensure_validated()?;
        let token = self.access_token().map_err(|e| {
            self.set_error(e.clone());
            e
        })?;
        let client = http_client()?;
        let range = format!("{first_sheet}!A1:A");
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
pub fn configure_postgres(state: State<AppState>, actor_id: String, connection_string: String, handle: AppHandle) -> Result<String, String> {
    if connection_string.trim().is_empty() {
        return Err("Connection string cannot be empty.".to_string());
    }
    // Permission check (fast)
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }
    // Spawn heavy work on background thread — frontend receives "testing" instantly
    let pg = state.pg.clone();
    let db = state.db.clone();
    std::thread::spawn(move || {
        // Network call (slow — up to 6s TCP connect timeout)
        match pg.configure(Some(connection_string.clone())) {
            Ok(()) => {
                // Persist + audit (fast) — release db BEFORE emit to avoid deadlock
                // (emit waits for webview; webview may invoke a command that needs db).
                let emit_payload = if let Ok(conn) = db.lock() {
                    let _ = set_setting(&conn, "pg_connection_string", &connection_string);
                    let _ = append_audit(&conn, &actor_id, "configured_postgres", None, Some(json!({ "connection_string": sanitize_conn_string(&connection_string) })));
                    let view = pg_sync_state_impl(&conn, &*pg).ok();
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

#[tauri::command]
pub fn disconnect_postgres(state: State<AppState>, actor_id: String, handle: AppHandle) -> Result<String, String> {
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
    // Phase 1: Permission check (fast)
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, INTEGRATION_PERM)?;
    }
    // Phase 2: Network call (slow — OAuth token fetch, no lock held)
    let email = state
        .sheets
        .configure(Some(service_account_json.clone()), Some(target_sheet_id.clone()))
        .map_err(|e| format!("Google Sheets configuration failed: {e}"))?;
    // Phase 3: Persist + audit (fast)
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
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
        let err = fetch_token(
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
