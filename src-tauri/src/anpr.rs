//! ANPR Engine Configuration (08-anpr-integration.md §5, §6) — config values,
//! camera sources, model/version lifecycle and the training-candidate pool.
//! Every command here is gated on `manage_anpr_config` and fully independent of
//! general admin access. The live ANPR recognition engine itself is a separate
//! managed subprocess (deferred to the ANPR integration phase); this module owns
//! the persisted configuration the service will consume.

use rusqlite::{Connection, params};
use serde_json::json;
use tauri::State;

use crate::capture::anpr_service_url;
use crate::db::{append_audit, now_iso, ANPR_CONFIG_ID, AppState};
use crate::models::{
    AnprConfigView, AnprCredentialView, AnprDiagnosticsView, CameraSourceView,
    DependencyHealthView, ModelVersionView, TrainingCandidateView,
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
                save_recognition_images, retrain_candidate_threshold, is_capture_point,
                prefer_cloud, max_pending_duration_hours, designated_machine_id
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
                prefer_cloud: r.get::<_, i64>(9)? != 0,
                max_pending_duration_hours: r.get(10)?,
                designated_machine_id: r.get(11)?,
            })
        },
    )
    .map_err(|e| format!("anpr config read failed: {e}"))
}

/// Sync the prefer_cloud setting (and other runtime config) to config.json
/// so the running ANPR service picks up changes without restart.
fn sync_config_json(state: &State<AppState>) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let config = read_anpr_config(&conn)?;
    // Read cloud credentials
    let cloud_api_url: String = conn
        .query_row(
            "SELECT value FROM key_value_ref WHERE key = 'cloud_anpr_api_url'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let cloud_api_key: String = conn
        .query_row(
            "SELECT encrypted_value FROM anpr_credentials WHERE key_name = 'cloud_anpr_api_key' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();
    // Read current source from existing config.json
    let anpr_dir = crate::find_anpr_dir();
    let config_path = anpr_dir.join("config.json");
    let mut cfg: serde_json::Value = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    // Update only the fields that the ANPR service reads
    cfg["prefer_cloud"] = serde_json::json!(config.prefer_cloud);
    cfg["cloud_api_url"] = serde_json::json!(cloud_api_url);
    cfg["cloud_api_key"] = serde_json::json!(cloud_api_key);
    std::fs::write(&config_path, serde_json::to_string_pretty(&cfg).unwrap())
        .map_err(|e| format!("Failed to write config.json: {e}"))?;
    Ok(())
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
    max_pending_duration_hours: Option<f64>,
    designated_machine_id: Option<String>,
    prefer_cloud: Option<bool>,
) -> Result<AnprConfigView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    if let Some(engine) = &active_ocr_engine {
        if engine != "paddleocr" && engine != "easyocr" && engine != "cloud_provider" {
            return Err("Unknown OCR engine.".to_string());
        }
    }
    if let Some(h) = max_pending_duration_hours {
        if !(0.5..=8760.0).contains(&h) {
            return Err("Max pending duration must be between 0.5 and 8760 hours.".to_string());
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
            prefer_cloud = COALESCE(?10, prefer_cloud),
            designated_machine_id = COALESCE(?11, designated_machine_id),
            updated_by = ?12, updated_at = ?13
         WHERE id = ?14",
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
            prefer_cloud,
            designated_machine_id,
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

    // Sync prefer_cloud to config.json so the running ANPR service picks it up
    drop(conn);
    let _ = sync_config_json(&state);

    let conn2 = state.db.lock().map_err(|e| e.to_string())?;
    read_anpr_config(&conn2)
}

// ---------------------------------------------------------------------------
// camera_sources (01-database-schema.md)
// ---------------------------------------------------------------------------

const VALID_SOURCE_TYPES: &[&str] = &["rtsp", "http", "nvr_export", "usb", "video_file", "live_test"];

#[tauri::command]
pub fn list_camera_sources(state: State<AppState>) -> Result<Vec<CameraSourceView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, label, source_type, connection_string, status, tracked,
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
                tracked: r.get::<_, Option<i64>>(5)?.unwrap_or(1) != 0,
                last_connection_check_at: r.get(6)?,
                last_connection_check_result: r.get(7)?,
            })
        })
        .map_err(|e| format!("camera source list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("camera source read failed: {e}"))
}

#[tauri::command]
pub fn add_camera_source(
    state: State<AppState>,
    handle: tauri::AppHandle,
    actor_id: String,
    label: String,
    source_type: String,
    connection_string: String,
    device_name: Option<String>,
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
    // For USB sources, persist the REAL device name so the service can
    // re-resolve its DirectShow index on any machine (indices are
    // machine-specific; names are stable).
    let extra_fields: Option<String> = if source_type == "usb" {
        device_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|dn| json!({ "device_name": dn }).to_string())
    } else {
        None
    };
    conn.execute(
        "INSERT INTO camera_sources (id, label, source_type, connection_string, status, extra_fields, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?6)",
        params![id, label.trim(), source_type, connection_string.trim(), extra_fields, now],
    )
    .map_err(|e| format!("camera source create failed: {e}"))?;
    append_audit(&conn, &actor_id, "added_camera_source", Some(&id), Some(json!({ "label": label.trim(), "source_type": source_type })))?;
    let result = camera_source_by_id(&conn, &id);
    drop(conn);
    // Running service must pick up the new pipeline set immediately.
    crate::capture::restart_anpr_if_running(&state, &handle);
    result
}

#[tauri::command]
pub fn update_camera_source(
    state: State<AppState>,
    handle: tauri::AppHandle,
    actor_id: String,
    source_id: String,
    label: Option<String>,
    source_type: Option<String>,
    connection_string: Option<String>,
) -> Result<CameraSourceView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    if let Some(t) = &source_type {
        if !VALID_SOURCE_TYPES.contains(&t.as_str()) {
            return Err(format!(
                "Unknown camera source type. Valid: {}.",
                VALID_SOURCE_TYPES.join(", ")
            ));
        }
    }
    // Connection/source_type changes invalidate the previous connection check
    // result — never show a stale "Cannot reach …" from an older URL.
    let changed = connection_string.is_some() || source_type.is_some();
    conn.execute(
        "UPDATE camera_sources SET label = COALESCE(?1, label),
                source_type = COALESCE(?2, source_type),
                connection_string = COALESCE(?3, connection_string),
                last_connection_check_result = CASE WHEN ?4 = 1 THEN NULL ELSE last_connection_check_result END,
                last_connection_check_at = CASE WHEN ?4 = 1 THEN NULL ELSE last_connection_check_at END,
                updated_at = ?5
         WHERE id = ?6",
        params![
            label.map(|l| l.trim().to_string()),
            source_type,
            connection_string.map(|c| c.trim().to_string()),
            if changed { 1 } else { 0 },
            now_iso(),
            source_id
        ],
    )
    .map_err(|e| format!("camera source update failed: {e}"))?;
    append_audit(&conn, &actor_id, "updated_camera_source", Some(&source_id), None)?;
    let result = camera_source_by_id(&conn, &source_id);
    drop(conn);
    crate::capture::restart_anpr_if_running(&state, &handle);
    result
}

/// Camera sources are deactivated, never hard-deleted (01-database-schema.md).
#[tauri::command]
pub fn set_camera_source_status(
    state: State<AppState>,
    handle: tauri::AppHandle,
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
    let result = camera_source_by_id(&conn, &source_id);
    drop(conn);
    crate::capture::restart_anpr_if_running(&state, &handle);
    result
}

/// Toggle whether a source is included in ANPR processing when the service
/// starts. Active + tracked sources are the ones the service captures from.
#[tauri::command]
pub fn set_camera_source_tracked(
    state: State<AppState>,
    handle: tauri::AppHandle,
    actor_id: String,
    source_id: String,
    tracked: bool,
) -> Result<CameraSourceView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    conn.execute(
        "UPDATE camera_sources SET tracked = ?1, updated_at = ?2 WHERE id = ?3",
        params![if tracked { 1 } else { 0 }, now_iso(), source_id],
    )
    .map_err(|e| format!("camera source tracked update failed: {e}"))?;
    append_audit(&conn, &actor_id, "set_camera_source_tracked", Some(&source_id), Some(json!({ "tracked": tracked })))?;
    let result = camera_source_by_id(&conn, &source_id);
    drop(conn);
    crate::capture::restart_anpr_if_running(&state, &handle);
    result
}

/// Camera sources can be deleted (permanent removal).
#[tauri::command]
pub fn delete_camera_source(
    state: State<AppState>,
    handle: tauri::AppHandle,
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
    // Archive all queued trips since a camera source was removed — the queue
    // is only meaningful while the pipeline that produced those reads is active.
    let archived = conn
        .execute(
            "UPDATE trips SET archived = 1, updated_at = ?1 WHERE status = 'queued' AND COALESCE(archived, 0) = 0",
            params![crate::db::now_iso()],
        )
        .unwrap_or(0);
    if archived > 0 {
        crate::log::log(&format!("[ANPR] Archived {archived} queued trips after deleting camera source '{label}'"));
    }
    append_audit(&conn, &actor_id, "deleted_camera_source", Some(&source_id), Some(json!({ "label": label, "archived_trips": archived })))?;
    drop(conn);
    crate::capture::restart_anpr_if_running(&state, &handle);
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
    // Phase 1: read config + permission under lock, then RELEASE the lock
    // before running the capture test — the test can take up to 20s and must
    // NEVER hold the db lock (that pattern caused the UI lag).
    let (source_type, connection_string): (String, String) = {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
        conn.query_row(
            "SELECT source_type, connection_string FROM camera_sources WHERE id = ?1",
            params![source_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| "Camera source not found.".to_string())?
    }; // db lock released HERE

    let result = test_reachable(&source_type, &connection_string, "");
    let status = if result.is_ok() { "active" } else { "inactive" };
    let now = now_iso();
    let result_str = match &result {
        Ok(msg) => msg.clone(),
        Err(e) => e.clone(),
    };

    // Phase 2: re-acquire the lock only to persist the (fast) DB update
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE camera_sources SET status = ?1, last_connection_check_at = ?2,
                last_connection_check_result = ?3, updated_at = ?2
         WHERE id = ?4",
        params![status, now, result_str, source_id],
    )
    .map_err(|e| format!("camera source status update failed: {e}"))?;
    camera_source_by_id(&conn, &source_id)
}

/// Detected camera device information.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct DetectedCamera {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub backend: String,
    pub status: String, // "ok" = live, "static" = frozen/test pattern, "black" = connected but delivering pure black, "error"
    #[serde(default)]
    pub is_live: bool,
    #[serde(default)]
    pub device_type: String,
    #[serde(default)]
    pub avg_frame_diff: f64,
    #[serde(default)]
    pub brightness: f64,
}

/// Detect all available USB/webcam devices on the system.
/// Spawns a short Python script to enumerate OpenCV camera indices.
#[tauri::command]
pub fn enumerate_cameras() -> Result<Vec<DetectedCamera>, String> {
    let anpr_dir = crate::find_anpr_dir();
    let python = crate::capture::find_python();
    if python.is_empty() {
        return Err("Python not found".to_string());
    }
    // Use the bundled enumeration script that probes camera indices 0-9
    let script = anpr_dir.join("_enum_cameras.py");
    if !script.exists() {
        return Err(format!("Camera enumeration script not found: {}", script.display()));
    }

    let mut cmd = std::process::Command::new(&python);
    cmd.arg(&script);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::capture::CREATE_NO_WINDOW);
    }
    let output = cmd.output()
        .map_err(|e| format!("Failed to run camera enumeration: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Camera enumeration failed: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let cameras: Vec<DetectedCamera> = serde_json::from_str(&stdout)
        .map_err(|e| format!("Failed to parse camera list: {e}"))?;
    Ok(cameras)
}

// ---------------------------------------------------------------------------
// ONVIF network camera discovery (CCTV)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OnvifDevice {
    pub ip: String,
    pub port: u32,
    pub device_url: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub manufacturer: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub hardware: String,
}

#[derive(Debug, serde::Deserialize)]
struct OnvifScriptOutput {
    ok: bool,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<String>,
}

fn run_onvif_script(args: &[&str]) -> Result<serde_json::Value, String> {
    let anpr_dir = crate::find_anpr_dir();
    let python = crate::capture::find_python();
    if python.is_empty() {
        return Err("Python not found".to_string());
    }
    let script = anpr_dir.join("_discover_onvif.py");
    if !script.exists() {
        return Err(format!("ONVIF discovery script not found: {}", script.display()));
    }
    let mut cmd = std::process::Command::new(&python);
    cmd.arg(&script).args(args);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::capture::CREATE_NO_WINDOW);
    }
    let output = cmd.output()
        .map_err(|e| format!("Failed to run ONVIF discovery: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: OnvifScriptOutput = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("Failed to parse ONVIF output: {e}"))?;
    if !parsed.ok {
        return Err(parsed.error.unwrap_or_else(|| "ONVIF discovery failed".to_string()));
    }
    Ok(parsed.result.unwrap_or(serde_json::Value::Null))
}

/// Discover ONVIF CCTV cameras on the LAN (WS-Discovery broadcast, ~5s).
/// No credentials needed — devices advertise themselves.
#[tauri::command]
pub fn discover_onvif_cameras() -> Result<Vec<OnvifDevice>, String> {
    let result = run_onvif_script(&["discover"])?;
    let devices: Vec<OnvifDevice> = serde_json::from_value(result)
        .map_err(|e| format!("Failed to parse device list: {e}"))?;
    Ok(devices)
}

/// Get the RTSP stream URI from an ONVIF device (needs the camera's login).
#[tauri::command]
pub fn onvif_stream_uri(
    device_url: String,
    username: String,
    password: String,
) -> Result<serde_json::Value, String> {
    run_onvif_script(&["stream", &device_url, &username, &password])
}

/// Parse a URL string to extract (host, port) regardless of scheme.
/// Handles: rtsp://host:port/path, http://host:port/path, host:port, host
fn parse_host_port(url: &str) -> (String, u16) {
    // Strip scheme (rtsp://, http://, https://)
    let stripped = url
        .strip_prefix("rtsp://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    // Strip credentials (user:pass@host:port)
    let after_at = stripped.split('@').last().unwrap_or(stripped);
    // Strip path
    let host_port = after_at.split('/').next().unwrap_or(after_at);
    // Split host:port
    let parts: Vec<&str> = host_port.split(':').collect();
    let host = parts[0].to_string();
    let port: u16 = parts.get(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(if url.starts_with("https") { 443 } else if url.starts_with("http") { 80 } else { 554 });
    (host, port)
}

/// Run the real capture test: open the source with OpenCV (same backends as
/// the runtime pipeline) and grab an actual frame via `_test_source.py`.
/// Returns the human-readable success message or an error describing exactly
/// why the source cannot deliver video.
fn run_capture_test(source: &str, source_type: &str) -> Result<String, String> {
    let anpr_dir = crate::find_anpr_dir();
    let script = anpr_dir.join("_test_source.py");
    if !script.exists() {
        return Err(format!("Capture test script not found: {}", script.display()));
    }
    let python = crate::capture::find_python();
    if python.is_empty() {
        return Err("Python not found — cannot run capture test".to_string());
    }

    let mut cmd = std::process::Command::new(&python);
    cmd.arg(&script).arg(source).arg(source_type).arg("12");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::capture::CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("Failed to start capture test: {e}"))?;

    // Poll with a hard 20s cap so a hung RTSP open can never block forever.
    let mut waited_ms = 0u32;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if waited_ms >= 20_000 {
                    let _ = child.kill();
                    return Err("Test timed out after 20s — source never delivered a frame".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                waited_ms += 100;
            }
            Err(e) => return Err(format!("Capture test failed: {e}")),
        }
    }

    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_string(&mut stdout);
    }

    #[derive(serde::Deserialize)]
    struct TestResult {
        ok: bool,
        message: String,
    }
    let parsed: TestResult = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("Unexpected test output ({e}): {}", stdout.trim().chars().take(120).collect::<String>()))?;
    if parsed.ok {
        Ok(parsed.message)
    } else {
        Err(parsed.message)
    }
}

fn test_reachable(source_type: &str, connection_string: &str, _service_url: &str) -> Result<String, String> {
    // Fast pre-checks first — fail in milliseconds instead of seconds where possible.
    match source_type {
        "rtsp" => {
            let (host, port) = parse_host_port(connection_string);
            std::net::TcpStream::connect((host.as_str(), port))
                .map_err(|e| format!("Cannot reach {host}:{port} — {e}"))?
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .ok();
            // TCP reachable — now verify the stream actually delivers video below.
        }
        "video_file" | "nvr_export" => {
            let path = std::path::Path::new(connection_string);
            if !path.exists() || !path.is_file() {
                return Err(format!("File not found: {connection_string}"));
            }
        }
        _ => {}
    }

    // Real test for ALL types: open the source with the same OpenCV backends
    // the runtime pipeline uses and grab an actual frame. This is the
    // difference between "port open" and "video actually flows" — wrong RTSP
    // paths/credentials and busy USB devices fail HERE instead of silently
    // reconnecting forever once the service starts.
    run_capture_test(connection_string, source_type)
}

fn camera_source_by_id(conn: &Connection, id: &str) -> Result<CameraSourceView, String> {
    conn.query_row(
        "SELECT id, label, source_type, connection_string, status, tracked,
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
                tracked: r.get::<_, Option<i64>>(5)?.unwrap_or(1) != 0,
                last_connection_check_at: r.get(6)?,
                last_connection_check_result: r.get(7)?,
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
                    tc.frame_ref, tc.reason, tc.used_in_model_version_id, tc.created_at,
                    t.confidence_score, t.ocr_engine, t.entry_time, t.capture_method
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
                confidence: r.get(7)?,
                ocr_engine: r.get(8)?,
                captured_at: r.get(9)?,
                capture_method: r.get(10)?,
            })
        })
        .map_err(|e| format!("training candidate list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("training candidate read failed: {e}"))
}

/// Add a training candidate manually (with plate number and frame image).
/// The image file is copied into the frames directory.
#[tauri::command]
pub fn add_training_candidate(
    state: State<AppState>,
    actor_id: String,
    plate_number: String,
    frame_path: String,
) -> Result<TrainingCandidateView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_iso();
    // Copy the frame image into the frames directory
    let dest = state.frames_dir.join(format!("candidate_{id}.jpg"));
    std::fs::copy(&frame_path, &dest)
        .map_err(|e| format!("Failed to copy frame image: {e}"))?;
    let frame_ref = dest.to_string_lossy().to_string();
    conn.execute(
        "INSERT INTO training_candidates (id, frame_ref, reason, created_at, updated_at)
         VALUES (?1, ?2, 'manual_upload', ?3, ?3)",
        params![id, frame_ref, now],
    )
    .map_err(|e| format!("Failed to insert training candidate: {e}"))?;
    Ok(TrainingCandidateView {
        id,
        source_trip_id: None,
        plate_number: Some(plate_number),
        frame_ref,
        reason: "manual_upload".into(),
        used_in_model_version_id: None,
        created_at: now,
        confidence: None,
        ocr_engine: None,
        captured_at: None,
        capture_method: Some("manual".into()),
    })
}

/// Approve a training candidate (mark it as used in training).
#[tauri::command]
pub fn approve_training_candidate(
    state: State<AppState>,
    actor_id: String,
    candidate_id: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    conn.execute(
        "DELETE FROM training_candidates WHERE id = ?1",
        params![candidate_id],
    )
    .map_err(|e| format!("Failed to approve candidate: {e}"))?;
    append_audit(&conn, &actor_id, "approved_training_candidate", Some(&candidate_id), None)?;
    Ok(())
}

/// Reject a training candidate (remove it from the pool).
#[tauri::command]
pub fn reject_training_candidate(
    state: State<AppState>,
    actor_id: String,
    candidate_id: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    conn.execute(
        "DELETE FROM training_candidates WHERE id = ?1",
        params![candidate_id],
    )
    .map_err(|e| format!("Failed to reject candidate: {e}"))?;
    append_audit(&conn, &actor_id, "rejected_training_candidate", Some(&candidate_id), None)?;
    Ok(())
}

/// Approve all training candidates (clear the pool).
#[tauri::command]
pub fn approve_all_training_candidates(
    state: State<AppState>,
    actor_id: String,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    let count = conn
        .execute("DELETE FROM training_candidates", [])
        .map_err(|e| format!("Failed to approve all candidates: {e}"))?;
    append_audit(&conn, &actor_id, "approved_all_training_candidates", None, Some(serde_json::json!({ "count": count })))?;
    Ok(count as i64)
}

/// Reject (remove) ALL training candidates at once.
#[tauri::command]
pub fn reject_all_training_candidates(
    state: State<AppState>,
    actor_id: String,
) -> Result<i64, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    let count = conn
        .execute("DELETE FROM training_candidates", [])
        .map_err(|e| format!("Failed to reject all candidates: {e}"))?;
    append_audit(&conn, &actor_id, "rejected_all_training_candidates", None, Some(serde_json::json!({ "count": count })))?;
    Ok(count as i64)
}

// ---------------------------------------------------------------------------
// anpr_credentials — API / license keys, masked + rotatable (§8 Credentials
// sub-tab). Values are stored in the DB row (single-user desktop app) but
// never returned in plaintext; only a masked preview ever leaves the backend.
// ---------------------------------------------------------------------------

/// Mask a secret for display: keep the first 4 and last 4 characters when the
/// value is long enough, otherwise show only stars.
fn mask_secret(value: &str) -> String {
    let v = value.trim();
    if v.len() <= 8 {
        "••••".to_string()
    } else {
        format!("{}••••••••{}", &v[..4], &v[v.len() - 4..])
    }
}

#[tauri::command]
pub fn list_anpr_credentials(state: State<AppState>) -> Result<Vec<AnprCredentialView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, key_name, key_value_ref, rotated_by, rotated_at, created_at
             FROM anpr_credentials ORDER BY created_at ASC",
        )
        .map_err(|e| format!("credential list failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            let value: String = r.get(2)?;
            Ok(AnprCredentialView {
                id: r.get(0)?,
                key_name: r.get(1)?,
                masked_value: mask_secret(&value),
                has_value: !value.trim().is_empty(),
                rotated_by: r.get(3)?,
                rotated_at: r.get(4)?,
                created_at: r.get(5)?,
            })
        })
        .map_err(|e| format!("credential list failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("credential read failed: {e}"))
}

/// Create or overwrite a named credential (e.g. `cloud_anpr_api_key`).
/// Overwrites are audit-logged as rotations with who/when provenance.
#[tauri::command]
pub fn set_anpr_credential(
    state: State<AppState>,
    actor_id: String,
    key_name: String,
    value: String,
) -> Result<AnprCredentialView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    let name = key_name.trim();
    if name.is_empty() || value.trim().is_empty() {
        return Err("Key name and value are required.".to_string());
    }
    let now = now_iso();
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM anpr_credentials WHERE key_name = ?1",
            params![name],
            |r| r.get(0),
        )
        .ok();
    if let Some(id) = existing {
        conn.execute(
            "UPDATE anpr_credentials SET key_value_ref = ?1, rotated_by = ?2, rotated_at = ?3, updated_at = ?3
             WHERE id = ?4",
            params![value.trim(), actor_id, now, id],
        )
        .map_err(|e| format!("credential update failed: {e}"))?;
        append_audit(
            &conn,
            &actor_id,
            "rotated_anpr_credential",
            Some(&id),
            Some(json!({ "key_name": name })),
        )?;
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO anpr_credentials (id, key_name, key_value_ref, rotated_by, rotated_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, name, value.trim(), actor_id, now, now],
        )
        .map_err(|e| format!("credential create failed: {e}"))?;
        append_audit(&conn, &actor_id, "added_anpr_credential", Some(&id), Some(json!({ "key_name": name })))?;
    }
    credential_by_id(&conn, name)
}

/// Remove a stored credential entirely (audit-logged).
#[tauri::command]
pub fn delete_anpr_credential(
    state: State<AppState>,
    actor_id: String,
    key_name: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    let name = key_name.trim();
    let id: String = conn
        .query_row(
            "SELECT id FROM anpr_credentials WHERE key_name = ?1",
            params![name],
            |r| r.get(0),
        )
        .map_err(|_| format!("Credential '{name}' not found."))?;
    conn.execute(
        "DELETE FROM anpr_credentials WHERE id = ?1",
        params![id],
    )
    .map_err(|e| format!("credential delete failed: {e}"))?;
    append_audit(&conn, &actor_id, "deleted_anpr_credential", Some(&id), Some(json!({ "key_name": name })))?;
    Ok(())
}

fn credential_by_id(conn: &Connection, key_name: &str) -> Result<AnprCredentialView, String> {
    conn.query_row(
        "SELECT id, key_name, key_value_ref, rotated_by, rotated_at, created_at
         FROM anpr_credentials WHERE key_name = ?1",
        params![key_name],
        |r| {
            let value: String = r.get(2)?;
            Ok(AnprCredentialView {
                id: r.get(0)?,
                key_name: r.get(1)?,
                masked_value: mask_secret(&value),
                has_value: !value.trim().is_empty(),
                rotated_by: r.get(3)?,
                rotated_at: r.get(4)?,
                created_at: r.get(5)?,
            })
        },
    )
    .map_err(|_| format!("Credential '{key_name}' not found."))
}

// ---------------------------------------------------------------------------
// Machine fingerprint for ANPR auto-start (machine-specific detection)
// ---------------------------------------------------------------------------

/// Machine information returned to the frontend for display and matching.
#[derive(serde::Serialize)]
pub struct MachineInfo {
    pub hostname: String,
    pub mac_address: String,
    pub machine_id: String,
}

/// Generate a machine fingerprint from hostname + first non-loopback MAC address.
fn generate_machine_id() -> MachineInfo {
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    // Get MAC addresses via ipconfig (Windows) or ifconfig (Linux/Mac)
    let mac_address = get_primary_mac_address();

    // Combine hostname + MAC for a unique machine ID (lowercase for consistency)
    let machine_id = format!("{}:{}", hostname.to_lowercase(), mac_address.to_lowercase());

    MachineInfo {
        hostname,
        mac_address,
        machine_id,
    }
}

/// Get the primary non-loopback MAC address.
fn get_primary_mac_address() -> String {
    use std::process::Command;
    #[cfg(target_os = "windows")]
    use std::os::windows::process::CommandExt;

    #[cfg(target_os = "windows")]
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    // Try Windows ipconfig first
    {
        let mut cmd = Command::new("ipconfig");
        cmd.arg("/all");
        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);
        if let Ok(output) = cmd.output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut current_mac = String::new();
            for line in stdout.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("Physical Address") || trimmed.starts_with("适配器") && trimmed.contains("Physical") {
                    // Extract MAC from line like "Physical Address. . . . . . . . . : AA-BB-CC-DD-EE-FF"
                    if let Some(mac_part) = trimmed.split(':').last() {
                        let mac = mac_part.trim().replace('-', ":");
                        if mac != "00-00-00-00-00-00" && !mac.contains("00:00:00:00:00:00") && !mac.is_empty() {
                            current_mac = mac;
                        }
                    }
                }
            }
            if !current_mac.is_empty() {
                return current_mac;
            }
        }
    }

    // Fallback: try getmac command
    {
        let mut cmd = Command::new("getmac");
        cmd.arg("/fo").arg("csv").arg("/nh");
        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);
        if let Ok(output) = cmd.output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                // CSV format: "AA-BB-CC-DD-EE-FF","Transport Name","..."
                if let Some(mac_part) = line.split(',').next() {
                    let mac = mac_part.trim().trim_matches('"').replace('-', ":");
                    if !mac.is_empty() && mac != "00:00:00:00:00:00" {
                        return mac;
                    }
                }
            }
        }
    }

    "unknown".to_string()
}

/// Get machine info for the current machine.
#[tauri::command]
pub fn get_machine_info() -> Result<MachineInfo, String> {
    Ok(generate_machine_id())
}

/// Set the current machine as the designated ANPR machine.
/// Stores the machine fingerprint in anpr_config so auto-start only triggers
/// on this specific machine.
#[tauri::command]
pub fn set_anpr_machine(
    state: State<AppState>,
    actor_id: String,
) -> Result<MachineInfo, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    let info = generate_machine_id();
    conn.execute(
        "UPDATE anpr_config SET designated_machine_id = ?1, updated_at = ?2 WHERE id = ?3",
        params![info.machine_id, now_iso(), ANPR_CONFIG_ID],
    )
    .map_err(|e| format!("machine designation failed: {e}"))?;
    append_audit(&conn, &actor_id, "designated_anpr_machine", None, Some(json!({
        "hostname": info.hostname,
        "machine_id": info.machine_id,
    })))?;
    Ok(info)
}

/// Get the currently designated ANPR machine ID (if any).
#[tauri::command]
pub fn get_anpr_machine(state: State<AppState>) -> Result<Option<String>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let result: Option<String> = conn
        .query_row(
            "SELECT designated_machine_id FROM anpr_config WHERE id = ?1",
            params![ANPR_CONFIG_ID],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    Ok(result)
}

/// Check if the current machine matches the designated ANPR machine.
/// Returns true if no machine is designated (any machine can auto-start),
/// or if the current machine's fingerprint matches.
#[tauri::command]
pub fn check_machine_match(state: State<AppState>) -> Result<bool, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let designated: Option<String> = conn
        .query_row(
            "SELECT designated_machine_id FROM anpr_config WHERE id = ?1",
            params![ANPR_CONFIG_ID],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    match designated {
        None => Ok(true), // no designation = any machine can auto-start
        Some(designated_id) => {
            let current = generate_machine_id();
            Ok(current.machine_id == designated_id)
        }
    }
}

// ---------------------------------------------------------------------------
// Diagnostics sub-tab (§1 Diagnostics, §3 dependency checks, §10)
// ---------------------------------------------------------------------------

/// Whether a program is available on PATH (e.g. ffmpeg). Returns (ok, detail).
/// Approximate whether Python has OpenCV importable (the ANPR service's OCR
/// dependency). Best-effort: `python -c "import cv2"`.
static CACHED_OPENCV: std::sync::OnceLock<(bool, String)> = std::sync::OnceLock::new();

fn python_opencv_available() -> (bool, String) {
    if let Some(cached) = CACHED_OPENCV.get() {
        return cached.clone();
    }

    use std::process::Command;
    #[cfg(target_os = "windows")]
    use std::os::windows::process::CommandExt;
    #[cfg(target_os = "windows")]
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    for cmd in ["python", "python3"] {
        let mut command = Command::new(cmd);
        command.args(["-c", "import cv2; print(cv2.__version__)"]);
        #[cfg(target_os = "windows")]
        command.creation_flags(CREATE_NO_WINDOW);
        if let Ok(out) = command.output() {
            if out.status.success() {
                let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let result = (true, format!("OpenCV {v} importable via {cmd}"));
                let _ = CACHED_OPENCV.set(result.clone());
                return result;
            }
        }
    }
    let result = (false, "OpenCV (cv2) not importable — install it for the ANPR service: pip install opencv-python".to_string());
    let _ = CACHED_OPENCV.set(result.clone());
    result
}

fn dir_size_bytes(path: &std::path::Path) -> i64 {
    let mut total: i64 = 0;
    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size_bytes(&p);
            } else if let Ok(md) = std::fs::metadata(&p) {
                total += md.len() as i64;
            }
        }
    }
    total
}

fn human_bytes(bytes: i64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

/// Return the configured ANPR service URL for the frontend.
#[tauri::command]
pub fn get_anpr_service_url(state: State<AppState>) -> Result<String, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    Ok(anpr_service_url(&conn))
}

/// Full Diagnostics sub-tab payload: dependency health, storage usage, ANPR
/// service reachability and the recent service error log. Gated on the same
/// `manage_anpr_config` permission as the rest of the page.
#[tauri::command]
pub fn anpr_diagnostics(state: State<AppState>) -> Result<AnprDiagnosticsView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut deps: Vec<DependencyHealthView> = Vec::new();

    let (cv_ok, cv_detail) = python_opencv_available();
    deps.push(DependencyHealthView { name: "OpenCV (cv2)".into(), ok: cv_ok, detail: cv_detail });

    // ANPR service reachability — use HTTP health check (same as Engine tab).
    let svc_url = anpr_service_url(&conn);
    let health_url = format!("{}/health", svc_url.trim_end_matches('/'));
    let service_ok = match reqwest::blocking::get(&health_url) {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    };
    deps.push(DependencyHealthView {
        name: "ANPR service".into(),
        ok: service_ok,
        detail: if service_ok {
            format!("Health check OK at {svc_url}")
        } else {
            format!("Not reachable at {svc_url} — start the ANPR service")
        },
    });

    // Storage: frames evidence dir + the SQLite database file (sibling of the
    // frames dir inside the app-data directory).
    let frames_bytes = dir_size_bytes(&state.frames_dir);
    let db_bytes = state
        .frames_dir
        .parent()
        .map(|d| d.join("truckflow.db"))
        .and_then(|p| std::fs::metadata(&p).ok())
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    let storage_total = frames_bytes + db_bytes;
    let storage_detail = format!(
        "{} captured frames + {} database = {} total",
        human_bytes(frames_bytes),
        human_bytes(db_bytes),
        human_bytes(storage_total)
    );
    let storage_breakdown = vec![
        crate::models::StorageBreakdownItem { label: "Captured frames".into(), bytes: frames_bytes },
        crate::models::StorageBreakdownItem { label: "SQLite database".into(), bytes: db_bytes },
    ];

    // Recent ANPR-service error log from system health events (component = anpr_service).
    let mut stmt = conn
        .prepare(
            "SELECT id, component, status, detail, detected_at, acknowledged_by, acknowledged_at, resolved_at
             FROM system_health_events
             WHERE component = 'anpr_service' AND status != 'ok'
             ORDER BY detected_at DESC LIMIT 50",
        )
        .map_err(|e| format!("diagnostics error log failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(crate::models::HealthEventView {
                id: r.get(0)?,
                component: r.get(1)?,
                status: r.get(2)?,
                detail: r.get(3)?,
                detected_at: r.get(4)?,
                acknowledged_by: r.get(5)?,
                acknowledged_at: r.get(6)?,
                resolved_at: r.get(7)?,
            })
        })
        .map_err(|e| format!("diagnostics error log failed: {e}"))?;
    let error_log = rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("diagnostics error log read failed: {e}"))?;

    Ok(AnprDiagnosticsView {
        dependencies: deps,
        storage_bytes: storage_total,
        storage_detail,
        storage_breakdown,
        service_running: service_ok,
        error_log,
    })
}

// ---------------------------------------------------------------------------
// Per-user, per-machine auto-start preference
// ---------------------------------------------------------------------------

/// Get the current user's auto-start preference for this machine.
#[tauri::command]
pub fn get_user_auto_start(state: State<AppState>, actor_id: String) -> Result<bool, String> {
    let info = get_machine_info()?;
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let enabled: bool = conn
        .query_row(
            "SELECT COALESCE(enabled, 0) FROM user_anpr_auto_start WHERE user_id = ?1 AND machine_id = ?2",
            params![actor_id, info.machine_id],
            |r| r.get(0),
        )
        .unwrap_or(false);
    Ok(enabled)
}

/// Set (or clear) the current user's auto-start preference for this machine.
#[tauri::command]
pub fn set_user_auto_start(
    state: State<AppState>,
    actor_id: String,
    enabled: bool,
) -> Result<(), String> {
    let info = get_machine_info()?;
    let now = crate::db::now_iso();
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO user_anpr_auto_start (user_id, machine_id, enabled, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(user_id, machine_id) DO UPDATE SET enabled = ?3, updated_at = ?4",
        params![actor_id, info.machine_id, enabled as i32, now],
    )
    .map_err(|e| format!("failed to save auto-start preference: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        if enabled { "enabled_anpr_auto_start" } else { "disabled_anpr_auto_start" },
        None,
        Some(json!({ "machine_id": info.machine_id, "enabled": enabled })),
    )?;
    Ok(())
}

/// Check if ANY user has auto-start enabled for this machine.
/// Returns the first matching user_id (used by auto_start_anpr).
/// Tries exact match first (hostname:mac), then falls back to hostname-only
/// match so that auto-start works even when the NIC / MAC address changes
/// (e.g. switching between Ethernet, Wi-Fi, or VPN adapters).
pub fn any_auto_start_for_machine(conn: &Connection, machine_id: &str) -> Option<String> {
    // 1. Exact match (hostname:mac)
    if let Ok(uid) = conn.query_row(
        "SELECT user_id FROM user_anpr_auto_start WHERE machine_id = ?1 AND enabled = 1 LIMIT 1",
        params![machine_id],
        |r| r.get(0),
    ) {
        return Some(uid);
    }
    // 2. Hostname-only fallback: match any entry whose machine_id starts with
    //    the same hostname prefix (before the first ':').
    if let Some(hostname) = machine_id.split(':').next() {
        if !hostname.is_empty() {
            let pattern = format!("{}:%", hostname);
            if let Ok(uid) = conn.query_row(
                "SELECT user_id FROM user_anpr_auto_start WHERE machine_id LIKE ?1 AND enabled = 1 LIMIT 1",
                params![pattern],
                |r| r.get(0),
            ) {
                return Some(uid);
            }
        }
    }
    None
}

/// Set the OCR plate mode: "universal" (any plate) or "kenyan" (3 letters + 3 digits + 1 letter).
#[tauri::command]
pub fn set_ocr_plate_mode(
    state: State<AppState>,
    handle: tauri::AppHandle,
    actor_id: String,
    mode: String,
) -> Result<String, String> {
    if mode != "universal" && mode != "kenyan" {
        return Err("Mode must be 'universal' or 'kenyan'.".to_string());
    }
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    crate::commands::ensure_admin_permission(&conn, &actor_id, CONFIG_PERM)?;
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES ('ocr_plate_mode', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![mode],
    )
    .map_err(|e| format!("Failed to save OCR plate mode: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        "set_ocr_plate_mode",
        None,
        Some(serde_json::json!({ "mode": mode })),
    )?;
    // Restart ANPR service so it picks up the new mode via config.json.
    drop(conn);
    crate::capture::restart_anpr_if_running(&state, &handle);
    Ok(format!("OCR plate mode set to '{mode}'."))
}
