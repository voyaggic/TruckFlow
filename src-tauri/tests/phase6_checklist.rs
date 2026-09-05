//! Phase 6 — Polish & readiness tests, driven by 05-ui-screens.md §4/§6h and
//! 07-build-plan.md Phase 6:
//!
//! - Self-service profile: phone, language preference, notification sound and
//!   profile photo (set/remove/read-back) — all audited, never touching
//!   credentials or permissions.
//! - ANPR confidence trend: the poller's per-read event series aggregates into
//!   the System Monitor trend, gated on `view_system_health`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tauri::test::mock_app;
use tauri::{App, Manager, State};

use truckflow_lib::capture::{record_read_event, SimulatorSource};
use truckflow_lib::commands;
use truckflow_lib::db::{open_db, AppState};
use truckflow_lib::models::{AnprFrame, AnprRead, SessionUser};
use truckflow_lib::monitor;
use truckflow_lib::sync::{MockPostgres, MockSheets};

const ADMIN_PASS: &str = "AdminPass!2024";
const PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

struct TempDb {
    dir: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("truckflow_p6_{}", uuid::Uuid::new_v4()));
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

    fn create_gate_user(&self, admin: &SessionUser) -> truckflow_lib::models::UserView {
        let company_id = admin.company_id.clone().unwrap_or_else(|| "default".to_string());
        commands::create_user(
            self.state(),
            admin.id.clone(),
            "Officer".to_string(),
            vec!["view_gate_entries".to_string()],
            company_id,
        )
        .expect("create gate user")
    }
}

fn read(plate: &str, confidence: f64, timestamp: &str) -> AnprRead {
    let frames: Vec<AnprFrame> = (0..2)
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
// Self-service profile (05 §4)
// ---------------------------------------------------------------------------

#[test]
fn own_profile_fields_update_and_are_audited() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    commands::update_own_profile(
        ctx.state(),
        admin.id.clone(),
        Some("+254712345678".to_string()),
        Some("en".to_string()),
        Some(false),
    )
    .expect("profile update");

    let relogin = commands::login_password(ctx.state(), "Boss".to_string(), ADMIN_PASS.to_string(), ctx.company_id())
        .expect("login")
        .user;
    assert_eq!(relogin.phone_number.as_deref(), Some("+254712345678"));
    assert_eq!(relogin.language_preference.as_deref(), Some("en"));
    assert_eq!(relogin.notification_sound, Some(false), "toggle off persisted");

    // Empty strings clear a field rather than storing blanks.
    commands::update_own_profile(ctx.state(), admin.id.clone(), Some("  ".to_string()), None, Some(true))
        .expect("clear phone");
    let again = commands::login_password(ctx.state(), "Boss".to_string(), ADMIN_PASS.to_string(), ctx.company_id())
        .expect("login")
        .user;
    assert_eq!(again.phone_number, None);
    assert_eq!(again.notification_sound, Some(true));

    let audit: i64 = ctx
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'updated_own_profile' AND actor_id = ?1",
            [&admin.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audit, 2, "each profile edit is an audited event");
}

#[test]
fn profile_photo_sets_round_trips_and_removes() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    // None before anything is set.
    assert_eq!(commands::get_profile_photo(ctx.state(), admin.id.clone()).expect("read"), None);

    commands::set_profile_photo(ctx.state(), admin.id.clone(), Some(PIXEL_PNG.to_string())).expect("set");
    let got = commands::get_profile_photo(ctx.state(), admin.id.clone()).expect("read back").expect("some");
    assert!(got.starts_with("data:image/png;base64,"), "round-trips as a data URL");

    // The artifact exists on disk under the app data folder.
    let dir = ctx.frames_dir().parent().unwrap().join("profile_photos");
    assert!(dir.join(format!("{}.png", admin.id)).exists());

    commands::set_profile_photo(ctx.state(), admin.id.clone(), None).expect("remove");
    assert_eq!(commands::get_profile_photo(ctx.state(), admin.id.clone()).expect("read"), None, "removed");
    assert!(!dir.join(format!("{}.png", admin.id)).exists());

    // Oversized / invalid images are rejected before touching anything.
    let bad = "not-base64!!!".to_string();
    let err = commands::set_profile_photo(ctx.state(), admin.id.clone(), Some(bad)).expect_err("invalid image");
    assert!(err.contains("base64"));
}

// ---------------------------------------------------------------------------
// ANPR confidence trend (05 §6h)
// ---------------------------------------------------------------------------

#[test]
fn read_events_aggregate_into_trend_and_are_gated() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);

    let conn = ctx.conn();
    let r1 = read("AAA111", 0.80, "2026-08-11T08:00:00Z");
    let r2 = read("BBB222", 0.90, "2026-08-11T09:30:00Z");
    let r3 = read("CCC333", 0.50, "2026-08-12T07:00:00Z");
    record_read_event(&conn, &r1, "simulator", "captured").unwrap();
    record_read_event(&conn, &r2, "simulator", "captured").unwrap();
    record_read_event(&conn, &r3, "simulator", "captured").unwrap();
    drop(conn);

    // A gate officer without view_system_health is refused.
    let err = monitor::anpr_confidence_trend(ctx.state(), gate.id.clone(), None, None).expect_err("gated");
    assert!(err.contains("permission"));

    let trend = monitor::anpr_confidence_trend(ctx.state(), admin.id.clone(), None, None).expect("trend");
    assert_eq!(trend.len(), 2, "one point per active day");
    let day1 = &trend[0];
    assert_eq!(day1.date, "2026-08-11");
    assert_eq!(day1.reads, 2);
    assert!((day1.avg_confidence.unwrap() - 0.85).abs() < 1e-9, "avg of 0.80/0.90");
    assert_eq!(trend[1].date, "2026-08-12");
    assert!((trend[1].avg_confidence.unwrap() - 0.50).abs() < 1e-9);

    // A date-only `to` bound must include the entire upper day.
    let ranged = monitor::anpr_confidence_trend(
        ctx.state(),
        admin.id.clone(),
        Some("2026-08-11".to_string()),
        Some("2026-08-11".to_string()),
    )
    .expect("range");
    assert_eq!(ranged.len(), 1);
    assert_eq!(ranged[0].reads, 2, "same-day both reads, not zero");
}
