//! Phase 5 — Reporting & oversight tests, driven by 05-ui-screens.md §5/§6c/§6g/§6h
//! and 07-build-plan.md §2.5:
//!
//! - Reporting Dashboard: strictly read-only, date-range + company filters,
//!   summary stats with prior-period comparison, daily trips-over-time, top
//!   companies, trips by vehicle, drill-down to underlying trip records with
//!   photo evidence reachable, and export rows with no monetary data.
//! - Audit log: chronological + filterable, gated on `view_audit_log`.
//! - Oversight: aggregate per-officer activity, never live-session access.
//! - System Monitor: per-component health from `system_health_events`,
//!   acknowledge action, incident history, and sync-failure wiring.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tauri::test::mock_app;
use tauri::{App, Manager, State};

use truckflow_lib::archive;
use truckflow_lib::capture::{ingest_read, set_capture_settings, SimulatorSource};
use truckflow_lib::commands;
use truckflow_lib::db::{open_db, AppState};
use truckflow_lib::models::{
    AnprFrame, AnprRead, ColumnInfo, ConfirmedColumn, ConfirmedSheet, ReferenceImportRequest,
    ReportFilters, SessionUser, VehicleView,
};
use truckflow_lib::reference;
use truckflow_lib::reporting;
use truckflow_lib::sync::{
    MockPostgres, MockSheets, PostgresAdapter, RealPostgres, run_trip_retention, set_trip_retention,
    simulate_connectivity, sync_now_pg,
};

const ADMIN_PASS: &str = "AdminPass!2024";

struct TempDb {
    dir: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("truckflow_p5_{}", uuid::Uuid::new_v4()));
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

    fn create_monitor_user(&self, admin: &SessionUser) -> truckflow_lib::models::UserView {
        commands::create_user(
            self.state(),
            admin.id.clone(),
            "Watchdog".to_string(),
            vec![
                "view_system_health".to_string(),
                "acknowledge_health_alerts".to_string(),
                "view_health_history".to_string(),
            ],
            "WatchdogPass!2024".to_string(),
        )
        .expect("create monitor user")
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

/// Seed a company + driver + vehicle so an exact-match read auto-logs.
fn seed_vehicle(ctx: &TestCtx, admin: &SessionUser, company_name: &str, plate: &str) -> VehicleView {
    let company = reference::create_company(ctx.state(), admin.id.clone(), company_name.into(), None).unwrap();
    let driver = reference::create_driver(ctx.state(), admin.id.clone(), "D. Singh".into(), None).unwrap();
    reference::create_vehicle(
        ctx.state(),
        admin.id.clone(),
        plate.into(),
        Some(company.id.clone()),
        Some(20.0),
        "litres".into(),
        Some(driver.id.clone()),
        None,
    )
    .unwrap()
}

fn log_trip(ctx: &TestCtx, officer: &SessionUser, plate: &str) -> truckflow_lib::models::TripView {
    set_capture_settings(ctx.state(), officer.id.clone(), Some("fully_automatic".to_string()), None, None, None, None).unwrap();
    let res = ingest_read(&ctx.conn(), Some(officer.id.clone()), &read(plate, 0.95, "2026-08-10T08:00:00Z"), "auto", &ctx.frames_dir())
        .unwrap();
    res.trip.expect("fully-automatic exact match must log")
}

fn stamp_trip(ctx: &TestCtx, trip_id: &str, time_in: &str) {
    ctx.conn()
        .execute("UPDATE trips SET time_in = ?1 WHERE id = ?2", rusqlite::params![time_in, trip_id])
        .unwrap();
}

// ---------------------------------------------------------------------------
// Reporting Dashboard (§5)
// ---------------------------------------------------------------------------

/// Aggregates honor filters, drill-down returns the real underlying trip rows,
/// and photo evidence is reachable from them. Zero write/delete reachable.
#[test]
fn reporting_dashboard_filters_drilldown_and_evidence() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);

    let veh_a = seed_vehicle(&ctx, &admin, "Acme Waste", "AAA111");
    let veh_b = seed_vehicle(&ctx, &admin, "Beta Logistics", "BBB222");
    let a = log_trip(&ctx, &admin, "AAA111");
    stamp_trip(&ctx, &a.id, "2026-08-01T08:00:00Z");
    let b1 = log_trip(&ctx, &admin, "BBB222");
    stamp_trip(&ctx, &b1.id, "2026-08-02T09:00:00Z");
    let b2 = log_trip(&ctx, &admin, "BBB222");
    stamp_trip(&ctx, &b2.id, "2026-08-03T10:00:00Z");

    // A gate officer (no reporting permission) is refused.
    let err = reporting::report_dashboard(ctx.state(), gate.id.clone(), ReportFilters::default())
        .expect_err("gate officer must not view the dashboard");
    assert!(err.contains("permission"));

    // Full range: 3 trips, 2 active companies.
    let dash = reporting::report_dashboard(ctx.state(), admin.id.clone(), ReportFilters::default()).expect("dashboard");
    assert_eq!(dash.summary.total_trips, 3);
    assert_eq!(dash.summary.active_companies, 2);
    assert_eq!(dash.trips_over_time.len(), 3, "one bucket per active day");
    let top = dash.top_companies.first().expect("top company");
    assert_eq!(top.company_name, "Beta Logistics");
    assert_eq!(top.count, 2);
    let veh = dash.trips_by_vehicle.iter().find(|v| v.plate_number == "BBB222").expect("beta row");
    assert_eq!(veh.trip_count, 2);
    assert_eq!(veh.total_capacity, 40.0, "capacity summed across trips");

    // Company filter narrows to that company only.
    let beta_only = reporting::report_dashboard(
        ctx.state(),
        admin.id.clone(),
        ReportFilters { from: None, to: None, company_id: Some(veh_b.company_id.clone().unwrap()) },
    )
    .expect("beta dashboard");
    assert_eq!(beta_only.summary.total_trips, 2);
    assert_eq!(beta_only.summary.active_companies, 1);

    // Date-range filter: only 2026-08-02 trips.
    let day2 = reporting::report_dashboard(
        ctx.state(),
        admin.id.clone(),
        ReportFilters {
            from: Some("2026-08-02T00:00:00Z".into()),
            to: Some("2026-08-02T23:59:59Z".into()),
            company_id: None,
        },
    )
    .expect("day dashboard");
    assert_eq!(day2.summary.total_trips, 1);

    // Drill-down: the real TripView rows behind the aggregates.
    let rows = reporting::report_trips_drill(ctx.state(), admin.id.clone(), ReportFilters::default(), 100).expect("drill");
    assert_eq!(rows.len(), 3);
    let beta = rows.iter().find(|r| r.plate_number == "BBB222").expect("beta trip");
    assert_eq!(beta.company_name.as_deref(), Some("Beta Logistics"));
    assert_eq!(beta.capacity_at_trip, Some(20.0));
    assert_eq!(beta.capacity_unit, "litres");
    assert!(beta.photo_count > 0, "evidence frames exist on the drill-down row");

    // Photo evidence itself is reachable from a drill-down trip id.
    let frames = truckflow_lib::capture::trip_frames(ctx.state(), beta.id.clone()).expect("trip frames");
    assert_eq!(frames.len(), 3);
    assert!(frames.iter().all(|f| f.data_base64.is_some()), "placeholder frames carry a payload");

    // Export rows: flat, correct count, no monetary field in the shape.
    let export = reporting::report_export(ctx.state(), admin.id.clone(), ReportFilters::default()).expect("export");
    assert_eq!(export.len(), 3);
    let e = export.iter().find(|r| r.plate == "BBB222").expect("export row");
    assert_eq!(e.company, "Beta Logistics");
    assert_eq!(e.capacity_unit, "litres");

    // Read-only invariant: running the whole surface changes nothing.
    let conn = ctx.conn();
    let before: (i64, i64) = conn
        .query_row("SELECT (SELECT COUNT(*) FROM trips), (SELECT COUNT(*) FROM audit_log)", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    let _ = reporting::report_dashboard(ctx.state(), admin.id.clone(), ReportFilters::default());
    let _ = reporting::report_trips_drill(ctx.state(), admin.id.clone(), ReportFilters::default(), 50);
    let _ = reporting::report_export(ctx.state(), admin.id.clone(), ReportFilters::default());
    let after: (i64, i64) = conn
        .query_row("SELECT (SELECT COUNT(*) FROM trips), (SELECT COUNT(*) FROM audit_log)", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(before, after, "reporting commands never mutate trips or the audit log");

    let _ = veh_a;
}

/// Summary stats include a prior-period comparison (05 §5).
#[test]
fn reporting_summary_compares_prior_period() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_vehicle(&ctx, &admin, "Acme Waste", "AAA111");

    let a = log_trip(&ctx, &admin, "AAA111");
    stamp_trip(&ctx, &a.id, "2026-08-10T08:00:00Z");
    let b = log_trip(&ctx, &admin, "AAA111");
    stamp_trip(&ctx, &b.id, "2026-08-11T08:00:00Z");
    let c = log_trip(&ctx, &admin, "AAA111");
    stamp_trip(&ctx, &c.id, "2026-08-06T08:00:00Z");

    let dash = reporting::report_dashboard(
        ctx.state(),
        admin.id.clone(),
        ReportFilters {
            from: Some("2026-08-08T00:00:00Z".into()),
            to: Some("2026-08-12T00:00:00Z".into()),
            company_id: None,
        },
    )
    .expect("dashboard");
    assert_eq!(dash.summary.total_trips, 2, "two trips in the selected window");
    assert_eq!(dash.summary.prior_period.prior_trips, 1, "one trip in the preceding 5-day window");
    assert_eq!(dash.summary.prior_period.delta_trips, 1);
    assert_eq!(dash.summary.prior_period.delta_percent, Some(100.0));
    assert!(dash.summary.avg_trips_per_day > 0.0);

    // Unbounded: no prior period exists.
    let full = reporting::report_dashboard(ctx.state(), admin.id.clone(), ReportFilters::default()).expect("full");
    assert_eq!(full.summary.prior_period.prior_trips, 0);
    assert_eq!(full.summary.prior_period.delta_percent, None);
}

// ---------------------------------------------------------------------------
// Audit log (§6g) + oversight (§6c)
// ---------------------------------------------------------------------------

/// Audit is gated on `view_audit_log`, chronological, and filterable.
#[test]
fn audit_log_gated_filtered_and_actions_available() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);

    let err = reporting::list_audit_log(ctx.state(), gate.id.clone(), Default::default())
        .expect_err("gate officer must not see the audit log");
    assert!(err.contains("permission"));

    let entries = reporting::list_audit_log(ctx.state(), admin.id.clone(), Default::default()).expect("audit");
    assert!(!entries.is_empty(), "login / user creation are already recorded");
    let first = entries.first().unwrap();
    assert!(!first.timestamp.is_empty());

    // Filter by action.
    let filtered = reporting::list_audit_log(
        ctx.state(),
        admin.id.clone(),
        truckflow_lib::models::AuditFilters { from: None, to: None, actor_id: None, action: Some("created_user".into()) },
    )
    .expect("filtered audit");
    assert!(!filtered.is_empty());
    assert!(filtered.iter().all(|e| e.action == "created_user"));

    let actions = reporting::list_audit_actions_command(ctx.state(), admin.id.clone()).expect("actions");
    assert!(actions.contains(&"created_user".to_string()));
    assert!(actions.contains(&"first_admin_created".to_string()));
}

/// Oversight is aggregate per-officer activity, never a live session.
#[test]
fn officer_activity_is_aggregate_and_permission_gated() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);
    seed_vehicle(&ctx, &admin, "Acme Waste", "AAA111");

    // Admin logs two trips directly (officer = admin), resolving one queue item too.
    let a = log_trip(&ctx, &admin, "AAA111");
    stamp_trip(&ctx, &a.id, "2026-08-10T08:00:00Z");
    let b = log_trip(&ctx, &admin, "AAA111");
    stamp_trip(&ctx, &b.id, "2026-08-11T08:00:00Z");

    let err = reporting::officer_activity(ctx.state(), gate.id.clone(), None, None)
        .expect_err("gate officer must not see oversight");
    assert!(err.contains("permission"));

    let act = reporting::officer_activity(ctx.state(), admin.id.clone(), None, None).expect("activity");
    let row = act.iter().find(|r| r.officer_id == admin.id).expect("admin activity row");
    assert!(row.trips_logged >= 2);
    assert!(row.last_active_at.is_some(), "last activity timestamp present");

    // Bounded window excludes the other trip.
    let bounded = reporting::officer_activity(
        ctx.state(),
        admin.id.clone(),
        Some("2026-08-10T00:00:00Z".into()),
        Some("2026-08-10T23:59:59Z".into()),
    )
    .expect("bounded activity");
    let row2 = bounded.iter().find(|r| r.officer_id == admin.id).expect("row");
    assert_eq!(row2.trips_logged, 1);
}

// ---------------------------------------------------------------------------
// System Monitor (§6h)
// ---------------------------------------------------------------------------

/// Health events: degraded opens one incident, duplicate opens stay one, "ok"
/// resolves it into history, acknowledge works, and everything is gated.
#[test]
fn health_events_record_acknowledge_resolve_and_history() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);
    let monitor = ctx.create_monitor_user(&admin);

    // Gate officer cannot view health.
    let err = truckflow_lib::monitor::health_dashboard(ctx.state(), gate.id.clone()).expect_err("gate officer must not see monitor");
    assert!(err.contains("permission"));

    let conn = ctx.conn();

    // Recording is wired, not user-facing: simulate a sync outage.
    truckflow_lib::monitor::record_health_event(&conn, "sync", "offline", Some("PostgreSQL unreachable")).unwrap();
    truckflow_lib::monitor::record_health_event(&conn, "sync", "offline", Some("PostgreSQL unreachable")).unwrap();

    let open: i64 = conn
        .query_row("SELECT COUNT(*) FROM system_health_events WHERE component = 'sync' AND resolved_at IS NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(open, 1, "duplicate outage never spams open incidents");

    // Monitor user sees the degraded sync card + the open alert.
    let dash = truckflow_lib::monitor::health_dashboard(ctx.state(), monitor.id.clone()).expect("health");
    let sync_card = dash.components.iter().find(|c| c.component == "sync").expect("sync card");
    assert_eq!(sync_card.status, "offline");
    assert_eq!(sync_card.open_events, 1);
    assert!(!dash.open_alerts.is_empty());

    // Acknowledge marks it as being handled.
    let event_id = dash.open_alerts[0].id.clone();
    let ack = truckflow_lib::monitor::acknowledge_health_event(ctx.state(), monitor.id.clone(), event_id.clone()).expect("ack");
    assert!(ack.acknowledged_at.is_some());

    // Recovery resolves it into incident history.
    truckflow_lib::monitor::record_health_event(&conn, "sync", "ok", None).unwrap();
    let dash2 = truckflow_lib::monitor::health_dashboard(ctx.state(), monitor.id.clone()).expect("health");
    let sync2 = dash2.components.iter().find(|c| c.component == "sync").expect("sync card");
    assert_eq!(sync2.status, "ok");
    assert!(dash2.open_alerts.is_empty(), "acknowledged + resolved alert no longer open");
    assert!(dash2.recent_history.iter().any(|e| e.id == event_id), "resolved incident stays in history");

    // Non-monitor admin without acknowledge permission cannot acknowledge.
    let admin_dash = truckflow_lib::monitor::health_dashboard(ctx.state(), admin.id.clone()).expect("admin sees health");
    if !admin_dash.open_alerts.is_empty() {
        let id = admin_dash.open_alerts[0].id.clone();
        let ack_err = truckflow_lib::monitor::acknowledge_health_event(ctx.state(), admin.id.clone(), id)
            .expect_err("admin preset lacks acknowledge_health_alerts");
        assert!(ack_err.contains("permission"));
    }
}

/// Sync failures surface as health events through the real command layer, and a
/// successful sync clears them.
#[test]
fn sync_failure_and_recovery_flow_into_monitor() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let monitor = ctx.create_monitor_user(&admin);
    seed_vehicle(&ctx, &admin, "Acme Waste", "AAA111");

    let trip = log_trip(&ctx, &admin, "AAA111");

    // Force the app's PG adapter offline, then run a manual sync: nothing
    // pushes, pending stays, and a sync health event is recorded.
    simulate_connectivity(ctx.state(), false, true).unwrap();
    let off = sync_now_pg(ctx.state(), admin.id.clone()).expect("sync is best-effort even offline");
    assert_eq!(off.pushed, 0, "offline sync pushes nothing");

    let dash = truckflow_lib::monitor::health_dashboard(ctx.state(), monitor.id.clone()).expect("health");
    let sync_card = dash.components.iter().find(|c| c.component == "sync").expect("sync card");
    assert_eq!(sync_card.status, "offline", "pending + unreachable sync must be flagged");
    assert!(!dash.open_alerts.is_empty());

    // Reconnect and sync successfully → health clears.
    simulate_connectivity(ctx.state(), true, true).unwrap();
    let ok = sync_now_pg(ctx.state(), admin.id.clone()).expect("reconnected sync");
    assert!(ok.pushed >= 1);

    let dash2 = truckflow_lib::monitor::health_dashboard(ctx.state(), monitor.id.clone()).expect("health");
    let sync2 = dash2.components.iter().find(|c| c.component == "sync").expect("sync card");
    assert_eq!(sync2.status, "ok", "successful sync resolves the incident");

    let _ = trip;
}

/// The Excel-compatible CSV export writes the current filter result to a file
/// in the app data folder and never touches data tables.
#[test]
fn report_csv_export_writes_file_and_stays_read_only() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);
    seed_vehicle(&ctx, &admin, "Acme Waste", "AAA111");
    let trip = log_trip(&ctx, &admin, "AAA111");
    stamp_trip(&ctx, &trip.id, "2026-08-05T08:00:00Z");

    let err = reporting::report_export_csv(ctx.state(), gate.id.clone(), ReportFilters::default(), None)
        .expect_err("gate officer must not export");
    assert!(err.contains("permission"));

    let audit_before: i64 = ctx.conn().query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0)).unwrap();
    let path = reporting::report_export_csv(ctx.state(), admin.id.clone(), ReportFilters::default(), None).expect("export");
    assert!(path.ends_with(".csv"));
    let content = std::fs::read_to_string(&path).expect("csv readable");
    assert!(content.contains("AAA111"));
    assert!(content.contains("Acme Waste"));
    assert!(content.starts_with("Trip ID,Plate,Time In"), "header row present");

    // The export is a generated artifact; the source data is untouched and no
    // audit entry is produced by the export itself.
    let conn = ctx.conn();
    let trips: i64 = conn.query_row("SELECT COUNT(*) FROM trips", [], |r| r.get(0)).unwrap();
    let audit_after: i64 = conn.query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0)).unwrap();
    assert_eq!(trips, 1);
    assert_eq!(audit_after, audit_before, "exporting must not create audit entries");

    let _ = std::fs::remove_file(&path);
}

/// Soft delete hides a trip from reporting and the recent list, keeps it in the
/// archived view (Postgres-safe), requires the admin password, and restores it.
#[test]
fn archive_soft_delete_hides_from_reporting_and_restores() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);
    seed_vehicle(&ctx, &admin, "Acme Waste", "AAA111");
    let trip = log_trip(&ctx, &admin, "AAA111");

    // Gate officer has no permission.
    let err = archive::soft_delete_trips(ctx.state(), gate.id.clone(), vec![trip.id.clone()], "GatePass!2024".into())
        .expect_err("gate officer must not archive trips");
    assert!(err.contains("permission"));

    // Wrong admin password is rejected.
    let err2 = archive::soft_delete_trips(ctx.state(), admin.id.clone(), vec![trip.id.clone()], "wrong".into())
        .expect_err("wrong password must be rejected");
    assert!(err2.contains("password"));

    let n = archive::soft_delete_trips(ctx.state(), admin.id.clone(), vec![trip.id.clone()], ADMIN_PASS.into())
        .expect("soft delete with correct password");
    assert_eq!(n, 1);

    // Hidden from reporting and the recent list.
    let dash = reporting::report_dashboard(ctx.state(), admin.id.clone(), ReportFilters::default()).expect("dashboard");
    assert_eq!(dash.summary.total_trips, 0, "archived trips are excluded from reporting");
    let recent = archive::list_recent_trips(ctx.state(), admin.id.clone(), 50).expect("recent list");
    assert_eq!(recent.len(), 0);

    // Visible in the archived view.
    let archived = archive::list_archived_trips(ctx.state(), admin.id.clone(), ReportFilters::default()).expect("archived list");
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].id, trip.id);

    // Restore brings it back everywhere.
    let r = archive::restore_trips(ctx.state(), admin.id.clone(), vec![trip.id.clone()]).expect("restore");
    assert_eq!(r, 1);
    let dash2 = reporting::report_dashboard(ctx.state(), admin.id.clone(), ReportFilters::default()).expect("dashboard after restore");
    assert_eq!(dash2.summary.total_trips, 1, "restored trips are back in reporting");
}

/// Hard delete removes a trip from local storage entirely (and asks the central
/// adapter to remove it too), gated on the admin password.
#[test]
fn archive_hard_delete_removes_trip_everywhere() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin);
    seed_vehicle(&ctx, &admin, "Beta Logistics", "BBB222");
    let trip = log_trip(&ctx, &admin, "BBB222");
    // Simulate it was already confirmed in Postgres.
    ctx.conn()
        .execute("UPDATE trips SET synced = 1 WHERE id = ?1", rusqlite::params![trip.id])
        .unwrap();

    let err = archive::hard_delete_trips(ctx.state(), gate.id.clone(), vec![trip.id.clone()], "GatePass!2024".into())
        .expect_err("gate officer must not hard delete");
    assert!(err.contains("permission"));

    let n = archive::hard_delete_trips(ctx.state(), admin.id.clone(), vec![trip.id.clone()], ADMIN_PASS.into())
        .expect("hard delete with correct password");
    assert_eq!(n, 1);

    let count: i64 = ctx
        .conn()
        .query_row("SELECT COUNT(*) FROM trips", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "hard-deleted trip is gone from local storage");

    let audited: i64 = ctx
        .conn()
        .query_row("SELECT COUNT(*) FROM audit_log WHERE action = 'hard_deleted_trips'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(audited, 1, "hard delete is audit-logged");
}

/// Local purge removes only logged trips already confirmed in Postgres; it
/// protects unsynced trips and requires the admin password.
#[test]
fn archive_purge_local_only_removes_synced_logged_trips() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_vehicle(&ctx, &admin, "Gamma Haulage", "CCC333");
    seed_vehicle(&ctx, &admin, "Delta Freight", "DDD444");
    let synced = log_trip(&ctx, &admin, "CCC333");
    let unsynced = log_trip(&ctx, &admin, "DDD444");
    ctx.conn()
        .execute("UPDATE trips SET synced = 1 WHERE id = ?1", rusqlite::params![synced.id])
        .unwrap();

    let err = archive::purge_local_trips(ctx.state(), admin.id.clone(), "wrong".into())
        .expect_err("wrong password must be rejected");
    assert!(err.contains("password"));

    let n = archive::purge_local_trips(ctx.state(), admin.id.clone(), ADMIN_PASS.into()).expect("purge");
    assert_eq!(n, 1, "only the synced logged trip is purged");

    let remaining: Vec<String> = ctx
        .conn()
        .prepare("SELECT id FROM trips")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(remaining, vec![unsynced.id], "unsynced trip is protected");
}

/// Daily-entry retention deletes trips older than the admin-set window from
/// local + Postgres in bulk, only for confirmed logged trips, and never
/// touches the reference registry or unsynced rows.
#[test]
fn trip_retention_ages_out_old_confirmed_entries() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_vehicle(&ctx, &admin, "Epsilon Tankers", "EEE555");
    seed_vehicle(&ctx, &admin, "Zeta Carriers", "FFF666");
    let old = log_trip(&ctx, &admin, "EEE555");
    let fresh = log_trip(&ctx, &admin, "FFF666");
    let unsynced = log_trip(&ctx, &admin, "EEE555");
    // Old + fresh confirmed in Postgres; one stays unsynced.
    ctx.conn()
        .execute("UPDATE trips SET synced = 1 WHERE id IN (?1, ?2)", rusqlite::params![old.id, fresh.id])
        .unwrap();
    stamp_trip(&ctx, &old.id, "2026-06-01T08:00:00Z"); // ~2 months back

    let mock = MockPostgres::new();
    set_trip_retention(ctx.state(), admin.id.clone(), Some(30)).expect("set retention");

    run_trip_retention(&ctx.conn(), &mock).expect("retention run");

    let remaining: Vec<String> = ctx
        .conn()
        .prepare("SELECT id FROM trips ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut expect = vec![fresh.id.clone(), unsynced.id.clone()];
    expect.sort();
    let mut got = remaining.clone();
    got.sort();
    assert_eq!(got, expect, "only the old confirmed trip ages out");

    let deleted = mock.deleted();
    assert_eq!(deleted.len(), 1, "central delete requested for the aged-out trip");
    assert_eq!(deleted[0], ("trips".to_string(), old.id.clone()));

    // Registry untouched.
    let vehicles: i64 = ctx.conn().query_row("SELECT COUNT(*) FROM vehicles", [], |r| r.get(0)).unwrap();
    assert_eq!(vehicles, 2);

    // Clearing the setting stops future pruning.
    set_trip_retention(ctx.state(), admin.id.clone(), None).expect("clear retention");
    let before: i64 = ctx.conn().query_row("SELECT COUNT(*) FROM trips", [], |r| r.get(0)).unwrap();
    run_trip_retention(&ctx.conn(), &mock).expect("retention run with retention disabled");
    let after: i64 = ctx.conn().query_row("SELECT COUNT(*) FROM trips", [], |r| r.get(0)).unwrap();
    assert_eq!(before, after, "no pruning when retention is disabled");
}

/// Reporting repoint (Phase 5): the mock adapter cannot answer aggregate
/// queries, so the dashboard falls back to the local working buffer and labels
/// itself `local`.
#[test]
fn reporting_falls_back_to_local_when_archive_unavailable() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    seed_vehicle(&ctx, &admin, "Acme Waste", "AAA111");
    let _trip = log_trip(&ctx, &admin, "AAA111");

    let dash = reporting::report_dashboard(ctx.state(), admin.id.clone(), ReportFilters::default()).expect("dashboard");
    assert_eq!(dash.summary.total_trips, 1, "local data served when archive unavailable");
    assert_eq!(dash.data_source, "local");
}

/// Reporting repoint live check: with a real archive configured, the same
/// command reads its numbers from PostgreSQL and labels itself `postgres`.
/// Skips when no local server is reachable (same guard as the Phase 4 live
/// roundtrip).
#[test]
fn reporting_reads_from_postgres_archive_when_connected() {
    let base = "postgresql://postgres@127.0.0.1:5432";
    let dbname = format!("truckflow_rpt_{}_{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let conn_string = format!("{base}/{dbname}");

    let pg = RealPostgres::new();
    if let Err(e) = pg.configure(Some(conn_string.clone())) {
        eprintln!("SKIP live reporting test — no reachable local server: {e}");
        return;
    }

    // Two companies, two logged trips (one declined, one archived) + one
    // queued — the archive must count only the two logged, non-archived ones.
    let company = |id: &str, name: &str| {
        serde_json::json!({
            "id": id, "name": name, "status": "active", "extra_fields": null,
            "created_at": "2026-07-01T00:00:00Z", "updated_at": "2026-07-01T00:00:00Z", "synced": 1,
        })
    };
    let trip = |id: &str, cid: &str, status: &str, archived: &str, time_in: &str| {
        serde_json::json!({
            "id": id, "vehicle_id": null, "driver_id": null, "company_id": cid,
            "capacity_at_trip": 40.0, "capacity_unit": "litres", "time_in": time_in,
            "receipt_no": null, "officer_id": null, "capture_method": "auto",
            "confidence_score": 0.95, "photo_refs": null, "status": status,
            "resolution_notes": null, "pushed_to_sheets": 0,
            "created_at": time_in, "updated_at": time_in, "synced": 1,
            "is_discharge_trip": "0", "model_version": "v1", "ocr_engine": "paddleocr",
            "archived": archived,
        })
    };

    pg.push_rows("companies", &[company("c-1", "Acme Waste"), company("c-2", "Beta Logistics")])
        .expect("companies pushed");
    pg.push_rows(
        "trips",
        &[
            trip("t-1", "c-1", "logged", "0", "2026-07-10T08:00:00Z"),
            trip("t-2", "c-2", "logged", "0", "2026-07-11T09:00:00Z"),
            trip("t-3", "c-1", "declined", "0", "2026-07-12T10:00:00Z"),
            trip("t-4", "c-1", "logged", "1", "2026-07-13T11:00:00Z"),
        ],
    )
    .expect("trips pushed");

    // Aggregate surface — count only logged + not-archived trips.
    let summary = reporting::pg_report_summary(&pg, &ReportFilters::default()).expect("central summary");
    assert_eq!(summary.total_trips, 2, "declined + archived excluded from the archive view");
    assert_eq!(summary.active_companies, 2);

    let by_time = reporting::pg_trips_over_time(&pg, &ReportFilters::default()).expect("central over-time");
    assert_eq!(by_time.len(), 2);

    let top = reporting::pg_top_companies(&pg, &ReportFilters::default(), 10).expect("central top companies");
    assert_eq!(top.len(), 2);

    let drill = reporting::pg_report_trips(&pg, &ReportFilters::default(), 50).expect("central drill");
    assert_eq!(drill.len(), 2);
    assert!(drill.iter().all(|t| t.status == "logged"));

    let export = reporting::pg_report_export_rows(&pg, &ReportFilters::default()).expect("central export");
    assert_eq!(export.len(), 2);

    // Company filter parity.
    let only_c1 = ReportFilters {
        from: None,
        to: None,
        company_id: Some("c-1".to_string()),
    };
    let summary_c1 = reporting::pg_report_summary(&pg, &only_c1).expect("central company filter");
    assert_eq!(summary_c1.total_trips, 1, "company filter applies on the archive");

    // Cleanup: close the connection, then drop the throwaway database.
    let _ = pg.configure(None);
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

// ---------------------------------------------------------------------------
// Reference database import / export round-trip (CSV & XLSX)
// ---------------------------------------------------------------------------

#[test]
fn reference_export_then_import_round_trips_custom_fields() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    // A custom vehicle field "fleet_no" ensures export/import covers extra_fields.
    reference::create_field_definition(
        ctx.state(),
        admin.id.clone(),
        "vehicle".into(),
        "fleet_no".into(),
        "Fleet No".into(),
        "text".into(),
        false,
        None,
    )
    .expect("create vehicle field def");

    let company = reference::create_company(ctx.state(), admin.id.clone(), "Acme Waste".into(), None).unwrap();
    let driver = reference::create_driver(ctx.state(), admin.id.clone(), "D. Singh".into(), None).unwrap();
    let vehicle = reference::create_vehicle(
        ctx.state(),
        admin.id.clone(),
        "DL 4G 8834".into(),
        Some(company.id.clone()),
        Some(20.0),
        "litres".into(),
        Some(driver.id.clone()),
        Some(r#"{"fleet_no":"A-4412"}"#.into()),
    )
    .unwrap();

    let tmp = TempDb::new();

    // CSV export → re-import into the same DB upserts the existing plate.
    let csv_path = tmp.dir.join("vehicles.csv");
    let csv_out = reference::reference_export(
        ctx.state(),
        admin.id.clone(),
        "vehicle".into(),
        "csv".into(),
        Some(csv_path.to_string_lossy().into_owned()),
    )
    .expect("export vehicles csv");
    assert!(std::path::Path::new(&csv_out).exists());

    // XLSX export writes a real workbook.
    let xlsx_path = tmp.dir.join("vehicles.xlsx");
    let xlsx_out = reference::reference_export(
        ctx.state(),
        admin.id.clone(),
        "vehicle".into(),
        "xlsx".into(),
        Some(xlsx_path.to_string_lossy().into_owned()),
    )
    .expect("export vehicles xlsx");
    assert!(std::path::Path::new(&xlsx_out).exists());

    let summary = reference::reference_import(
        ctx.state(),
        admin.id.clone(),
        "vehicle".into(),
        csv_path.to_string_lossy().into_owned(),
    )
    .expect("import vehicles csv");
    assert_eq!(summary.created, 0, "same DB → update only");
    assert_eq!(summary.updated, 1, "one existing vehicle row");
    assert_eq!(summary.errors.len(), 0, "no import errors");

    // Custom field survived the round trip.
    let conn = ctx.conn();
    let plate_from_db: String = conn
        .query_row(
            "SELECT upper(plate_number) FROM vehicles WHERE id = ?1",
            rusqlite::params![vehicle.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(plate_from_db, "DL4G8834", "plate normalised on import");
    let extra: Option<String> = conn
        .query_row("SELECT extra_fields FROM vehicles WHERE id = ?1", rusqlite::params![vehicle.id], |r| r.get(0))
        .unwrap();
    assert!(
        extra.as_deref().map(|s| s.contains("A-4412")).unwrap_or(false),
        "custom field preserved through export/import: {extra:?}"
    );

    // Import into an empty DB via xlsx creates fresh rows.
    let fresh = TempDb::new();
    let fdir = fresh.dir.join("frames");
    std::fs::create_dir_all(&fdir).unwrap();
    let fconn = open_db(&fresh.db_path()).expect("open fresh db");
    let fapp = mock_app();
    fapp.manage(AppState {
        db: Mutex::new(fconn),
        session: Mutex::new(None),
        simulator: Arc::new(SimulatorSource::new()),
        anpr_last: Mutex::new(None),
        running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        frames_dir: fdir,
        pg: Arc::new(MockPostgres::new()),
        sheets: Arc::new(MockSheets::new()),
    });
    let admin2 = commands::create_first_admin(fapp.state(), "Boss2".into(), ADMIN_PASS.into())
        .expect("create second admin")
        .user;
    // Company + driver + the same vehicle field so xlsx custom columns are recognised.
    reference::create_company(fapp.state(), admin2.id.clone(), "Acme Waste".into(), None).unwrap();
    reference::create_driver(fapp.state(), admin2.id.clone(), "D. Singh".into(), None).unwrap();
    reference::create_field_definition(
        fapp.state(),
        admin2.id.clone(),
        "vehicle".into(),
        "fleet_no".into(),
        "Fleet No".into(),
        "text".into(),
        false,
        None,
    )
    .expect("create vehicle field def on second db");

    let summary2 = reference::reference_import(fapp.state(), admin2.id.clone(), "vehicle".into(), xlsx_out)
        .expect("import vehicles xlsx");
    assert_eq!(summary2.created, 1, "vehicle created from xlsx import");
    assert_eq!(summary2.updated, 0);
    assert_eq!(summary2.errors.len(), 0);

    // Unrecognised company/driver names are reported, not silently linked.
    let broken = fresh.dir.join("broken.csv");
    std::fs::write(&broken, "plate_number,company,status\nNO-SUCH-1,Nope Corp,active\n").unwrap();
    let summary3 = reference::reference_import(fapp.state(), admin2.id.clone(), "vehicle".into(), broken.to_string_lossy().into_owned())
        .expect("import broken csv");
    assert_eq!(summary3.created, 0, "vehicle skipped");
    assert_eq!(summary3.errors.len(), 1, "one per-row error reported");
    assert!(summary3.errors[0].contains("Nope Corp"));
}

// ---------------------------------------------------------------------------
// Combined reference import/export (one workbook, all entity types)
// ---------------------------------------------------------------------------

#[test]
fn combined_export_then_import_round_trips_all_entities() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    // Custom fields on vehicle + company so extra_fields survive the round trip.
    reference::create_field_definition(
        ctx.state(),
        admin.id.clone(),
        "vehicle".into(),
        "fleet_no".into(),
        "Fleet No".into(),
        "text".into(),
        false,
        None,
    )
    .expect("create vehicle field def");
    reference::create_field_definition(
        ctx.state(),
        admin.id.clone(),
        "company".into(),
        "region".into(),
        "Region".into(),
        "text".into(),
        false,
        None,
    )
    .expect("create company field def");

    let company = reference::create_company(
        ctx.state(),
        admin.id.clone(),
        "Acme Waste".into(),
        Some(r#"{"region":"North"}"#.into()),
    )
    .unwrap();
    let driver = reference::create_driver(ctx.state(), admin.id.clone(), "D. Singh".into(), None).unwrap();
    reference::create_vehicle(
        ctx.state(),
        admin.id.clone(),
        "DL 4G 8834".into(),
        Some(company.id.clone()),
        Some(20.0),
        "litres".into(),
        Some(driver.id.clone()),
        Some(r#"{"fleet_no":"A-4412"}"#.into()),
    )
    .unwrap();

    let tmp = TempDb::new();
    let out_path = tmp.dir.join("combined.xlsx");
    let path = reference::reference_export_combined(
        ctx.state(),
        admin.id.clone(),
        Some(out_path.to_string_lossy().into_owned()),
    )
    .expect("export combined xlsx");
    assert!(std::path::Path::new(&path).exists());

    // Preview classifies all three sheets and the custom columns.
    let preview = reference::reference_import_preview(ctx.state(), admin.id.clone(), path.clone()).expect("preview");
    assert_eq!(preview.sheets.len(), 3, "one sheet per entity");
    let vehicle_sheet = preview.sheets.iter().find(|s| s.entity_type == "vehicle").expect("vehicle sheet");
    assert_eq!(vehicle_sheet.row_count, 1);
    assert!(
        vehicle_sheet
            .columns
            .iter()
            .any(|c| matches!(c, ColumnInfo::Standard { field_key, .. } if field_key == "plate_number")),
        "plate column classified as standard"
    );
    assert!(
        vehicle_sheet
            .columns
            .iter()
            .any(|c| matches!(c, ColumnInfo::ExistingCustom { field_key, .. } if field_key == "fleet_no")),
        "fleet_no classified as existing custom field"
    );
    let company_sheet = preview.sheets.iter().find(|s| s.entity_type == "company").expect("company sheet");
    assert!(
        company_sheet
            .columns
            .iter()
            .any(|c| matches!(c, ColumnInfo::ExistingCustom { field_key, .. } if field_key == "region")),
        "region classified as existing custom field"
    );

    // Confirm the auto classification and apply the import back into the same DB.
    let sheets: Vec<ConfirmedSheet> = preview
        .sheets
        .iter()
        .map(|s| ConfirmedSheet {
            sheet_name: s.sheet_name.clone(),
            entity_type: s.entity_type.clone(),
            columns: s
                .columns
                .iter()
                .map(|c| match c {
                    ColumnInfo::Standard { header, field_key, .. } => ConfirmedColumn {
                        header: header.clone(),
                        mapping: field_key.clone(),
                        new_field_key: None,
                        new_field_type: None,
                        new_is_required: None,
                    },
                    ColumnInfo::ExistingCustom { header, field_key, .. } => ConfirmedColumn {
                        header: header.clone(),
                        mapping: field_key.clone(),
                        new_field_key: None,
                        new_field_type: None,
                        new_is_required: None,
                    },
                    ColumnInfo::NewCustom { header, field_key, field_type, is_required, .. } => ConfirmedColumn {
                        header: header.clone(),
                        mapping: "new".into(),
                        new_field_key: Some(field_key.clone()),
                        new_field_type: Some(field_type.clone()),
                        new_is_required: Some(*is_required),
                    },
                })
                .collect(),
        })
        .collect();
    let req = ReferenceImportRequest {
        file_path: path.clone(),
        sheets,
    };
    let summary = reference::reference_import_combined(ctx.state(), admin.id.clone(), req).expect("apply combined import");
    assert_eq!(summary.vehicles.updated, 1, "same DB → vehicle updated, not duplicated");
    assert_eq!(summary.companies.updated, 1);
    assert_eq!(summary.drivers.updated, 1);
    assert_eq!(summary.vehicles.created, 0);
    assert!(summary.vehicles.errors.is_empty(), "vehicle errors: {:?}", summary.vehicles.errors);
    assert!(summary.companies.errors.is_empty(), "company errors: {:?}", summary.companies.errors);

    // Custom field values survived the full round trip.
    let companies = reference::list_companies(ctx.state(), None).unwrap();
    let acme = companies.iter().find(|c| c.name == "Acme Waste").unwrap();
    assert_eq!(
        acme.extra_fields.as_ref().and_then(|m| m.get("region")).and_then(|v| v.as_str()),
        Some("North"),
        "company custom field preserved"
    );
    let vehicles = reference::list_vehicles(ctx.state(), None).unwrap();
    let v = vehicles.iter().find(|v| v.plate_number == "DL4G8834").unwrap();
    assert_eq!(v.company_name.as_deref(), Some("Acme Waste"));
    assert_eq!(
        v.extra_fields.as_ref().and_then(|m| m.get("fleet_no")).and_then(|v| v.as_str()),
        Some("A-4412"),
        "vehicle custom field preserved"
    );
}
