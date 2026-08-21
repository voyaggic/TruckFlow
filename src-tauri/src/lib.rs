pub mod anpr;
pub mod archive;
pub mod auth;
pub mod capture;
pub mod commands;
pub mod db;
pub mod evidence;
pub mod models;
pub mod monitor;
pub mod reference;
pub mod reporting;
pub mod sync;

use std::sync::atomic::Ordering;

use rusqlite::params;
use tauri::{Emitter, Manager};

use crate::capture::AnprSource;
use db::AppState;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let state = db::init_state(app.handle())?;
            // Auto-start ANPR service in background (non-blocking)
            {
                let state_handle = app.handle().clone();
                std::thread::spawn(move || {
                    // Small delay to let the app finish rendering first
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let Some(st) = state_handle.try_state::<AppState>() else {
                        return;
                    };
                    match auto_start_anpr(&st) {
                        Ok(()) => println!("[ANPR] Auto-started successfully"),
                        Err(e) => println!("[ANPR] Auto-start skipped: {e}"),
                    }
                });
            }
            spawn_anpr_poller(app.handle(), &state);
            spawn_sync_poller(app.handle(), &state);
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_status,
            commands::create_first_admin,
            commands::login_password,
            commands::logout,
            commands::get_current_user,
            commands::get_user_permissions,
            commands::list_permissions,
            commands::list_role_presets,
            commands::list_users,
            commands::create_user,
            commands::set_user_permissions,
            commands::complete_auth_upgrade,
            commands::change_own_credential,
            commands::set_user_theme,
            commands::set_user_status,
            commands::delete_user,
            commands::restore_user,
            commands::purge_user,
            commands::reset_user_password,
            commands::recover_admin_password,
            commands::check_recovery_code,
            commands::create_password_reset_request,
            commands::list_password_reset_requests,
            commands::dismiss_password_reset_request,
            commands::get_recovery_code,
            commands::regenerate_recovery_code,
            commands::get_pending_upgrade,
            commands::validate_password_strength,
            reference::list_companies,
            reference::create_company,
            reference::update_company,
            reference::set_company_status,
            reference::delete_company,
            reference::list_drivers,
            reference::create_driver,
            reference::update_driver,
            reference::set_driver_status,
            reference::delete_driver,
            reference::list_vehicles,
            reference::create_vehicle,
            reference::update_vehicle,
            reference::set_vehicle_status,
            reference::delete_vehicle,
            reference::list_field_definitions,
            reference::create_field_definition,
            reference::update_field_definition,
            reference::delete_field_definition,
            reference::reference_export,
            reference::reference_import,
            reference::reference_export_combined,
            reference::reference_import_preview,
            reference::reference_import_combined,
            reference::list_entity_labels,
            reference::set_entity_label,
            reference::list_reference_entities,
            reference::create_reference_entity,
            reference::rename_reference_entity,
            reference::delete_reference_entity,
            reference::list_entity_records,
            reference::create_entity_record,
            reference::update_entity_record,
            reference::delete_entity_record,
            capture::simulate_read,
            capture::manual_entry,
            capture::approve_trip,
            capture::update_trip_fields,
            capture::list_today_trips,
            capture::search_trips,
            capture::export_today_csv,
            capture::archive_trip,
            capture::clear_today_trips,
            capture::list_queued,
            capture::get_capture_settings,
            capture::set_capture_settings,
            capture::anpr_status,
            capture::simulator_push_reads,
            capture::resolve_queued_existing,
            capture::resolve_queued_new,
            capture::discard_trip,
            capture::decline_trip,
            capture::list_declined,
            capture::purge_declined,
            capture::classify_discharge,
            capture::trip_frames,
            capture::list_detection_images,
            capture::delete_detection_frames,
            capture::load_detection_image,
            capture::write_anpr_config,
            capture::start_anpr_service,
            capture::stop_anpr_service,
            anpr::get_anpr_config,
            anpr::update_anpr_config,
            anpr::get_anpr_service_url,
            anpr::list_camera_sources,
            anpr::add_camera_source,
            anpr::update_camera_source,
            anpr::set_camera_source_status,
            anpr::delete_camera_source,
            anpr::test_camera_connection,
            anpr::list_model_versions,
            anpr::register_model_version,
            anpr::deploy_model_version,
            anpr::rollback_model_version,
            anpr::list_training_candidates,
            anpr::add_training_candidate,
            anpr::approve_training_candidate,
            anpr::reject_training_candidate,
            anpr::approve_all_training_candidates,
            anpr::list_anpr_credentials,
            anpr::set_anpr_credential,
            anpr::delete_anpr_credential,
            anpr::anpr_diagnostics,
            anpr::get_machine_info,
            anpr::set_anpr_machine,
            anpr::get_anpr_machine,
            anpr::check_machine_match,
            sync::sync_status,
            sync::sync_now_pg,
            sync::connect_google_sheets,
            sync::disconnect_google_sheets,
            sync::set_google_sheets_frequency,
            sync::sync_now_sheets,
            sync::simulate_connectivity,
            sync::configure_postgres,
            sync::disconnect_postgres,
            sync::configure_google_sheets,
            sync::set_sheets_retention,
            sync::set_trip_retention,
            sync::clear_exported_trips,
            sync::get_sheet_column_mapping,
            sync::set_sheet_column_mapping,
            reporting::report_dashboard,
            reporting::report_trips_drill,
            reporting::report_export,
            reporting::report_export_csv,
            reporting::report_export_xlsx,
            archive::list_recent_trips,
            archive::list_archived_trips,
            archive::soft_delete_trips,
            archive::restore_trips,
            archive::hard_delete_trips,
            archive::purge_local_trips,
            reporting::list_audit_log,
            reporting::list_audit_actions_command,
            reporting::officer_activity,
            reporting::delete_audit_entries,
            monitor::health_dashboard,
            monitor::acknowledge_health_event,
            monitor::anpr_confidence_trend,
            monitor::delete_health_events,
            commands::update_own_profile,
            commands::set_profile_photo,
            commands::get_profile_photo,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Find the anpr-service directory by trying multiple known locations.
pub fn find_anpr_dir() -> std::path::PathBuf {
    // Try current dir (dev mode from project root)
    if let Ok(dir) = std::env::current_dir() {
        let p = dir.join("anpr-service");
        if p.exists() { return p; }
    }
    // Try relative to exe
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let p = parent.join("anpr-service");
            if p.exists() { return p; }
        }
    }
    // Try next to the source directory during development
    if let Ok(dir) = std::env::current_dir() {
        let p = dir.join("src-tauri").join("anpr-service");
        if p.exists() { return p; }
    }
    std::path::PathBuf::from("anpr-service") // fallback — will error later
}

/// Auto-start the ANPR service on app launch. Finds the first active camera,
/// writes config.json, spawns the Python process, and sets anpr_source=http.
/// Only auto-starts if the current machine matches the designated machine
/// (or no machine is designated).
fn auto_start_anpr(state: &AppState) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    // Check if this machine is configured as a capture point
    let is_capture: i64 = conn
        .query_row(
            "SELECT COALESCE(is_capture_point, 0) FROM anpr_config LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if is_capture == 0 {
        return Err("This machine is not a capture point. Enable it in ANPR Settings.".to_string());
    }

    // Check machine fingerprint: only auto-start if current machine matches
    // the designated machine (or no machine is designated)
    let designated: Option<String> = conn
        .query_row(
            "SELECT designated_machine_id FROM anpr_config WHERE id = ?1",
            params![crate::db::ANPR_CONFIG_ID],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    drop(conn);

    if let Some(designated_id) = designated {
        let current_info = anpr::get_machine_info().unwrap_or_else(|_| anpr::MachineInfo {
            hostname: "unknown".to_string(),
            mac_address: "unknown".to_string(),
            machine_id: "unknown".to_string(),
        });
        if current_info.machine_id != designated_id {
            return Err(format!(
                "This machine ({}) does not match the designated ANPR machine. Auto-start skipped.",
                current_info.hostname
            ));
        }
    }

    // Read cloud/settings from database
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let prefer_cloud: bool = conn
        .query_row(
            "SELECT COALESCE(prefer_cloud, 0) FROM anpr_config WHERE id = ?1",
            params![crate::db::ANPR_CONFIG_ID],
            |r| r.get(0),
        )
        .unwrap_or(0) != 0;
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

    // Find the first active camera source (optional — service runs in idle if none)
    let camera: Option<(String, String)> = conn
        .query_row(
            "SELECT source_type, connection_string FROM camera_sources WHERE status = 'active' LIMIT 1",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .ok();
    drop(conn);

    let (source_type, connection_string) = match camera {
        Some(c) => {
            println!("[ANPR] Auto-starting with {}: {}", c.0, c.1);
            c
        }
        None => {
            println!("[ANPR] No active camera source — starting in idle mode");
            (String::new(), String::new())
        }
    };

    // Write config.json for the ANPR service
    let anpr_dir = find_anpr_dir();
    let config_path = anpr_dir.join("config.json");
    let cfg = serde_json::json!({
        "source": connection_string,
        "source_type": source_type,
        "prefer_cloud": prefer_cloud,
        "cloud_api_url": cloud_api_url,
        "cloud_api_key": cloud_api_key,
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&cfg).unwrap())
        .map_err(|e| format!("Failed to write config.json: {e}"))?;

    // Spawn the ANPR service process
    let mut cmd = std::process::Command::new("python");
    cmd.arg("-u").arg("main.py").arg("--port").arg("9800");
    cmd.current_dir(&anpr_dir);
    let log_path = anpr_dir.join("anpr.log");
    let log_file = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let log_file2 = log_file.try_clone().map_err(|e| e.to_string())?;
    cmd.stdout(std::process::Stdio::from(log_file));
    cmd.stderr(std::process::Stdio::from(log_file2));
    let child = cmd.spawn().map_err(|e| format!("Failed to start ANPR: {e}"))?;
    let pid = child.id();
    println!("[ANPR] Service started (PID {pid})");

    // Store the child handle
    {
        let mut procs = state.anpr_processes.lock().map_err(|e| e.to_string())?;
        procs.push(child);
    }

    // Set anpr_source to http so the poller picks it up
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES ('anpr_source', 'http')\n         ON CONFLICT(key) DO UPDATE SET value = 'http'",
        [],
    ).map_err(|e| e.to_string())?;

    Ok(())
}

/// Background poller for the ANPR source (02-architecture.md §4). Runs
/// independently of the UI; an unreachable/unconfigured source degrades
/// gracefully and never blocks the rest of the app.
fn spawn_anpr_poller(app: &tauri::AppHandle, state: &AppState) {
    let handle = app.clone();
    let running = state.running.clone();
    // Read the configured service URL from DB before spawning the thread
    let service_url = {
        let Ok(conn) = state.db.lock() else { return };
        let url = capture::anpr_service_url(&conn);
        drop(conn);
        url
    };
    std::thread::spawn(move || {
        let http = capture::HttpSource::new(service_url);
        while running.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let Some(st) = handle.try_state::<AppState>() else {
                continue;
            };
            let Ok(conn) = st.db.lock() else { continue };
            if !capture::anpr_enabled(&conn) {
                drop(conn);
                continue;
            }
            drop(conn);

            // The ANPR service (port 9800) handles ALL source types internally
            // (HTTP, RTSP, USB, video file). The app always polls from it —
            // the source type doesn't matter at the poller level.
            if http.reachable() {
                let Ok(conn) = st.db.lock() else { continue };
                let _ = monitor::record_health_event(&conn, "anpr_service", "ok", None);
                drop(conn);
            } else {
                let Ok(conn) = st.db.lock() else { continue };
                let _ = monitor::record_health_event(
                    &conn,
                    "anpr_service",
                    "offline",
                    Some(&format!("ANPR service unreachable at {}", http.service_url)),
                );
                drop(conn);
                continue; // service down — skip this cycle
            }

            let Some(read) = http.poll() else { continue };

            if let Ok(mut last) = st.anpr_last.lock() {
                *last = Some((read.timestamp.clone(), read.plate.clone()));
            }
            let officer = st.session.lock().ok().and_then(|s| s.as_ref().map(|s| s.user_id.clone()));
            let conn = match st.db.lock() {
                Ok(c) => c,
                Err(_) => continue,
            };
            let _ = capture::ingest_read(&conn, officer, &read, "auto", &st.frames_dir);
            let _ = capture::record_read_event(&conn, &read, "auto", "captured");
            let _ = monitor::record_health_event(&conn, "anpr_service", "ok", None);
            drop(conn);
            let _ = handle.emit("capture-updated", ());
        }
    });
}

/// Background sync retry loop (06-data-flow.md §5). Every ~10s both targets
/// get a best-effort pass — Postgres whenever connected, Sheets on its
/// frequency schedule — so nothing waits for a manual "send" action. A failure
/// never blocks capture; rows stay pending and retry on the next pass.
fn spawn_sync_poller(app: &tauri::AppHandle, state: &AppState) {
    let handle = app.clone();
    let running = state.running.clone();
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(10));
            let Some(st) = handle.try_state::<AppState>() else {
                continue;
            };
            let Ok(conn) = st.db.lock() else {
                continue;
            };
            sync::run_background_sync(&conn, &*st.pg, &*st.sheets);
        }
    });
}
