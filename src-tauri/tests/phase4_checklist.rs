//! Phase 4 — Sync & distribution tests, driven by 02-architecture.md §3 and
//! 06-data-flow.md (Step 5 + the "Testing checklist" failure matrix):
//!
//! - Offline-first: capture never waits on connectivity; pending rows are
//!   retried on reconnect with zero loss and zero duplicates (UUID-keyed).
//! - Two fully independent pipelines (`synced` vs `pushed_to_sheets`) — one
//!   failing never affects the other or local capture.
//! - `manage_integrations` gating, audit entries, frequency validation, and
//!   dev-only connectivity simulation.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tauri::test::mock_app;
use tauri::{App, Manager, State};

use truckflow_lib::capture::{classify_discharge_impl, ingest_read, manual_entry_impl, set_capture_settings, SimulatorSource};
use truckflow_lib::commands;
use truckflow_lib::db::{open_db, AppState};
use truckflow_lib::models::{AnprFrame, AnprRead, SessionUser, VehicleView};
use truckflow_lib::reference;
use truckflow_lib::sync::{
    MockPostgres, MockSheets, PostgresAdapter, SheetsProvider, RealPostgres, connect_google_sheets,
    configure_google_sheets, configure_postgres, disconnect_google_sheets, disconnect_postgres,
    run_pg_sync_impl, run_sheets_sync_impl, set_google_sheets_frequency, simulate_connectivity,
    sync_now_pg, sync_now_sheets, sync_status,
};

const ADMIN_PASS: &str = "AdminPass!2024";

struct TempDb {
    dir: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("truckflow_p4_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn db_path(&self) -> PathBuf {
        self.dir.join("test.db")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct TestCtx {
    _tmp: TempDb,
    app: App<tauri::test::MockRuntime>,
}

impl TestCtx {
    fn new() -> Self {
        let tmp = TempDb::new();
        let frames_dir = tmp.dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        let conn = open_db(&tmp.db_path()).expect("open temp db");
        let app = mock_app();
        app.manage(AppState {
            db: Mutex::new(conn),
            session: Mutex::new(None),
            simulator: Arc::new(SimulatorSource::new()),
            anpr_last: Mutex::new(None),
            running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            frames_dir,
            pg: Arc::new(MockPostgres::new()),
            sheets: Arc::new(MockSheets::new()),
        });
        Self { _tmp: tmp, app }
    }

    fn state(&self) -> State<'_, AppState> {
        self.app.state()
    }

    fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self._tmp.db_path()).expect("reopen db")
    }

    fn frames_dir(&self) -> PathBuf {
        self._tmp.dir.join("frames")
    }

    fn create_admin(&self) -> SessionUser {
        commands::create_first_admin(self.state(), "Boss".to_string(), ADMIN_PASS.to_string())
            .expect("create first admin")
            .user
    }

    fn create_gate_user(&self, admin: &SessionUser) -> truckflow_lib::models::UserView {
        commands::create_user(
            self.state(),
            admin.id.clone(),
            "Officer".to_string(),
            vec!["view_gate_entries".to_string(), "resolve_queue".to_string()],
            "GatePass!2024".to_string(),
        )
        .expect("create gate user")
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Build a structured ANPR read with 3 evidence frames and provenance metadata.
fn read(plate: &str, confidence: f64, timestamp: &str) -> AnprRead {
    let frames: Vec<AnprFrame> = (0..3)
        .map(|index| AnprFrame {
            index,
            captured_at: timestamp.to_string(),
            kind: "test".to_string(),
            data: None,
        })
        .collect();
    AnprRead {
        plate: plate.to_string(),
        confidence,
        timestamp: timestamp.to_string(),
        frames,
        model_version: Some("test-model-1".to_string()),
        ocr_engine: Some("paddleocr".to_string()),
    }
}

/// Seed one company + driver + vehicle so an exact-match read auto-logs.
fn seed_reference(ctx: &TestCtx, admin: &SessionUser) -> VehicleView {
    let company = reference::create_company(ctx.state(), admin.id.clone(), "Acme Waste".into(), None).unwrap();
    let driver = reference::create_driver(ctx.state(), admin.id.clone(), "D. Singh".into(), None).unwrap();
    reference::create_vehicle(
        ctx.state(),
        admin.id.clone(),
        "A123AB".into(),
        Some(company.id.clone()),
        Some(20.0),
        "litres".into(),
        Some(driver.id.clone()),
        None,
    )
    .unwrap()
}

/// Put the app in fully-automatic mode and log a trip for `plate` end to end.
fn log_trip(ctx: &TestCtx, admin: &SessionUser, plate: &str) -> truckflow_lib::models::TripView {
    set_capture_settings(ctx.state(), admin.id.clone(), Some("fully_automatic".to_string()), None, None, None, None).unwrap();
    let res = ingest_read(&ctx.conn(), None, &read(plate, 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    res.trip.expect("fully-automatic exact match must log")
}

/// Queue a trip (no reference match → queued as possible-new-vehicle).
fn queue_trip(ctx: &TestCtx, plate: &str) -> truckflow_lib::models::TripView {
    let res = ingest_read(&ctx.conn(), None, &read(plate, 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    res.queued.expect("unknown plate with confirm-required mode must queue")
}

fn synced_flag(conn: &rusqlite::Connection, table: &str, id: &str) -> i64 {
    conn.query_row(&format!("SELECT synced FROM {table} WHERE id = ?1"), rusqlite::params![id], |r| r.get(0))
        .unwrap()
}

fn pushed_to_sheets_flag(conn: &rusqlite::Connection, trip_id: &str) -> i64 {
    conn.query_row("SELECT pushed_to_sheets FROM trips WHERE id = ?1", rusqlite::params![trip_id], |r| r.get(0))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Offline-first + reconnect (06 §5, "extended-offline-then-reconnect")
// ---------------------------------------------------------------------------

/// Capture happens while "offline" (mock pg down): the trip logs locally,
/// syncs nothing, stays pending. On reconnect it is pushed exactly once, the
/// flag flips only on confirmed receipt, and re-running is idempotent.
#[test]
fn offline_first_capture_then_reconnect_no_loss_no_duplicates() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let veh = seed_reference(&ctx, &admin);

    let conn = ctx.conn();
    let pg = MockPostgres::new();
    pg.simulate_connectivity(false).unwrap();

    let trip = log_trip(&ctx, &admin, "A123AB");
    let trip_id = trip.id.clone();

    // Offline pass: nothing pushed, trip still unsynced — no error surfaced.
    let off = run_pg_sync_impl(&conn, &pg).expect("offline sync must not error");
    assert_eq!(off.pushed, 0, "nothing can be pushed while offline");
    assert_eq!(synced_flag(&conn, "trips", &trip_id), 0, "trip must stay pending offline");
    assert_eq!(synced_flag(&conn, "vehicles", &veh.id), 0);
    assert!(pg.pushed().is_empty());

    // Reconnect → full pass, in dependency order (reference before trips).
    pg.simulate_connectivity(true).unwrap();
    let on = run_pg_sync_impl(&conn, &pg).expect("reconnect sync succeeds");
    assert!(on.pushed >= 2, "vehicle + trip pushed on reconnect, pushed={}", on.pushed);
    assert_eq!(synced_flag(&conn, "trips", &trip_id), 1);
    assert_eq!(synced_flag(&conn, "vehicles", &veh.id), 1);

    let pushed = pg.pushed();
    let trip_pushes = pushed.iter().filter(|(t, id, _)| t == "trips" && id == &trip_id).count();
    assert_eq!(trip_pushes, 1, "each trip pushed exactly once — no duplicates");

    // Idempotent re-run: nothing new to push, flags unchanged.
    let again = run_pg_sync_impl(&conn, &pg).expect("repeat sync succeeds");
    assert_eq!(again.pushed, 0, "already-synced rows are never re-pushed");
    assert_eq!(pg.pushed().len(), pushed.len(), "no duplicate rows sent to central");
}

// ---------------------------------------------------------------------------
// Sheets pipeline independence (06 §5 / failure matrix)
// ---------------------------------------------------------------------------

/// Sheets failing (offline/revoked) leaves Postgres sync and local capture
/// fully untouched; conversely Postgres being down never blocks the sheet.
#[test]
fn sheets_and_postgres_sync_fail_independently() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let veh = seed_reference(&ctx, &admin);

    let conn = ctx.conn();

    // --- Scenario A: Sheets down, Postgres up. ---
    let sheets = MockSheets::new();
    sheets.simulate_connectivity(false).unwrap();
    let pg = MockPostgres::new();

    let trip = log_trip(&ctx, &admin, "A123AB");
    let trip_id = trip.id.clone();

    let s = run_sheets_sync_impl(&conn, &sheets).expect("sheets sync must not error while down");
    assert_eq!(s.pushed, 0);
    assert_eq!(pushed_to_sheets_flag(&conn, &trip_id), 0, "failed sheets push leaves flag false");

    let p = run_pg_sync_impl(&conn, &pg).expect("postgres sync must be unaffected by sheets failure");
    assert!(p.pushed >= 2);
    assert_eq!(synced_flag(&conn, "trips", &trip_id), 1, "trip synced to postgres while sheets down");
    assert_eq!(synced_flag(&conn, "vehicles", &veh.id), 1);
    assert!(sheets.pushed().is_empty(), "nothing reached the sheet");

    // --- Scenario B: Postgres down, Sheets up. ---
    let ctx2 = TestCtx::new();
    let admin2 = ctx2.create_admin();
    seed_reference(&ctx2, &admin2);
    let conn2 = ctx2.conn();
    let pg_down = MockPostgres::new();
    pg_down.simulate_connectivity(false).unwrap();
    let sheets_up = MockSheets::new();

    let trip2 = log_trip(&ctx2, &admin2, "A123AB");
    let trip2_id = trip2.id.clone();

    let p2 = run_pg_sync_impl(&conn2, &pg_down).expect("postgres sync must not error while down");
    assert_eq!(p2.pushed, 0);

    let s2 = run_sheets_sync_impl(&conn2, &sheets_up).expect("sheets sync must be unaffected by postgres failure");
    assert_eq!(s2.pushed, 1);
    assert_eq!(pushed_to_sheets_flag(&conn2, &trip2_id), 1);
    assert_eq!(
        sheets_up.pushed().iter().filter(|row| row["id"].as_str() == Some(trip2_id.as_str())).count(),
        1,
        "logged trip reached the sheet exactly once"
    );
}

/// Only logged trips are exported to the sheet (06 §5); queued work stays
/// pending until resolved and pushed_to_sheets flips only on confirmation.
#[test]
fn sheets_pushes_logged_trips_only() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let conn = ctx.conn();
    let sheets = MockSheets::new();

    let logged = log_trip(&ctx, &admin, "A123AB");
    let queued = queue_trip(&ctx, "UNKNOWN123");

    let res = run_sheets_sync_impl(&conn, &sheets).unwrap();
    assert_eq!(res.pushed, 1, "only the logged trip is exported");
    assert_eq!(pushed_to_sheets_flag(&conn, &logged.id), 1);
    assert_eq!(pushed_to_sheets_flag(&conn, &queued.id), 0, "queued trip must never reach the sheet");
}

/// 08 §9: Sheets receives a trip only when (a) it's an auto-detected plate
/// matched to the reference DB (auto-pushed), or (b) it's a manual entry the
/// officer classified as discharge (Yes) with confirmation. Non-discharge and
/// unclassified manual entries stay local — never exported.
#[test]
fn sheets_pushes_auto_matches_but_only_discharge_confirmed_manual() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let conn = ctx.conn();
    let sheets = MockSheets::new();

    // Auto-detected plate matched to the DB → auto-pushed, no human step.
    let auto = log_trip(&ctx, &admin, "A123AB");

    // Register a second vehicle for manual entry (avoids entry/exit matching
    // with the open auto trip for A123AB).
    reference::create_vehicle(
        ctx.state(), admin.id.clone(), "A223AB".into(),
        None, Some(15.0), "litres".into(), None, None,
    ).unwrap();

    // Manual entry logs but is UNclassified → must NOT reach the sheet.
    let manual = manual_entry_impl(&ctx.conn(), &admin.id, "A223AB", &ctx.frames_dir(), None)
        .unwrap()
        .trip
        .expect("manual exact match logs");
    assert_eq!(manual.capture_method, "manual_entry");
    assert_eq!(manual.is_discharge_trip, None, "logged but not yet classified");

    let first = run_sheets_sync_impl(&conn, &sheets).unwrap();
    assert_eq!(first.pushed, 1, "only the auto trip is pushed; unclassified manual stays local");
    assert_eq!(pushed_to_sheets_flag(&conn, &auto.id), 1);
    assert_eq!(pushed_to_sheets_flag(&conn, &manual.id), 0, "unclassified manual must not export");

    // Classify the manual entry as NON-discharge (No) → stays local forever.
    classify_discharge_impl(&ctx.conn(), &manual.id, &admin.id, false).expect("classify non-discharge");
    let second = run_sheets_sync_impl(&conn, &sheets).unwrap();
    assert_eq!(second.pushed, 0, "non-discharge manual entry never reaches the sheet");
    assert_eq!(pushed_to_sheets_flag(&conn, &manual.id), 0);

    // Another manual entry classified as discharge (Yes) → exported.
    let discharge = manual_entry_impl(&ctx.conn(), &admin.id, "A123AB", &ctx.frames_dir(), None)
        .unwrap()
        .trip
        .expect("manual exact match logs");
    classify_discharge_impl(&ctx.conn(), &discharge.id, &admin.id, true).expect("classify discharge");
    let third = run_sheets_sync_impl(&conn, &sheets).unwrap();
    assert_eq!(third.pushed, 1, "discharge-confirmed manual entry is exported");
    assert_eq!(pushed_to_sheets_flag(&conn, &discharge.id), 1);
}

// ---------------------------------------------------------------------------
// Command layer: gating, audit, frequency, connectivity simulation
// ---------------------------------------------------------------------------

/// All integration commands are gated on `manage_integrations` (05 §6f) and
/// meaningful actions are audit-logged with provenance.
#[test]
fn sync_commands_are_permission_gated_and_audited() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);

    let err = connect_google_sheets(ctx.state(), gate.id.clone(), None, None, "realtime".into())
        .expect_err("gate officer must not connect sheets");
    assert!(err.contains("permission"));

    let err2 = sync_now_pg(ctx.state(), gate.id.clone()).expect_err("gate officer must not trigger pg sync");
    assert!(err2.contains("permission"));

    let err3 = set_google_sheets_frequency(ctx.state(), gate.id.clone(), "every_15_min".into())
        .expect_err("gate officer must not change frequency");
    assert!(err3.contains("permission"));

    let st = connect_google_sheets(ctx.state(), admin.id.clone(), Some("sheet-123".into()), Some("ops@acme".into()), "realtime".into())
        .expect("admin connects sheets");
    assert!(st.connected);
    assert_eq!(st.target_sheet_id.as_deref(), Some("sheet-123"));
    assert_eq!(st.shared_group.as_deref(), Some("ops@acme"));
    assert_eq!(st.frequency, "realtime");

    let st2 = disconnect_google_sheets(ctx.state(), admin.id.clone()).expect("admin disconnects");
    assert!(!st2.connected);
    assert_eq!(st2.status, "disconnected");

    let audited: i64 = ctx
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action IN ('connected_google_sheets', 'disconnected_google_sheets')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audited, 2, "connect and disconnect are both audit-logged");
}

/// Frequency is validated (realtime | every_15_min), changes are pushed to the
/// live integrations row and audit-logged.
#[test]
fn sheets_frequency_validated_and_change_pushed_live() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    connect_google_sheets(ctx.state(), admin.id.clone(), None, None, "realtime".into()).unwrap();

    let st = set_google_sheets_frequency(ctx.state(), admin.id.clone(), "every_15_min".into()).unwrap();
    assert_eq!(st.frequency, "every_15_min");

    let stored: String = ctx
        .conn()
        .query_row(
            "SELECT sync_frequency FROM integrations WHERE type = 'google_sheets'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "every_15_min", "frequency change is persisted live");

    let bad = set_google_sheets_frequency(ctx.state(), admin.id.clone(), "hourly".into())
        .expect_err("invalid frequency rejected");
    assert!(bad.contains("realtime") && bad.contains("every_15_min"));

    let audited: i64 = ctx
        .conn()
        .query_row("SELECT COUNT(*) FROM audit_log WHERE action = 'set_google_sheets_frequency'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(audited, 1);
}

/// Dev-only connectivity simulation drives the reported status and both sync
/// engines, proving offline-first behavior through the real command layer.
#[test]
fn connectivity_simulation_toggles_status_and_sync() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);
    let trip = log_trip(&ctx, &admin, "A123AB");

    // Connect sheets so its integration state is genuinely "connected".
    connect_google_sheets(ctx.state(), admin.id.clone(), None, None, "realtime".into()).unwrap();

    // Default: Postgres adapter up, Sheets adapter up + OAuth connected.
    let st0 = sync_status(ctx.state()).expect("status");
    assert!(st0.online);
    assert!(st0.pg.connected);
    assert!(st0.sheets.connected, "connected integration + online adapter");

    // Go fully offline: reported offline, both engines push nothing.
    simulate_connectivity(ctx.state(), false, false).unwrap();
    let st1 = sync_status(ctx.state()).expect("status");
    assert!(!st1.online);
    assert!(!st1.sheets.connected, "adapter offline overrides connected integration");

    let pg_off = sync_now_pg(ctx.state(), admin.id.clone()).expect("offline pg sync is best-effort, never an error");
    assert_eq!(pg_off.pushed, 0);
    let sh_off = sync_now_sheets(ctx.state(), admin.id.clone()).expect("offline sheets sync is best-effort, never an error");
    assert_eq!(sh_off.pushed, 0);

    // Reconnect: everything flows again.
    simulate_connectivity(ctx.state(), true, true).unwrap();
    let st2 = sync_status(ctx.state()).expect("status");
    assert!(st2.online);
    assert!(st2.sheets.connected);

    let pg_on = sync_now_pg(ctx.state(), admin.id.clone()).expect("reconnected pg sync succeeds");
    assert!(pg_on.pushed >= 1);
    let sh_on = sync_now_sheets(ctx.state(), admin.id.clone()).expect("reconnected sheets sync succeeds");
    assert_eq!(sh_on.pushed, 1, "logged trip exported on reconnect");

    let conn = ctx.conn();
    assert_eq!(synced_flag(&conn, "trips", &trip.id), 1);
    assert_eq!(pushed_to_sheets_flag(&conn, &trip.id), 1);

    let audited: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action IN ('manual_postgres_sync', 'manual_sheets_sync')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audited, 4, "both manual sync triggers are audited (offline + reconnect runs)");
}

/// The real Postgres adapter reports an honest unconfigured/offline state with
/// an explanatory error, and rejects malformed connection strings.
#[test]
fn real_postgres_adapter_reports_unconfigured_state() {
    let pg = RealPostgres::new();
    assert!(!pg.configured());
    assert!(!pg.connected());
    assert!(pg.last_error().is_some());
    assert!(pg.configure(Some("garbage connection string".into())).is_err());
}

/// Configuring Postgres persists the connection string, audits the change and
/// flips the state view; disconnecting clears it again. (Mock adapter, so no
/// network is involved; the real adapter path is covered by the live test.)
#[test]
fn postgres_configuration_persists_and_audits() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);

    let err = configure_postgres(ctx.state(), gate.id.clone(), "postgresql://x".into())
        .expect_err("gate officer must not configure postgres");
    assert!(err.contains("permission"));

    let st = configure_postgres(ctx.state(), admin.id.clone(), "postgresql://postgres@127.0.0.1:5432/truckflow_central".into())
        .expect("admin configures postgres");
    assert!(st.configured, "adapter reports configured after connect");

    let conn = ctx.conn();
    let saved: String = conn
        .query_row("SELECT value FROM app_settings WHERE key = 'pg_connection_string'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(saved, "postgresql://postgres@127.0.0.1:5432/truckflow_central");

    let st2 = disconnect_postgres(ctx.state(), admin.id.clone()).expect("admin disconnects");
    assert!(!st2.configured);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM app_settings WHERE key = 'pg_connection_string'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "connection string removed on disconnect");

    let audited: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action IN ('configured_postgres', 'disconnected_postgres')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audited, 2);
}

/// Sheets configuration requires the JSON + sheet id, persists the credentials
/// and the integrations row, and surfaces the service-account email.
#[test]
fn sheets_configuration_persists_and_audits() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);

    let err = configure_google_sheets(
        ctx.state(),
        gate.id.clone(),
        "{}".into(),
        "sheet-1".into(),
        None,
        "realtime".into(),
    )
    .expect_err("gate officer must not configure sheets");
    assert!(err.contains("permission"));

    let err2 = configure_google_sheets(
        ctx.state(),
        admin.id.clone(),
        "{}".into(),
        "".into(),
        None,
        "realtime".into(),
    )
    .expect_err("empty sheet id must be rejected");
    assert!(err2.contains("sheet"));

    let st = configure_google_sheets(
        ctx.state(),
        admin.id.clone(),
        "{ \"client_email\": \"svc@acme.iam.gserviceaccount.com\" }".into(),
        "sheet-1".into(),
        Some("ops@acme".into()),
        "every_15_min".into(),
    )
    .expect("admin configures sheets");
    assert!(st.configured);
    assert_eq!(st.service_account_email.as_deref(), Some("svc@acme.iam.gserviceaccount.com"));
    assert_eq!(st.frequency, "every_15_min");

    let conn = ctx.conn();
    let saved: String = conn
        .query_row("SELECT value FROM app_settings WHERE key = 'sheets_service_account_json'", [], |r| r.get(0))
        .unwrap();
    assert!(saved.contains("svc@acme"));

    let st2 = disconnect_google_sheets(ctx.state(), admin.id.clone()).expect("admin disconnects");
    assert!(!st2.configured);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM app_settings WHERE key IN ('sheets_service_account_json', 'sheets_target_sheet_id')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "credentials removed on disconnect");
}

/// Live end-to-end check against the local PostgreSQL (Phase 4 exit criteria:
/// real driver, database auto-creation, schema mirror, UUID upsert). Skipped
/// gracefully when no local server is reachable, so the suite never depends on
/// the machine having Postgres installed.
#[test]
fn live_local_postgres_sync_roundtrip() {
    let base = "postgresql://postgres@127.0.0.1:5432";
    let dbname = format!("truckflow_it_{}_{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let conn_string = format!("{base}/{dbname}");

    let pg = RealPostgres::new();
    if let Err(e) = pg.configure(Some(conn_string.clone())) {
        eprintln!("SKIP live postgres test — no reachable local server: {e}");
        return;
    }

    // Push a reference row and a trip row; both must be acked and upserted.
    let company = json!({ "id": "c-1", "name": "Acme", "status": "active", "extra_fields": null, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z", "synced": 0 });
    let trip = json!({ "id": "t-1", "vehicle_id": null, "driver_id": null, "company_id": "c-1", "capacity_at_trip": 40.5, "time_in": "2026-01-01T10:00:00Z", "receipt_no": "R-1", "officer_id": null, "capture_method": "auto", "confidence_score": 0.95, "photo_refs": null, "status": "logged", "resolution_notes": null, "pushed_to_sheets": 0, "created_at": "2026-01-01T10:00:00Z", "updated_at": "2026-01-01T10:00:00Z", "synced": 0 });

    let acked1 = pg.push_rows("companies", &[company]).expect("companies push acked");
    assert_eq!(acked1, vec!["c-1".to_string()]);
    let acked2 = pg.push_rows("trips", &[trip]).expect("trips push acked");
    assert_eq!(acked2, vec!["t-1".to_string()]);

    // Idempotency: re-pushing the same ids is safe (ON CONFLICT DO UPDATE).
    let again = pg.push_rows("companies", &[json!({ "id": "c-1", "name": "Acme Renamed", "status": "active", "extra_fields": null, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z", "synced": 0 })]).expect("idempotent re-push");
    assert_eq!(again, vec!["c-1".to_string()]);

    // Verify the central rows directly.
    let pg2 = RealPostgres::new();
    pg2.configure(Some(conn_string.clone())).expect("reconnect");
    assert!(pg2.connected());
    let check = pg2.push_rows("companies", &[]).expect("empty push ok");
    assert!(check.is_empty());

    // Cleanup: close our connections, then drop the throwaway database.
    let _ = pg.configure(None);
    let _ = pg2.configure(None);
    let admin_cs = format!("{base}/postgres");
    let mut admin = admin_cs
        .parse::<postgres::Config>()
        .expect("admin config")
        .connect(postgres::NoTls)
        .expect("connect to maintenance db");
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{}\"", dbname.replace('"', "\"\"")))
        .expect("drop throwaway db");
}
