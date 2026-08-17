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
pub mod seeds;
pub mod sync;

use std::sync::atomic::Ordering;

use tauri::{Emitter, Manager};

use crate::capture::AnprSource;
use db::AppState;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let state = db::init_state(app.handle())?;
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
            capture::simulate_read,
            capture::manual_entry,
            capture::approve_trip,
            capture::update_trip_fields,
            capture::list_today_trips,
            capture::search_trips,
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
            anpr::get_anpr_config,
            anpr::update_anpr_config,
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

/// Background poller for the ANPR source (02-architecture.md §4). Runs
/// independently of the UI; an unreachable/unconfigured source degrades
/// gracefully and never blocks the rest of the app.
fn spawn_anpr_poller(app: &tauri::AppHandle, state: &AppState) {
    let handle = app.clone();
    let running = state.running.clone();
    std::thread::spawn(move || {
        let http = capture::HttpSource::new();
        while running.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let Some(st) = handle.try_state::<AppState>() else {
                continue;
            };
            let source_name = {
                let Ok(conn) = st.db.lock() else { continue };
                if !capture::anpr_enabled(&conn) {
                    continue;
                }
                capture::anpr_source(&conn)
            };

            // Surface ANPR service outages to System Monitor immediately
            // (08 §3): an unreachable HTTP source becomes a health event and is
            // cleared as soon as the service answers again.
            if source_name == "http" {
                let Ok(conn) = st.db.lock() else { continue };
                if http.reachable() {
                    let _ = monitor::record_health_event(&conn, "anpr_service", "ok", None);
                } else {
                    let _ = monitor::record_health_event(
                        &conn,
                        "anpr_service",
                        "offline",
                        Some("ANPR service unreachable at 127.0.0.1:9800"),
                    );
                }
            }

            let read = if source_name == "http" { http.poll() } else { st.simulator.poll() };
            let Some(read) = read else { continue };

            if let Ok(mut last) = st.anpr_last.lock() {
                *last = Some((read.timestamp.clone(), read.plate.clone()));
            }
            let officer = st.session.lock().ok().and_then(|s| s.as_ref().map(|s| s.user_id.clone()));
            let conn = match st.db.lock() {
                Ok(c) => c,
                Err(_) => continue,
            };
            let _ = capture::ingest_read(&conn, officer, &read, "auto", &st.frames_dir);
            // A successful read is itself evidence the pipeline is healthy, and
            // every read feeds the confidence-over-time series (05 §6h).
            let _ = capture::record_read_event(&conn, &read, &source_name, "captured");
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
