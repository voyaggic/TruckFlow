//! Reconciliation tests for R6 (ANPR Engine Configuration commands) and R9
//! (per-engine confidence thresholds + plate_vehicle_ratio_threshold), driven
//! by 08-anpr-integration.md §5 / §6 / §8 and 01-database-schema.md.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tauri::test::mock_app;
use tauri::{App, Manager, State};

use truckflow_lib::anpr::{
    add_camera_source, deploy_model_version, get_anpr_config, list_camera_sources, list_model_versions,
    list_training_candidates, register_model_version, rollback_model_version, set_camera_source_status,
    update_anpr_config, update_camera_source,
};
use truckflow_lib::capture::{set_capture_settings, SimulatorSource};
use truckflow_lib::commands;
use truckflow_lib::db::{open_db, AppState};
use truckflow_lib::sync::{MockPostgres, MockSheets};
use truckflow_lib::models::{AnprFrame, AnprRead, SessionUser};

const ADMIN_PASS: &str = "AdminPass!2024";

struct TempDb {
    dir: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("truckflow_anpr_{}", uuid::Uuid::new_v4()));
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

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Seeded anpr_config defaults (01-database-schema.md): paddleocr active,
/// 0.7/0.7 thresholds, 0.05 plate-vehicle ratio, discharge confirm required.
#[test]
fn anpr_config_seeded_with_defaults() {
    let ctx = TestCtx::new();
    let cfg = get_anpr_config(ctx.state()).expect("get_anpr_config");
    assert_eq!(cfg.active_ocr_engine, "paddleocr");
    assert_eq!(cfg.confidence_threshold_paddleocr, 0.7);
    assert_eq!(cfg.confidence_threshold_easyocr, 0.7);
    assert_eq!(cfg.plate_vehicle_ratio_threshold, 0.05);
    assert!(cfg.discharge_confirmation_required);
    assert!(cfg.save_recognition_images);
}

/// Config commands are gated on `manage_anpr_config` (08 §5) and audit-logged.
#[test]
fn anpr_config_updates_are_permission_gated_and_audited() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);

    let err = update_anpr_config(ctx.state(), gate.id.clone(), Some("easyocr".to_string()), None, None, None, None, None, None, None, None, None)
        .expect_err("gate officer must not change ANPR config");
    assert!(err.contains("permission"));

    let cfg = update_anpr_config(
        ctx.state(),
        admin.id.clone(),
        Some("easyocr".to_string()),
        Some(0.85),
        Some(0.55),
        Some(0.06),
        Some(r"^\d{3}[A-Z]{2,3}$".to_string()),
        Some(false),
        Some(false),
        Some(100),
        None,
        Some(48.0),
    )
    .expect("admin updates config");
    assert_eq!(cfg.active_ocr_engine, "easyocr");
    assert_eq!(cfg.confidence_threshold_paddleocr, 0.85);
    assert_eq!(cfg.confidence_threshold_easyocr, 0.55);
    assert_eq!(cfg.plate_vehicle_ratio_threshold, 0.06);
    assert!(!cfg.discharge_confirmation_required);
    assert!(!cfg.save_recognition_images);
    assert_eq!(cfg.retrain_candidate_threshold, Some(100));
    assert_eq!(cfg.max_pending_duration_hours, Some(48.0), "pending window saved");

    let state = ctx.state();
    let audited: i64 = state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'updated_anpr_config'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audited, 1, "config update is audit-logged");

    // Engine swaps are a distinct audited event with from/to provenance (08 §5).
    let switched: (i64, String) = state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*), COALESCE(details, '') FROM audit_log WHERE action = 'switched_ocr_engine'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(switched.0, 1, "engine swap is audit-logged");
    assert!(switched.1.contains("paddleocr") && switched.1.contains("easyocr"), "swap records from/to: {switched:?}");

    let bad = update_anpr_config(ctx.state(), admin.id.clone(), None, None, Some(1.5), None, None, None, None, None, None, None)
        .expect_err("threshold out of range rejected");
    assert!(bad.contains("between 0 and 1"));
}

/// R9: the effective confidence threshold is the ACTIVE engine's, per-engine,
/// never a shared value — and `set_capture_settings` writes to that column.
#[test]
fn confidence_threshold_is_per_engine_and_tracks_active_engine() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    // Admin switches to easyocr (active) with its own threshold; paddleocr stays
    // at the seed 0.7. The active threshold is easyocr's.
    update_anpr_config(ctx.state(), admin.id.clone(), Some("easyocr".to_string()), Some(0.7), Some(0.55), None, None, None, None, None, None, None)
        .expect("switch to easyocr");
    let state = ctx.state();
    let conn = state.db.lock().unwrap();
    assert_eq!(truckflow_lib::capture::confidence_threshold(&conn), 0.55);
    drop(conn);

    // set_capture_settings updates the active engine's threshold.
    set_capture_settings(ctx.state(), admin.id.clone(), None, Some(0.9), None, None, None).expect("set threshold");
    let conn = state.db.lock().unwrap();
    assert_eq!(truckflow_lib::capture::confidence_threshold(&conn), 0.9);
    drop(conn);

    // PaddleOCR threshold untouched by the easyocr update (isolation).
    let cfg = get_anpr_config(ctx.state()).unwrap();
    assert_eq!(cfg.confidence_threshold_paddleocr, 0.7);
    assert_eq!(cfg.confidence_threshold_easyocr, 0.9);
}

/// camera_sources: add/update, and deactivate rather than hard-delete.
#[test]
fn camera_sources_crud_and_deactivate_not_delete() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    let src = add_camera_source(ctx.state(), admin.id.clone(), "Gate cam".into(), "rtsp".into(), "rtsp://10.0.0.5".into())
        .expect("add camera source");
    assert_eq!(src.status, "active");

    let bad = add_camera_source(ctx.state(), admin.id.clone(), "Bad".into(), "hls".into(), "x".into())
        .expect_err("invalid source type rejected");
    assert!(bad.contains("Unknown camera source type"));

    let updated = update_camera_source(ctx.state(), admin.id.clone(), src.id.clone(), Some("Main gate".into()), None, None)
        .expect("update camera source");
    assert_eq!(updated.label, "Main gate");

    let inactive = set_camera_source_status(ctx.state(), admin.id.clone(), src.id.clone(), "inactive".into())
        .expect("deactivate camera source");
    assert_eq!(inactive.status, "inactive");
    let listed = list_camera_sources(ctx.state()).unwrap();
    assert_eq!(listed.len(), 1, "deactivated source is retained, not deleted");
    assert_eq!(listed[0].status, "inactive");
}

/// model_versions: deploy requires validation; deployment is explicit; exactly
/// one live per component; rollback records provenance.
#[test]
fn model_version_deploy_requires_validation_and_is_never_automatic() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    let no_validation = register_model_version(ctx.state(), admin.id.clone(), "plate-det-v1".into(), "detection".into(), None)
        .expect("register candidate without validation");
    assert!(!no_validation.is_live);

    let err = deploy_model_version(ctx.state(), admin.id.clone(), no_validation.id.clone())
        .expect_err("unvalidated model cannot go live");
    assert!(err.contains("validation"));

    let candidate = register_model_version(
        ctx.state(),
        admin.id.clone(),
        "plate-det-v2".into(),
        "detection".into(),
        Some(0.982),
    )
    .expect("register validated candidate");
    assert!(!candidate.is_live, "registered candidate is never auto-live");

    let deployed = deploy_model_version(ctx.state(), admin.id.clone(), candidate.id.clone())
        .expect("explicit deploy of validated model");
    assert!(deployed.is_live);
    assert!(deployed.deployed_at.is_some());

    let versions = list_model_versions(ctx.state()).unwrap();
    let live: Vec<_> = versions.iter().filter(|v| v.is_live).collect();
    assert_eq!(live.len(), 1, "exactly one live version per component");
    assert_eq!(live[0].id, candidate.id);

    // Rollback to the earlier version records provenance (rolled_back_from).
    let rolled = rollback_model_version(ctx.state(), admin.id.clone(), no_validation.id.clone())
        .expect("rollback to earlier version");
    assert!(rolled.is_live);
    assert_eq!(rolled.rolled_back_from.as_deref(), Some(candidate.id.as_str()));
}

/// training_candidates view returns flagged frames (ingest low-confidence +
/// human-corrected), per 08 §6.2.
#[test]
fn training_candidates_list_returns_flagged_frames() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    // Create a company/driver/vehicle via reference commands.
    let company = truckflow_lib::reference::create_company(ctx.state(), admin.id.clone(), "Acme Waste".into(), None).unwrap();
    let driver = truckflow_lib::reference::create_driver(ctx.state(), admin.id.clone(), "D. Singh".into(), None).unwrap();
    let veh = truckflow_lib::reference::create_vehicle(
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

    let state = ctx.state();
    let conn = state.db.lock().unwrap();
    let frames_dir = state.frames_dir.clone();
    let res =
        truckflow_lib::capture::ingest_read(&conn, None, &read("A123AB", 0.5, &now_iso()), "auto", &frames_dir).unwrap();
    let queued = res.queued.expect("low-confidence queues");
    assert_eq!(queued.reason.as_deref(), Some("low_confidence"));
    drop(conn);

    let candidates = list_training_candidates(ctx.state()).expect("list candidates");
    assert_eq!(candidates.len(), 3);
    assert!(candidates.iter().all(|c| c.reason == "low_confidence"));
    assert!(candidates.iter().all(|c| c.plate_number.as_deref() == Some("A123AB")));
    assert!(candidates.iter().all(|c| c.source_trip_id.as_deref() == Some(queued.id.as_str())));

    // Sanity: the reference vehicle still resolves (plumbing intact).
    assert_eq!(veh.plate_number, "A123AB");
}

/// 01-database-schema.md checklist: only one `is_live = 1` row per component is
/// possible — enforced by the partial unique index, not just app logic.
#[test]
fn one_live_model_version_per_component_is_enforced_at_db_level() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    let v1 = register_model_version(ctx.state(), admin.id.clone(), "m1".into(), "detection".into(), Some(0.9))
        .unwrap();
    let v2 = register_model_version(ctx.state(), admin.id.clone(), "m2".into(), "detection".into(), Some(0.95))
        .unwrap();
    deploy_model_version(ctx.state(), admin.id.clone(), v1.id.clone()).unwrap();

    let state = ctx.state();
    let conn = state.db.lock().unwrap();
    // Direct attempt to force a second live row for the same component must fail.
    let err = conn
        .execute(
            "UPDATE model_versions SET is_live = 1 WHERE id = ?1",
            rusqlite::params![v2.id],
        )
        .expect_err("second live row for same component must be rejected");
    assert!(err.to_string().to_lowercase().contains("unique"), "got: {err}");
    let live: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM model_versions WHERE component = 'detection' AND is_live = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live, 1, "exactly one live version survives the rejection");
}

/// 01-database-schema.md checklist: trips record model_version + ocr_engine, and
/// filtering by model_version never mixes results across engines or versions.
#[test]
fn trips_filtered_by_model_version_never_mix_engines() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    let company = truckflow_lib::reference::create_company(ctx.state(), admin.id.clone(), "Acme Waste".into(), None).unwrap();
    let driver = truckflow_lib::reference::create_driver(ctx.state(), admin.id.clone(), "D. Singh".into(), None).unwrap();
    truckflow_lib::reference::create_vehicle(
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

    set_capture_settings(ctx.state(), admin.id.clone(), Some("fully_automatic".to_string()), None, None, None, None)
        .expect("fully automatic so reads log");

    let state = ctx.state();
    let conn = state.db.lock().unwrap();
    let frames_dir = state.frames_dir.clone();

    let read_m1 = read("A123AB", 0.92, &now_iso());
    let mut read_m2 = read("A123AB", 0.88, &now_iso());
    read_m2.model_version = Some("model-v2".to_string());
    read_m2.ocr_engine = Some("easyocr".to_string());
    read_m2.timestamp = now_iso();

    let logged1 = truckflow_lib::capture::ingest_read(&conn, None, &read_m1, "auto", &frames_dir)
        .unwrap()
        .trip
        .expect("high-confidence exact match logs");
    let logged2 = truckflow_lib::capture::ingest_read(&conn, None, &read_m2, "auto", &frames_dir)
        .unwrap()
        .trip
        .expect("high-confidence exact match logs");

    // Each trip pins its own model_version + ocr_engine (never omit on auto).
    assert_eq!(logged1.model_version.as_deref(), Some("test-model-1"));
    assert_eq!(logged1.ocr_engine.as_deref(), Some("paddleocr"));
    assert_eq!(logged2.model_version.as_deref(), Some("model-v2"));
    assert_eq!(logged2.ocr_engine.as_deref(), Some("easyocr"));
    assert_ne!(logged1.id, logged2.id, "two reads, two trips");

    // Filter by model_version: the m1 set only contains paddleocr reads.
    let m1_rows: Vec<(String, String)> = conn
        .prepare("SELECT model_version, ocr_engine FROM trips WHERE model_version = ?1")
        .unwrap()
        .query_map([logged1.model_version.as_deref().unwrap()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(m1_rows.len(), 1);
    assert!(m1_rows.iter().all(|(_, engine)| engine == "paddleocr"));

    let m2_rows: Vec<(String, String)> = conn
        .prepare("SELECT model_version, ocr_engine FROM trips WHERE model_version = ?1")
        .unwrap()
        .query_map([logged2.model_version.as_deref().unwrap()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(m2_rows.len(), 1);
    assert!(m2_rows.iter().all(|(_, engine)| engine == "easyocr"));
}
