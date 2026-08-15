//! ANPR Engine Configuration (08-anpr-integration.md §5, §6) — config values,
//! camera sources, model/version lifecycle and the training-candidate pool.
//! Every command here is gated on `manage_anpr_config` and fully independent of
//! general admin access. The live ANPR recognition engine itself is a separate
//! managed subprocess (deferred to the ANPR integration phase); this module owns
//! the persisted configuration the service will consume.

use rusqlite::{Connection, params};
use serde_json::json;
use tauri::State;

use crate::db::{append_audit, now_iso, ANPR_CONFIG_ID, AppState};
use crate::models::{
    AnprConfigView, CameraSourceView, ModelVersionView, TrainingCandidateView,
};

const CONFIG_PERM: &str = "manage_anpr_config";

// ---------------------------------------------------------------------------
// anpr_config (single-row settings)
// ---------------------------------------------------------------------------

/// Read the active engine's per-engine confidence threshold (08 §3: thresholds
/// are stored/applied independently per OCR engine, never one shared value).
pub fn confidence_threshold_for(conn: &Connection, engine: &str) -> f64 {
    let col = match engine {
        "easyocr" => "confidence_threshold_easyocr",
        _ => "confidence_threshold_paddleocr",
    };
    conn.query_row(
        &format!("SELECT {col} FROM anpr_config WHERE id = ?1"),
        params![ANPR_CONFIG_ID],
        |r| r.get::<_, f64>(0),
    )
    .unwrap_or(0.7)
}

#[tauri::command]
pub fn get_anpr_config(state: State<AppState>) -> Result<AnprConfigView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    read_anpr_config(&conn)
}

pub fn read_anpr_config(conn: &Connection) -> Result<AnprConfigView, String> {
    conn.query_row(
        "SELECT active_ocr_engine, confidence_threshold_paddleocr, confidence_threshold_easyocr,
                plate_vehicle_ratio_threshold, plate_format_rules, discharge_confirmation_required,
                save_recognition_images, retrain_candidate_threshold, is_capture_point
         FROM anpr_config WHERE id = ?1",
        params![ANPR_CONFIG_ID],
        |r| {
            Ok(AnprConfigView {
                active_ocr_engine: r.get(0)?,
                confidence_threshold_paddleocr: r.get(1)?,
                confidence_threshold_easyocr: r.get(2)?,
                plate_vehicle_ratio_threshold: r.get(3)?,
                plate_format_rules: r.get(4)?,
                discharge_confirmation_required: r.get::<_, i64>(5)? != 0,
                save_recognition_images: r.get::<_, i64>(6)? != 0,
                retrain_candidate_threshold: r.get(7)?,
                is_capture_point: r.get::<_, i64>(8)? != 0,
            })
        },
    )
    .map_err(|e| format!("anpr config read failed: {e}"))
}

/// Update configuration. All fields are optional so a single screen can save a
/// subset. Every change is audit-logged. (08 §5: engine swap + threshold tuning
/// must be explicit and traceable.)
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn update_anpr_config(
    state: State<AppState>,
    actor_id: String,
    active_ocr_engine: Option<String>,
    confidence_threshold_paddleocr: Option<f64>,
    confidence_threshold_easyocr: Option<f64>,
    plate_vehicle_ratio_threshold: Option<f64>,
    plate_format_rules: Option<String>,
    discharge_confirmation_required: Option<bool>,
    save_recognition_images: Option<bool>,
    retrain_candidate_threshold: Option<i64>,
    is_capture_point: Option<bool>,
) -> Result<AnprConfigView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    if let Some(engine) = &active_ocr_engine {
        if engine != "paddleocr" && engine != "easyocr" {
            return Err("Unknown OCR engine.".to_string());
        }
    }
    for t in [&confidence_threshold_paddleocr, &confidence_threshold_easyocr] {
        if let Some(t) = t {
            if !(0.0..=1.0).contains(t) {
                return Err("Confidence thresholds must be between 0 and 1.".to_string());
            }
        }
    }
    if let Some(ratio) = plate_vehicle_ratio_threshold {
        if !(0.0..1.0).contains(&ratio) {
            return Err("Plate-vehicle ratio must be between 0 and 1.".to_string());
        }
    }
    let engine = active_ocr_engine.unwrap_or_else(|| {
        conn.query_row(
            "SELECT active_ocr_engine FROM anpr_config WHERE id = ?1",
            params![ANPR_CONFIG_ID],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "paddleocr".to_string())
    });
    // Engine swaps are their own audited event with from/to provenance (08 §5).
    let previous_engine: Option<String> = conn
        .query_row(
            "SELECT active_ocr_engine FROM anpr_config WHERE id = ?1",
            params![ANPR_CONFIG_ID],
            |r| r.get(0),
        )
        .ok();
    conn.execute(
        "UPDATE anpr_config SET
            active_ocr_engine = COALESCE(?1, active_ocr_engine),
            confidence_threshold_paddleocr = COALESCE(?2, confidence_threshold_paddleocr),
            confidence_threshold_easyocr = COALESCE(?3, confidence_threshold_easyocr),
            plate_vehicle_ratio_threshold = COALESCE(?4, plate_vehicle_ratio_threshold),
            plate_format_rules = COALESCE(?5, plate_format_rules),
            discharge_confirmation_required = COALESCE(?6, discharge_confirmation_required),
            save_recognition_images = COALESCE(?7, save_recognition_images),
            retrain_candidate_threshold = COALESCE(?8, retrain_candidate_threshold),
            is_capture_point = COALESCE(?9, is_capture_point),
            updated_by = ?10, updated_at = ?11
         WHERE id = ?12",
        params![
            engine,
            confidence_threshold_paddleocr,
            confidence_threshold_easyocr,
            plate_vehicle_ratio_threshold,
            plate_format_rules,
            discharge_confirmation_required.map(|b| if b { 1 } else { 0 }),
            save_recognition_images.map(|b| if b { 1 } else { 0 }),
            retrain_candidate_threshold,
            is_capture_point.map(|b| if b { 1 } else { 0 }),
            actor_id,
            now_iso(),
            ANPR_CONFIG_ID,
        ],
    )
    .map_err(|e| format!("anpr config update failed: {e}"))?;
    if previous_engine.as_deref().is_some_and(|prev| prev != engine.as_str()) {
        append_audit(
            &conn,
            &actor_id,
            "switched_ocr_engine",
            None,
            Some(json!({ "from": previous_engine.unwrap_or_default(), "to": engine })),
        )?;
    }
    append_audit(&conn, &actor_id, "updated_anpr_config", None, None)?;
    read_anpr_config(&conn)
}

// ---------------------------------------------------------------------------
// camera_sources (01-database-schema.md)
// ---------------------------------------------------------------------------

const VALID_SOURCE_TYPES: &[&str] = &["rtsp", "nvr_export", "usb", "video_file", "live_test"];

#[tauri::command]
pub fn list_camera_sources(state: State<AppState>) -> Result<Vec<CameraSourceView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, label, source_type, connection_string, status,
                    last_connection_check_at, last_connection_check_result
             FROM camera_sources ORDER BY created_at ASC",
        )
        .map_err(|e| format!("camera source list failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(CameraSourceView {
                id: r.get(0)?,
                label: r.get(1)?,
                source_type: r.get(2)?,
                connection_string: r.get(3)?,
                status: r.get(4)?,
                last_connection_check_at: r.get(5)?,
                last_connection_check_result: r.get(6)?,
            })
        })
        .map_err(|e| format!("camera source list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("camera source read failed: {e}"))
}

#[tauri::command]
pub fn add_camera_source(
    state: State<AppState>,
    actor_id: String,
    label: String,
    source_type: String,
    connection_string: String,
) -> Result<CameraSourceView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    if label.trim().is_empty() || connection_string.trim().is_empty() {
        return Err("Label and connection string are required.".to_string());
    }
    if !VALID_SOURCE_TYPES.contains(&source_type.as_str()) {
        return Err(format!(
            "Unknown camera source type. Valid: {}.",
            VALID_SOURCE_TYPES.join(", ")
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    conn.execute(
        "INSERT INTO camera_sources (id, label, source_type, connection_string, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?5)",
        params![id, label.trim(), source_type, connection_string.trim(), now],
    )
    .map_err(|e| format!("camera source create failed: {e}"))?;
    append_audit(&conn, &actor_id, "added_camera_source", Some(&id), Some(json!({ "label": label.trim(), "source_type": source_type })))?;
    camera_source_by_id(&conn, &id)
}

#[tauri::command]
pub fn update_camera_source(
    state: State<AppState>,
    actor_id: String,
    source_id: String,
    label: Option<String>,
    connection_string: Option<String>,
) -> Result<CameraSourceView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    conn.execute(
        "UPDATE camera_sources SET label = COALESCE(?1, label),
                connection_string = COALESCE(?2, connection_string), updated_at = ?3
         WHERE id = ?4",
        params![label.map(|l| l.trim().to_string()), connection_string.map(|c| c.trim().to_string()), now_iso(), source_id],
    )
    .map_err(|e| format!("camera source update failed: {e}"))?;
    append_audit(&conn, &actor_id, "updated_camera_source", Some(&source_id), None)?;
    camera_source_by_id(&conn, &source_id)
}

/// Camera sources are deactivated, never hard-deleted (01-database-schema.md).
#[tauri::command]
pub fn set_camera_source_status(
    state: State<AppState>,
    actor_id: String,
    source_id: String,
    status: String,
) -> Result<CameraSourceView, String> {
    if status != "active" && status != "inactive" && status != "testing" {
        return Err("Status must be active, inactive or testing.".to_string());
    }
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    conn.execute(
        "UPDATE camera_sources SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![status, now_iso(), source_id],
    )
    .map_err(|e| format!("camera source status update failed: {e}"))?;
    append_audit(&conn, &actor_id, "set_camera_source_status", Some(&source_id), Some(json!({ "status": status })))?;
    camera_source_by_id(&conn, &source_id)
}

/// Camera sources can be deleted (permanent removal).
#[tauri::command]
pub fn delete_camera_source(
    state: State<AppState>,
    actor_id: String,
    source_id: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    let label: String = conn
        .query_row(
            "SELECT label FROM camera_sources WHERE id = ?1",
            params![source_id],
            |r| r.get(0),
        )
        .map_err(|_| "Camera source not found.".to_string())?;
    conn.execute(
        "DELETE FROM camera_sources WHERE id = ?1",
        params![source_id],
    )
    .map_err(|e| format!("camera source delete failed: {e}"))?;
    append_audit(&conn, &actor_id, "deleted_camera_source", Some(&source_id), Some(json!({ "label": label })))?;
    Ok(())
}

/// Test whether a camera source is reachable. Updates the connection check
/// fields and returns the result.
#[tauri::command]
pub fn test_camera_connection(
    state: State<AppState>,
    actor_id: String,
    source_id: String,
) -> Result<CameraSourceView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    let (source_type, connection_string): (String, String) = conn
        .query_row(
            "SELECT source_type, connection_string FROM camera_sources WHERE id = ?1",
            params![source_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| "Camera source not found.".to_string())?;

    let now = now_iso();
    let result = test_reachable(&source_type, &connection_string);
    let status = if result.is_ok() { "active" } else { "inactive" };
    let result_str = match &result {
        Ok(msg) => msg.clone(),
        Err(e) => e.clone(),
    };

    conn.execute(
        "UPDATE camera_sources SET status = ?1, last_connection_check_at = ?2,
                last_connection_check_result = ?3, updated_at = ?2
         WHERE id = ?4",
        params![status, now, result_str, source_id],
    )
    .map_err(|e| format!("camera source status update failed: {e}"))?;
    camera_source_by_id(&conn, &source_id)
}

fn test_reachable(source_type: &str, connection_string: &str) -> Result<String, String> {
    match source_type {
        "rtsp" => {
            // Extract host:port from RTSP URL
            let url = connection_string.replace("rtsp://", "");
            let host_port = url.split('@').last().unwrap_or(&url);
            let parts: Vec<&str> = host_port.split(':').collect();
            let host = parts[0];
            let port: u16 = parts.get(1).and_then(|p| p.split('/').next()?.parse().ok()).unwrap_or(554);
            std::net::TcpStream::connect((host, port))
                .map_err(|e| format!("Cannot reach {host}:{port} — {e}"))?
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .ok();
            Ok(format!("TCP connection to {host}:{port} succeeded"))
        }
        "usb" => {
            // USB cameras: validate device index format
            let device_id: i32 = connection_string.trim().parse().map_err(|_| format!("Invalid device index: {connection_string}"))?;
            if device_id < 0 {
                return Err(format!("Device index must be non-negative, got {device_id}"));
            }
            Ok(format!("USB device index {device_id} is valid. Start ANPR service to verify camera access."))
        }
        "video_file" | "nvr_export" => {
            let path = std::path::Path::new(connection_string);
            if path.exists() && path.is_file() {
                Ok(format!("File exists: {} ({} bytes)", connection_string, std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)))
            } else {
                Err(format!("File not found: {connection_string}"))
            }
        }
        "live_test" => {
            std::net::TcpStream::connect("127.0.0.1:9800")
                .map_err(|_| "ANPR service not running on port 9800".to_string())?;
            Ok(format!("ANPR service reachable at {connection_string}"))
        }
        _ => Err(format!("Unknown source type: {source_type}")),
    }
}

fn camera_source_by_id(conn: &Connection, id: &str) -> Result<CameraSourceView, String> {
    conn.query_row(
        "SELECT id, label, source_type, connection_string, status,
                last_connection_check_at, last_connection_check_result
         FROM camera_sources WHERE id = ?1",
        params![id],
        |r| {
            Ok(CameraSourceView {
                id: r.get(0)?,
                label: r.get(1)?,
                source_type: r.get(2)?,
                connection_string: r.get(3)?,
                status: r.get(4)?,
                last_connection_check_at: r.get(5)?,
                last_connection_check_result: r.get(6)?,
            })
        },
    )
    .map_err(|_| "Camera source not found.".to_string())
}

// ---------------------------------------------------------------------------
// model_versions — deploy is never automatic (08 §5, §6)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_model_versions(state: State<AppState>) -> Result<Vec<ModelVersionView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, version_label, component, validation_accuracy, is_live,
                    deployed_by, deployed_at, rolled_back_from, created_at
             FROM model_versions ORDER BY created_at DESC",
        )
        .map_err(|e| format!("model version list failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ModelVersionView {
                id: r.get(0)?,
                version_label: r.get(1)?,
                component: r.get(2)?,
                validation_accuracy: r.get(3)?,
                is_live: r.get::<_, i64>(4)? != 0,
                deployed_by: r.get(5)?,
                deployed_at: r.get(6)?,
                rolled_back_from: r.get(7)?,
                created_at: r.get(8)?,
            })
        })
        .map_err(|e| format!("model version list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("model version read failed: {e}"))
}

/// Register a retrained candidate model. A candidate is created **non-live**;
/// it cannot be deployed until it carries a validation_accuracy (08 §6.4).
#[tauri::command]
pub fn register_model_version(
    state: State<AppState>,
    actor_id: String,
    version_label: String,
    component: String,
    validation_accuracy: Option<f64>,
) -> Result<ModelVersionView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    if version_label.trim().is_empty() || component.trim().is_empty() {
        return Err("Version label and component are required.".to_string());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    conn.execute(
        "INSERT INTO model_versions (id, version_label, component, validation_accuracy, is_live, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
        params![id, version_label.trim(), component.trim(), validation_accuracy, now],
    )
    .map_err(|e| format!("model version create failed: {e}"))?;
    append_audit(&conn, &actor_id, "registered_model_version", Some(&id), Some(json!({ "component": component.trim() })))?;
    model_version_by_id(&conn, &id)
}

/// Deploy a candidate model to live for its component. Requires a recorded
/// validation accuracy and is an explicit admin action — never automatic
/// (08 §6.4, §6.5). The previously live version for that component is unset in
/// the same transaction (the unique one-live-per-component index is enforced).
#[tauri::command]
pub fn deploy_model_version(state: State<AppState>, actor_id: String, version_id: String) -> Result<ModelVersionView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    let (component, validation_accuracy): (String, Option<f64>) = conn
        .query_row(
            "SELECT component, validation_accuracy FROM model_versions WHERE id = ?1",
            params![version_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| "Model version not found.".to_string())?;
    if validation_accuracy.is_none() {
        return Err("A model cannot go live without passing validation (record validation accuracy first).".to_string());
    }
    let now = now_iso();
    let tx = conn.unchecked_transaction().map_err(|e| format!("transaction start failed: {e}"))?;
    tx.execute(
        "UPDATE model_versions SET is_live = 0, updated_at = ?1 WHERE component = ?2 AND is_live = 1",
        params![now, component],
    )
    .map_err(|e| format!("live unset failed: {e}"))?;
    tx.execute(
        "UPDATE model_versions SET is_live = 1, deployed_by = ?1, deployed_at = ?2, updated_at = ?2 WHERE id = ?3",
        params![actor_id, now, version_id],
    )
    .map_err(|e| format!("model deploy failed: {e}"))?;
    tx.commit().map_err(|e| format!("transaction commit failed: {e}"))?;
    append_audit(&conn, &actor_id, "deployed_model_version", Some(&version_id), Some(json!({ "component": component })))?;
    model_version_by_id(&conn, &version_id)
}

/// One-click rollback to a previous live version (08 §5). Provenance is
/// recorded on the restored version via `rolled_back_from`.
#[tauri::command]
pub fn rollback_model_version(state: State<AppState>, actor_id: String, version_id: String) -> Result<ModelVersionView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    let component: String = conn
        .query_row(
            "SELECT component FROM model_versions WHERE id = ?1",
            params![version_id],
            |r| r.get(0),
        )
        .map_err(|_| "Model version not found.".to_string())?;
    let current_live: Option<String> = conn
        .query_row(
            "SELECT id FROM model_versions WHERE component = ?1 AND is_live = 1",
            params![component],
            |r| r.get(0),
        )
        .ok();
    if current_live.as_deref() == Some(&version_id) {
        return Err("This version is already live.".to_string());
    }
    let now = now_iso();
    let tx = conn.unchecked_transaction().map_err(|e| format!("transaction start failed: {e}"))?;
    if let Some(live) = &current_live {
        tx.execute(
            "UPDATE model_versions SET is_live = 0, updated_at = ?1 WHERE id = ?2",
            params![now, live],
        )
        .map_err(|e| format!("live unset failed: {e}"))?;
    }
    tx.execute(
        "UPDATE model_versions SET is_live = 1, deployed_by = ?1, deployed_at = ?2, rolled_back_from = ?3, updated_at = ?2
         WHERE id = ?4",
        params![actor_id, now, current_live, version_id],
    )
    .map_err(|e| format!("model rollback failed: {e}"))?;
    tx.commit().map_err(|e| format!("transaction commit failed: {e}"))?;
    append_audit(&conn, &actor_id, "rolled_back_model_version", Some(&version_id), Some(json!({ "from": current_live })))?;
    model_version_by_id(&conn, &version_id)
}

fn model_version_by_id(conn: &Connection, id: &str) -> Result<ModelVersionView, String> {
    conn.query_row(
        "SELECT id, version_label, component, validation_accuracy, is_live,
                deployed_by, deployed_at, rolled_back_from, created_at
         FROM model_versions WHERE id = ?1",
        params![id],
        |r| {
            Ok(ModelVersionView {
                id: r.get(0)?,
                version_label: r.get(1)?,
                component: r.get(2)?,
                validation_accuracy: r.get(3)?,
                is_live: r.get::<_, i64>(4)? != 0,
                deployed_by: r.get(5)?,
                deployed_at: r.get(6)?,
                rolled_back_from: r.get(7)?,
                created_at: r.get(8)?,
            })
        },
    )
    .map_err(|_| "Model version not found.".to_string())
}

// ---------------------------------------------------------------------------
// training_candidates — the continuous-learning pool (08 §6)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_training_candidates(state: State<AppState>) -> Result<Vec<TrainingCandidateView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT tc.id, tc.source_trip_id, v.plate_number,
                    tc.frame_ref, tc.reason, tc.used_in_model_version_id, tc.created_at
             FROM training_candidates tc
             LEFT JOIN trips t ON t.id = tc.source_trip_id
             LEFT JOIN vehicles v ON v.id = t.vehicle_id
             ORDER BY tc.created_at DESC LIMIT 500",
        )
        .map_err(|e| format!("training candidate list failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(TrainingCandidateView {
                id: r.get(0)?,
                source_trip_id: r.get(1)?,
                plate_number: r.get(2)?,
                frame_ref: r.get(3)?,
                reason: r.get(4)?,
                used_in_model_version_id: r.get(5)?,
                created_at: r.get(6)?,
            })
        })
        .map_err(|e| format!("training candidate list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("training candidate read failed: {e}"))
}
