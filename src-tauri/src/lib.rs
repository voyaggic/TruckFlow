pub mod anpr;
pub mod archive;
pub mod auth;
pub mod capture;
pub mod commands;
pub mod db;
pub mod evidence;
pub mod log;
pub mod models;
pub mod monitor;
pub mod reference;
pub mod reporting;
pub mod sync;

use std::sync::{Arc, Mutex, atomic::Ordering};

use rusqlite::{params, Connection};
use tauri::{Emitter, Manager, State};

use crate::capture::AnprSource;
use db::AppState;
use sync::PostgresAdapter;

pub fn run() {
    // Initialize the async log system FIRST — all subsequent log() calls
    // are non-blocking (~0µs) via a channel to a background writer thread.
    crate::log::init();

    // Install a panic hook that logs the panic to a file before the app dies.
    // This is critical for diagnosing crashes — without it, the app just closes silently.
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let loc = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_default();
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<dyn Any>".to_string()
        };
        let backtrace = std::backtrace::Backtrace::force_capture();
        let entry = format!("[PANIC] thread={name} loc={loc} msg={msg}\n{backtrace}\n");
        // Log to stderr (visible in Tauri dev console)
        eprintln!("{entry}");
        // Also write to crash.log file next to the exe
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("crash.log") {
            use std::io::Write;
            let _ = f.write_all(entry.as_bytes());
        }
    }));
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let (state, sync_rx) = db::init_state(app.handle())?;
            // Auto-start ANPR service in background (non-blocking)
            {
                let state_handle = app.handle().clone();
                std::thread::spawn(move || {
                    // Delay to let the app finish rendering first — 1.5s avoids
                    // competing with the main thread for DB locks during startup.
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    let Some(st) = state_handle.try_state::<AppState>() else {
                        return;
                    };
                    match auto_start_anpr(&st) {
                        Ok(()) => crate::log::log(&format!("[ANPR] Auto-started successfully")),
                        Err(e) => crate::log::log(&format!("[ANPR] Auto-start skipped: {e}")),
                    }
                });
            }
            spawn_anpr_poller(app.handle(), &state);
            spawn_sync_poller(app.handle(), &state, sync_rx);
            spawn_keepalive_pinger(&state);
            spawn_heartbeat(&state);
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_status,
            get_app_setting,
            commands::create_first_admin,
            commands::create_company_and_admin,
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
            capture::anpr_service_status,
            capture::simulator_push_reads,
            capture::resolve_queued_existing,
            capture::resolve_queued_new,
            capture::resolve_queued_manual,
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
            capture::check_anpr_ready,
            capture::ensure_anpr_setup,
            anpr::get_anpr_config,
            anpr::update_anpr_config,
            anpr::get_anpr_service_url,
            anpr::list_camera_sources,
            anpr::add_camera_source,
            anpr::update_camera_source,
            anpr::set_camera_source_status,
            anpr::set_camera_source_tracked,
            anpr::delete_camera_source,
            anpr::test_camera_connection,
            anpr::enumerate_cameras,
            anpr::discover_onvif_cameras,
            anpr::onvif_stream_uri,
            anpr::list_model_versions,
            anpr::register_model_version,
            anpr::deploy_model_version,
            anpr::rollback_model_version,
            anpr::list_training_candidates,
            anpr::add_training_candidate,
            anpr::approve_training_candidate,
            anpr::reject_training_candidate,
            anpr::approve_all_training_candidates,
            anpr::reject_all_training_candidates,
            anpr::list_anpr_credentials,
            anpr::set_anpr_credential,
            anpr::delete_anpr_credential,
            anpr::anpr_diagnostics,
            anpr::get_machine_info,
            anpr::set_anpr_machine,
            anpr::set_ocr_plate_mode,
            anpr::get_anpr_machine,
            anpr::check_machine_match,
            anpr::get_user_auto_start,
            anpr::set_user_auto_start,
            sync::sync_status,
            sync::sync_now_pg,
            sync::load_archive_trips,
            sync::get_date_range_presets,
            sync::connect_google_sheets,
            sync::disconnect_google_sheets,
            sync::set_google_sheets_frequency,
            sync::sync_now_sheets,
            sync::simulate_connectivity,
            sync::configure_postgres,
            sync::disconnect_postgres,
            sync::create_postgres_tables,
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
            monitor::monitoring_dashboard,
            commands::update_own_profile,
            commands::set_profile_photo,
            commands::get_profile_photo,
            get_frames_dir,
            set_frames_dir,
            pick_folder,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                if let Some(st) = window.try_state::<AppState>() {
                    if let Ok(mut procs) = st.anpr_processes.try_lock() {
                        for mut child in procs.drain(..) {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Find the anpr-service directory.
///
/// Search order (first match wins):
///   1. `$RESOURCE_DIR/anpr-service/` — Tauri's own resolved resource directory.
///      This is the definitive location for production installs produced by
///      `tauri build`. Tauri copies the bundle resources there at install time.
///   2. Walk up from the process working directory — covers `cargo tauri dev`
///      where cwd is set to src-tauri/, which walks up to the project root.
///   3. Walk up from the running executable's directory — covers debug builds
///      (target/debug/) and any production layout where the exe sits inside
///      the install tree.
///
/// This approach is path-agnostic: it never hard-codes machine-specific paths
/// and works identically in dev, CI, and production.
pub fn find_anpr_dir() -> std::path::PathBuf {
    fn walk_up(start: &std::path::Path) -> Option<std::path::PathBuf> {
        let mut dir = start.to_path_buf();
        loop {
            let candidate = dir.join("anpr-service");
            // Must have main.py — src-tauri/anpr-service only has the compiled exe,
            // project-root anpr-service has the Python source we need.
            if candidate.is_dir() && candidate.join("main.py").is_file() {
                return Some(candidate);
            }
            if !dir.pop() {
                break;
            }
        }
        None
    }

    // 1. Ask Tauri for the resolved resource directory (production installs).
    //    TAURI_RESOURCE_DIR is set by the Tauri runtime when the process starts;
    //    it points to the exact folder Tauri installed resources into.
    //    Must also check main.py exists — the bundled PyInstaller directory
    //    has anpr-service.exe but no main.py, which the Python spawn path needs.
    if let Ok(res_dir) = std::env::var("TAURI_RESOURCE_DIR") {
        let p = std::path::Path::new(&res_dir).join("anpr-service");
        if p.is_dir() && p.join("main.py").is_file() {
            return p;
        }
    }
    // Also check one level up from the exe (NSIS installs exe in bin/, resources
    // in the parent — or the installer may place them alongside the exe).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // Check $EXE_DIR/resources/anpr-service (common Tauri NSIS layout)
            let p = exe_dir.join("resources").join("anpr-service");
            if p.is_dir() && p.join("main.py").is_file() {
                return p;
            }
            // Check $EXE_DIR/../resources/anpr-service (MSI layout)
            if let Some(parent) = exe_dir.parent() {
                let p = parent.join("resources").join("anpr-service");
                if p.is_dir() && p.join("main.py").is_file() {
                    return p;
                }
            }
        }
    }

    // 2. Walk up from the running executable directory first.
    //    In dev mode (cargo tauri dev), the exe is at target/debug/ — walking up
    //    reaches the project root where anpr-service/ contains main.py.
    //    Walking from CWD first would incorrectly find src-tauri/anpr-service/
    //    (which only has the compiled exe, no Python source).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            if let Some(p) = walk_up(exe_dir) {
                return p;
            }
        }
    }

    // 3. Walk up from the current working directory (fallback).
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(p) = walk_up(&cwd) {
            return p;
        }
    }

    // Nothing found — return a relative path so the caller produces a clear
    // "directory not found" error instead of a silent panic.
    std::path::PathBuf::from("anpr-service")
}

/// Find the compiled ANPR service executable.
///
/// Returns `Some(path)` when the PyInstaller-compiled standalone exe exists,
/// `None` when only the raw Python source is available (dev / no build yet).
///
/// This is the single source of truth for the compiled exe path — callers
/// should use this instead of reconstructing the path themselves.
pub fn find_anpr_exe() -> Option<std::path::PathBuf> {
    let dir = find_anpr_dir();
    // Installed location (Tauri bundles files flat): anpr-service/anpr-service.exe
    let installed = dir.join("anpr-service.exe");
    if installed.exists() { return Some(installed); }
    // Dev/build location: anpr-service/dist/anpr-service/anpr-service.exe
    let dev = dir.join("dist").join("anpr-service").join("anpr-service.exe");
    if dev.exists() { Some(dev) } else { None }
}


/// Auto-start the ANPR service on app launch. Finds the first active camera,
/// writes config.json, spawns the Python process, and sets anpr_source=http.
/// Only auto-starts if ANY user has enabled auto-start for this machine.
/// Machine fingerprint identifies the computer; per-user preferences determine
/// who gets auto-start on their own machines.
fn auto_start_anpr(state: &AppState) -> Result<(), String> {
    let machine_info = anpr::get_machine_info().unwrap_or_else(|_| anpr::MachineInfo {
        hostname: "unknown".to_string(),
        mac_address: "unknown".to_string(),
        machine_id: "unknown".to_string(),
    });

    // Single lock hold for the database reads — avoids locking/unlocking twice.
    let (
        auto_start_user,
        is_capture_point,
        camera,
    ) = {
        // Use try_lock — never block UI commands. If db is busy, skip this cycle.
        let conn = match state.db.try_lock() {
            Ok(c) => c,
            Err(_) => return Err("db busy, will retry next cycle".to_string()),
        };

        let auto_start_user: Option<String> = anpr::any_auto_start_for_machine(&conn, &machine_info.machine_id);
        if auto_start_user.is_none() {
            return Err(format!(
                "No user has enabled auto-start on this machine ({}). Enable it in ANPR Settings.",
                machine_info.hostname
            ));
        }

        // Explicitly check is_capture_point — reporting-only machines must never
        // auto-start the ANPR service, even if a user accidentally enabled auto-start.
        let is_capture_point: bool = conn
            .query_row(
                "SELECT COALESCE(is_capture_point, 0) FROM anpr_config WHERE id = ?1",
                params![crate::db::ANPR_CONFIG_ID],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) != 0;

        let camera: Option<(String, String)> = conn
            .query_row(
                "SELECT source_type, connection_string FROM camera_sources WHERE status = 'active' LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();

        // Set anpr_source to http so the poller picks it up — same lock hold.
        conn.execute(
            "INSERT INTO app_settings (key, value) VALUES ('anpr_source', 'http')\n             ON CONFLICT(key) DO UPDATE SET value = 'http'",
            [],
        ).map_err(|e| e.to_string())?;

        (auto_start_user, is_capture_point, camera)
    }; // lock dropped here — ONE acquisition total

    // Guard: reporting-only machines do not run the ANPR service at all.
    if !is_capture_point {
        crate::log::log(&format!(
            "[ANPR] This machine ({}) is NOT a capture point — ANPR service will not auto-start. \
             Enable 'Is Capture Point' in ANPR Settings if this machine has a camera.",
            machine_info.hostname
        ));
        return Ok(());
    }

    crate::log::log(&format!("[ANPR] Auto-start triggered by user {} on machine {}", auto_start_user.unwrap_or_default(), machine_info.hostname));

    match &camera {
        Some(c) => crate::log::log(&format!("[ANPR] Auto-starting with {}: {}", c.0, c.1)),
        None => crate::log::log("[ANPR] No active camera source — starting in idle mode"),
    };
    let connection_string = camera.as_ref().map(|c| c.1.clone()).unwrap_or_default();

    // Write config.json via the shared resolver that re-maps USB indices
    // against THIS machine's DirectShow device order and writes the sources array.
    let anpr_dir = find_anpr_dir();
    {
        let conn = match state.db.try_lock() {
            Ok(c) => c,
            Err(_) => return Err("db busy during config write".to_string()),
        };
        if let Err(e) = capture::write_anpr_config_file(&conn, &anpr_dir) {
            crate::log::log(&format!("[ANPR] Config write failed: {e}"));
        }
    }

    // Validate: warn if camera source is empty (ANPR will start but read nothing).
    if connection_string.is_empty() {
        crate::log::log(&format!("[ANPR] WARNING: No camera connection string configured — ANPR service will start but cannot read plates. Configure a camera in ANPR Settings."));
    }

    // Always use Python — the compiled exe (PyInstaller bundle) crashes on
    // startup and can kill the WebView via GPU contention or pipe error.
    // If Python isn't installed, try to install it.
    if capture::find_python().is_empty() {
        if let Err(e) = capture::ensure_anpr_deps(&anpr_dir, None) {
            crate::log::log(&format!("[ANPR] Could not set up Python: {e}"));
        }
    }
    let effective_python = capture::find_python();

    let main_py = anpr_dir.join("main.py");
    crate::log::log(&format!("[ANPR] Using python={effective_python}, main={}", main_py.display()));
    let mut cmd = {
        let mut c = std::process::Command::new(&effective_python);
        c.arg("-u").arg(&main_py).arg("--port").arg("9800");
        c.current_dir(&anpr_dir);
        c
    };
    // Pipe stdout/stderr to log files so we can diagnose crashes/startup failures.
    let log_file = anpr_dir.join("anpr-service.log");
    let err_file = anpr_dir.join("anpr-service.err");
    match std::fs::OpenOptions::new().create(true).append(true).open(&log_file) {
        Ok(f) => { cmd.stdout(std::process::Stdio::from(f)); }
        Err(_) => { cmd.stdout(std::process::Stdio::null()); }
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(&err_file) {
        Ok(f) => { cmd.stderr(std::process::Stdio::from(f)); }
        Err(_) => { cmd.stderr(std::process::Stdio::null()); }
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW — keeps the heavy
        // Python/PaddleOCR process from starving the WebView of CPU/GPU.
        cmd.creation_flags(0x08008000);
    }
    let child = cmd.spawn().map_err(|e| {
        format!("Failed to start ANPR: {e} (python, dir: {})", anpr_dir.display())
    })?;
    let pid = child.id();
    crate::log::log(&format!("[ANPR] Service started (PID {pid})"));

    // Store the child handle
    {
        let mut procs = state.anpr_processes.lock().map_err(|e| e.to_string())?;
        procs.push(child);
    }

    Ok(())
}

/// Background poller for the ANPR source (02-architecture.md §4). Runs
/// independently of the UI; an unreachable/unconfigured source degrades
/// gracefully and never blocks the rest of the app.
///
/// Also monitors the ANPR process health: if the process dies, it attempts
/// an automatic restart so the service recovers without user intervention.
fn spawn_anpr_poller(app: &tauri::AppHandle, state: &AppState) {
    let handle = app.clone();
    let running = state.running.clone();
    let anpr_db = state.anpr_db.clone();
    // Read the configured service URL from dedicated ANPR connection
    let service_url = {
        let Ok(conn) = anpr_db.try_lock() else { return };
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
            let Ok(conn) = anpr_db.try_lock() else { continue };
            // Skip entirely on non-capture-point machines — no ANPR service
            // will ever run here, so don't waste cycles or spam health events.
            if !capture::is_capture_point(&conn) {
                crate::log::log("[ANPR] Skipping — is_capture_point is false");
                drop(conn);
                continue;
            }
            if !capture::anpr_enabled(&conn) {
                crate::log::log("[ANPR] Skipping — anpr_enabled is false");
                drop(conn);
                continue;
            }
            drop(conn);

            // Skip auto-restart while a user-initiated start is in progress.
            // This prevents the poller from racing with the user's stop→start
            // sequence and killing the freshly spawned Python process.
            if st.anpr_starting.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            // Check if the ANPR process is still alive. If it died, attempt restart.
            let needs_restart = match st.anpr_processes.try_lock() {
                Ok(mut procs) => {
                    let alive = procs.iter_mut().any(|c| c.try_wait().ok().flatten().is_none());
                    // Restart if: (a) tracked processes exist but all dead, or
                    // (b) procs is empty (crash cascade cleared it) but the
                    // service port is not responding — the service should be
                    // running but isn't.
                    if procs.is_empty() {
                        !http.reachable()
                    } else {
                        !alive
                    }
                }
                Err(_) => false, // can't lock, assume alive (don't restart under contention)
            };
            if needs_restart {
                crate::log::log(&format!("[ANPR] Process died — attempting auto-restart"));
                let Ok(mut procs) = st.anpr_processes.try_lock() else { continue };
                procs.clear(); // remove dead handles
                drop(procs);
                let Ok(conn) = anpr_db.try_lock() else { continue };
                let _ = monitor::record_health_event(&conn, "anpr_service", "restarted", Some("ANPR process died, auto-restarted"));
                drop(conn);
                continue; // skip this cycle, let the new process start
            }

            // The ANPR service (port 9800) handles ALL source types internally
            // (HTTP, RTSP, USB, video file). The app always polls from it —
            // the source type doesn't matter at the poller level.
            if http.reachable() {
                let Ok(conn) = anpr_db.try_lock() else { continue };
                let _ = monitor::record_health_event(&conn, "anpr_service", "ok", None);
                drop(conn);
            } else {
                let Ok(conn) = anpr_db.try_lock() else { continue };
                let _ = monitor::record_health_event(
                    &conn,
                    "anpr_service",
                    "offline",
                    Some(&format!("ANPR service unreachable at {}", http.service_url)),
                );
                drop(conn);
                continue; // service down — skip this cycle
            }

            let Some(read) = http.poll() else {
                crate::log::log("[ANPR] poll() returned None — no new read available");
                continue;
            };

            crate::log::log(&format!(
                "[ANPR] Got read: plate={} conf={} frames={}",
                read.plate, read.confidence, read.frames.len()
            ));

            if let Ok(mut last) = st.anpr_last.try_lock() {
                *last = Some((read.timestamp.clone(), read.plate.clone()));
            }
            let officer = st.session.lock().ok().and_then(|s| s.as_ref().map(|s| s.user_id.clone()));
            let conn = match anpr_db.try_lock() {
                Ok(c) => c,
                Err(_) => continue,
            };
            let result = capture::ingest_read(&conn, officer, &read, "auto", &st.frames_dir);
            match &result {
                Ok(r) => {
                    crate::log::log(&format!(
                        "[ANPR] ingest_read ok: trip_id={:?} queued_id={:?} outcome={} msg={}",
                        r.trip.as_ref().map(|t| &t.id),
                        r.queued.as_ref().map(|q| &q.id),
                        r.outcome.state,
                        r.message
                    ));
                }
                Err(e) => {
                    crate::log::log(&format!("[ANPR] ingest_read error: {}", e));
                }
            }
            let _ = capture::record_read_event(&conn, &read, "auto", "captured");
            let _ = monitor::record_health_event(&conn, "anpr_service", "ok", None);
            drop(conn);
            let _ = handle.emit("capture-updated", ());
            let _ = st.sync_notify.try_send(());
        }
    });
}

/// Event-driven sync worker. Sleeps on the notification channel until a trip
/// is created, then immediately processes all pending sync work (PG + Sheets).
/// No polling timer — zero latency from trip creation to export.
/// Sheets and Postgres run in parallel so one can't block the other.
fn spawn_keepalive_pinger(state: &AppState) {
    let sync_notify = state.sync_notify.clone();
    let running = state.running.clone();
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(30));
            // Send a signal to wake the sync poller. This ensures pending rows
            // get pushed even when no new trips are created (e.g., after internet
            // comes back online). The channel has capacity 1, so extra signals
            // while the poller is busy are safely dropped.
            let _ = sync_notify.try_send(());
        }
    });
}

/// Machine heartbeat: updates machine_status every 30s and marks stale
/// machines as offline. Runs on a dedicated thread.
fn spawn_heartbeat(state: &AppState) {
    let sync_db = state.sync_db.clone();
    let running = state.running.clone();
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(30));
            if let Ok(conn) = sync_db.try_lock() {
                // Mark machines offline if not seen in 90s (3 missed heartbeats)
                let _ = conn.execute(
                    "UPDATE machine_status SET is_online = 0 WHERE last_seen_at < datetime('now', '-90 seconds')",
                    [],
                );
            }
        }
    });
}

/// Update this machine's heartbeat. Call on login and periodically.
pub fn update_machine_heartbeat(state: &AppState, user_id: &str, company_id: &str, role: &str) {
    update_machine_heartbeat_raw(&state.db, &state.pg, user_id, company_id, role);
}

/// Raw version for use in background threads (takes individual parameters).
pub fn update_machine_heartbeat_raw(
    db: &Arc<Mutex<Connection>>,
    pg: &Arc<dyn PostgresAdapter>,
    user_id: &str,
    company_id: &str,
    role: &str,
) {
    let machine_id = get_machine_id();
    let pc_name = get_pc_name();
    let now = crate::db::now_iso();
    
    // Update local SQLite
    if let Ok(conn) = db.try_lock() {
        let _ = conn.execute(
            "INSERT INTO machine_status (id, machine_id, user_id, company_id, role, last_seen_at, is_online, pc_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)
             ON CONFLICT(machine_id) DO UPDATE SET 
                 user_id = excluded.user_id,
                 role = excluded.role,
                 last_seen_at = excluded.last_seen_at,
                 is_online = 1,
                 pc_name = excluded.pc_name",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), machine_id, user_id, company_id, role, now, pc_name],
        );
    }
    
    // Sync to PostgreSQL (best-effort)
    if pg.configured() && pg.connected() {
        let sql = format!(
            "INSERT INTO machine_status (machine_id, user_id, company_id, role, last_seen_at, is_online, pc_name)
             VALUES ('{}', '{}', '{}', '{}', '{}', 1, '{}')
             ON CONFLICT (machine_id) DO UPDATE SET 
                 user_id = EXCLUDED.user_id,
                 role = EXCLUDED.role,
                 last_seen_at = EXCLUDED.last_seen_at,
                 is_online = 1,
                 pc_name = EXCLUDED.pc_name",
            crate::sync::pg_literal_string(&machine_id),
            crate::sync::pg_literal_string(user_id),
            crate::sync::pg_literal_string(company_id),
            crate::sync::pg_literal_string(role),
            crate::sync::pg_literal_string(&now),
            crate::sync::pg_literal_string(&pc_name),
        );
        let _ = pg.query_rows(&sql, &[]);
    }
}

/// Get a stable machine identifier (hardware-based or generated).
fn get_machine_id() -> String {
    // Try to get a stable machine ID from the OS
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        if let Ok(output) = Command::new("wmic").args(["csproduct", "get", "UUID"]).output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<&str> = stdout.lines().collect();
            if lines.len() > 1 {
                return lines[1].trim().to_string();
            }
        }
    }
    
    // Fallback: generate and store a UUID
    "machine-".to_string() + &uuid::Uuid::new_v4().to_string()
}

/// Get the PC name for display.
fn get_pc_name() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn spawn_sync_poller(app: &tauri::AppHandle, state: &AppState, sync_rx: std::sync::mpsc::Receiver<()>) {
    let handle = app.clone();
    let running = state.running.clone();
    let pg = state.pg.clone();
    let sheets = state.sheets.clone();
    let sync_db = state.sync_db.clone();
    let pending_marks = state.pending_sync_marks.clone();
    std::thread::spawn(move || {
        let mut last_sheets_run = std::time::Instant::now();
        let sheets_interval = std::time::Duration::from_secs(30);
        while running.load(Ordering::Relaxed) {
            // Block until a trip is created (or shutdown).
            let got_signal = match sync_rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(()) => true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };

            // Drain any buffered signals (multiple trips created at once).
            while sync_rx.try_recv().is_ok() {}

            // On timeout without signal: only run Sheets if interval elapsed
            // (catches exit updates that don't trigger a new signal).
            if !got_signal && last_sheets_run.elapsed() < sheets_interval {
                continue;
            }
            last_sheets_run = std::time::Instant::now();

            // Brief pause so the UI has time to show "pending" state
            // before the sync completes and clears the counter.
            std::thread::sleep(std::time::Duration::from_secs(3));

            let Some(_st) = handle.try_state::<AppState>() else {
                continue;
            };

            // ── Drain pending mark-synced from previous cycles ─────────
            if let Ok(mut marks) = pending_marks.try_lock() {
                while let Some((table, ids)) = marks.pop() {
                    if let Ok(conn) = sync_db.try_lock() {
                        if let Err(e) = sync::mark_rows_synced(&conn, &table, &ids) {
                            crate::log::log(&format!("[sync] deferred mark {table}: {e}"));
                        }
                    } else {
                        marks.push((table, ids));
                        break;
                    }
                }
            }

            // ── Fire Sheets and PG in parallel ──────────────────────────
            let pg_handle = pg.clone();
            let sync_db_pg = sync_db.clone();
            let pending_marks_pg = pending_marks.clone();

            let pg_thread = std::thread::spawn(move || {
                if !pg_handle.configured() { return; }

                // ── Postgres push (local → central) ──────────────────────
                let mut any_pushed = false;
                for (table, _display) in sync::PG_SYNC_TABLES {
                    let rows = {
                        let conn = match sync_db_pg.lock() {
                            Ok(c) => c,
                            Err(e) => { crate::log::log(&format!("[sync] sync_db poisoned: {e}")); continue; }
                        };
                        match sync::collect_unsynced_rows(&conn, table) {
                            Ok(r) => r,
                            Err(e) => { crate::log::log(&format!("[sync] collect {table}: {e}")); continue; }
                        }
                    };
                    if rows.is_empty() { continue; }
                    let pushed_ids = match sync::push_rows_to_central(&*pg_handle, table, &rows) {
                        Ok(ids) => ids,
                        Err(e) => { crate::log::log(&format!("[sync] push {table}: {e}")); continue; }
                    };
                    if !pushed_ids.is_empty() {
                        any_pushed = true;
                        crate::log::log(&format!("[sync] push {table}: {}/{} rows synced", pushed_ids.len(), rows.len()));
                    }
                    if !pushed_ids.is_empty() {
                        match sync_db_pg.lock() {
                            Ok(conn) => {
                                if let Err(e) = sync::mark_rows_synced(&conn, table, &pushed_ids) {
                                    crate::log::log(&format!("[sync] mark {table}: {e}"));
                                }
                            }
                            Err(_) => {
                                if let Ok(mut marks) = pending_marks_pg.try_lock() {
                                    marks.push((table.to_string(), pushed_ids));
                                }
                            }
                        }
                    }
                }
                if any_pushed {
                    if let Ok(conn) = sync_db_pg.lock() {
                        let _ = crate::db::set_setting(&conn, "pg_last_synced_at", &crate::db::now_iso());
                    }
                }

                // ── Postgres pull (central → local) ──────────────────────
                if pg_handle.connected() {
                    for &table in sync::REFERENCE_TABLES {
                        let last_pull = {
                            let conn = match sync_db_pg.lock() {
                                Ok(c) => c,
                                Err(_) => continue,
                            };
                            crate::db::get_setting(&conn, "pg_last_pulled_at").unwrap_or_default()
                        };
                        let central_rows = match sync::fetch_central_rows(&*pg_handle, table, &last_pull) {
                            Ok(r) => r,
                            Err(e) => { crate::log::log(&format!("[sync] pull {table}: {e}")); continue; }
                        };
                        if central_rows.is_empty() { continue; }
                        {
                            let conn = match sync_db_pg.lock() {
                                Ok(c) => c,
                                Err(_) => continue,
                            };
                            let _ = sync::upsert_central_rows(&conn, table, &central_rows);
                        }
                    }
                    if let Ok(conn) = sync_db_pg.lock() {
                        let _ = crate::db::set_setting(&conn, "pg_last_pulled_at", &crate::db::now_iso());
                    }
                }

                // ── Process pending deletes (local deletions → central) ───
                // Try to process deletes even when offline - delete_rows will attempt connection
                if pg_handle.configured() {
                    match {
                        let conn = match sync_db_pg.lock() {
                            Ok(c) => c,
                            Err(_) => return,
                        };
                        sync::get_all_pending_deletes(&conn)
                    } {
                        Ok(pending_deletes) if !pending_deletes.is_empty() => {
                            crate::log::log(&format!("[sync] processing {} pending deletes", pending_deletes.len()));
                            // Group by table
                            let mut by_table: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
                            for (table, id) in pending_deletes {
                                by_table.entry(table).or_default().push(id);
                            }
                            for (table, ids) in by_table {
                                match pg_handle.delete_rows(&table, &ids) {
                                    Ok(()) => {
                                        crate::log::log(&format!("[sync] delete_rows {table}: deleted {} rows from central", ids.len()));
                                        if let Ok(conn) = sync_db_pg.lock() {
                                            let _ = sync::clear_pending_deletes(&conn, &table, &ids);
                                        }
                                    }
                                    Err(e) => {
                                        crate::log::log(&format!("[sync] delete_rows {table}: failed to delete from central: {e}"));
                                    }
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            crate::log::log(&format!("[sync] pending deletes query failed: {e}"));
                        }
                    }
                }
            });

            // ── Google Sheets sync (runs on poller thread) ─────────────
            {
                let conn = match sync_db.lock() {
                    Ok(c) => c,
                    Err(_) => { let _ = pg_thread.join(); continue; }
                };
                let timer_due = sync::sheets_due(&conn, &*sheets);
                let pending_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM trips WHERE status = 'logged'
                     AND (
                       (sheet_row IS NULL AND (capture_method = 'auto' OR is_discharge_trip = 1))
                       OR (sheet_row IS NOT NULL AND sheet_exit_pushed = 0 AND exit_time IS NOT NULL)
                     )",
                    [],
                    |r| r.get(0),
                ).unwrap_or(0);
                let has_pending = pending_count > 0;
                crate::log::log(&format!("[sync] sheets poll: timer_due={timer_due} has_pending={has_pending} pending_count={pending_count} configured={}", sheets.configured()));
                if timer_due || has_pending {
                    let data = match sync::prepare_sheets_data(&conn, &*sheets) {
                        Ok(d) => d,
                        Err(e) => { crate::log::log(&format!("[sync] sheets prepare: {e}")); let _ = pg_thread.join(); continue; }
                    };
                    drop(conn);
                    crate::log::log(&format!("[sync] sheets data: pending={} new_rows={} update_rows={}", data.pending, data.new_rows.len(), data.update_rows.len()));
                    if data.pending > 0 && sheets.configured() {
                        let mut sheets_data = data;
                        sync::dedup_sheets_rows(&*sheets, &mut sheets_data);
                        crate::log::log(&format!("[sync] sheets after dedup: new_rows={} update_rows={}", sheets_data.new_rows.len(), sheets_data.update_rows.len()));
                        let has_work = !sheets_data.new_rows.is_empty() || !sheets_data.update_rows.is_empty();
                        if has_work {
                            match sync::execute_sheets_network(&*sheets, &sheets_data.mapping, &sheets_data.new_rows, &sheets_data.update_rows) {
                                Ok(acked_ids) => {
                                    crate::log::log(&format!("[sync] sheets network OK: acked_ids={} new_rows={} update_rows={}", acked_ids.len(), sheets_data.new_rows.len(), sheets_data.update_rows.len()));
                                    if let Ok(conn) = sync_db.lock() {
                                        let _ = sync::finalize_sheets_results(&conn, &sheets_data.new_rows, &sheets_data.update_rows, &acked_ids);
                                    }
                                }
                                Err(e) => { crate::log::log(&format!("[sync] sheets network: {e}")); }
                            }
                        } else {
                            crate::log::log("[sync] sheets: no work after dedup");
                        }
                    }
                }
            }

            // Wait for PG to finish before next cycle.
            let _ = pg_thread.join();
        }
    });
}

/// Read a value from app_settings by key. Returns None if not set.
#[tauri::command]
fn get_app_setting(state: State<AppState>, key: String) -> Result<Option<String>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    Ok(crate::db::get_setting(&conn, &key))
}

/// Get the current frames storage directory.
#[tauri::command]
fn get_frames_dir(state: State<AppState>) -> Result<String, String> {
    Ok(state.frames_dir.to_string_lossy().to_string())
}

/// Set a new frames storage directory. Saves to database; takes effect on next restart.
#[tauri::command]
fn set_frames_dir(state: State<AppState>, new_dir: String) -> Result<String, String> {
    let new_path = std::path::PathBuf::from(&new_dir);
    if !new_path.exists() {
        std::fs::create_dir_all(&new_path).map_err(|e| format!("Cannot create directory: {e}"))?;
    }
    // Save to app_settings so it persists across restarts
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::db::set_setting(&conn, "frames_dir", &new_dir).map_err(|e| e.to_string())?;
    }
    Ok(new_dir)
}

/// Open a folder picker dialog and return the selected path.
#[tauri::command]
async fn pick_folder(handle: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let folder = handle.dialog().file().blocking_pick_folder();
    Ok(folder.map(|p| p.to_string()))
}
