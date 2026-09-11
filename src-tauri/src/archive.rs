//! Trip archive management (admin only) — Phase 5/6 follow-up per client
//! direction:
//!
//! - **Soft delete**: hidden from the app and future sheet exports, but kept in
//!   the local DB and the PostgreSQL archive (the permanent record). Restorable.
//! - **Hard delete**: physically removed from local *and* the central Postgres,
//!   and pruned from the sheet. Password + confirmation required.
//! - **Local purge**: frees space by dropping local copies of trips already
//!   confirmed in Postgres (the permanent archive stays intact).
//!
//! Every destructive action requires the actor's own password and is
//! audit-logged. Reporting stays strictly read-only; these commands are the
//! only write path against the trip archive.

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params};
use serde_json::json;
use tauri::State;

use crate::db::{append_audit, now_iso, AppState};
use crate::models::{ReportFilters, TripView};

const ARCHIVE_PERM: &str = "manage_users";

fn ensure_admin(conn: &Connection, actor_id: &str) -> Result<(), String> {
    crate::commands::ensure_admin_permission(conn, actor_id, ARCHIVE_PERM)
}

/// Same filter shape as reporting (date range + company), pinned to the
/// logged/archived combination.
fn where_clause_for(filters: &ReportFilters, archived: bool) -> (String, Vec<SqlValue>) {
    let mut parts = vec![format!(
        "t.status = 'logged' AND t.archived = {}",
        if archived { 1 } else { 0 }
    )];
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(from) = filters.from.as_deref() {
        parts.push("t.time_in >= ?".to_string());
        params.push(SqlValue::Text(from.to_string()));
    }
    if let Some(to) = filters.to.as_deref() {
        parts.push("t.time_in <= ?".to_string());
        params.push(SqlValue::Text(to.to_string()));
    }
    if let Some(cid) = filters.company_id.as_deref() {
        parts.push("t.company_id = ?".to_string());
        params.push(SqlValue::Text(cid.to_string()));
    }
    (parts.join(" AND "), params)
}

fn exported_archived_ids(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT id FROM trips WHERE archived = 1 AND pushed_to_sheets = 1")
        .map_err(|e| format!("exported ids failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("exported ids failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("exported ids read failed: {e}"))
}

/// The most recent non-archived trips, for picking rows to delete.
#[tauri::command]
pub fn list_recent_trips(state: State<AppState>, actor_id: String, limit: i64) -> Result<Vec<TripView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin(&conn, &actor_id)?;
    let limit = limit.clamp(1, 500);
    let sql = format!(
        "{} WHERE t.archived = 0 ORDER BY t.time_in DESC LIMIT ?",
        crate::capture::TRIP_SELECT
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("recent trips failed: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![limit], crate::capture::read_trip)
        .map_err(|e| format!("recent trips failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("recent trips read failed: {e}"))
}

/// The archived (soft-deleted) trips for the "show archived" view.
#[tauri::command]
pub fn list_archived_trips(
    state: State<AppState>,
    actor_id: String,
    filters: ReportFilters,
) -> Result<Vec<TripView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin(&conn, &actor_id)?;
    let (where_sql, params) = where_clause_for(&filters, true);
    let sql = format!(
        "{} WHERE {where_sql} ORDER BY t.time_in DESC LIMIT 500",
        crate::capture::TRIP_SELECT
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("archived list failed: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), crate::capture::read_trip)
        .map_err(|e| format!("archived list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("archived list read failed: {e}"))
}

/// Hide trips from the app and future sheet exports. They stay in the local DB
/// and the Postgres archive, and can be restored. Already-exported rows are
/// pruned from the sheet.
#[tauri::command]
pub fn soft_delete_trips(
    state: State<AppState>,
    actor_id: String,
    trip_ids: Vec<String>,
    actor_credential: String,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin(&conn, &actor_id)?;
    crate::commands::verify_actor_password(&conn, &actor_id, &actor_credential)?;
    if trip_ids.is_empty() {
        return Err("No trips selected.".to_string());
    }
    let mut changed: usize = 0;
    for id in &trip_ids {
        changed += conn
            .execute(
                "UPDATE trips SET archived = 1, updated_at = ?1 WHERE id = ?2 AND archived = 0",
                params![now_iso(), id],
            )
            .map_err(|e| format!("soft delete failed: {e}"))?;
    }
    let exported = exported_archived_ids(&conn)?;
    let _ = state.sheets.prune(None, &exported);
    append_audit(
        &conn,
        &actor_id,
        "soft_deleted_trips",
        None,
        Some(json!({ "count": changed, "trip_ids": trip_ids })),
    )?;
    Ok(changed as i64)
}

/// Bring soft-deleted trips back into normal views (and future exports).
#[tauri::command]
pub fn restore_trips(
    state: State<AppState>,
    actor_id: String,
    trip_ids: Vec<String>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin(&conn, &actor_id)?;
    let mut changed: usize = 0;
    for id in &trip_ids {
        changed += conn
            .execute(
                "UPDATE trips SET archived = 0, updated_at = ?1 WHERE id = ?2 AND archived = 1",
                params![now_iso(), id],
            )
            .map_err(|e| format!("restore failed: {e}"))?;
    }
    append_audit(
        &conn,
        &actor_id,
        "restored_trips",
        None,
        Some(json!({ "count": changed, "trip_ids": trip_ids })),
    )?;
    Ok(changed as i64)
}

/// Physically remove trips everywhere: local DB, central Postgres (for rows
/// already synced), and the sheet. Requires the admin's password.
#[tauri::command]
pub fn hard_delete_trips(
    state: State<AppState>,
    actor_id: String,
    trip_ids: Vec<String>,
    actor_credential: String,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin(&conn, &actor_id)?;
    crate::commands::verify_actor_password(&conn, &actor_id, &actor_credential)?;
    if trip_ids.is_empty() {
        return Err("No trips selected.".to_string());
    }
    let mut synced: Vec<String> = Vec::new();
    let mut deleted: usize = 0;
    for id in &trip_ids {
        let is_synced: i64 = conn
            .query_row(
                "SELECT COALESCE(synced, 0) FROM trips WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if is_synced == 1 {
            synced.push(id.clone());
        }
        deleted += conn
            .execute("DELETE FROM trips WHERE id = ?1", params![id])
            .map_err(|e| format!("hard delete failed: {e}"))?;
    }
    if !synced.is_empty() {
        state
            .pg
            .delete_rows("trips", &synced)
            .map_err(|e| format!("central delete failed: {e}"))?;
    }
    let _ = state.sheets.prune(None, &trip_ids);
    append_audit(
        &conn,
        &actor_id,
        "hard_deleted_trips",
        None,
        Some(json!({ "count": deleted, "trip_ids": trip_ids })),
    )?;
    Ok(deleted as i64)
}

/// Free local space by dropping local copies of trips already confirmed in
/// Postgres. The central archive is untouched. Logged trips only — declined /
/// discarded / unsynced rows are protected.
#[tauri::command]
pub fn purge_local_trips(
    state: State<AppState>,
    actor_id: String,
    actor_credential: String,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin(&conn, &actor_id)?;
    crate::commands::verify_actor_password(&conn, &actor_id, &actor_credential)?;
    let n = conn
        .execute("DELETE FROM trips WHERE status = 'logged' AND synced = 1", [])
        .map_err(|e| format!("local purge failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "purged_local_trips",
        None,
        Some(json!({ "removed": n })),
    )?;
    Ok(n as i64)
}
