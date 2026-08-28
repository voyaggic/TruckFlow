//! Phase 3 exit-criteria checklist tests (04-capture-pipeline.md §6/§9,
//! 05-ui-screens.md §3): verification-queue resolution paths and photo/frame
//! evidence retention. Drives the real pipeline + resolution logic against a
//! fresh temp database and temp frame directory.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::test::mock_app;
use tauri::{App, Manager, State};

use truckflow_lib::capture::{
    approve_trip_impl, classify_discharge_impl, decline_trip_impl, discard_trip_impl, ingest_read, list_declined,
    purge_declined_impl, resolve_queued_existing_impl, resolve_queued_new_impl, SimulatorSource,
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
        let dir = std::env::temp_dir().join(format!("truckflow_p3test_{}", uuid::Uuid::new_v4()));
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

    fn conn(&self) -> Connection {
        Connection::open(self._tmp.db_path()).expect("reopen db")
    }

    fn frames_dir(&self) -> PathBuf {
        self._tmp.dir.join("frames")
    }

    fn create_admin(&self) -> SessionUser {
        commands::create_first_admin(self.state(), "Boss".to_string(), ADMIN_PASS.to_string())
            .expect("create first admin")
            .user
    }
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

/// Create one company + driver + two vehicles (A123AB cap 20, A223AB cap 21);
/// returns veh_a.
fn seed_reference(ctx: &TestCtx, admin: &SessionUser) -> VehicleView {
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
    reference::create_vehicle(
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
    veh_a
}

/// Queue a trip through a realistic exception path: a `no_match` read.
fn queue_no_match(ctx: &TestCtx, plate: &str, timestamp: &str) -> truckflow_lib::models::TripView {
    let res = ingest_read(&ctx.conn(), None, &read(plate, 0.9, timestamp), "auto", &ctx.frames_dir()).unwrap();
    res.queued.expect("no-match read must queue")
}

/// Frame files exist on disk for a trip and the photo_refs point at them.
fn assert_frames_persisted(ctx: &TestCtx, trip_id: &str, expected: usize) {
    let photo_refs: String = ctx
        .conn()
        .query_row(
            "SELECT COALESCE(photo_refs, '[]') FROM trips WHERE id = ?1",
            rusqlite::params![trip_id],
            |r| r.get(0),
        )
        .unwrap();
    let refs: Vec<serde_json::Value> = serde_json::from_str(&photo_refs).unwrap();
    assert_eq!(refs.len(), expected, "photo_refs must list every captured frame");
    for entry in &refs {
        let file = entry.get("file").and_then(|v| v.as_str()).expect("each frame has a file");
        assert!(
            ctx.frames_dir().join(file).exists(),
            "frame file must exist on disk for {file}"
        );
    }
}

// ---------------------------------------------------------------------------
// 04-capture-pipeline.md §6 queue resolution + 05-ui-screens.md §3
// ---------------------------------------------------------------------------

/// §6/§9: frames are persisted to disk for EVERY trip, logged or queued, and
/// remain retrievable — evidence never depends on how a trip was captured.
#[test]
fn frames_persist_and_retrieve_for_logged_and_queued() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    // Queued (no-match) trip retains all 3 frames on disk.
    let queued = queue_no_match(&ctx, "X999ZZ", &now_iso());
    assert_frames_persisted(&ctx, &queued.id, 3);

    // Fully-automatic mode → instantly logged trip also retains all frames.
    set_fully_automatic(&ctx, &admin);
    let res = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    let logged = res.trip.expect("fully-automatic exact match logs instantly");
    assert_eq!(logged.status, "logged");
    assert_frames_persisted(&ctx, &logged.id, 3);

    // Retrieval path returns display payloads for both.
    for trip_id in [queued.id.as_str(), logged.id.as_str()] {
        let frames = truckflow_lib::evidence::trip_evidence(&ctx.conn(), &ctx.frames_dir(), trip_id).unwrap();
        assert_eq!(frames.len(), 3, "every frame retrievable for {trip_id}");
    }
}

fn set_fully_automatic(ctx: &TestCtx, admin: &SessionUser) {
    let state = ctx.state();
    truckflow_lib::capture::set_capture_settings(
        state,
        admin.id.clone(),
        Some("fully_automatic".to_string()),
        None,
        None,
        None,
        None,
    )
    .unwrap();
}

/// §6: confirm existing match finalizes the trip, fills point-in-time fields
/// from the selected vehicle, records the resolver, and never touches time_in.
#[test]
fn confirm_existing_match_resolves_and_keeps_time_in() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let veh_a = seed_reference(&ctx, &admin);

    let captured_at = iso_minutes_ago(30);
    let queued = queue_no_match(&ctx, "A123AB", &captured_at);
    assert_eq!(queued.plate_number, "A123AB", "best-guess plate shown for the officer");

    let resolved = resolve_queued_existing_impl(
        &ctx.conn(),
        &queued.id,
        &admin.id,
        &veh_a.id,
        None,
        None,
        None,
        "litres".into(),
        None,
    )
    .expect("confirm existing must resolve");
    assert_eq!(resolved.status, "logged");
    assert_eq!(resolved.capacity_at_trip, Some(20.0), "point-in-time capacity from the vehicle");
    assert_eq!(resolved.company_name.as_deref(), Some("Acme Waste"));
    assert_eq!(resolved.driver_name.as_deref(), Some("D. Singh"));
    assert_eq!(resolved.time_in, captured_at, "resolution must never rewrite the capture time");

    // Attribution: resolver + resolution recorded in resolution_notes.
    let notes: String = ctx
        .conn()
        .query_row(
            "SELECT resolution_notes FROM trips WHERE id = ?1",
            rusqlite::params![queued.id],
            |r| r.get(0),
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&notes).unwrap();
    assert_eq!(v["resolution"], "confirm_existing");
    assert_eq!(v["resolved_by"], admin.id);
    assert!(v["resolved_at"].is_string());

    // Resolving twice is rejected — queue items resolve once.
    let err = resolve_queued_existing_impl(
        &ctx.conn(),
        &queued.id,
        &admin.id,
        &veh_a.id,
        None,
        None,
        None,
        "litres".into(),
        None,
    )
    .expect_err("already-resolved trip must not re-resolve");
    assert!(err.contains("not awaiting resolution"));
}

/// §6/§3: officer can edit auto-filled fields inline before confirming, and the
/// edit lands on the trip (still keeping the original capture time).
#[test]
fn confirm_existing_with_inline_edit() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let veh_a = seed_reference(&ctx, &admin);

    let captured_at = iso_minutes_ago(45);
    let queued = queue_no_match(&ctx, "A123AB", &captured_at);

    let resolved = resolve_queued_existing_impl(
        &ctx.conn(),
        &queued.id,
        &admin.id,
        &veh_a.id,
        None,
        None,
        Some(33.0),
        "litres".into(),
        Some("RC-7".to_string()),
    )
    .expect("confirm with edits must resolve");
    assert_eq!(resolved.status, "logged");
    assert_eq!(resolved.capacity_at_trip, Some(33.0), "inline edit wins over the vehicle default");
    assert_eq!(resolved.receipt_no.as_deref(), Some("RC-7"));
    assert_eq!(resolved.time_in, captured_at, "editing fields must never rewrite time_in");
}

/// §6/§3: "register new vehicle" path creates the vehicle in the reference DB,
/// logs the trip against it, and the vehicle is usable for future trips.
#[test]
fn register_new_vehicle_resolves_and_creates_reference_record() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let captured_at = iso_minutes_ago(20);
    let queued = queue_no_match(&ctx, "Z777ZC", &captured_at);

    let resolved = resolve_queued_new_impl(
        &ctx.conn(),
        &queued.id,
        &admin.id,
        "z 777-zc",
        None,
        Some(44.0),
        "litres".into(),
        None,
        false,
        None,
    )
    .expect("register new must resolve");
    assert_eq!(resolved.status, "logged");
    assert_eq!(resolved.plate_number, "Z777ZC", "normalized plate lands on the trip");
    assert_eq!(resolved.capacity_at_trip, Some(44.0));
    assert_eq!(resolved.capacity_unit, "litres", "capacity unit defaults to litres on register-new");
    assert_eq!(resolved.time_in, captured_at);

    // The vehicle now exists in the reference DB (active) and matches future reads.
    let vehicles = reference::list_vehicles(ctx.state(), None).unwrap();
    let created = vehicles.iter().find(|v| v.plate_number == "Z777ZC").expect("vehicle registered");
    assert_eq!(created.status, "active");
    assert_eq!(created.registered_capacity, Some(44.0));
    assert_eq!(created.capacity_unit, "litres", "new vehicle stores its capacity unit");

    // A fresh read of the same plate now resolves to it exactly.
    let res = ingest_read(&ctx.conn(), None, &read("Z777ZC", 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(res.outcome.state, "exact");
    assert_eq!(res.outcome.matched_vehicle_id.as_deref(), Some(created.id.as_str()));
}

/// §3: duplicate-plate warning — typing an existing plate under "register new"
/// is rejected unless the officer explicitly confirms reuse.
#[test]
fn register_new_warns_on_duplicate_plate() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let queued = queue_no_match(&ctx, "X111XX", &now_iso());
    let err = resolve_queued_new_impl(
        &ctx.conn(),
        &queued.id,
        &admin.id,
        "A123AB",
        None,
        None,
        "litres".into(),
        None,
        false,
        None,
    )
    .expect_err("existing plate must trigger duplicate-plate warning");
    assert!(err.contains("already registered"), "warning must say the plate exists");

    // Re-submit with confirmation → attaches to the existing vehicle.
    let resolved = resolve_queued_new_impl(
        &ctx.conn(),
        &queued.id,
        &admin.id,
        "A123AB",
        None,
        None,
        "litres".into(),
        None,
        true,
        None,
    )
    .expect("confirmed reuse must resolve");
    assert_eq!(resolved.status, "logged");
    assert_eq!(resolved.plate_number, "A123AB");
}

/// §6: Discard marks the trip `discarded` (no counted trip), retains the row and
/// frames, and never rewrites time_in.
#[test]
fn discard_marks_trip_not_counted_and_keeps_evidence() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let captured_at = iso_minutes_ago(60);
    let queued = queue_no_match(&ctx, "FAKE-PL", &captured_at);
    assert_frames_persisted(&ctx, &queued.id, 3);

    let discarded = discard_trip_impl(&ctx.conn(), &queued.id, &admin.id).expect("discard must succeed");
    assert_eq!(discarded.status, "discarded");
    assert_eq!(discarded.time_in, captured_at);

    // Row still exists with its evidence — nothing deleted.
    let count: i64 = ctx
        .conn()
        .query_row("SELECT COUNT(*) FROM trips WHERE id = ?1", rusqlite::params![queued.id], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "discarded rows are retained, never deleted");
    assert_frames_persisted(&ctx, &queued.id, 3);

    let notes: String = ctx
        .conn()
        .query_row(
            "SELECT resolution_notes FROM trips WHERE id = ?1",
            rusqlite::params![queued.id],
            |r| r.get(0),
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&notes).unwrap();
    assert_eq!(v["resolution"], "discarded");
    assert_eq!(v["resolved_by"], admin.id);
}

/// 08 §9 (R1): a declined read is saved locally with status `declined`, excluded
/// from the main trip views/queue, and purgeable — the only physical-delete path.
#[test]
fn declined_saved_locally_excluded_and_purgeable() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let captured_at = iso_minutes_ago(20);
    let res = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &captured_at), "auto", &ctx.frames_dir()).unwrap();
    let queued = res.queued.expect("confirm-required capture queues");
    assert_eq!(queued.status, "queued");

    // Decline keeps the row locally with status `declined` and preserves time_in.
    let declined = decline_trip_impl(&ctx.conn(), &queued.id, &admin.id).expect("decline must succeed");
    assert_eq!(declined.status, "declined");
    assert_eq!(declined.time_in, captured_at);

    // Excluded from the main trip listing and the queue.
    let listed: i64 = ctx
        .conn()
        .query_row("SELECT COUNT(*) FROM trips WHERE id = ?1 AND status = 'declined'", rusqlite::params![queued.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(listed, 1, "declined row exists locally");
    let in_queue: i64 = ctx
        .conn()
        .query_row("SELECT COUNT(*) FROM trips WHERE status = 'queued'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(in_queue, 0, "declined trip is not in the verification queue");
    let declined_list = list_declined(ctx.state()).expect("declined list command works");
    assert_eq!(declined_list.len(), 1);
    assert_eq!(declined_list[0].id, queued.id);
    assert_frames_persisted(&ctx, &queued.id, 3);

    // Only `declined` rows can be purged; the row + its frames are then removed.
    let not_declined_err = purge_declined_impl(&ctx.conn(), &ctx.frames_dir(), "00000000-0000-0000-0000-000000000000", &admin.id)
        .expect_err("missing trip must not purge");
    assert!(not_declined_err.contains("not found"));
    purge_declined_impl(&ctx.conn(), &ctx.frames_dir(), &queued.id, &admin.id).expect("purge declined");
    let gone: i64 = ctx
        .conn()
        .query_row("SELECT COUNT(*) FROM trips WHERE id = ?1", rusqlite::params![queued.id], |r| r.get(0))
        .unwrap();
    assert_eq!(gone, 0, "purged declined row is physically removed");
    assert!(!ctx.frames_dir().join(&queued.id).exists(), "purge removes the trip's frame directory");
}

/// 08 §9 (R7): `is_discharge_trip` stays null until the officer classifies a
/// logged trip; classification applies to logged trips only.
#[test]
fn discharge_classification_applies_to_logged_only_and_stays_null_until_classified() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let res = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    let queued = res.queued.expect("confirm-required capture queues");
    assert_eq!(queued.is_discharge_trip, None, "queued trip has no discharge classification yet");

    // Queued trips cannot be classified — the officer must finalize first.
    let err = classify_discharge_impl(&ctx.conn(), &queued.id, &admin.id, true).expect_err("queued not classifiable");
    assert!(err.contains("logged"));

    let logged = approve_trip_impl(&ctx.conn(), &queued.id, &admin.id).unwrap();
    assert_eq!(logged.status, "logged");
    assert_eq!(logged.is_discharge_trip, None, "approval alone leaves classification null (two-step confirm)");

    let discharge = classify_discharge_impl(&ctx.conn(), &queued.id, &admin.id, true).expect("classify discharge");
    assert_eq!(discharge.is_discharge_trip, Some(true));

    let non_discharge = classify_discharge_impl(&ctx.conn(), &queued.id, &admin.id, false).expect("classify non-discharge");
    assert_eq!(non_discharge.is_discharge_trip, Some(false), "re-classification updates the flag");
}

/// §5: consent-mode confirm-required queues exact matches as pending approval,
/// and one-tap approval logs them while preserving the capture time.
#[test]
fn pending_approval_queues_and_approves() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let captured_at = iso_minutes_ago(11);
    let res = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &captured_at), "auto", &ctx.frames_dir()).unwrap();
    let queued = res.queued.expect("confirm-required exact match queues");
    assert_eq!(queued.status, "queued");
    assert_eq!(queued.reason.as_deref(), Some("pending_approval"));
    assert_frames_persisted(&ctx, &queued.id, 3);

    let approved = approve_trip_impl(&ctx.conn(), &queued.id, &admin.id).unwrap();
    assert_eq!(approved.status, "logged");
    assert_eq!(approved.time_in, captured_at, "approval preserves the capture time");

    let err = approve_trip_impl(&ctx.conn(), &queued.id, &admin.id).expect_err("double approval rejected");
    assert!(err.contains("Only trips awaiting approval"));
}

/// §3: all four exception reasons surface as explicit, machine-checkable flags
/// alongside their evidence.
#[test]
fn every_queue_reason_flag_is_explicit() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let _veh_a = seed_reference(&ctx, &admin);

    let multiple = ingest_read(&ctx.conn(), None, &read("A*23AB", 0.8, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(multiple.queued.unwrap().reason.as_deref(), Some("multiple_matches"));

    let none = ingest_read(&ctx.conn(), None, &read("X999ZZ", 0.8, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(none.queued.unwrap().reason.as_deref(), Some("no_match"));

    let low = ingest_read(&ctx.conn(), None, &read("A123AB", 0.5, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(low.queued.unwrap().reason.as_deref(), Some("low_confidence"));

    // A second registered plate with no prior read in the window → pending approval.
    let r1 = ingest_read(&ctx.conn(), None, &read("A223AB", 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(r1.queued.unwrap().reason.as_deref(), Some("pending_approval"));

    // A123AB already has a queued trip from the same moment — handled exactly
    // like any other capture (no time-based duplicate detection, 08 §8).
    let r2 = ingest_read(&ctx.conn(), None, &read("A123AB", 0.95, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    assert_eq!(r2.queued.unwrap().reason.as_deref(), Some("pending_approval"));
}

/// §3: frame evidence retrieves back as base64 payloads (or placeholder images
/// from the simulator) for the verification screen.
#[test]
fn trip_frames_retrieve_display_payloads() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_reference(&ctx, &admin);

    let queued = queue_no_match(&ctx, "X777XY", &now_iso());
    let frames = truckflow_lib::evidence::trip_evidence(&ctx.conn(), &ctx.frames_dir(), &queued.id).unwrap();
    assert_eq!(frames.len(), 3, "all frames retrievable");
    for frame in &frames {
        assert_eq!(frame.kind, "test");
        assert!(
            frame.data_base64.is_some(),
            "simulator/placeholder frames still yield a display payload"
        );
    }
}

/// 08 §6.2 (R8): low-confidence reads are auto-flagged into `training_candidates`
/// at ingest; human-corrected queue resolutions are flagged at resolve (dedup'd —
/// a corrected low-confidence read keeps its ingest-time row).
#[test]
fn training_candidates_flag_low_confidence_and_human_corrected() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let veh_a = seed_reference(&ctx, &admin);

    // Low-confidence read → auto-flagged with reason `low_confidence`, one row
    // per retained frame, frame_ref pointing at the persisted file.
    let low = ingest_read(&ctx.conn(), None, &read("A123AB", 0.5, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    let queued = low.queued.unwrap();
    assert_eq!(queued.reason.as_deref(), Some("low_confidence"));
    let rows = candidate_rows(&ctx, &queued.id);
    assert_eq!(rows.len(), 3, "one candidate per frame");
    assert!(rows.iter().all(|(_fr, reason)| reason == "low_confidence"));
    let mut files: Vec<&str> = rows.iter().map(|(fr, _)| fr.as_str()).collect();
    files.dedup();
    assert_eq!(files.len(), 3, "distinct frame files flagged");
    for fr in &files {
        let prefix = format!("{}/", queued.id);
        assert!(
            fr.starts_with(&prefix) && fr.ends_with(".png"),
            "frame_ref must reference the persisted frame file, got {fr}"
        );
    }

    // Resolving the low-confidence trip does NOT duplicate its rows.
    resolve_queued_existing_impl(
        &ctx.conn(),
        &queued.id,
        &admin.id,
        &veh_a.id,
        None,
        None,
        None,
        "litres".into(),
        None,
    )
    .expect("resolve corrected low-confidence trip");
    assert_eq!(candidate_rows(&ctx, &queued.id).len(), 3, "no duplicate candidates on correction");

    // A multiple-matches trip is flagged only once the human corrects it.
    let multi = ingest_read(&ctx.conn(), None, &read("A*23AB", 0.8, &now_iso()), "auto", &ctx.frames_dir()).unwrap();
    let multi_q = multi.queued.unwrap();
    assert_eq!(candidate_rows(&ctx, &multi_q.id).len(), 0, "not flagged until a human resolves");
    resolve_queued_existing_impl(
        &ctx.conn(),
        &multi_q.id,
        &admin.id,
        &veh_a.id,
        None,
        None,
        None,
        "litres".into(),
        None,
    )
    .expect("resolve ambiguous trip");
    let multi_rows = candidate_rows(&ctx, &multi_q.id);
    assert_eq!(multi_rows.len(), 3, "human-corrected resolution flags all frames");
    assert!(multi_rows.iter().all(|(_, reason)| reason == "human_corrected"));

    // Manual entry has no frames → nothing flagged.  Under §4.3 entry/exit,
    // manual entry of A123AB closes the earlier open trip as its exit, so
    // logged.id is the old trip (which already has candidates from the low-
    // confidence flagging). Use A223AB (registered, no open trip).
    let manual = truckflow_lib::capture::manual_entry_impl(&ctx.conn(), &admin.id, "A223AB", &ctx.frames_dir()).unwrap();
    let logged = manual.trip.expect("manual exact match logs");
    assert_eq!(candidate_rows(&ctx, &logged.id).len(), 0, "manual entries carry no frames to flag");
}

fn candidate_rows(ctx: &TestCtx, trip_id: &str) -> Vec<(String, String)> {
    let conn = ctx.conn();
    let mut stmt = conn
        .prepare("SELECT frame_ref, reason FROM training_candidates WHERE source_trip_id = ?1 ORDER BY frame_ref")
        .unwrap();
    stmt.query_map(rusqlite::params![trip_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

