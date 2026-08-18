use std::path::PathBuf;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

use crate::capture::SimulatorSource;
use crate::sync::{PostgresAdapter, SheetsProvider};

pub struct Session {
    pub user_id: String,
    #[allow(dead_code)]
    pub logged_in_at: String,
    #[allow(dead_code)]
    pub auth_type: String,
}

pub struct AppState {
    pub db: Mutex<Connection>,
    pub session: Mutex<Option<Session>>,
    pub simulator: Arc<SimulatorSource>,
    pub anpr_last: Mutex<Option<(String, String)>>,
    pub running: Arc<std::sync::atomic::AtomicBool>,
    /// Root directory where frame evidence files are stored (04 §7.4).
    pub frames_dir: PathBuf,
    /// PostgreSQL sync adapter (mock in dev, real driver swappable later).
    pub pg: Arc<dyn PostgresAdapter>,
    /// Google Sheets export adapter (mock in dev).
    pub sheets: Arc<dyn SheetsProvider>,
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn init_state(app: &AppHandle) -> Result<AppState, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("cannot resolve app data dir: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create app data dir: {e}"))?;
    let db_path = dir.join("truckflow.db");
    let conn = open_db(&db_path)?;
    let frames_dir = dir.join("frames");
    std::fs::create_dir_all(&frames_dir).map_err(|e| format!("cannot create frames dir: {e}"))?;
    let pg = crate::sync::real_postgres(&conn);
    let sheets = crate::sync::real_sheets(&conn);
    Ok(AppState {
        db: Mutex::new(conn),
        session: Mutex::new(None),
        simulator: Arc::new(SimulatorSource::new()),
        anpr_last: Mutex::new(None),
        running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        frames_dir,
        pg,
        sheets,
    })
}

/// Open a database file, enable foreign keys, run migrations and seed data.
/// Shared by the app and integration tests.
pub fn open_db(db_path: &std::path::Path) -> Result<Connection, String> {
    let conn = Connection::open(db_path).map_err(|e| format!("cannot open database: {e}"))?;
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
        .map_err(|e| format!("database pragma failed: {e}"))?;
    migrate(&conn)?;
    seed(&conn)?;
    ensure_recovery_code(&conn, db_path)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<(), String> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| format!("cannot read schema version: {e}"))?;

    if current < 1 {
        conn.execute_batch(
            r#"
            CREATE TABLE companies (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                extra_fields TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE drivers (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                extra_fields TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE vehicles (
                id TEXT PRIMARY KEY,
                plate_number TEXT NOT NULL,
                company_id TEXT REFERENCES companies(id),
                registered_capacity REAL,
                default_driver_id TEXT REFERENCES drivers(id),
                status TEXT NOT NULL DEFAULT 'active',
                extra_fields TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX idx_vehicles_plate ON vehicles(plate_number);

            CREATE TABLE users (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                auth_type TEXT NOT NULL,
                credential_hash TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                revoked_by TEXT REFERENCES users(id),
                revoked_at TEXT,
                profile_photo_ref TEXT,
                phone_number TEXT,
                theme_mode TEXT DEFAULT 'light',
                theme_accent TEXT,
                language_preference TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE trips (
                id TEXT PRIMARY KEY,
                vehicle_id TEXT REFERENCES vehicles(id),
                driver_id TEXT REFERENCES drivers(id),
                company_id TEXT,
                capacity_at_trip REAL,
                time_in TEXT NOT NULL,
                receipt_no TEXT,
                officer_id TEXT REFERENCES users(id),
                capture_method TEXT NOT NULL DEFAULT 'auto',
                confidence_score REAL,
                photo_refs TEXT,
                status TEXT NOT NULL DEFAULT 'logged',
                resolution_notes TEXT,
                pushed_to_sheets INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX idx_trips_time_in ON trips(time_in);
            CREATE INDEX idx_trips_status ON trips(status);

            CREATE TABLE permissions (
                id TEXT PRIMARY KEY,
                key TEXT NOT NULL UNIQUE,
                min_auth_level TEXT NOT NULL,
                description TEXT
            );

            CREATE TABLE user_permissions (
                user_id TEXT REFERENCES users(id),
                permission_id TEXT REFERENCES permissions(id),
                granted_by TEXT REFERENCES users(id),
                granted_at TEXT NOT NULL,
                PRIMARY KEY (user_id, permission_id)
            );

            CREATE TABLE role_presets (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                permission_ids TEXT NOT NULL
            );

            CREATE TABLE audit_log (
                id TEXT PRIMARY KEY,
                actor_id TEXT REFERENCES users(id),
                action TEXT NOT NULL,
                target_id TEXT,
                details TEXT,
                timestamp TEXT NOT NULL
            );

            CREATE TABLE system_health_events (
                id TEXT PRIMARY KEY,
                component TEXT NOT NULL,
                status TEXT NOT NULL,
                detected_at TEXT NOT NULL,
                acknowledged_by TEXT REFERENCES users(id),
                resolved_at TEXT
            );

            CREATE TABLE integrations (
                id TEXT PRIMARY KEY,
                type TEXT NOT NULL,
                connected_by TEXT REFERENCES users(id),
                oauth_token_ref TEXT,
                target_sheet_id TEXT,
                shared_group TEXT,
                sync_frequency TEXT NOT NULL DEFAULT 'realtime',
                last_synced_at TEXT,
                status TEXT NOT NULL DEFAULT 'disconnected',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE app_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )
        .map_err(|e| format!("migration 1 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 1;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 2 {
        conn.execute_batch(
            r#"
            ALTER TABLE trips ADD COLUMN is_discharge_trip INTEGER;
            ALTER TABLE trips ADD COLUMN model_version TEXT;
            ALTER TABLE trips ADD COLUMN ocr_engine TEXT;

            CREATE TABLE anpr_config (
                id TEXT PRIMARY KEY,
                active_ocr_engine TEXT NOT NULL DEFAULT 'paddleocr',
                confidence_threshold_paddleocr REAL NOT NULL DEFAULT 0.7,
                confidence_threshold_easyocr REAL NOT NULL DEFAULT 0.7,
                plate_vehicle_ratio_threshold REAL NOT NULL DEFAULT 0.05,
                discharge_confirmation_required INTEGER NOT NULL DEFAULT 1,
                plate_format_rules TEXT,
                save_recognition_images INTEGER NOT NULL DEFAULT 1,
                retrain_candidate_threshold INTEGER,
                updated_by TEXT REFERENCES users(id),
                extra_fields TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE anpr_credentials (
                id TEXT PRIMARY KEY,
                key_name TEXT NOT NULL,
                key_value_ref TEXT NOT NULL,
                rotated_by TEXT REFERENCES users(id),
                rotated_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE camera_sources (
                id TEXT PRIMARY KEY,
                label TEXT NOT NULL,
                source_type TEXT NOT NULL,
                connection_string TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                last_connection_check_at TEXT,
                last_connection_check_result TEXT,
                extra_fields TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE model_versions (
                id TEXT PRIMARY KEY,
                version_label TEXT NOT NULL,
                component TEXT NOT NULL,
                validation_accuracy REAL,
                is_live INTEGER NOT NULL DEFAULT 0,
                deployed_by TEXT REFERENCES users(id),
                deployed_at TEXT,
                rolled_back_from TEXT REFERENCES model_versions(id),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX idx_model_versions_component ON model_versions(component);
            CREATE UNIQUE INDEX idx_model_versions_one_live
                ON model_versions(component) WHERE is_live = 1;

            CREATE TABLE training_candidates (
                id TEXT PRIMARY KEY,
                source_trip_id TEXT REFERENCES trips(id),
                frame_ref TEXT NOT NULL,
                reason TEXT NOT NULL,
                used_in_model_version_id TEXT REFERENCES model_versions(id),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )
        .map_err(|e| format!("migration 2 failed: {e}"))?;
        conn.execute(
            "INSERT OR IGNORE INTO anpr_config
                (id, active_ocr_engine, confidence_threshold_paddleocr,
                 confidence_threshold_easyocr, plate_vehicle_ratio_threshold,
                 discharge_confirmation_required, save_recognition_images, created_at, updated_at)
             VALUES ('00000000-0000-0000-0000-000000000001', 'paddleocr', 0.7, 0.7, 0.05, 1, 1, ?1, ?1)",
            params![now_iso()],
        )
        .map_err(|e| format!("anpr_config seed failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 2;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 3 {
        // Phase-4 addition: capacity is recorded in a unit that defaults to
        // litres (00-project-overview.md paper log "Capacity(L)") and can be
        // switched per vehicle. The unit is snapshotted onto trips the same way
        // `capacity_at_trip` is (01-database-schema.md snapshot rule).
        conn.execute_batch(
            r#"
            ALTER TABLE vehicles ADD COLUMN capacity_unit TEXT NOT NULL DEFAULT 'litres';
            ALTER TABLE trips ADD COLUMN capacity_unit TEXT NOT NULL DEFAULT 'litres';
            "#,
        )
        .map_err(|e| format!("migration 3 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 3;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 4 {
        // Phase-5 addition: health events carry a free-text detail and a
        // distinct acknowledged-at timestamp (05-ui-screens.md §6h).
        conn.execute_batch(
            r#"
            ALTER TABLE system_health_events ADD COLUMN detail TEXT;
            ALTER TABLE system_health_events ADD COLUMN acknowledged_at TEXT;
            "#,
        )
        .map_err(|e| format!("migration 4 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 4;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 5 {
        // Phase-6 addition: per-user notification preference (05 §4). The rest
        // of the profile fields (phone, photo, language) already exist from the
        // original schema — only the sound toggle is new.
        conn.execute_batch(
            "ALTER TABLE users ADD COLUMN notification_sound INTEGER NOT NULL DEFAULT 1;",
        )
        .map_err(|e| format!("migration 5 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 5;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 6 {
        // Phase-6 addition: a persistent per-read ANPR event series so System
        // Monitor can plot confidence over time (05-ui-screens.md §6h). The
        // poller appends one row per read; the trend query aggregates it.
        conn.execute_batch(
            r#"
            CREATE TABLE anpr_read_events (
                id TEXT PRIMARY KEY,
                timestamp TEXT NOT NULL,
                plate TEXT,
                confidence REAL,
                engine TEXT,
                source TEXT,
                status TEXT NOT NULL DEFAULT 'captured',
                created_at TEXT NOT NULL
            );
            CREATE INDEX idx_anpr_read_events_ts ON anpr_read_events(timestamp);
            "#,
        )
        .map_err(|e| format!("migration 6 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 6;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 7 {
        // The app default appearance is now the light theme with the blue
        // accent. Previously the stored default was 'system' (follows the OS);
        // flip rows still holding that old implicit default so existing
        // installs match fresh ones. Explicit 'dark'/'light'/'system' choices
        // made in Settings after this point are untouched on later runs.
        conn.execute(
            "UPDATE users SET theme_mode = 'light' WHERE theme_mode IS NULL OR theme_mode = 'system'",
            [],
        )
        .map_err(|e| format!("migration 7 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 7;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 8 {
        // The anpr_config table gained a capture-point flag (08 §6): whether
        // this machine acts as an ANPR capture point. Added for fresh and
        // existing installs alike.
        conn.execute(
            "ALTER TABLE anpr_config ADD COLUMN is_capture_point INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(|e| format!("migration 8 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 8;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 9 {
        // The Admin role bundle now includes manage_anpr_config, but role
        // presets are only applied when an account is created. Backfill the
        // grant for existing accounts that already hold admin powers
        // (manage_users) so current admins see the ANPR tab without needing to
        // be re-created. Idempotent: the (user_id, permission_id) primary key
        // makes INSERT OR IGNORE safe to re-run.
        conn.execute(
            "INSERT OR IGNORE INTO user_permissions (user_id, permission_id, granted_by, granted_at)
             SELECT u.id, p.id, u.id, ?1
             FROM users u
             JOIN permissions p ON p.key = 'manage_anpr_config'
             WHERE EXISTS (
                 SELECT 1 FROM user_permissions up
                 JOIN permissions pp ON pp.id = up.permission_id
                 WHERE up.user_id = u.id AND pp.key = 'manage_users'
             )",
            params![now_iso()],
        )
        .map_err(|e| format!("migration 9 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 9;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 10 {
        // PIN auth is gone — every account signs in with username + password.
        // Existing rows keep their stored credential hash (a PIN user can sign
        // in with their PIN once as the password and change it afterwards).
        conn.execute("UPDATE users SET auth_type = 'password'", [])
            .map_err(|e| format!("migration 10 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 10;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 11 {
        // Admin password resets set a temporary password that must be replaced
        // at the next sign-in; the flag gates the app until the user changes it.
        conn.execute(
            "ALTER TABLE users ADD COLUMN must_change_password INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(|e| format!("migration 11 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 11;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 12 {
        // Forgot-password flow for staff: a request lands here and an admin
        // reviews it (and resets the password) from the Admin panel.
        conn.execute_batch(
            "CREATE TABLE password_reset_requests (
                id TEXT PRIMARY KEY,
                username TEXT NOT NULL,
                requested_at TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending'
            );
            CREATE INDEX idx_reset_requests_username ON password_reset_requests(username);",
        )
        .map_err(|e| format!("migration 12 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 12;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 13 {
        // Archive management (soft delete): archived trips are hidden from the
        // app and excluded from future sheet exports, but stay in the local DB
        // and the Postgres archive (the permanent record). Hard delete removes
        // them everywhere; the flag also lets the sheet prune them.
        conn.execute(
            "ALTER TABLE trips ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(|e| format!("migration 13 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 13;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 14 {
        // Dynamic field definitions for the reference database (companies,
        // vehicles, drivers). Admins can add custom fields with a type
        // (text/number/boolean/mixed) and the registration forms auto-generate
        // from these definitions. Values are stored in the existing
        // `extra_fields` JSON column on each entity.
        conn.execute_batch(
            r#"
            CREATE TABLE field_definitions (
                id TEXT PRIMARY KEY,
                entity_type TEXT NOT NULL,
                field_key TEXT NOT NULL,
                field_label TEXT NOT NULL,
                field_type TEXT NOT NULL DEFAULT 'text',
                is_required INTEGER NOT NULL DEFAULT 0,
                sort_order INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE UNIQUE INDEX idx_field_definitions_unique
                ON field_definitions(entity_type, field_key);
            "#,
        )
        .map_err(|e| format!("migration 14 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 14;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 15 {
        // Field definitions become the single source of truth for the
        // reference-database schema: the built-in fields (plate, company,
        // driver, capacity, name) are seeded as *standard* definitions so
        // admins can rename their labels, change their type, or hide them,
        // while custom fields stay fully dynamic. `is_hidden` lets standard
        // fields be removed from the UI/import/export without dropping the
        // real column they map to.
        conn.execute_batch(
            r#"
            ALTER TABLE field_definitions ADD COLUMN is_standard INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE field_definitions ADD COLUMN is_hidden INTEGER NOT NULL DEFAULT 0;
            "#,
        )
        .map_err(|e| format!("migration 15 alter failed: {e}"))?;
        let now = now_iso();
        // Seed the built-in fields (only when not already present — the unique
        // index on (entity_type, field_key) makes this safe to re-run).
        let seeds: &[(&str, &str, &str, &str, i32, bool)] = &[
            ("vehicle", "plate_number", "Plate", "text", 0, true),
            ("vehicle", "company", "Company", "text", 1, false),
            ("vehicle", "driver", "Driver", "text", 2, false),
            ("vehicle", "registered_capacity", "Capacity (L)", "number", 3, false),
            ("vehicle", "capacity_unit", "Capacity Unit", "text", 4, false),
            ("vehicle", "status", "Status", "text", 5, false),
            ("company", "name", "Company Name", "text", 0, true),
            ("company", "status", "Status", "text", 1, false),
            ("driver", "name", "Driver Name", "text", 0, true),
            ("driver", "status", "Status", "text", 1, false),
        ];
        for (entity_type, field_key, field_label, field_type, sort_order, is_required) in seeds {
            conn.execute(
                "INSERT OR IGNORE INTO field_definitions
                    (id, entity_type, field_key, field_label, field_type, is_required, sort_order, is_standard, is_hidden, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, 0, ?8, ?8)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    entity_type,
                    field_key,
                    field_label,
                    field_type,
                    *is_required as i32,
                    sort_order,
                    now,
                ],
            )
            .map_err(|e| format!("migration 15 seed failed: {e}"))?;
        }
        conn.execute_batch("PRAGMA user_version = 15;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 16 {
        // Field keys become freely editable. Standard fields get a fixed
        // internal `binding` (the real column they map to) so renaming the key
        // doesn't break forms/import/export; custom fields keep binding NULL.
        conn.execute_batch(
            r#"
            ALTER TABLE field_definitions ADD COLUMN binding TEXT;
            UPDATE field_definitions SET binding = field_key WHERE is_standard = 1;
            "#,
        )
        .map_err(|e| format!("migration 16 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 16;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 17 {
        // Display names for the three reference entities (Vehicles / Companies /
        // Drivers) become user-editable so the whole app reflects the operation's
        // own vocabulary. Keys stay fixed (vehicle/company/driver).
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS entity_labels (
                entity_type TEXT PRIMARY KEY,
                label TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .map_err(|e| format!("migration 17 failed: {e}"))?;
        let now = now_iso();
        for (et, label) in [("vehicle", "Vehicles"), ("company", "Companies"), ("driver", "Drivers")] {
            conn.execute(
                "INSERT OR IGNORE INTO entity_labels (entity_type, label, updated_at) VALUES (?1, ?2, ?3)",
                params![et, label, now],
            )
            .map_err(|e| format!("migration 17 seed failed: {e}"))?;
        }
        conn.execute_batch("PRAGMA user_version = 17;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 18 {
        // Parents become dynamic: the three built-in entities (vehicle / company /
        // driver) plus any the admin adds (e.g. "Trailers"). Each parent owns its
        // field definitions (children). New parents store their records in the
        // generic entity_records table; the built-ins keep their dedicated tables.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS reference_entities (
                entity_type TEXT PRIMARY KEY,
                label TEXT NOT NULL,
                is_core INTEGER NOT NULL DEFAULT 0,
                sort_order INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS entity_records (
                id TEXT PRIMARY KEY,
                entity_type TEXT NOT NULL,
                data TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_entity_records_type ON entity_records(entity_type);
            "#,
        )
        .map_err(|e| format!("migration 18 failed: {e}"))?;
        let now = now_iso();
        for (i, (et, label)) in [("vehicle", "Vehicles"), ("company", "Companies"), ("driver", "Drivers")]
            .iter()
            .enumerate()
        {
            conn.execute(
                "INSERT OR IGNORE INTO reference_entities (entity_type, label, is_core, sort_order, created_at, updated_at) VALUES (?1, ?2, 1, ?3, ?4, ?4)",
                params![et, label, i as i32, now],
            )
            .map_err(|e| format!("migration 18 seed failed: {e}"))?;
            // Carry over any user-renamed label from entity_labels (migration 17).
            let renamed: Option<String> = conn
                .query_row(
                    "SELECT label FROM entity_labels WHERE entity_type = ?1",
                    params![et],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| format!("migration 18 label carry failed: {e}"))?;
            if let Some(lbl) = renamed {
                conn.execute(
                    "UPDATE reference_entities SET label = ?1 WHERE entity_type = ?2",
                    params![lbl, et],
                )
                .map_err(|e| format!("migration 18 label update failed: {e}"))?;
            }
        }
        conn.execute_batch("PRAGMA user_version = 18;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 19 {
        // Custom fields can be measurements with a unit (Fuel in litres, weight
        // in kg, length in cm…). `field_type = 'measurement'` pairs with the
        // new `field_unit` column.
        conn.execute_batch(
            "ALTER TABLE field_definitions ADD COLUMN field_unit TEXT;",
        )
        .map_err(|e| format!("migration 19 failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 19;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 20 {
        // ANPR page redesign (09-anpr-page-complete-spec.md §9): the
        // entry/exit matcher needs a configurable max-pending window (hours)
        // and the active engine enum now includes the optional cloud provider.
        conn.execute_batch(
            "ALTER TABLE anpr_config ADD COLUMN max_pending_duration_hours REAL;
             ALTER TABLE camera_sources ADD COLUMN camera_role TEXT;
             ALTER TABLE camera_sources ADD COLUMN redundant_of_camera_id TEXT;",
        )
        .map_err(|e| format!("migration 20 failed: {e}"))?;
        conn.execute(
            "UPDATE anpr_config SET max_pending_duration_hours = 24 WHERE max_pending_duration_hours IS NULL",
            [],
        )
        .map_err(|e| format!("migration 20 default failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 20;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    if current < 21 {
        // Entry/exit trip model (09-anpr-page-complete-spec.md §9): one trip
        // record spans a full visit. `entry_time` replaces the old single
        // `time_in`; `exit_time` is set only when the exit sighting is matched;
        // `trip_status` (open / complete / missed_exit) is auto-derived;
        // photos split into entry_photo_refs / exit_photo_refs (never merged).
        // Old columns stay as a compatibility mirror so untouched code paths
        // keep working; all reads now use the new columns.
        // Idempotent: only add columns that don't already exist.
        let existing_cols: Vec<String> = {
            let mut stmt = conn
                .prepare("PRAGMA table_info(trips)")
                .map_err(|e| format!("migration 21 pragma failed: {e}"))?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))
                .map_err(|e| format!("migration 21 query failed: {e}"))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for (col, typ, default) in &[
            ("entry_time", "TEXT", None),
            ("exit_time", "TEXT", None),
            ("trip_status", "TEXT", Some("'complete'")),
            ("entry_photo_refs", "TEXT", None),
            ("exit_photo_refs", "TEXT", None),
            ("sheet_row", "INTEGER", None),
            ("sheet_exit_pushed", "INTEGER", Some("0")),
        ] {
            if !existing_cols.contains(&col.to_string()) {
                let default_clause = default.map(|d| format!(" NOT NULL DEFAULT {d}")).unwrap_or_default();
                conn.execute_batch(&format!("ALTER TABLE trips ADD COLUMN {col} {typ}{default_clause};"))
                    .map_err(|e| format!("migration 21 add {col} failed: {e}"))?;
            }
        }
        // Historical rows were single-sighting events under the old model: they
        // are complete trips (verified activity that must keep counting).
        conn.execute(
            "UPDATE trips SET entry_time = time_in, entry_photo_refs = photo_refs
             WHERE entry_time IS NULL",
            [],
        )
        .map_err(|e| format!("migration 21 backfill failed: {e}"))?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_trips_entry_time ON trips(entry_time);
             CREATE INDEX IF NOT EXISTS idx_trips_trip_status ON trips(trip_status);",
        )
        .map_err(|e| format!("migration 21 indexes failed: {e}"))?;
        conn.execute_batch("PRAGMA user_version = 21;")
            .map_err(|e| format!("version bump failed: {e}"))?;
    }

    Ok(())
}

/// Name of the file that holds the admin recovery code, written next to the
/// database file so the admin can open it and copy the code.
pub const RECOVERY_CODE_FILE: &str = "recovery-code.txt";

pub fn write_recovery_file(dir: &std::path::Path, code: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("recovery dir create failed: {e}"))?;
    let content = format!(
        "TruckFlow — Admin recovery code\n================================\n\nThis file holds the recovery code for admin accounts.\nKeep it private: anyone with this code can reset an admin password.\n\nRecovery code: {code}\n"
    );
    std::fs::write(dir.join(RECOVERY_CODE_FILE), content).map_err(|e| format!("recovery file write failed: {e}"))?;
    Ok(())
}

/// Make sure the recovery code exists whenever an admin account exists and the
/// code (or its file) is missing — regenerate both. Fresh installs generate
/// theirs in create_first_admin; existing installs get one here on next launch.
fn ensure_recovery_code(conn: &Connection, db_path: &std::path::Path) -> Result<(), String> {
    let admin_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM users u WHERE EXISTS (
                SELECT 1 FROM user_permissions up JOIN permissions p ON p.id = up.permission_id
                WHERE up.user_id = u.id AND p.key = 'manage_users'
            )",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("admin count failed: {e}"))?;
    if admin_count == 0 {
        return Ok(()); // first-run will create the code
    }
    let dir = db_path.parent().ok_or("database path has no parent directory")?;
    if dir.join(RECOVERY_CODE_FILE).exists() {
        return Ok(());
    }
    let code = crate::commands::generate_recovery_code();
    crate::commands::save_recovery_code(conn, &code)?;
    write_recovery_file(dir, &code)
}

/// Resolve the SQLite database path for the CLI tools, mirroring where the
/// Tauri app stores its data (`%APPDATA%\com.truckflow.app\truckflow.db` on
/// Windows). Overridable with the TRUCKFLOW_DB environment variable.
pub fn default_db_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("TRUCKFLOW_DB") {
        return std::path::PathBuf::from(p);
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        return std::path::Path::new(&appdata).join("com.truckflow.app").join("truckflow.db");
    }
    if let Ok(home) = std::env::var("HOME") {
        return std::path::Path::new(&home).join(".config").join("com.truckflow.app").join("truckflow.db");
    }
    std::path::PathBuf::from("truckflow.db")
}

/// Fixed primary key for the single-row `anpr_config` settings row.
pub const ANPR_CONFIG_ID: &str = "00000000-0000-0000-0000-000000000001";

/// System-defined permission catalog. Keys are the stable identifiers the whole
/// app gates on; min_auth_level enforces credential strength per 03-auth-permissions.md §3.
pub const PERMISSION_CATALOG: &[(&str, &str, &str, &str)] = &[
    ("perm-view-gate-entries", "view_gate_entries", "pin", "View the gate officer main view and recent entries"),
    ("perm-resolve-queue", "resolve_queue", "pin", "Resolve items in the verification queue"),
    ("perm-view-reporting", "view_reporting_dashboard", "pin", "View the reporting dashboard"),
    ("perm-view-system-health", "view_system_health", "pin", "View system health / monitor section"),
    ("perm-manage-users", "manage_users", "password", "Create, edit, disable and grant permissions to users"),
    ("perm-manage-reference", "manage_reference_database", "password", "Manage companies, vehicles and drivers"),
    ("perm-manage-integrations", "manage_integrations", "password", "Connect and manage external integrations"),
    ("perm-manage-anpr-config", "manage_anpr_config", "password", "Configure the ANPR engine, models, camera sources and thresholds"),
    ("perm-view-audit-log", "view_audit_log", "password", "View the audit log"),
    ("perm-edit-existing-vehicles", "edit_existing_vehicles", "pin", "Edit existing vehicles in the reference database"),
    ("perm-acknowledge-health-alerts", "acknowledge_health_alerts", "pin", "Acknowledge system health alerts"),
    ("perm-view-health-history", "view_health_history", "pin", "View system health incident history"),
];

pub const ROLE_PRESETS: &[(&str, &str, &[&str])] = &[
    ("preset-gate-officer", "Gate Officer", &["view_gate_entries", "resolve_queue", "edit_existing_vehicles"]),
    (
        "preset-admin",
        "Admin",
        &[
            "manage_users",
            "manage_reference_database",
            "view_audit_log",
            "manage_integrations",
            "manage_anpr_config",
            "view_reporting_dashboard",
            "view_system_health",
            "resolve_queue",
            "view_gate_entries",
        ],
    ),
    ("preset-reporting", "Reporting", &["view_reporting_dashboard"]),
    (
        "preset-system-monitor",
        "System Monitor",
        &["view_system_health", "acknowledge_health_alerts", "view_health_history"],
    ),
];

/// Standard fields the three core entities always need. Keyed by binding:
/// the real column the field maps to (plate_number, name, status…).
const STANDARD_FIELD_SKELETON: &[(&str, &str, &str, &str, i32, bool)] = &[
    ("vehicle", "plate_number", "Plate", "text", 0, true),
    ("vehicle", "company", "Company", "text", 1, false),
    ("vehicle", "driver", "Driver", "text", 2, false),
    ("vehicle", "registered_capacity", "Capacity (L)", "number", 3, false),
    ("vehicle", "capacity_unit", "Capacity Unit", "text", 4, false),
    ("vehicle", "status", "Status", "text", 5, false),
    ("company", "name", "Company Name", "text", 0, true),
    ("company", "status", "Status", "text", 1, false),
    ("driver", "name", "Driver Name", "text", 0, true),
    ("driver", "status", "Status", "text", 1, false),
];

/// Re-create any missing standard field definitions (INSERT OR IGNORE by
/// binding). Runs on every launch so a deleted standard field can never
/// silently break imports/forms/exports again.
fn seed_standard_field_skeleton(conn: &Connection, now: &str) -> Result<(), String> {
    for (entity_type, key, label, ftype, sort_order, is_required) in STANDARD_FIELD_SKELETON {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM field_definitions WHERE entity_type = ?1 AND binding = ?2",
                params![entity_type, key],
                |r| r.get(0),
            )
            .map_err(|e| format!("field skeleton lookup failed: {e}"))?;
        if exists > 0 {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO field_definitions
                (id, entity_type, field_key, field_label, field_type, is_required, sort_order,
                 is_standard, is_hidden, binding, field_unit, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, 0, ?3, NULL, ?8, ?8)",
            params![
                uuid::Uuid::new_v4().to_string(),
                entity_type,
                key,
                label,
                ftype,
                *is_required as i32,
                sort_order,
                now,
            ],
        )
        .map_err(|e| format!("field skeleton seed failed: {e}"))?;
    }
    Ok(())
}

fn seed(conn: &Connection) -> Result<(), String> {
    let now = now_iso();
    // The standard field skeleton (plate, name, status…) is required for the
    // gate pipeline, imports and exports. Self-heal on every launch so a
    // deleted standard field can never break the app again.
    seed_standard_field_skeleton(conn, &now)?;
    // Permission catalog is INSERT OR IGNORE (never early-return) so new
    // permission keys reach existing databases too, not only fresh installs.
    for (id, key, min_auth, desc) in PERMISSION_CATALOG {
        conn.execute(
            "INSERT OR IGNORE INTO permissions (id, key, min_auth_level, description) VALUES (?1, ?2, ?3, ?4)",
            params![id, key, min_auth, desc],
        )
        .map_err(|e| format!("seed permission failed: {e}"))?;
    }

    let perm_id_for = |key: &str| -> String {
        PERMISSION_CATALOG
            .iter()
            .find(|(_, k, _, _)| *k == key)
            .map(|(id, _, _, _)| id.to_string())
            .unwrap()
    };

    // Presets are upserted to the current bundle (convenience layer only; the
    // source of truth is always user_permissions — 01-database-schema.md).
    for (id, name, keys) in ROLE_PRESETS {
        let ids: Vec<String> = keys.iter().map(|k| perm_id_for(k)).collect();
        conn.execute(
            "INSERT INTO role_presets (id, name, permission_ids) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, permission_ids = excluded.permission_ids",
            params![id, name, serde_json::to_string(&ids).unwrap()],
        )
        .map_err(|e| format!("seed preset failed: {e}"))?;
    }

    conn.execute(
        "INSERT OR IGNORE INTO app_settings (key, value) VALUES ('pending_auth_upgrades', '{}')",
        [],
    )
    .map_err(|e| format!("seed settings failed: {e}"))?;
    conn.execute(
        "INSERT OR IGNORE INTO app_settings (key, value) VALUES ('consent_mode_default', 'confirm_required')",
        [],
    )
    .map_err(|e| format!("seed settings failed: {e}"))?;

    let _ = now;
    Ok(())
}

pub fn permission_id_for_key(conn: &Connection, key: &str) -> Result<String, String> {
    conn.query_row(
        "SELECT id FROM permissions WHERE key = ?1",
        params![key],
        |r| r.get(0),
    )
    .map_err(|_| format!("unknown permission key: {key}"))
}

pub fn append_audit(conn: &Connection, actor_id: &str, action: &str, target_id: Option<&str>, details: Option<serde_json::Value>) -> Result<(), String> {
    conn.execute(
        "INSERT INTO audit_log (id, actor_id, action, target_id, details, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            uuid::Uuid::new_v4().to_string(),
            actor_id,
            action,
            target_id,
            details.map(|d| d.to_string()),
            now_iso(),
        ],
    )
    .map(|_| ())
    .map_err(|e| format!("audit insert failed: {e}"))
}

/// Read a string-valued `app_settings` row (shared by sync / capture / monitor).
pub fn get_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM app_settings WHERE key = ?1", params![key], |r| r.get(0)).ok()
}

/// Upsert a string-valued `app_settings` row.
pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map(|_| ())
    .map_err(|e| format!("app_settings write failed: {e}"))
}
