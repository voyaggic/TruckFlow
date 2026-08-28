//! Phase 5 — System Monitor (05-ui-screens.md §6h, 08-anpr-integration.md §5).
//!
//! Per-component health from `system_health_events` for the four components
//! (`camera`, `anpr_service`, `sync`, `database`). Monitoring is observational
//! only and never a gatekeeper (02-architecture.md §2): nothing in this module
//! can block capture. Rows are created by wiring (sync failures, poller
//! unreachability) via `record_health_event`; "ok" never inserts a row, it only
//! resolves open incidents so history stays incident-only.

use rusqlite::{params, Connection};
use tauri::State;

use crate::db::{now_iso, AppState};
use crate::models::{ComponentHealth, ConfidenceTrendPoint, HealthDashboard, HealthEventView};

pub const COMPONENTS: &[&str] = &["camera", "anpr_service", "sync", "database"];

/// Record a health observation for a component.
///
/// - `status = "ok"` closes every open incident for the component (recovery).
/// - `status = "degraded" | "offline"` opens an incident unless one is already
///   open for the component (no duplicate spam); if one is open, its detail and
///   detection time are refreshed so it never goes stale.
pub fn record_health_event(conn: &Connection, component: &str, status: &str, detail: Option<&str>) -> Result<(), String> {
    let now = now_iso();
    if status == "ok" {
        conn.execute(
            "UPDATE system_health_events SET resolved_at = ?1
             WHERE component = ?2 AND resolved_at IS NULL",
            params![now, component],
        )
        .map_err(|e| format!("health resolve failed: {e}"))?;
        return Ok(());
    }

    let open: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM system_health_events WHERE component = ?1 AND resolved_at IS NULL",
            params![component],
            |r| r.get(0),
        )
        .map_err(|e| format!("health scan failed: {e}"))?;
    if open > 0 {
        conn.execute(
            "UPDATE system_health_events SET detail = ?1, detected_at = ?2
             WHERE component = ?3 AND resolved_at IS NULL",
            params![detail, now, component],
        )
        .map_err(|e| format!("health refresh failed: {e}"))?;
    } else {
        conn.execute(
            "INSERT INTO system_health_events (id, component, status, detail, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![uuid::Uuid::new_v4().to_string(), component, status, detail, now],
        )
        .map_err(|e| format!("health insert failed: {e}"))?;
    }
    Ok(())
}

fn read_event(row: &rusqlite::Row) -> rusqlite::Result<HealthEventView> {
    Ok(HealthEventView {
        id: row.get(0)?,
        component: row.get(1)?,
        status: row.get(2)?,
        detail: row.get(3)?,
        detected_at: row.get(4)?,
        acknowledged_by: row.get(5)?,
        acknowledged_at: row.get(6)?,
        resolved_at: row.get(7)?,
    })
}

fn open_events(conn: &Connection) -> Result<Vec<HealthEventView>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT e.id, e.component, e.status, e.detail, e.detected_at,
                    u.name, e.acknowledged_at, e.resolved_at
             FROM system_health_events e
             LEFT JOIN users u ON u.id = e.acknowledged_by
             WHERE e.resolved_at IS NULL
             ORDER BY e.detected_at DESC",
        )
        .map_err(|e| format!("open alerts failed: {e}"))?;
    let rows = stmt
        .query_map([], read_event)
        .map_err(|e| format!("open alerts failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("open alerts read failed: {e}"))
}

fn recent_history(conn: &Connection, limit: i64) -> Result<Vec<HealthEventView>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT e.id, e.component, e.status, e.detail, e.detected_at,
                    u.name, e.acknowledged_at, e.resolved_at
             FROM system_health_events e
             LEFT JOIN users u ON u.id = e.acknowledged_by
             ORDER BY e.detected_at DESC LIMIT ?",
        )
        .map_err(|e| format!("health history failed: {e}"))?;
    let rows = stmt
        .query_map(params![limit], read_event)
        .map_err(|e| format!("health history failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("health history read failed: {e}"))
}

fn components_status(conn: &Connection) -> Result<Vec<ComponentHealth>, String> {
    let mut out = Vec::with_capacity(COMPONENTS.len());
    for component in COMPONENTS {
        let (status, detail, last_detected, open_count) = conn
            .query_row(
                "SELECT COALESCE((SELECT status FROM system_health_events
                                  WHERE component = ?1 AND resolved_at IS NULL
                                  ORDER BY detected_at DESC LIMIT 1), 'ok'),
                        (SELECT detail FROM system_health_events
                         WHERE component = ?1 AND resolved_at IS NULL
                         ORDER BY detected_at DESC LIMIT 1),
                        (SELECT detected_at FROM system_health_events
                         WHERE component = ?1 AND resolved_at IS NULL
                         ORDER BY detected_at DESC LIMIT 1),
                        (SELECT COUNT(*) FROM system_health_events
                         WHERE component = ?1 AND resolved_at IS NULL)",
                params![component],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?)),
            )
            .map_err(|e| format!("component health failed: {e}"))?;
        out.push(ComponentHealth {
            component: component.to_string(),
            status,
            detail,
            last_detected_at: last_detected,
            open_events: open_count,
        });
    }
    Ok(out)
}

/// Live database ping — a real failure here becomes a `database` health event.
fn ping_database(conn: &Connection) -> Result<(), String> {
    conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
        .map(|_| ())
        .map_err(|e| format!("database ping failed: {e}"))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn health_dashboard(state: State<AppState>, actor_id: String) -> Result<HealthDashboard, String> {
    // Single lock hold for ALL database queries — eliminates the triple
    // lock cycle (db -> sync_status -> anpr_status) that caused UI lag.
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, "view_system_health")?;
    if let Err(e) = ping_database(&conn) {
        let _ = record_health_event(&conn, "database", "offline", Some(&e));
    }
    let components = components_status(&conn)?;
    let open_alerts = open_events(&conn)?;
    let recent_history = recent_history(&conn, 50)?;
    // Build sync + anpr status inline (no separate lock acquisitions).
    let pg = crate::sync::pg_sync_state_impl(&conn, &*state.pg)?;
    let sheets = crate::sync::sheets_state_impl(&conn, &*state.sheets)?;
    let sync = crate::models::SyncStatusView {
        online: pg.connected,
        pg,
        sheets,
    };
    let anpr_source = crate::capture::anpr_source(&conn);
    let anpr_enabled = crate::capture::anpr_enabled(&conn);
    let anpr_pending = if anpr_source == "simulator" { state.simulator.pending() } else { 0 };
    let (last_at, last_plate) = match state.anpr_last.try_lock() {
        Ok(last) => last.as_ref().map(|(a, p)| (Some(a.clone()), Some(p.clone()))).unwrap_or((None, None)),
        Err(_) => (None, None),
    };
    let anpr = crate::models::AnprStatus {
        enabled: anpr_enabled,
        source: anpr_source,
        last_read_at: last_at,
        last_plate,
        pending_reads: anpr_pending,
    };
    Ok(HealthDashboard {
        components,
        open_alerts,
        recent_history,
        sync,
        anpr,
    })
}

#[tauri::command]
pub fn acknowledge_health_event(state: State<AppState>, actor_id: String, event_id: String) -> Result<HealthEventView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, "acknowledge_health_alerts")?;
    let n = conn
        .execute(
            "UPDATE system_health_events SET acknowledged_by = ?1, acknowledged_at = ?2
             WHERE id = ?3 AND resolved_at IS NULL",
            params![actor_id, now_iso(), event_id],
        )
        .map_err(|e| format!("acknowledge failed: {e}"))?;
    if n == 0 {
        return Err("Alert not found or already resolved.".to_string());
    }
    let event = conn
        .query_row(
            "SELECT e.id, e.component, e.status, e.detail, e.detected_at,
                    u.name, e.acknowledged_at, e.resolved_at
             FROM system_health_events e LEFT JOIN users u ON u.id = e.acknowledged_by
             WHERE e.id = ?1",
            params![event_id],
            read_event,
        )
        .map_err(|e| format!("event read failed: {e}"))?;
    Ok(event)
}

/// Per-day ANPR confidence trend for the System Monitor chart (05 §6h). Aggregates
/// the persistent read-event series into one point per active day: average
/// confidence and read volume. `from`/`to` are optional RFC3339 / `YYYY-MM-DD`
/// bounds; empty means unbounded.
#[tauri::command]
pub fn anpr_confidence_trend(
    state: State<AppState>,
    actor_id: String,
    from: Option<String>,
    to: Option<String>,
) -> Result<Vec<ConfidenceTrendPoint>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    // Reachable from two places: the System Monitor page (view_system_health)
    // and the ANPR Diagnostics sub-tab (manage_anpr_config, 09 §1).
    let ok = has_permission(&conn, &actor_id, "view_system_health")?
        || has_permission(&conn, &actor_id, "manage_anpr_config")?;
    if !ok {
        return Err("You do not have permission to perform this action.".to_string());
    }
    let from = from.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty());
    let to = to.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty());

    let mut sql = String::from(
        "SELECT date(timestamp) AS day, AVG(confidence) AS avg_conf, COUNT(*) AS reads
         FROM anpr_read_events WHERE timestamp IS NOT NULL AND timestamp != ''",
    );
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(f) = from {
        sql.push_str(" AND timestamp >= ?");
        args.push(rusqlite::types::Value::Text(f.to_string()));
    }
    if let Some(t) = to {
        // A bare `YYYY-MM-DD` upper bound must cover the whole day, so extend it
        // to end-of-day before comparing against full RFC3339 timestamps.
        let upper = if t.len() == 10 && t.as_bytes()[4] == b'-' {
            format!("{t}T23:59:59Z")
        } else {
            t.to_string()
        };
        sql.push_str(" AND timestamp <= ?");
        args.push(rusqlite::types::Value::Text(upper));
    }
    sql.push_str(" GROUP BY day HAVING day IS NOT NULL ORDER BY day ASC");

    let mut stmt = conn.prepare(&sql).map_err(|e| format!("trend query failed: {e}"))?;
    let mut out = Vec::new();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok(ConfidenceTrendPoint {
                date: r.get(0)?,
                avg_confidence: r.get::<_, Option<f64>>(1)?,
                reads: r.get(2)?,
            })
        })
        .map_err(|e| format!("trend read failed: {e}"))?;
    for r in rows {
        out.push(r.map_err(|e| format!("trend read failed: {e}"))?);
    }
    Ok(out)
}

fn has_permission(conn: &Connection, actor_id: &str, key: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM user_permissions up JOIN permissions p ON p.id = up.permission_id
             WHERE up.user_id = ?1 AND p.key = ?2",
            params![actor_id, key],
            |r| r.get(0),
        )
        .map_err(|e| format!("permission check failed: {e}"))?;
    Ok(count > 0)
}

/// Batch-delete health events so the incident history stays manageable.
/// Gated on `acknowledge_health_alerts` — only the System Monitor role
/// (not general admins) can purge incident history.
#[tauri::command]
pub fn delete_health_events(
    state: State<AppState>,
    actor_id: String,
    event_ids: Vec<String>,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, "acknowledge_health_alerts")?;
    let mut deleted: usize = 0;
    for id in &event_ids {
        deleted += conn
            .execute("DELETE FROM system_health_events WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| format!("health event delete failed: {e}"))?;
    }
    crate::db::append_audit(
        &conn,
        &actor_id,
        "deleted_health_events",
        None,
        Some(serde_json::json!({ "count": event_ids.len() })),
    )?;
    Ok(deleted as i64)
}
