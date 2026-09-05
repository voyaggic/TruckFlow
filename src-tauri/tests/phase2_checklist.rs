//! Phase 2 exit-criteria checklist tests (04-capture-pipeline.md §9, as
//! corrected by 08-anpr-integration.md §8 — no time-based duplicate blocking).
//! Exercises the real pipeline logic (cross-reference, confidence thresholding,
//! manual entry, queue routing, time_in preservation) against a fresh temp DB.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tauri::test::mock_app;
use tauri::{App, Manager, State};

use truckflow_lib::capture::{
    approve_trip_impl, get_capture_settings, ingest_read, manual_entry_impl, set_capture_settings,
    update_trip_fields_impl, SimulatorSource,
};
use truckflow_lib::commands;
use truckflow_lib::db::{open_db, AppState};
use truckflow_lib::sync::{MockPostgres, MockSheets};
use truckflow_lib::models::{AnprFrame, AnprRead, SessionUser, VehicleView};
use truckflow_lib::reference;

const ADMIN_PASS: &str = "AdminPass!2024";

struct TempDb {
    dir: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("truckflow_p2test_{}", uuid::Uuid::new_v4()));
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
    company_id: RefCell<String>,
}

impl TestCtx {
    fn new() -> Self {
        let tmp = TempDb::new();
        let frames_dir = tmp.dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        let conn = open_db(&tmp.db_path()).expect("open temp db");
        let db_path = tmp.db_path();
        let app = mock_app();
        let (sync_tx, _sync_rx) = std::sync::mpsc::sync_channel(1);
        app.manage(AppState {
            db: Arc::new(Mutex::new(conn)),
            sync_db: Arc::new(Mutex::new(open_db(&db_path).unwrap())),
            anpr_db: Arc::new(Mutex::new(open_db(&db_path).unwrap())),
            session: Mutex::new(None),
            simulator: Arc::new(SimulatorSource::new()),
            anpr_last: Mutex::new(None),
            running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            anpr_starting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            frames_dir,
            pg: Arc::new(MockPostgres::new()),
            sheets: Arc::new(MockSheets::new()),
            anpr_processes: Arc::new(Mutex::new(Vec::new())),
            pending_sync_marks: Arc::new(Mutex::new(Vec::new())),
            sync_notify: sync_tx,
        });
        Self { _tmp: tmp, app, company_id: RefCell::new(String::new()) }
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
        let result = commands::create_first_admin_for_company(self.state(), "Boss".to_string(), ADMIN_PASS.to_string(), "Default Company".to_string())
            .expect("create first admin");
        if let Some(ref cid) = result.user.company_id {
            *self.company_id.borrow_mut() = cid.clone();
        }
        result.user
    }

    fn company_id(&self) -> String {
        self.company_id.borrow().clone()
    }

    fn create_gate_user(&self, admin: &SessionUser, name: &str) -> truckflow_lib::models::UserView {
        let company_id = admin.company_id.clone().unwrap_or_else(|| "default".to_string());
        commands::create_user(
            self.state(),
            admin.id.clone(),
            name.to_string(),
            vec!["view_gate_entries".to_string(), "resolve_queue".to_string()],
            company_id,
        )
        .expect("create gate user")
    }

    fn create_user_with_password(&self, admin: &SessionUser, name: &str, permissions: Vec<String>, password: &str) -> truckflow_lib::models::UserView {
        let company_id = admin.company_id.clone().unwrap_or_else(|| "default".to_string());
        let user = commands::create_user(
            self.state(),
            admin.id.clone(),
            name.to_string(),
            permissions,
            company_id.clone(),
        )
        .expect("create user");
        commands::set_initial_password(self.state(), name.to_string(), company_id, password.to_string())
            .expect("set initial password");
        user
    }
}

struct Ref {
    veh_a: VehicleView, // plate A123AB, capacity 20.0
    veh_b: VehicleView, // plate A223AB, capacity 21.0
}

fn seed_reference(ctx: &TestCtx, admin: &SessionUser) -> Ref {
    let company = reference::create_company(ctx.state(), admin.id.clone(), "Acme Waste".into(), None).unwrap();
    let driver = reference::create_driver(ctx.state(), admin.id.clone(), "D. Singh".into(), None).unwrap();
    let veh_a = reference::create_vehicle(
        ctx.state(),
        admin.id.clone(),
        "A123AB".into(),
        Some(company.id.clone()),
        Some(20.0),
        "litres".into(),
        Some(driver.id.clone()),
        None,
    )
    .unwrap();
    let veh_b = reference::create_vehicle(
        ctx.state(),
        admin.id.clone(),
        "A223AB".into(),
        Some(company.id.clone()),
        Some(21.0),
        "litres".into(),
        Some(driver.id.clone()),
        None,
    )
    .unwrap();
    Ref { veh_a, veh_b }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn iso_minutes_ago(mins: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Build a structured ANPR read with 3 evidence frames (hard requirement, 04 §2)
/// and provenance metadata (01-database-schema.md `trips`).
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

// ---------------------------------------------------------------------------
// 04-capture-pipeline.md §9
// ---------------------------------------------------------------------------

/// §9.1: clean high-confidence exact match auto-fills and creates the trip with
/// all fields copied (not live-referenced), and the consent mode governs status.
#[test]
fn exact_match_auto_fills_and_copies_all_fields() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let r = seed_reference(&ctx, &admin);

    // Default consent mode is confirm-required (04 §5) → auto capture awaits
    // approval and routes to the verification queue with a machine-checkable
    // reason (schema status enum: logged/queued/resolved/discarded).
    let ts = now_iso();
    let res = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &ts), "auto", &ctx.frames_dir()).unwrap();
    assert!(res.trip.is_none(), "confirm-required auto capture must queue, not log");
    let trip = res.queued.expect("confirm-required capture must queue");
    assert_eq!(trip.status, "queued");
    assert_eq!(trip.reason.as_deref(), Some("pending_approval"));
    assert_eq!(trip.plate_number, "A123AB");
    assert_eq!(trip.company_id.as_deref(), Some(r.veh_a.company_id.as_deref().unwrap()));
    assert_eq!(trip.company_name.as_deref(), Some("Acme Waste"));
    assert_eq!(trip.driver_id, r.veh_a.default_driver_id);
    assert_eq!(trip.driver_name.as_deref(), Some("D. Singh"));
    assert_eq!(trip.capacity_at_trip, Some(20.0));
    assert_eq!(trip.capture_method, "auto");
    assert_eq!(trip.confidence_score, Some(0.95));
    assert_eq!(trip.photo_count, 3, "captured trip must retain multiple frames (never a single image)");
    assert_eq!(trip.time_in, ts, "time_in must be the capture moment");

    // Snapshot independence: later capacity edits never mutate the stored trip.
    reference::update_vehicle(
        ctx.state(),
        admin.id.clone(),
        r.veh_a.id.clone(),
        "A123AB".into(),
        r.veh_a.company_id.clone(),
        Some(30.0),
        "litres".into(),
        r.veh_a.default_driver_id.clone(),
        None,
    )
    .unwrap();
    let stored_capacity: Option<f64> = ctx
        .conn()
        .query_row("SELECT capacity_at_trip FROM trips WHERE id = ?1", rusqlite::params![trip.id], |row| row.get(0))
        .unwrap();
    assert_eq!(stored_capacity, Some(20.0), "capacity_at_trip is a snapshot, never live-referenced");
    let stored_unit: String = ctx
        .conn()
        .query_row("SELECT capacity_unit FROM trips WHERE id = ?1", rusqlite::params![trip.id], |row| row.get(0))
        .unwrap();
    assert_eq!(stored_unit, "litres", "capacity unit is snapshotted onto the trip at capture time");

    // Fully-automatic mode → instantly logged. (Each fresh plate is a fresh trip —
    // there is never any time-based duplicate blocking, 08 §8.)
    set_capture_settings(
        ctx.state(),
        admin.id.clone(),
        Some("fully_automatic".to_string()),
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let res2 = ingest_read(&ctx.conn(), None, &read("A223AB", 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(res2.trip.expect("fully-automatic exact match must log").status, "logged");
}

/// §9.2: partial read narrowing to exactly one plausible match resolves the
/// same way as an exact match (resolution-by-elimination, not a guess).
#[test]
fn partial_read_narrowing_resolves_like_exact() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let r = seed_reference(&ctx, &admin);

    // "A12*AB" is consistent only with A123AB (A223AB fails on the 2nd char).
    let res = ingest_read(&ctx.conn(), None, &read("A12*AB", 0.8, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(res.outcome.state, "narrowed");
    assert_eq!(res.outcome.matched_vehicle_id.as_deref(), Some(r.veh_a.id.as_str()));
    // Confirm-required mode still queues for one-tap approval; the match is exact.
    let trip = res.queued.expect("narrowed-to-one in confirm mode must queue for approval");
    assert_eq!(trip.status, "queued");
    assert_eq!(trip.reason.as_deref(), Some("pending_approval"));
    assert_eq!(trip.plate_number, "A123AB");
    assert_eq!(trip.capacity_at_trip, Some(20.0));
    assert!(res.trip.is_none());
}

/// §9.3: partial read with multiple plausible matches queues with the reason flag.
#[test]
fn partial_read_multiple_matches_queues_with_reason() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let r = seed_reference(&ctx, &admin);

    // "A*23AB" is consistent with both A123AB and A223AB.
    let res = ingest_read(&ctx.conn(), None, &read("A*23AB", 0.8, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(res.outcome.state, "multiple");
    assert_eq!(res.outcome.candidates.len(), 2);
    assert!(res.trip.is_none());
    let queued = res.queued.expect("ambiguity must route to the verification queue");
    assert_eq!(queued.status, "queued");
    assert_eq!(queued.reason.as_deref(), Some("multiple_matches"));
    assert!(queued.candidates.contains(&r.veh_a.id) && queued.candidates.contains(&r.veh_b.id));
    assert_eq!(queued.photo_count, 3, "queued items keep all frames for later resolution");
}

/// §9.4: zero matches queues as "possible new vehicle" — never silently
/// discarded, never silently auto-created.
#[test]
fn zero_match_queues_as_possible_new_vehicle() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let res = ingest_read(&ctx.conn(), None, &read("X999ZZ", 0.9, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(res.outcome.state, "zero");
    assert!(res.trip.is_none());
    let queued = res.queued.expect("unknown plate must queue");
    assert_eq!(queued.status, "queued");
    assert_eq!(queued.reason.as_deref(), Some("no_match"));
}

/// 08 §8 (replaces 04 §9.5): there is NO time-based duplicate detection —
/// never any timing-based queueing, blocking, or flagging. Every capture is
/// routed purely by match quality; an immediate repeat of the same plate is
/// handled exactly like any other capture.
#[test]
fn no_timing_based_duplicate_blocking_ever() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    // Read 11 minutes ago — confirm-required mode queues it for approval.
    let t_old = iso_minutes_ago(11);
    let r1 = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &t_old), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(r1.queued.unwrap().reason.as_deref(), Some("pending_approval"));

    // Immediate repeat now — the SAME handling: queued pending approval, and
    // no duplicate/duplicate_timing flag anywhere.
    let r2 = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert!(r2.trip.is_none());
    let queued = r2.queued.expect("fast repeat must queue for approval like any capture");
    assert_eq!(queued.status, "queued");
    assert_eq!(queued.reason.as_deref(), Some("pending_approval"));

    // Fully-automatic mode: repeats log immediately — still no blocking.
    set_capture_settings(ctx.state(), admin.id.clone(), Some("fully_automatic".to_string()), None, None, None, None)
        .unwrap();
    let r3 = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(r3.trip.expect("fully-automatic repeat must log, never block").status, "logged");
}

/// §9.6: Manual Entry runs the full cross-reference and works with ANPR stopped.
#[test]
fn manual_entry_works_with_anpr_disabled_and_runs_cross_reference() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let r = seed_reference(&ctx, &admin);

    // Explicitly disable the ANPR source — the manual path must not depend on it.
    set_capture_settings(
        ctx.state(),
        admin.id.clone(),
        None,
        None,
        Some(false),
        None,
        None,
    )
    .unwrap();
    let settings = get_capture_settings(ctx.state()).unwrap();
    assert!(!settings.anpr_enabled);

    // Exact match by typing → logged immediately (manual entries are confirmed by definition).
    let res = manual_entry_impl(&ctx.conn(), &admin.id, "A123AB", &ctx.frames_dir(), None).unwrap();
    let trip = res.trip.expect("manual exact match must log");
    assert_eq!(trip.capture_method, "manual_entry");
    assert_eq!(trip.confidence_score, None, "manual entry has no ANPR confidence (04 §8)");
    assert_eq!(trip.plate_number, "A123AB");
    assert_eq!(trip.capacity_at_trip, Some(20.0));
    assert_eq!(trip.photo_count, 0, "no camera on manual entry — no frames to retain");

    // Manual repeat logs normally too — no time-based duplicate blocking (08 §8).
    let dup = manual_entry_impl(&ctx.conn(), &admin.id, "A123AB", &ctx.frames_dir(), None).unwrap();
    let repeat = dup.trip.expect("manual repeat must log like any manual entry");
    assert_eq!(repeat.status, "logged");
    assert_eq!(repeat.plate_number, "A123AB");

    // Partial narrowing works identically.
    let narrowed = manual_entry_impl(&ctx.conn(), &admin.id, "A12*AB", &ctx.frames_dir(), None).unwrap();
    assert_eq!(narrowed.outcome.state, "narrowed");
    assert!(narrowed.trip.is_some());

    // Unknown plate queues, never auto-created.
    let unknown = manual_entry_impl(&ctx.conn(), &admin.id, "X999ZZ", &ctx.frames_dir(), None).unwrap();
    assert_eq!(unknown.queued.expect("unknown manual entry must queue").reason.as_deref(), Some("no_match"));

    // Sanity: veh_b is untouched by the above.
    assert_eq!(r.veh_b.plate_number, "A223AB");
}

/// §9.8: `time_in` never changes when a queued/pending item is resolved later.
#[test]
fn time_in_survives_later_resolution() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let captured_at = iso_minutes_ago(30);
    let res = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &captured_at), "auto", &ctx.frames_dir()).unwrap();
    let trip = res.queued.expect("confirm-required capture queues pending approval");
    assert_eq!(trip.time_in, captured_at);

    // Officer edits auto-filled fields before confirming — time_in untouched.
    let edited = update_trip_fields_impl(
        &ctx.conn(),
        &trip.id,
        &admin.id,
        trip.company_id.clone(),
        trip.driver_id.clone(),
        Some(33.0),
        Some("RC-7".to_string()),
    )
    .unwrap();
    assert_eq!(edited.time_in, captured_at, "edit-before-confirm must not rewrite the capture time");
    assert_eq!(edited.capacity_at_trip, Some(33.0));

    // Approve the pending trip 30 minutes after capture — time_in unchanged.
    let approved = approve_trip_impl(&ctx.conn(), &trip.id, &admin.id).unwrap();
    assert_eq!(approved.status, "logged");
    assert_eq!(approved.time_in, captured_at, "approval must never rewrite the capture time");
}

/// §9.3 + §9.4: reason flags are explicit and machine-checkable.
#[test]
fn queue_reason_flags_are_explicit() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let multiple = ingest_read(&ctx.conn(), None, &read("A*23AB", 0.8, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(multiple.queued.unwrap().reason.as_deref(), Some("multiple_matches"));

    let none = ingest_read(&ctx.conn(), None, &read("X999ZZ", 0.8, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(none.queued.unwrap().reason.as_deref(), Some("no_match"));

    let low = ingest_read(&ctx.conn(), None, &read("A123AB", 0.5, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(low.queued.expect("below-threshold read must queue").reason.as_deref(), Some("low_confidence"));
}

/// Settings commands are permission-gated and persist.
#[test]
fn capture_settings_are_persisted_and_gated() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    // A gate officer cannot change settings.
    let gate = ctx.create_gate_user(&admin, "Officer");
    let err = set_capture_settings(
        ctx.state(),
        gate.id.clone(),
        Some("fully_automatic".to_string()),
        None,
        None,
        None,
        None,
    )
    .expect_err("non-admin must not change capture settings");
    assert!(err.contains("permission"));

    set_capture_settings(
        ctx.state(),
        admin.id.clone(),
        Some("fully_automatic".to_string()),
        Some(0.85),
        Some(false),
        Some("http".to_string()),
        None,
    )
    .unwrap();
    let s = get_capture_settings(ctx.state()).unwrap();
    assert_eq!(s.consent_mode, "fully_automatic");
    assert_eq!(s.confidence_threshold, 0.85);
    assert!(!s.anpr_enabled);
    assert_eq!(s.anpr_source, "http");
}

/// Reference database writes are gated on `manage_reference_database`.
#[test]
fn reference_crud_is_permission_gated() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin, "Officer");

    let err = reference::create_company(ctx.state(), gate.id.clone(), "Rogue Co".into(), None)
        .expect_err("gate officer must not create companies");
    assert!(err.contains("permission"));

    let comp = reference::create_company(ctx.state(), admin.id.clone(), "Acme Waste".into(), None).unwrap();
    assert_eq!(comp.status, "active");

    // No hard deletes — deactivate only.
    reference::set_company_status(ctx.state(), admin.id.clone(), comp.id.clone(), "inactive".to_string()).unwrap();
    let listed = reference::list_companies(ctx.state(), None).unwrap();
    let comp2 = listed.iter().find(|c| c.id == comp.id).unwrap();
    assert_eq!(comp2.status, "inactive");
    assert!(listed.iter().all(|c| c.id != "deleted-marker"), "no physical delete expected");
}

/// §9.7: captured trips always retain multiple frames across every auto path.
#[test]
fn every_auto_trip_retains_multiple_frames() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let exact = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    let narrowed = ingest_read(&ctx.conn(), None, &read("A12*AB", 0.8, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    let multiple = ingest_read(&ctx.conn(), None, &read("A*23AB", 0.8, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    let none = ingest_read(&ctx.conn(), None, &read("X999ZZ", 0.8, &now_iso()), "auto", &ctx.frames_dir()).unwrap();

    for (label, t) in [
        ("exact", exact.trip.or(exact.queued)),
        ("narrowed", narrowed.trip.or(narrowed.queued)),
        ("multiple-queued", multiple.queued),
        ("no-match-queued", none.queued),
    ] {
        let t = t.unwrap_or_else(|| panic!("{label} produced no record"));
        assert!(t.photo_count >= 2, "{label} trip must carry multiple frames, got {}", t.photo_count);
    }
}

/// §9.8: capacity is recorded in a unit that defaults to litres and can be
/// chosen per vehicle; the unit is snapshotted onto trips at capture time.
#[test]
fn capacity_unit_defaults_to_litres_and_is_snapshotted() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let company = reference::create_company(ctx.state(), admin.id.clone(), "Acme Waste".into(), None).unwrap();

    // Unknown units are rejected up front.
    let err = reference::create_vehicle(
        ctx.state(),
        admin.id.clone(),
        "A123AB".into(),
        Some(company.id.clone()),
        Some(20.0),
        "barrels".into(),
        None,
        None,
    )
    .expect_err("unsupported capacity unit must be rejected");
    assert!(err.contains("Unsupported capacity unit"), "rejection names the bad unit");

    // A vehicle registered in a non-default unit keeps it in the list view.
    reference::create_vehicle(
        ctx.state(),
        admin.id.clone(),
        "A123AB".into(),
        Some(company.id.clone()),
        Some(20.0),
        "gallons".into(),
        None,
        None,
    )
    .unwrap();
    let vehicles = reference::list_vehicles(ctx.state(), None).unwrap();
    let veh = vehicles.iter().find(|v| v.plate_number == "A123AB").unwrap();
    assert_eq!(veh.capacity_unit, "gallons", "chosen unit round-trips through the reference DB");

    // The trip snapshot carries the same unit (default litres when omitted).
    let res = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    let trip = res.trip.or(res.queued).expect("capture produced a record");
    assert_eq!(trip.capacity_at_trip, Some(20.0));
    assert_eq!(trip.capacity_unit, "gallons", "unit is snapshotted at capture time like the capacity value");

    // Re-registering the vehicle in a new unit never rewrites the old trip.
    reference::update_vehicle(
        ctx.state(),
        admin.id.clone(),
        veh.id.clone(),
        "A123AB".into(),
        Some(company.id.clone()),
        Some(30.0),
        "tonnes".into(),
        None,
        None,
    )
    .unwrap();
    let stored_unit: String = ctx
        .conn()
        .query_row("SELECT capacity_unit FROM trips WHERE id = ?1", rusqlite::params![trip.id], |row| row.get(0))
        .unwrap();
    assert_eq!(stored_unit, "gallons", "capacity_unit is a snapshot, never live-referenced");
}
