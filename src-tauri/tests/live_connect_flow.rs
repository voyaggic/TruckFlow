//! Live verification: the connect flow must take effect WITHOUT an app restart.
//!
//! Reproduces the original bug scenario: app starts with the pgbouncer (Real)
//! adapter, admin connects to Supabase REST. Before the SharedPg fix, the new
//! REST adapter was configured in a background thread but never installed, so
//! `state.pg` stayed unconfigured until relaunch.
//!
//! This test performs the same steps `configure_postgres` performs (it is a
//! `#[tauri::command]`, which cannot be invoked directly outside the runtime
//! IPC), against the REAL Supabase project using the connection string copied
//! from the local app DB (never printed), then asserts that state.pg is
//! immediately usable: label switched, configured, connected, a real push is
//! confirmed by the cloud, a read-back returns the row, and cleanup deletes it.
//!
//! Run: cargo test --test live_connect_flow -- --ignored --nocapture
//! (marked #[ignore] so normal `cargo test` runs stay offline/hermetic)

use std::sync::Arc;

use rusqlite::Connection;
use tauri::test::mock_app;
use tauri::Manager;

use truckflow_lib::capture::SimulatorSource;
use truckflow_lib::db::{self, AppState};
use truckflow_lib::sync::{
    collect_unsynced_rows, pg_literal_string, push_rows_to_central, PostgresAdapter, SharedPg,
};

#[test]
#[ignore = "live network test — run explicitly with --ignored"]
fn connect_flow_installs_adapter_without_restart() {
    // ── Arrange: copy the real app DB and extract the saved connection ──────
    let src = db::default_db_path();
    assert!(src.exists(), "live test needs the real app DB at {}", src.display());
    let tmp_dir = std::env::temp_dir().join("tf_live_connect_test");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let dst = tmp_dir.join("copy.sqlite3");
    let _ = std::fs::remove_file(&dst);
    std::fs::copy(&src, &dst).expect("copy app db");
    let conn = db::open_db(&dst).expect("open copied db");

    let conn_string = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'pg_connection_string'",
            [],
            |r| r.get::<_, String>(0),
        )
        .expect("pg_connection_string must exist in the app DB for the live test");
    assert!(conn_string.starts_with("REST|"), "expected a REST connection string");

    let actor_id: String = conn
        .query_row("SELECT id FROM users ORDER BY created_at ASC LIMIT 1", [], |r| r.get(0))
        .expect("copied DB must contain at least one user");
    let _ = actor_id; // used implicitly: the copied DB carries the admin row

    // ── Build app state in the PRE-CONNECT shape (pgbouncer, unconfigured) ──
    let app = mock_app();
    let (sync_tx, _sync_rx) = std::sync::mpsc::sync_channel(1);
    app.manage(AppState {
        db: Arc::new(std::sync::Mutex::new(conn)),
        sync_db: Arc::new(std::sync::Mutex::new(Connection::open_in_memory().unwrap())),
        anpr_db: Arc::new(std::sync::Mutex::new(Connection::open_in_memory().unwrap())),
        session: std::sync::Mutex::new(None),
        simulator: Arc::new(SimulatorSource::new()),
        anpr_last: std::sync::Mutex::new(None),
        running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        anpr_starting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        frames_dir: std::env::temp_dir().join("tf_live_connect_frames"),
        pg: Arc::new(SharedPg::new(Arc::new(
            truckflow_lib::sync::RealPostgres::new(),
        ))),
        sheets: Arc::new(truckflow_lib::sync::MockSheets::new()),
        anpr_processes: Arc::new(std::sync::Mutex::new(Vec::new())),
        pending_sync_marks: Arc::new(std::sync::Mutex::new(Vec::new())),
        sync_notify: sync_tx,
    });

    {
        let state = app.state::<AppState>();
        assert!(!state.pg.configured(), "pre-connect adapter must be unconfigured");
        assert_eq!(state.pg.label(), "postgres", "starts on the pgbouncer adapter");
    }

    // ── Act: the exact steps configure_postgres performs ────────────────────
    {
        let state = app.state::<AppState>();

        // Step 1: save the connection string (mirrors configure_postgres)
        {
            let conn = state.db.lock().unwrap();
            db::set_setting(&conn, "pg_connection_string", &conn_string).unwrap();
        }

        // Step 2: build the new adapter exactly like the app does on startup
        // (real_postgres reads the saved setting and returns a REST adapter
        // with the config applied — lazy connect on first use).
        let new_adapter: Arc<dyn PostgresAdapter> = {
            let conn = state.db.lock().unwrap();
            truckflow_lib::sync::real_postgres(&conn)
        };
        assert_eq!(new_adapter.label(), "rest-postgres", "must build the REST adapter");

        // Step 3: configure (real network call — connection test to Supabase)
        new_adapter
            .configure(Some(conn_string.clone()))
            .expect("REST configure must succeed against the live project");

        // Step 4: the fix under test — install the adapter immediately
        state.pg.swap(new_adapter);
    }

    // ── Assert: the swap is visible immediately — NO restart ────────────────
    {
        let state = app.state::<AppState>();
        assert_eq!(state.pg.label(), "rest-postgres", "adapter label must switch live");
        assert!(state.pg.configured(), "adapter must be configured live");
        assert!(state.pg.connected(), "adapter must report connected live");
    }

    // ── Assert: a REAL push through state.pg reaches Supabase ───────────────
    let test_id = format!("live-connect-{}", uuid::Uuid::new_v4());
    {
        let state = app.state::<AppState>();
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO audit_log (id, actor_id, action, target_id, details, timestamp, synced, updated_at)
             VALUES (?1, NULL, 'live_connect_test', NULL, '{}', ?2, 0, ?2)",
            rusqlite::params![test_id, chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
    }

    let pushed_ids = {
        let state = app.state::<AppState>();
        let rows = {
            let conn = state.db.lock().unwrap();
            collect_unsynced_rows(&conn, "audit_log").unwrap()
        };
        assert!(rows.iter().any(|r| r["id"] == test_id), "test row must be among unsynced");
        push_rows_to_central(state.pg.get().as_ref(), "audit_log", &rows)
            .expect("push through the swapped adapter must succeed against live Supabase")
    };
    assert!(
        pushed_ids.iter().any(|id| id == &test_id),
        "the test row must be confirmed by the cloud (got {pushed_ids:?})"
    );

    // Read it back from the cloud — full round trip through the live adapter
    {
        let state = app.state::<AppState>();
        let rows = state
            .pg
            .query_rows(
                &format!("SELECT id FROM audit_log WHERE id = '{}'", pg_literal_string(&test_id)),
                &[],
            )
            .expect("cloud read-back must work");
        assert_eq!(rows.len(), 1, "exactly the test row should come back");
        assert_eq!(rows[0]["id"], test_id);
    }

    // Cleanup: delete the probe row from the cloud
    {
        let state = app.state::<AppState>();
        state.pg.delete_rows("audit_log", &[test_id]).expect("cleanup delete");
    }

    println!("LIVE CONNECT FLOW: PASS — adapter swap, push, read-back and cleanup all succeeded without restart");
}
