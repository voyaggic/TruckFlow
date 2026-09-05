//! Phase 1 exit-criteria checklist tests (01-database-schema.md §"Testing checklist"
//! and 03-auth-permissions.md §12). Drives the real tauri commands through a mock app
//! against a fresh temp database.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::test::{mock_app, MockRuntime};
use tauri::{App, Manager, State};

use truckflow_lib::capture::SimulatorSource;
use truckflow_lib::commands;
use truckflow_lib::db::{open_db, AppState};
use truckflow_lib::sync::{MockPostgres, MockSheets};

const ADMIN_PASS: &str = "AdminPass!2024";

struct TempDb {
    dir: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("truckflow_test_{}", uuid::Uuid::new_v4()));
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
    app: App<MockRuntime>,
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

    fn conn(&self) -> Connection {
        // Re-open a second handle to the same file (WAL mode allows concurrent readers).
        Connection::open(self._tmp.db_path()).expect("reopen db")
    }

    fn create_admin(&self) -> truckflow_lib::models::SessionUser {
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

    fn create_gate_user(&self, admin: &truckflow_lib::models::SessionUser, name: &str) -> truckflow_lib::models::UserView {
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

    fn create_user_with_password(&self, admin: &truckflow_lib::models::SessionUser, name: &str, permissions: Vec<String>, password: &str) -> truckflow_lib::models::UserView {
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

// ---------------------------------------------------------------------------
// 01-database-schema.md §"Testing checklist"
// ---------------------------------------------------------------------------

#[test]
fn first_run_appears_only_on_empty_users_table() {
    let ctx = TestCtx::new();
    let s0 = commands::app_status(ctx.state()).expect("app status");
    assert!(s0.needs_first_run, "fresh DB must need first run");

    let _admin = ctx.create_admin();

    let s1 = commands::app_status(ctx.state()).expect("app status");
    assert!(!s1.needs_first_run, "first run must not appear after first admin");

    let err = commands::create_first_admin(ctx.state(), "Boss2".to_string(), ADMIN_PASS.to_string())
        .expect_err("second first-admin must fail");
    assert!(err.contains("already"), "unexpected error: {err}");
}

#[test]
fn admin_gets_full_admin_permission_preset() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let keys: Vec<&str> = admin.permissions.iter().map(|p| p.key.as_str()).collect();
    for must in [
        "manage_users",
        "manage_reference_database",
        "view_audit_log",
        "manage_integrations",
        "view_reporting_dashboard",
        "view_system_health",
        "resolve_queue",
        "view_gate_entries",
    ] {
        assert!(keys.contains(&must), "admin preset missing {must}: {keys:?}");
    }
}

#[test]
fn uuids_are_generated_client_side_and_unique() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let a = ctx.create_gate_user(&admin, "Gate A");
    let b = ctx.create_gate_user(&admin, "Gate B");
    assert_ne!(a.id, b.id);
    assert_ne!(a.id, admin.id);
    // v4 hex uuid shape
    for id in [&a.id, &b.id, &admin.id] {
        assert_eq!(id.len(), 36, "uuid shape wrong: {id}");
    }
}

#[test]
fn fk_relationships_are_enforced() {
    let ctx = TestCtx::new();
    let conn = ctx.conn();
    let bad = conn.execute(
        "INSERT INTO trips (id, vehicle_id, time_in, created_at, updated_at)
         VALUES ('t1', 'no-such-vehicle', 'now', 'now', 'now')",
        [],
    );
    assert!(bad.is_err(), "FK violation for missing vehicle must be rejected");

    conn.execute(
        "INSERT INTO companies (id, name, created_at, updated_at) VALUES ('c1', 'Acme', 'now', 'now')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO drivers (id, name, created_at, updated_at) VALUES ('d1', 'Driver A', 'now', 'now')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO vehicles (id, plate_number, company_id, registered_capacity, created_at, updated_at)
         VALUES ('v1', 'AB12CDE', 'c1', 50, 'now', 'now')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO trips (id, vehicle_id, driver_id, company_id, capacity_at_trip, time_in, created_at, updated_at)
         VALUES ('t2', 'v1', 'd1', 'c1', 50, 'now', 'now', 'now')",
        [],
    )
    .unwrap();
}

#[test]
fn trip_capacity_is_a_snapshot_not_a_live_reference() {
    let ctx = TestCtx::new();
    let conn = ctx.conn();
    conn.execute(
        "INSERT INTO companies (id, name, created_at, updated_at) VALUES ('c1', 'Acme', 'now', 'now')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO vehicles (id, plate_number, company_id, registered_capacity, created_at, updated_at)
         VALUES ('v1', 'AB12CDE', 'c1', 50, 'now', 'now')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO trips (id, vehicle_id, company_id, capacity_at_trip, time_in, created_at, updated_at)
         VALUES ('t1', 'v1', 'c1', 50, 'now', 'now', 'now')",
        [],
    )
    .unwrap();

    conn.execute(
        "UPDATE vehicles SET registered_capacity = 40, updated_at = 'later' WHERE id = 'v1'",
        [],
    )
    .unwrap();

    let trip_cap: f64 = conn
        .query_row("SELECT capacity_at_trip FROM trips WHERE id = 't1'", [], |r| r.get(0))
        .unwrap();
    let veh_cap: f64 = conn
        .query_row("SELECT registered_capacity FROM vehicles WHERE id = 'v1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(trip_cap, 50.0, "capacity_at_trip must remain the snapshot");
    assert_eq!(veh_cap, 40.0, "vehicle capacity must have changed");
}

#[test]
fn disabling_user_does_not_orphan_trip_references() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin, "Gate A");
    let conn = ctx.conn();
    conn.execute(
        "INSERT INTO companies (id, name, created_at, updated_at) VALUES ('c1', 'Acme', 'now', 'now')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO vehicles (id, plate_number, company_id, registered_capacity, created_at, updated_at)
         VALUES ('v1', 'AB12CDE', 'c1', 50, 'now', 'now')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO trips (id, vehicle_id, company_id, capacity_at_trip, time_in, officer_id, created_at, updated_at)
         VALUES ('t1', 'v1', 'c1', 50, 'now', ?1, 'now', 'now')",
        rusqlite::params![gate.id],
    )
    .unwrap();

    commands::set_user_status(ctx.state(), admin.id.clone(), gate.id.clone(), "disabled".to_string())
        .expect("disable user");

    let still_resolves: String = conn
        .query_row(
            "SELECT u.name FROM trips t JOIN users u ON u.id = t.officer_id WHERE t.id = 't1'",
            [],
            |r| r.get(0),
        )
        .expect("officer reference must still resolve after disable");
    assert_eq!(still_resolves, "Gate A");
    let user_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM users WHERE status = 'disabled' AND id = ?1", rusqlite::params![gate.id], |r| r.get(0))
        .unwrap();
    assert_eq!(user_count, 1, "row is disabled, never deleted");
}

#[test]
fn permission_changes_are_staged_until_the_account_confirms() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin, "Gate A");
    assert_eq!(gate.auth_type, "password");

    let staged = commands::set_user_permissions(
        ctx.state(),
        admin.id.clone(),
        gate.id.clone(),
        vec!["manage_users".to_string()],
        ADMIN_PASS.to_string(),
    )
    .expect("admin stages a role change");
    assert!(!staged.applied, "role change must be staged, not applied");
    assert!(staged.auth_upgrade_required);

    let pending = commands::get_pending_upgrade(ctx.state(), gate.id.clone()).expect("pending lookup");
    let pending = pending.expect("change must be flagged");
    assert!(pending.permission_keys.contains(&"manage_users".to_string()));
    assert!(
        !pending.previous_permission_keys.contains(&"manage_users".to_string()),
        "staged diff must record the old permission set for the added/removed view"
    );
    assert!(!pending.requester_name.is_empty(), "staged diff must record who changed the role");

    let current = commands::get_user_permissions(ctx.state(), gate.id.clone()).expect("current perms");
    let current_keys: Vec<String> = current.iter().map(|p| p.key.clone()).collect();
    assert!(
        !current_keys.contains(&"manage_users".to_string()),
        "staged permission must not apply before confirmation: {current_keys:?}"
    );
}

#[test]
fn role_change_confirmation_requires_the_current_password() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_user_with_password(&admin, "Gate A", vec!["view_gate_entries".to_string(), "resolve_queue".to_string()], "GatePass!2024");

    commands::set_user_permissions(
        ctx.state(),
        admin.id.clone(),
        gate.id.clone(),
        vec!["manage_users".to_string()],
        ADMIN_PASS.to_string(),
    )
    .expect("stage role change");

    let wrong = commands::complete_auth_upgrade(ctx.state(), gate.id.clone(), "WrongPass!".to_string())
        .expect_err("wrong current password must fail");
    assert!(wrong.contains("Current credential"), "unexpected: {wrong}");

    let ok = commands::complete_auth_upgrade(ctx.state(), gate.id.clone(), "GatePass!2024".to_string())
        .expect("correct current password applies the change");
    assert!(ok.applied);

    let perms = commands::get_user_permissions(ctx.state(), gate.id.clone()).expect("perms");
    let keys: Vec<String> = perms.iter().map(|p| p.key.clone()).collect();
    assert!(keys.contains(&"manage_users".to_string()));

    // The account's password is unchanged — confirmation only, no new credential.
    let pw_login = commands::login_password(ctx.state(), "Gate A".to_string(), "GatePass!2024".to_string(), ctx.company_id())
        .expect("password login still works");
    assert_eq!(pw_login.user.auth_type, "password");
}

// ---------------------------------------------------------------------------
// 03-auth-permissions.md §12
// ---------------------------------------------------------------------------

#[test]
fn disabling_active_user_does_not_interrupt_session_but_blocks_next_login() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_user_with_password(&admin, "Gate A", vec!["view_gate_entries".to_string(), "resolve_queue".to_string()], "GatePass!2024");

    commands::logout(ctx.state()).ok();
    commands::login_password(ctx.state(), "Gate A".to_string(), "GatePass!2024".to_string(), ctx.company_id()).expect("gate login");

    // Admin disables the user while they hold the active session.
    commands::set_user_status(ctx.state(), admin.id.clone(), gate.id.clone(), "disabled".to_string())
        .expect("disable user");

    let cur = commands::get_current_user(ctx.state()).expect("current user");
    assert!(cur.is_some(), "active session must survive the account being disabled");
    let status = commands::app_status(ctx.state()).expect("app status");
    assert!(status.current_user.is_some(), "app must stay usable for the active session");
    assert!(!status.needs_first_run);

    commands::logout(ctx.state()).expect("logout");

    let err = commands::login_password(ctx.state(), "Gate A".to_string(), "GatePass!2024".to_string(), ctx.company_id()).expect_err("disabled user must be blocked");
    assert!(err.contains("disabled"), "unexpected: {err}");
    let err = commands::login_password(ctx.state(), "Gate A".to_string(), "whatever".to_string(), ctx.company_id()).expect_err("disabled user must be blocked");
    assert!(err.contains("disabled"), "unexpected: {err}");
}

#[test]
fn disabling_one_user_has_zero_effect_on_others() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let _a = ctx.create_user_with_password(&admin, "Gate A", vec!["view_gate_entries".to_string(), "resolve_queue".to_string()], "GatePass!2024");
    let b = ctx.create_user_with_password(&admin, "Gate B", vec!["view_gate_entries".to_string(), "resolve_queue".to_string()], "GatePass!2024");

    commands::logout(ctx.state()).ok();
    commands::login_password(ctx.state(), "Gate A".to_string(), "GatePass!2024".to_string(), ctx.company_id()).expect("A login");

    commands::set_user_status(ctx.state(), admin.id.clone(), b.id.clone(), "disabled".to_string())
        .expect("disable B");

    let cur = commands::get_current_user(ctx.state()).expect("current user");
    assert_eq!(cur.expect("A session intact").name, "Gate A");

    let list = commands::list_users(ctx.state()).expect("list users");
    let b_row = list.iter().find(|u| u.name == "Gate B").expect("B present");
    assert_eq!(b_row.status, "disabled");

    commands::logout(ctx.state()).ok();
    commands::login_password(ctx.state(), "Gate B".to_string(), "GatePass!2024".to_string(), ctx.company_id()).expect_err("B blocked");
    commands::login_password(ctx.state(), "Gate A".to_string(), "GatePass!2024".to_string(), ctx.company_id()).expect("A still logs in");
}

#[test]
fn reenabling_restores_full_history_and_permissions() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_user_with_password(&admin, "Gate A", vec!["view_gate_entries".to_string(), "resolve_queue".to_string()], "GatePass!2024");

    let before = commands::get_user_permissions(ctx.state(), gate.id.clone()).expect("perms before");

    commands::set_user_status(ctx.state(), admin.id.clone(), gate.id.clone(), "disabled".to_string())
        .expect("disable");
    commands::set_user_status(ctx.state(), admin.id.clone(), gate.id.clone(), "active".to_string())
        .expect("re-enable");

    let after = commands::get_user_permissions(ctx.state(), gate.id.clone()).expect("perms after");
    let b: Vec<String> = before.iter().map(|p| p.key.clone()).collect();
    let a: Vec<String> = after.iter().map(|p| p.key.clone()).collect();
    assert_eq!(a, b, "re-enabled account must keep its full permission set");

    commands::login_password(ctx.state(), "Gate A".to_string(), "GatePass!2024".to_string(), ctx.company_id()).expect("re-enabled user logs in");
}

#[test]
fn password_strength_rules_enforced() {
    let weak = commands::validate_password_strength("abc".to_string());
    assert!(!weak.valid);
    let strong = commands::validate_password_strength("Str0ng!Pass".to_string());
    assert!(strong.valid);
}

#[test]
fn downgrade_requires_only_current_password_confirmation() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let deputy = ctx.create_user_with_password(&admin, "Deputy", vec!["manage_reference_database".to_string()], "DeputyPass!2024");
    assert_eq!(deputy.auth_type, "password");

    // The acting admin must confirm their own password before any change.
    let blocked = commands::set_user_permissions(
        ctx.state(),
        admin.id.clone(),
        deputy.id.clone(),
        vec!["view_gate_entries".to_string()],
        "WrongPass!".to_string(),
    )
    .expect_err("wrong admin password must block the edit");
    assert!(blocked.contains("password is incorrect"), "unexpected: {blocked}");

    // Role change: staged, not applied, no new credential.
    let staged = commands::set_user_permissions(
        ctx.state(),
        admin.id.clone(),
        deputy.id.clone(),
        vec!["view_gate_entries".to_string()],
        ADMIN_PASS.to_string(),
    )
    .expect("stage role change");
    assert!(!staged.applied && staged.auth_upgrade_required);

    let pending = commands::get_pending_upgrade(ctx.state(), deputy.id.clone())
        .expect("pending lookup")
        .expect("change must be flagged");

    let current = commands::get_user_permissions(ctx.state(), deputy.id.clone()).expect("perms");
    assert!(
        current.iter().all(|p| p.key != "view_gate_entries"),
        "staged downgrade must not apply before confirmation"
    );

    // Wrong current password fails; correct one applies without a new credential.
    commands::complete_auth_upgrade(ctx.state(), deputy.id.clone(), "WrongPass!".to_string())
        .expect_err("wrong current password must fail");

    let ok = commands::complete_auth_upgrade(ctx.state(), deputy.id.clone(), "DeputyPass!2024".to_string())
        .expect("confirming the current password applies the change");
    assert!(ok.applied);

    let conn = ctx.conn();
    let auth_type: String = conn
        .query_row("SELECT auth_type FROM users WHERE id = ?1", rusqlite::params![deputy.id], |r| r.get(0))
        .unwrap();
    assert_eq!(auth_type, "password", "the password is never replaced — confirmation only");

    let after = commands::get_user_permissions(ctx.state(), deputy.id.clone()).expect("perms after");
    let keys: Vec<String> = after.iter().map(|p| p.key.clone()).collect();
    assert!(keys.contains(&"view_gate_entries".to_string()));
    assert!(!keys.contains(&"manage_reference_database".to_string()));

    let pending_after = commands::get_pending_upgrade(ctx.state(), deputy.id.clone()).expect("pending lookup");
    assert!(pending_after.is_none(), "confirmation must clear the staged change");

    // The account still signs in with the unchanged password.
    let pw_login = commands::login_password(ctx.state(), "Deputy".to_string(), "DeputyPass!2024".to_string(), ctx.company_id())
        .expect("password login still works");
    assert_eq!(pw_login.user.auth_type, "password");
}

#[test]
fn admin_can_soft_delete_and_restore_a_user() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_user_with_password(&admin, "Gate A", vec!["view_gate_entries".to_string(), "resolve_queue".to_string()], "GatePass!2024");

    // Wrong admin password blocks the delete.
    let blocked = commands::delete_user(ctx.state(), admin.id.clone(), gate.id.clone(), "WrongPass!".to_string())
        .expect_err("wrong admin password must block the delete");
    assert!(blocked.contains("password is incorrect"), "unexpected: {blocked}");

    commands::delete_user(ctx.state(), admin.id.clone(), gate.id.clone(), ADMIN_PASS.to_string())
        .expect("delete the user");

    // Deleted accounts cannot sign in.
    let err = commands::login_password(ctx.state(), "Gate A".to_string(), "GatePass!2024".to_string(), ctx.company_id())
        .expect_err("deleted account must be blocked from login");
    assert!(err.contains("deleted"), "unexpected: {err}");

    // The account stays listed with status 'deleted' — history is intact.
    let users = commands::list_users(ctx.state()).expect("list users");
    let deleted = users.iter().find(|u| u.id == gate.id).expect("deleted user still listed");
    assert_eq!(deleted.status, "deleted");

    // Restore brings it back to full sign-in.
    commands::restore_user(ctx.state(), admin.id.clone(), gate.id.clone()).expect("restore");
    let login = commands::login_password(ctx.state(), "Gate A".to_string(), "GatePass!2024".to_string(), ctx.company_id())
        .expect("restored account can sign in");
    assert_eq!(login.user.id, gate.id);
}

#[test]
fn cannot_delete_self_or_the_last_admin() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();

    // Deleting yourself is blocked.
    let self_err = commands::delete_user(ctx.state(), admin.id.clone(), admin.id.clone(), ADMIN_PASS.to_string())
        .expect_err("deleting yourself must be blocked");
    assert!(self_err.contains("own account"), "unexpected: {self_err}");

    // A second admin: deleting them is fine — the actor remains.
    let second = commands::create_user(
        ctx.state(),
        admin.id.clone(),
        "Second Admin".to_string(),
        vec!["manage_users".to_string()],
        ctx.company_id(),
    )
    .expect("create second admin");
    commands::delete_user(ctx.state(), admin.id.clone(), second.id.clone(), ADMIN_PASS.to_string())
        .expect("deleting a second admin is allowed while one remains");

    // Edge case the guard protects: a disabled admin with a live session tries
    // to delete the only remaining active admin — blocked, no lockout possible.
    let third = commands::create_user(
        ctx.state(),
        admin.id.clone(),
        "Third Admin".to_string(),
        vec!["manage_users".to_string()],
        ctx.company_id(),
    )
    .expect("create third admin");
    commands::set_user_status(ctx.state(), third.id.clone(), admin.id.clone(), "disabled".to_string())
        .expect("disable the original admin");
    let guard = commands::delete_user(ctx.state(), admin.id.clone(), third.id.clone(), ADMIN_PASS.to_string())
        .expect_err("deleting the last active admin must be blocked");
    assert!(guard.contains("last admin"), "unexpected: {guard}");
}

#[test]
fn purge_removes_the_account_and_its_references() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin, "Gate A");
    let gate_id = gate.id.clone();

    // Purge requires a deleted account first.
    commands::purge_user(ctx.state(), admin.id.clone(), gate_id.clone(), ADMIN_PASS.to_string())
        .expect_err("purge only applies to deleted accounts");

    commands::delete_user(ctx.state(), admin.id.clone(), gate_id.clone(), ADMIN_PASS.to_string())
        .expect("soft delete first");
    commands::purge_user(ctx.state(), admin.id.clone(), gate_id.clone(), ADMIN_PASS.to_string())
        .expect("purge the deleted account");

    let users = commands::list_users(ctx.state()).expect("list users");
    assert!(
        users.iter().all(|u| u.id != gate_id),
        "purged account must be gone from the user list"
    );
    let conn = ctx.conn();
    let leftover: i64 = conn
        .query_row("SELECT COUNT(*) FROM users WHERE id = ?1", rusqlite::params![gate_id], |r| r.get(0))
        .unwrap();
    assert_eq!(leftover, 0, "user row must be physically removed");
    let perms_left: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM user_permissions WHERE user_id = ?1",
            rusqlite::params![gate_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(perms_left, 0, "permissions must be removed with the account");
}

#[test]
fn admin_password_reset_forces_a_new_password() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_gate_user(&admin, "Gate A");

    // Wrong admin password blocks the reset.
    commands::reset_user_password(
        ctx.state(),
        admin.id.clone(),
        gate.id.clone(),
        "TempPass!2024".to_string(),
        "WrongPass!".to_string(),
    )
    .expect_err("wrong admin password must block the reset");

    commands::reset_user_password(
        ctx.state(),
        admin.id.clone(),
        gate.id.clone(),
        "TempPass!2024".to_string(),
        ADMIN_PASS.to_string(),
    )
    .expect("admin resets the password");

    // Old password stops working; the temporary one works but flags a forced change.
    commands::login_password(ctx.state(), "Gate A".to_string(), "GatePass!2024".to_string(), ctx.company_id())
        .expect_err("old password must stop working");
    let login = commands::login_password(ctx.state(), "Gate A".to_string(), "TempPass!2024".to_string(), ctx.company_id())
        .expect("temporary password works");
    assert!(login.must_change_password, "reset must force a password change");

    // The user picks their own password; the flag clears.
    commands::change_own_credential(
        ctx.state(),
        gate.id.clone(),
        "TempPass!2024".to_string(),
        "FreshPass!2024".to_string(),
    )
    .expect("user sets their own password");
    let again = commands::login_password(ctx.state(), "Gate A".to_string(), "FreshPass!2024".to_string(), ctx.company_id())
        .expect("new password works");
    assert!(!again.must_change_password, "flag cleared after the user changes the password");
}

#[test]
fn recovery_code_resets_the_admin_password() {
    let ctx = TestCtx::new();
    let created = commands::create_first_admin_for_company(ctx.state(), "Boss".to_string(), ADMIN_PASS.to_string(), "Default Company".to_string())
        .expect("create first admin");
    let code = created.recovery_code.expect("first-run shows the recovery code");
    let admin_id = created.user.id.clone();
    if let Some(ref cid) = created.user.company_id {
        *ctx.company_id.borrow_mut() = cid.clone();
    }

    // A non-admin account cannot use the recovery code.
    ctx.create_gate_user(&created.user, "Gate A");
    let non_admin = commands::recover_admin_password(
        ctx.state(),
        "Gate A".to_string(),
        code.clone(),
        "NewAdminPass!2024".to_string(),
    )
    .expect_err("recovery is for admin accounts only");
    assert!(non_admin.contains("admin accounts"), "unexpected: {non_admin}");

    // Wrong code fails; correct code resets the password and signs in.
    commands::recover_admin_password(
        ctx.state(),
        "Boss".to_string(),
        "WRONG-CODE".to_string(),
        "NewAdminPass!2024".to_string(),
    )
    .expect_err("wrong recovery code must fail");

    let recovered = commands::recover_admin_password(
        ctx.state(),
        "Boss".to_string(),
        code,
        "NewAdminPass!2024".to_string(),
    )
    .expect("recovery resets the admin password");
    assert_eq!(recovered.user.id, admin_id);

    commands::login_password(ctx.state(), "Boss".to_string(), ADMIN_PASS.to_string(), ctx.company_id())
        .expect_err("old password no longer works");
    let login = commands::login_password(ctx.state(), "Boss".to_string(), "NewAdminPass!2024".to_string(), ctx.company_id())
        .expect("new password works");
    assert!(!login.must_change_password);
}

#[test]
fn staff_can_request_a_password_reset_and_the_admin_resolves_it() {
    let ctx = TestCtx::new();
    let admin = ctx.create_admin();
    let gate = ctx.create_user_with_password(&admin, "Gate A", vec!["view_gate_entries".to_string(), "resolve_queue".to_string()], "GatePass!2024");

    // A staff account requests a reset (no auth — login screen).
    commands::create_password_reset_request(ctx.state(), "Gate A".to_string())
        .expect("staff request a reset");
    // Duplicate requests collapse into one pending entry.
    commands::create_password_reset_request(ctx.state(), "Gate A".to_string())
        .expect("duplicate request replaces the pending one");

    let requests = commands::list_password_reset_requests(ctx.state(), admin.id.clone()).expect("list requests");
    assert_eq!(requests.len(), 1, "duplicate requests must not pile up");
    assert_eq!(requests[0].username, "Gate A");

    // A non-admin cannot list requests.
    commands::list_password_reset_requests(ctx.state(), gate.id.clone())
        .expect_err("non-admins must not see the request queue");

    // Resetting the password clears the request.
    commands::reset_user_password(
        ctx.state(),
        admin.id.clone(),
        gate.id.clone(),
        "TempPass!2024".to_string(),
        ADMIN_PASS.to_string(),
    )
    .expect("admin resets the password");
    let after = commands::list_password_reset_requests(ctx.state(), admin.id.clone()).expect("list requests");
    assert!(after.is_empty(), "a fulfilled reset must clear the request");

    // Ignore flow: a new request can be dismissed without a reset.
    commands::create_password_reset_request(ctx.state(), "Gate A".to_string()).expect("new request");
    let again = commands::list_password_reset_requests(ctx.state(), admin.id.clone()).expect("list");
    commands::dismiss_password_reset_request(ctx.state(), admin.id.clone(), again[0].id.clone())
        .expect("admin dismisses the request");
    let gone = commands::list_password_reset_requests(ctx.state(), admin.id.clone()).expect("list");
    assert!(gone.is_empty());

    // Requests for unknown or deleted accounts are rejected.
    commands::create_password_reset_request(ctx.state(), "Nobody".to_string())
        .expect_err("unknown accounts cannot request");
    commands::delete_user(ctx.state(), admin.id.clone(), gate.id.clone(), ADMIN_PASS.to_string())
        .expect("soft delete");
    commands::create_password_reset_request(ctx.state(), "Gate A".to_string())
        .expect_err("deleted accounts cannot request");
}

#[test]
fn recovery_code_lives_in_a_file_and_can_be_regenerated() {
    let ctx = TestCtx::new();
    let created = commands::create_first_admin_for_company(ctx.state(), "Boss".to_string(), ADMIN_PASS.to_string(), "Default Company".to_string())
        .expect("create first admin");
    let code = created.recovery_code.expect("code generated at first-run");

    // The code is written to a file next to the app data folder.
    let file = ctx._tmp.dir.join("recovery-code.txt");
    assert!(file.exists(), "recovery file must be written: {}", file.display());
    let content = std::fs::read_to_string(&file).expect("read recovery file");
    assert!(content.contains(&code), "file must contain the plain code");

    // check_recovery_code validates username + code; wrong code fails.
    commands::check_recovery_code(ctx.state(), "Boss".to_string(), "WRONG-CODE".to_string())
        .expect_err("wrong code must fail");
    commands::check_recovery_code(ctx.state(), "Boss".to_string(), code.clone()).expect("correct code passes");
    // Non-admin accounts cannot use the code path.
    let gate = ctx.create_gate_user(&created.user, "Gate A");
    commands::check_recovery_code(ctx.state(), "Gate A".to_string(), code.clone())
        .expect_err("recovery code is for admin accounts only");

    // Only admins can read or regenerate the code.
    commands::get_recovery_code(ctx.state(), gate.id.clone())
        .expect_err("non-admins must not read the recovery code");
    commands::regenerate_recovery_code(ctx.state(), gate.id.clone())
        .expect_err("non-admins must not regenerate the recovery code");

    // Regenerating replaces the code everywhere: new code differs, old dies.
    let info = commands::regenerate_recovery_code(ctx.state(), created.user.id.clone()).expect("regenerate");
    assert_ne!(info.code, code, "regenerated code must differ");
    commands::check_recovery_code(ctx.state(), "Boss".to_string(), code)
        .expect_err("old code must stop working after regeneration");
    commands::check_recovery_code(ctx.state(), "Boss".to_string(), info.code.clone())
        .expect("new code works");
    let read = commands::get_recovery_code(ctx.state(), created.user.id.clone()).expect("read code");
    assert_eq!(read.code, info.code);
    let fresh = std::fs::read_to_string(&file).expect("recovery file rewritten");
    assert!(fresh.contains(&info.code), "file must be rewritten with the new code");
}
