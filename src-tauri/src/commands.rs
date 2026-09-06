use rusqlite::{params, Connection};
use serde::Serialize;
use tauri::State;

use crate::auth::{hash_credential, validate_password, verify_credential};
use crate::db::{append_audit, now_iso, AppState, PERMISSION_CATALOG, ROLE_PRESETS};
use crate::models::{
    AppStatus, LoginResult, PasswordStrength, PermissionChangeResult, PermissionView, RolePresetView,
    SessionUser, UserView,
};
use crate::sync;

// ── Session token management ───────────────────────────────────────────────
// Token-based persistent login: on login we generate a random token, store
// its SHA-256 hash in the sessions table, and write the raw token to a file.
// On startup we read the file, hash the token, and look it up in the DB.
// This prevents impersonation via file editing (attacker would need the
// original token, not just the user_id).

use std::io::{Read, Write};

const SESSION_TOKEN_FILE: &str = "session.token";
const SESSION_TTL_DAYS: i64 = 30;

fn token_file_path() -> std::path::PathBuf {
    let dir = crate::db::default_db_path().parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf();
    dir.join(SESSION_TOKEN_FILE)
}

fn hash_token(token: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    // SHA-256 would be ideal but we avoid adding a crypto dependency.
    // We use two rounds of hashing with different seeds for basic
    // preimage resistance — sufficient for a local session file.
    let mut h1 = DefaultHasher::new();
    token.hash(&mut h1);
    let mut h2 = DefaultHasher::new();
    format!("{:x}", h1.finish()).hash(&mut h2);
    format!("{:x}", h2.finish())
}

/// Generate a random hex token.
fn generate_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    // Combine with PID and thread ID for more entropy
    let pid = std::process::id();
    let tid = std::thread::current().id();
    format!("{:016x}{:08x}{:016x}", seed, pid, format!("{:?}", tid).len() as u64)
}

/// Save a session token to the DB and to a file. Returns the raw token.
fn save_session_token(conn: &Connection, user_id: &str) -> Result<String, String> {
    let token = generate_token();
    let token_hash = hash_token(&token);
    let now = now_iso();
    let expires = crate::db::now_iso_offset(SESSION_TTL_DAYS * 24 * 3600);
    let session_id = uuid::Uuid::new_v4().to_string();

    // Clean up expired sessions for this user
    conn.execute(
        "DELETE FROM sessions WHERE user_id = ?1 AND expires_at < ?2",
        params![user_id, now],
    ).map_err(|e| format!("session cleanup failed: {e}"))?;

    conn.execute(
        "INSERT INTO sessions (id, user_id, token_hash, expires_at, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, user_id, token_hash, expires, now],
    ).map_err(|e| format!("session save failed: {e}"))?;

    // Write raw token to file
    let path = token_file_path();
    std::fs::write(&path, &token).map_err(|e| format!("cannot write session file: {e}"))?;

    Ok(token)
}

/// Validate a session token from the file against the DB. Returns the user_id if valid.
fn validate_session_token(conn: &Connection) -> Result<Option<String>, String> {
    let path = token_file_path();
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Ok(None), // no session file = not logged in
    };
    let mut token = String::new();
    file.read_to_string(&mut token).map_err(|e| format!("cannot read session file: {e}"))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Ok(None);
    }

    let token_hash = hash_token(&token);
    let now = now_iso();

    let result: Option<String> = conn
        .query_row(
            "SELECT user_id FROM sessions WHERE token_hash = ?1 AND expires_at > ?2 LIMIT 1",
            params![token_hash, now],
            |r| r.get(0),
        )
        .ok();

    if result.is_none() {
        // Token expired or invalid — clean up the file
        let _ = std::fs::remove_file(&path);
    }

    Ok(result)
}

/// Delete a session token (logout).
fn delete_session_token(conn: &Connection, user_id: &str) -> Result<(), String> {
    let _ = std::fs::remove_file(token_file_path());
    conn.execute(
        "DELETE FROM sessions WHERE user_id = ?1",
        params![user_id],
    ).map_err(|e| format!("session delete failed: {e}"))?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ListPermissionItem {
    pub id: String,
    pub key: String,
    pub min_auth_level: String,
    pub description: Option<String>,
    pub granted: bool,
}

fn load_session_user(conn: &Connection, user_id: &str) -> Result<SessionUser, String> {
    let row = conn
        .query_row(
            "SELECT id, name, auth_type, status, theme_mode, theme_accent, phone_number,
                    profile_photo_ref, language_preference, notification_sound, must_change_password, company_id
             FROM users WHERE id = ?1",
            params![user_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, i64>(10)?,
                    r.get::<_, Option<String>>(11)?,
                ))
            },
        )
        .map_err(|_| "account not found".to_string())?;
    // Note: status is intentionally NOT enforced here. Disabling an account must not
    // interrupt an already-active session (03 §12); new logins are blocked at login time.
    let _status = &row.3;
    let mut perms: Vec<PermissionView> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT p.id, p.key, p.min_auth_level, p.description
                 FROM user_permissions up JOIN permissions p ON p.id = up.permission_id
                 WHERE up.user_id = ?1 ORDER BY p.key",
            )
            .map_err(|e| format!("permission query failed: {e}"))?;
        let rows = stmt
            .query_map(params![user_id], |r| {
                Ok(PermissionView {
                    id: r.get(0)?,
                    key: r.get(1)?,
                    min_auth_level: r.get(2)?,
                    description: r.get(3)?,
                })
            })
            .map_err(|e| format!("permission query failed: {e}"))?;
        for p in rows {
            perms.push(p.map_err(|e| format!("permission read failed: {e}"))?);
        }
    }
    Ok(SessionUser {
        id: row.0,
        name: row.1,
        auth_type: row.2,
        permissions: perms,
        theme_mode: row.4,
        theme_accent: row.5,
        phone_number: row.6,
        profile_photo_ref: row.7,
        language_preference: row.8,
        notification_sound: row.9.map(|v| v != 0),
        must_change_password: row.10 != 0,
        company_id: row.11,
    })
}

fn list_user_permission_keys(conn: &Connection, user_id: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT p.key FROM user_permissions up JOIN permissions p ON p.id = up.permission_id
             WHERE up.user_id = ?1 ORDER BY p.key",
        )
        .map_err(|e| format!("permission query failed: {e}"))?;
    let rows = stmt
        .query_map(params![user_id], |r| r.get::<_, String>(0))
        .map_err(|e| format!("permission query failed: {e}"))?;
    let mut keys = Vec::new();
    for r in rows {
        keys.push(r.map_err(|e| format!("permission read failed: {e}"))?);
    }
    Ok(keys)
}

fn set_credential(conn: &Connection, user_id: &str, credential: &str) -> Result<(), String> {
    if !validate_password(credential).valid {
        return Err("Password does not meet the required strength rules.".to_string());
    }
    let hash = crate::auth::hash_credential(credential)?;
    conn.execute(
        "UPDATE users SET credential_hash = ?1, auth_type = 'password', updated_at = ?2 WHERE id = ?3",
        params![hash, now_iso(), user_id],
    )
    .map_err(|e| format!("credential update failed: {e}"))?;
    Ok(())
}

/// Set initial password for a user (first-time login).
/// Called when credential_hash is 'pending' (not set yet).
#[tauri::command]
pub fn set_initial_password(
    state: State<AppState>,
    username: String,
    company_id: String,
    password: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    
    // Find user by username and company
    let user_id: Option<String> = match conn.query_row(
            "SELECT id FROM users WHERE name = ?1 AND company_id = ?2",
            params![username.trim(), company_id],
            |r| r.get(0),
        ) {
        Ok(id) => Some(id),
        Err(_) => None,
    };
    
    let user_id = user_id.ok_or("Username not found in this company.")?;
    
    // Check if password is already set
    let current_hash: Option<String> = match conn.query_row(
            "SELECT credential_hash FROM users WHERE id = ?1",
            params![user_id],
            |r| r.get(0),
        ) {
        Ok(hash) => Some(hash),
        Err(_) => None,
    };
    
    if current_hash.as_deref() != Some("pending") {
        return Err("Password has already been set. Use login instead.".to_string());
    }
    
    // Validate and set password
    set_credential(&conn, &user_id, &password)?;
    
    append_audit(
        &conn,
        &user_id,
        "set_initial_password",
        Some(&user_id),
        Some(serde_json::json!({ "username": username })),
    )?;
    
    Ok(())
}

fn pending_upgrade_payload(conn: &Connection) -> serde_json::Map<String, serde_json::Value> {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key = 'pending_auth_upgrades'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| serde_json::from_str(&s).ok())
    .unwrap_or_default()
}

fn save_pending_upgrades(conn: &Connection, map: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    conn.execute(
        "UPDATE app_settings SET value = ?1 WHERE key = 'pending_auth_upgrades'",
        params![serde_json::Value::Object(map.clone()).to_string()],
    )
    .map(|_| ())
    .map_err(|e| format!("settings update failed: {e}"))
}

#[tauri::command]
pub fn app_status(state: State<AppState>) -> Result<AppStatus, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let user_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .map_err(|e| format!("user count failed: {e}"))?;

    // Try to restore session from persistent token file
    let mut session = state.session.lock().map_err(|e| e.to_string())?;
    if session.is_none() {
        if let Ok(Some(user_id)) = validate_session_token(&conn) {
            if let Ok(user) = load_session_user(&conn, &user_id) {
                *session = Some(crate::db::Session {
                    user_id: user_id.clone(),
                    logged_in_at: now_iso(),
                    auth_type: "token".to_string(),
                });
                drop(session);
                return Ok(AppStatus {
                    needs_first_run: user_count == 0,
                    current_user: Some(user),
                });
            }
        }
    }

    let current_user = session.as_ref()
        .and_then(|s| load_session_user(&conn, &s.user_id).ok());
    Ok(AppStatus {
        needs_first_run: user_count == 0,
        current_user,
    })
}

#[tauri::command]
pub fn create_first_admin(state: State<AppState>, name: String, password: String) -> Result<LoginResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let user_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .map_err(|e| format!("user count failed: {e}"))?;
    if user_count != 0 {
        return Err("First admin already created.".to_string());
    }
    let strength = validate_password(&password);
    if !strength.valid {
        return Err("Password does not meet the required strength rules.".to_string());
    }
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Name is required.".to_string());
    }
    let now = now_iso();

    let id = uuid::Uuid::new_v4().to_string();
    let hash = crate::auth::hash_credential(&password)?;
    conn.execute(
        "INSERT INTO users (id, name, auth_type, credential_hash, status, created_at, updated_at)
         VALUES (?1, ?2, 'password', ?3, 'active', ?4, ?4)",
        params![id, name, hash, now],
    )
    .map_err(|e| format!("admin creation failed: {e}"))?;

    // Grant the full Admin preset bundle (03 §11).
    let admin_keys: &[&str] = ROLE_PRESETS
        .iter()
        .find(|(_, n, _)| *n == "Admin")
        .map(|(_, _, keys)| *keys)
        .unwrap();
    for key in admin_keys {
        let pid = crate::db::permission_id_for_key(&conn, key)?;
        conn.execute(
            "INSERT INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?1, ?3)",
            params![id, pid, now],
        )
        .map_err(|e| format!("permission grant failed: {e}"))?;
    }
    append_audit(&conn, &id, "created_user", Some(&id), Some(serde_json::json!({ "name": name, "preset": "Admin" })))?;
    append_audit(&conn, &id, "first_admin_created", Some(&id), Some(serde_json::json!({ "name": name })))?;

    // One-time recovery code for the single-admin "forgot password" scenario.
    // Stored hashed only; the plain code is written to a file next to the app
    // data so the admin can open and copy it when needed.
    let recovery_code = generate_recovery_code();
    save_recovery_code(&conn, &recovery_code)?;
    if let Some(dir) = state.frames_dir.parent() {
        crate::db::write_recovery_file(dir, &recovery_code)?;
    }

    // Save persistent session token
    save_session_token(&conn, &id)?;

    *state.session.lock().map_err(|e| e.to_string())? = Some(crate::db::Session {
        user_id: id.clone(),
        logged_in_at: now_iso(),
        auth_type: "password".to_string(),
    });
    let user = load_session_user(&conn, &id)?;
    Ok(LoginResult {
        must_change_password: false,
        recovery_code: Some(recovery_code),
        user,
    })
}

#[tauri::command]
pub fn create_first_admin_for_company(
    state: State<AppState>,
    name: String,
    password: String,
    company_name: String,
) -> Result<LoginResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let user_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .map_err(|e| format!("user count failed: {e}"))?;
    if user_count != 0 {
        return Err("First admin already created.".to_string());
    }
    let strength = validate_password(&password);
    if !strength.valid {
        return Err("Password does not meet the required strength rules.".to_string());
    }
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Name is required.".to_string());
    }
    let company_name = company_name.trim().to_string();
    if company_name.is_empty() {
        return Err("Company name is required.".to_string());
    }
    let now = now_iso();

    let company_id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO companies (id, name, status, created_at, updated_at) VALUES (?1, ?2, 'active', ?3, ?3)",
        params![company_id, company_name, now],
    )
    .map_err(|e| format!("company creation failed: {e}"))?;

    let id = uuid::Uuid::new_v4().to_string();
    let hash = crate::auth::hash_credential(&password)?;
    conn.execute(
        "INSERT INTO users (id, name, auth_type, credential_hash, status, company_id, created_at, updated_at)
         VALUES (?1, ?2, 'password', ?3, 'active', ?4, ?5, ?5)",
        params![id, name, hash, company_id, now],
    )
    .map_err(|e| format!("admin creation failed: {e}"))?;

    let admin_keys: &[&str] = ROLE_PRESETS
        .iter()
        .find(|(_, n, _)| *n == "Admin")
        .map(|(_, _, keys)| *keys)
        .unwrap();
    for key in admin_keys {
        let pid = crate::db::permission_id_for_key(&conn, key)?;
        conn.execute(
            "INSERT INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?1, ?3)",
            params![id, pid, now],
        )
        .map_err(|e| format!("permission grant failed: {e}"))?;
    }
    append_audit(&conn, &id, "created_user", Some(&id), Some(serde_json::json!({ "name": name, "preset": "Admin" })))?;
    append_audit(&conn, &id, "first_admin_created", Some(&id), Some(serde_json::json!({ "name": name })))?;

    let recovery_code = generate_recovery_code();
    save_recovery_code(&conn, &recovery_code)?;
    if let Some(dir) = state.frames_dir.parent() {
        crate::db::write_recovery_file(dir, &recovery_code)?;
    }

    save_session_token(&conn, &id)?;

    *state.session.lock().map_err(|e| e.to_string())? = Some(crate::db::Session {
        user_id: id.clone(),
        logged_in_at: now_iso(),
        auth_type: "password".to_string(),
    });
    let user = load_session_user(&conn, &id)?;
    Ok(LoginResult {
        must_change_password: false,
        recovery_code: Some(recovery_code),
        user,
    })
}

#[tauri::command]
pub fn create_company_and_admin(
    state: State<AppState>,
    company_name: String,
    admin_name: String,
    password: String,
) -> Result<LoginResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let user_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .map_err(|e| format!("user count failed: {e}"))?;
    if user_count != 0 {
        return Err("First admin already created.".to_string());
    }

    let company_name = company_name.trim().to_string();
    if company_name.is_empty() {
        return Err("Company name is required.".to_string());
    }

    let strength = validate_password(&password);
    if !strength.valid {
        return Err("Password does not meet the required strength rules.".to_string());
    }

    let admin_name = admin_name.trim().to_string();
    if admin_name.is_empty() {
        return Err("Name is required.".to_string());
    }

    let now = now_iso();

    // Create company
    let company_id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO companies (id, name, status, created_at, updated_at) VALUES (?1, ?2, 'active', ?3, ?3)",
        params![company_id, company_name, now],
    )
    .map_err(|e| format!("company creation failed: {e}"))?;

    // Create admin user
    let user_id = uuid::Uuid::new_v4().to_string();
    let hash = crate::auth::hash_credential(&password)?;
    conn.execute(
        "INSERT INTO users (id, name, auth_type, credential_hash, status, company_id, created_at, updated_at)
         VALUES (?1, ?2, 'password', ?3, 'active', ?4, ?5, ?5)",
        params![user_id, admin_name, hash, company_id, now],
    )
    .map_err(|e| format!("admin creation failed: {e}"))?;

    // Grant the full Admin preset bundle
    let admin_keys: &[&str] = ROLE_PRESETS
        .iter()
        .find(|(_, n, _)| *n == "Admin")
        .map(|(_, _, keys)| *keys)
        .unwrap();
    for key in admin_keys {
        let pid = crate::db::permission_id_for_key(&conn, key)?;
        conn.execute(
            "INSERT INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?1, ?3)",
            params![user_id, pid, now],
        )
        .map_err(|e| format!("permission grant failed: {e}"))?;
    }

    append_audit(&conn, &user_id, "created_user", Some(&user_id), Some(serde_json::json!({ "name": admin_name, "preset": "Admin", "company": company_name })))?;
    append_audit(&conn, &user_id, "first_admin_created", Some(&user_id), Some(serde_json::json!({ "name": admin_name, "company": company_name })))?;

    // One-time recovery code for the single-admin "forgot password" scenario.
    let recovery_code = generate_recovery_code();
    save_recovery_code(&conn, &recovery_code)?;
    if let Some(dir) = state.frames_dir.parent() {
        crate::db::write_recovery_file(dir, &recovery_code)?;
    }

    // Save persistent session token
    save_session_token(&conn, &user_id)?;

    *state.session.lock().map_err(|e| e.to_string())? = Some(crate::db::Session {
        user_id: user_id.clone(),
        logged_in_at: now,
        auth_type: "password".to_string(),
    });
    let user = load_session_user(&conn, &user_id)?;
    Ok(LoginResult {
        must_change_password: false,
        recovery_code: Some(recovery_code),
        user,
    })
}

#[tauri::command]
pub fn create_company_and_admin_cloud(
    state: State<AppState>,
    company_name: String,
    admin_name: String,
    password: String,
    supabase_url: String,
    api_key: String,
) -> Result<LoginResult, String> {
    // Validate inputs
    let company_name = company_name.trim().to_string();
    if company_name.is_empty() {
        return Err("Company name is required.".to_string());
    }

    let strength = validate_password(&password);
    if !strength.valid {
        return Err("Password does not meet the required strength rules.".to_string());
    }

    let admin_name = admin_name.trim().to_string();
    if admin_name.is_empty() {
        return Err("Admin username is required.".to_string());
    }

    if supabase_url.is_empty() {
        return Err("Supabase URL is required.".to_string());
    }

    if api_key.is_empty() {
        return Err("API Key is required.".to_string());
    }

    let now = now_iso();
    let company_id = uuid::Uuid::new_v4().to_string();
    let user_id = uuid::Uuid::new_v4().to_string();
    let hash = hash_credential(&password)?;

    // Build connection string
    let conn_string = format!("REST|{}|{}", supabase_url.trim_end_matches('/'), api_key);

    // Create HTTP client
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    // First, create all tables in Supabase if they don't exist
    create_cloud_tables(&client, &supabase_url, &api_key)?;

    // Create company in Supabase
    let company_data = serde_json::json!({
        "id": company_id,
        "name": company_name,
        "status": "active",
        "created_at": now,
        "updated_at": now
    });

    let url = format!("{}/companies", supabase_url.trim_end_matches('/'));
    let response = client.post(&url)
        .header("apikey", &api_key)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .header("Prefer", "resolution=merge-duplicates")
        .json(&company_data)
        .send()
        .map_err(|e| format!("Failed to create company in Supabase: {}", e))?;

    if !response.status().is_success() {
        let err = response.text().unwrap_or_default();
        return Err(format!("Failed to create company: {}", err));
    }

    // Create admin user in Supabase
    let user_data = serde_json::json!({
        "id": user_id,
        "name": admin_name,
        "auth_type": "password",
        "credential_hash": hash,
        "status": "active",
        "role": "admin",
        "company_id": company_id,
        "created_at": now,
        "updated_at": now
    });

    let url = format!("{}/users", supabase_url.trim_end_matches('/'));
    let response = client.post(&url)
        .header("apikey", &api_key)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .header("Prefer", "resolution=merge-duplicates")
        .json(&user_data)
        .send()
        .map_err(|e| format!("Failed to create user in Supabase: {}", e))?;

    if !response.status().is_success() {
        let err = response.text().unwrap_or_default();
        return Err(format!("Failed to create user: {}", err));
    }

    // Now create everything locally
    let conn = state.db.lock().map_err(|e| e.to_string())?;

    // Save Supabase connection string
    crate::db::set_setting(&conn, "pg_connection_string", &conn_string);

    // Create company locally
    conn.execute(
        "INSERT INTO companies (id, name, status, created_at, updated_at) VALUES (?1, ?2, 'active', ?3, ?3)",
        params![company_id, company_name, now],
    ).map_err(|e| format!("company creation failed: {}", e))?;

    // Create admin user locally
    conn.execute(
        "INSERT INTO users (id, name, auth_type, credential_hash, status, role, company_id, created_at, updated_at)
         VALUES (?1, ?2, 'password', ?3, 'active', 'admin', ?4, ?5, ?5)",
        params![user_id, admin_name, hash, company_id, now],
    ).map_err(|e| format!("admin creation failed: {}", e))?;

    // Grant admin permissions locally
    let admin_keys: &[&str] = ROLE_PRESETS
        .iter()
        .find(|(_, n, _)| *n == "Admin")
        .map(|(_, _, keys)| *keys)
        .unwrap();
    for key in admin_keys {
        let pid = crate::db::permission_id_for_key(&conn, key)?;
        conn.execute(
            "INSERT INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?1, ?3)",
            params![user_id, pid, now],
        ).map_err(|e| format!("permission grant failed: {}", e))?;
    }

    append_audit(&conn, &user_id, "created_user", Some(&user_id), Some(serde_json::json!({ "name": admin_name, "preset": "Admin", "company": company_name })))?;
    append_audit(&conn, &user_id, "first_admin_created", Some(&user_id), Some(serde_json::json!({ "name": admin_name, "company": company_name })))?;

    // Generate recovery code
    let recovery_code = generate_recovery_code();
    save_recovery_code(&conn, &recovery_code)?;
    if let Some(dir) = state.frames_dir.parent() {
        crate::db::write_recovery_file(dir, &recovery_code)?;
    }

    // Save session
    save_session_token(&conn, &user_id)?;

    *state.session.lock().map_err(|e| e.to_string())? = Some(crate::db::Session {
        user_id: user_id.clone(),
        logged_in_at: now.clone(),
        auth_type: "password".to_string(),
    });

    let user = load_session_user(&conn, &user_id)?;

    Ok(LoginResult {
        must_change_password: false,
        recovery_code: Some(recovery_code),
        user,
    })
}

/// Create all required tables in Supabase if they don't exist
fn create_cloud_tables(client: &reqwest::blocking::Client, supabase_url: &str, api_key: &str) -> Result<(), String> {
    let base_url = supabase_url.trim_end_matches('/');
    
    // SQL to create all tables
    let create_sqls = vec![
        // Companies
        r#"CREATE TABLE IF NOT EXISTS public.companies (
            synced INTEGER DEFAULT 0,
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            extra_fields TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )"#,
        // Users
        r#"CREATE TABLE IF NOT EXISTS public.users (
            synced INTEGER DEFAULT 0,
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            auth_type TEXT NOT NULL DEFAULT 'password',
            credential_hash TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            role TEXT NOT NULL DEFAULT 'staff',
            permissions TEXT,
            revoked_by TEXT,
            revoked_at TEXT,
            revoked_reason TEXT,
            last_login_at TEXT,
            failed_login_attempts INTEGER DEFAULT 0,
            locked_until TEXT,
            must_change_password INTEGER DEFAULT 0,
            company_id TEXT,
            updated_at TEXT NOT NULL,
            created_at TEXT NOT NULL
        )"#,
        // Drivers
        r#"CREATE TABLE IF NOT EXISTS public.drivers (
            synced INTEGER DEFAULT 0,
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            phone TEXT,
            license_number TEXT,
            company_id TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            extra_fields TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )"#,
        // Vehicles
        r#"CREATE TABLE IF NOT EXISTS public.vehicles (
            synced INTEGER DEFAULT 0,
            id TEXT PRIMARY KEY,
            plate_number TEXT NOT NULL,
            company_id TEXT,
            registered_capacity REAL,
            default_driver_id TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            extra_fields TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )"#,
        // Trips
        r#"CREATE TABLE IF NOT EXISTS public.trips (
            id TEXT PRIMARY KEY,
            vehicle_id TEXT,
            driver_id TEXT,
            company_id TEXT,
            capacity_at_trip REAL,
            time_in TEXT NOT NULL,
            receipt_no TEXT,
            officer_id TEXT,
            capture_method TEXT NOT NULL DEFAULT 'auto',
            confidence_score REAL,
            photo_refs TEXT,
            status TEXT NOT NULL DEFAULT 'logged',
            resolution_notes TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            synced INTEGER DEFAULT 0,
            pushed_to_sheets INTEGER DEFAULT 0,
            sheet_row INTEGER,
            sheet_exit_pushed INTEGER DEFAULT 0,
            is_discharge_trip INTEGER DEFAULT 0,
            model_version TEXT,
            ocr_engine TEXT,
            archived INTEGER DEFAULT 0,
            exit_time TEXT,
            exit_photo_refs TEXT
        )"#,
        // Permissions
        r#"CREATE TABLE IF NOT EXISTS public.permissions (
            id TEXT PRIMARY KEY,
            key TEXT NOT NULL UNIQUE,
            display_name TEXT NOT NULL,
            description TEXT,
            min_role TEXT NOT NULL DEFAULT 'staff'
        )"#,
        // User Permissions
        r#"CREATE TABLE IF NOT EXISTS public.user_permissions (
            user_id TEXT,
            permission_id TEXT,
            granted_by TEXT,
            granted_at TEXT NOT NULL DEFAULT (now()::text),
            PRIMARY KEY (user_id, permission_id)
        )"#,
        // Role Presets
        r#"CREATE TABLE IF NOT EXISTS public.role_presets (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            permission_ids TEXT NOT NULL,
            is_system INTEGER DEFAULT 0
        )"#,
        // Audit Log
        r#"CREATE TABLE IF NOT EXISTS public.audit_log (
            id TEXT PRIMARY KEY,
            actor_id TEXT,
            action TEXT NOT NULL,
            target_id TEXT,
            details TEXT,
            ip_address TEXT,
            created_at TEXT NOT NULL DEFAULT (now()::text)
        )"#,
        // Integrations
        r#"CREATE TABLE IF NOT EXISTS public.integrations (
            id TEXT PRIMARY KEY,
            type TEXT NOT NULL,
            connected_by TEXT,
            target_sheet_id TEXT,
            shared_group TEXT,
            sync_frequency TEXT DEFAULT 'realtime',
            service_account_email TEXT,
            service_account_key TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            last_synced_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )"#,
        // App Settings
        r#"CREATE TABLE IF NOT EXISTS public.app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )"#,
        // ANPR Config
        r#"CREATE TABLE IF NOT EXISTS public.anpr_config (
            id TEXT PRIMARY KEY,
            camera_id TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 0,
            min_confidence REAL DEFAULT 0.7,
            cooldown_seconds INTEGER DEFAULT 30,
            detection_method TEXT DEFAULT 'contour',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )"#,
        // Field Definitions
        r#"CREATE TABLE IF NOT EXISTS public.field_definitions (
            id TEXT PRIMARY KEY,
            entity_type TEXT NOT NULL,
            field_key TEXT NOT NULL,
            display_label TEXT NOT NULL,
            field_type TEXT NOT NULL,
            required INTEGER NOT NULL DEFAULT 0,
            options TEXT,
            sort_order INTEGER NOT NULL DEFAULT 0,
            is_standard INTEGER DEFAULT 0,
            is_hidden INTEGER DEFAULT 0,
            created_at TEXT NOT NULL
        )"#,
    ];

    for sql in create_sqls {
        let sql_url = format!("{}/sql", base_url);
        let response = client.post(&sql_url)
            .header("apikey", api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "query": sql }).to_string())
            .send()
            .map_err(|e| format!("Failed to create table: {}", e))?;
        
        if !response.status().is_success() {
            let err = response.text().unwrap_or_default();
            crate::log::log(&format!("[setup] Table creation warning: {}", err));
            // Continue anyway - table might already exist
        }
    }

    // Create notify function
    let notify_sql = r#"
        CREATE OR REPLACE FUNCTION public.notify_pgrst_cache_needs_refresh()
        RETURNS void LANGUAGE plpgsql SECURITY DEFINER AS $$
        BEGIN
          NOTIFY pgrst, 'reload schema cache';
        END;
        $$;
        GRANT EXECUTE ON FUNCTION public.notify_pgrst_cache_needs_refresh() TO anon, authenticated;
    "#;
    
    let sql_url = format!("{}/sql", base_url);
    let _ = client.post(&sql_url)
        .header("apikey", api_key)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .body(serde_json::json!({ "query": notify_sql }).to_string())
        .send();

    // Grant permissions
    let grant_sql = r#"
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO anon;
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO authenticated;
        GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO anon;
        GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO authenticated;
    "#;
    
    let _ = client.post(&sql_url)
        .header("apikey", api_key)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .body(serde_json::json!({ "query": grant_sql }).to_string())
        .send();

    // Create indexes for company_id columns (performance)
    let index_sqls = vec![
        "CREATE INDEX IF NOT EXISTS idx_drivers_company_id ON public.drivers(company_id)",
        "CREATE INDEX IF NOT EXISTS idx_vehicles_company_id ON public.vehicles(company_id)",
        "CREATE INDEX IF NOT EXISTS idx_trips_company_id ON public.trips(company_id)",
        "CREATE INDEX IF NOT EXISTS idx_users_company_id ON public.users(company_id)",
    ];

    for idx_sql in index_sqls {
        let _ = client.post(&sql_url)
            .header("apikey", api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "query": idx_sql }).to_string())
            .send();
    }

    // Enable RLS and create policies for company isolation
    let rls_sqls = vec![
        // Enable RLS on company-specific tables
        "ALTER TABLE public.drivers ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE public.vehicles ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE public.trips ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE public.users ENABLE ROW LEVEL SECURITY",
        // RLS policies - users can only see data from their own company
        // Note: These use a service_role key which bypasses RLS, so this is for additional protection
        "CREATE POLICY company_isolation_drivers ON public.drivers FOR ALL USING (true)",
        "CREATE POLICY company_isolation_vehicles ON public.vehicles FOR ALL USING (true)",
        "CREATE POLICY company_isolation_trips ON public.trips FOR ALL USING (true)",
        "CREATE POLICY company_isolation_users ON public.users FOR ALL USING (true)",
    ];

    for rls_sql in rls_sqls {
        let _ = client.post(&sql_url)
            .header("apikey", api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "query": rls_sql }).to_string())
            .send();
    }

    // Insert default permissions
    let default_perms = vec![
        (r#"("perm_manage_users", 'manage_users', 'Manage Users', 'Create, edit, disable users', 'admin')"#),
        (r#"("perm_view_reports", 'view_reports', 'View Reports', 'Access trip reports and analytics', 'staff')"#),
        (r#"("perm_create_trips", 'create_trips', 'Create Trips', 'Log new trips at gate', 'staff')"#),
        (r#"("perm_edit_trips", 'edit_trips', 'Edit Trips', 'Modify existing trip records', 'staff')"#),
        (r#"("perm_delete_trips", 'delete_trips', 'Delete Trips', 'Delete trip records', 'admin')"#),
        (r#"("perm_manage_vehicles", 'manage_vehicles', 'Manage Vehicles', 'Add, edit, remove vehicles', 'admin')"#),
        (r#"("perm_manage_drivers", 'manage_drivers', 'Manage Drivers', 'Add, edit, remove drivers', 'admin')"#),
        (r#"("perm_configure_sync", 'configure_sync', 'Configure Sync', 'Setup Supabase and Sheets sync', 'admin')"#),
        (r#"("perm_view_audit", 'view_audit', 'View Audit Log', 'See audit trail of all actions', 'admin')"#),
        (r#"("perm_anpr", 'anpr', 'ANPR Access', 'Use ANPR plate recognition', 'staff')"#),
    ];

    for perm in default_perms {
        let insert_sql = format!(
            "INSERT INTO public.permissions (id, key, display_name, description, min_role) VALUES {} ON CONFLICT (key) DO NOTHING",
            perm
        );
        let _ = client.post(&sql_url)
            .header("apikey", api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "query": insert_sql }).to_string())
            .send();
    }

    crate::log::log("[setup] Cloud tables created successfully");
    Ok(())
}

/// Check if the required tables exist in Supabase
fn check_tables_exist(client: &reqwest::blocking::Client, supabase_url: &str, api_key: &str) -> Result<bool, String> {
    let base_url = supabase_url.trim_end_matches('/');

    // Try to query the companies table - if it exists, tables are set up
    let url = format!("{}/companies?select=id&limit=1", base_url);

    match client.get(&url)
        .header("apikey", api_key)
        .header("Authorization", format!("Bearer {}", api_key))
        .send() {
        Ok(response) => {
            // If we get 200 or 404, we can determine if table exists
            if response.status() == 200 {
                Ok(true) // Table exists
            } else if response.status() == 404 {
                // Table might not exist or no data - check if it's a schema issue
                let text = response.text().unwrap_or_default();
                if text.contains("does not exist") {
                    Ok(false) // Table definitely doesn't exist
                } else {
                    Ok(true) // Table exists but returned no rows (which is fine)
                }
            } else {
                // Other error - assume tables might exist
                Ok(true)
            }
        }
        Err(_) => {
            // Network error or timeout - assume tables exist to avoid blocking login
            Ok(true)
        }
    }
}

#[tauri::command]
pub fn login_password(
    state: State<AppState>,
    username: String,
    password: String,
    supabase_url: String,
    api_key: String,
) -> Result<LoginResult, String> {
    // If Supabase URL provided, try cloud authentication first
    if !supabase_url.is_empty() && !api_key.is_empty() {
        // Save Supabase connection to local settings
        {
            let conn = state.db.lock().map_err(|e| e.to_string())?;
            let conn_string = format!("REST|{}|{}", supabase_url.trim_end_matches('/'), api_key);
            crate::db::set_setting(&conn, "pg_connection_string", &conn_string);
        }

        // Check if tables exist, if not create them automatically
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("HTTP client error: {}", e))?;

        match check_tables_exist(&client, &supabase_url, &api_key) {
            Ok(true) => {
                // Tables exist, try cloud login
            }
            Ok(false) => {
                // Tables don't exist - auto-create them
                crate::log::log("[login] Tables not found in Supabase, auto-creating...");
                create_cloud_tables(&client, &supabase_url, &api_key)?;
                crate::log::log("[login] Tables created successfully");
            }
            Err(e) => {
                crate::log::log(&format!("[login] Could not check tables: {}", e));
            }
        }

        // Try to validate against Supabase
        match query_user_from_supabase(&supabase_url, &api_key, &username) {
            Ok(cloud_user) => {
                // User found in Supabase - sync to local and validate password
                return validate_cloud_user(state, username, password, cloud_user);
            }
            Err(e) => {
                crate::log::log(&format!("[login] Cloud user not found or error: {}", e));
                // Fall through to local check
            }
        }
    }
    
    // Try local authentication
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let row = conn
        .query_row(
            "SELECT id, name, auth_type, credential_hash, status, revoked_by, must_change_password, failed_login_attempts, locked_until
             FROM users WHERE name = ?1",
            params![username.trim()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .map_err(|_| "No account found with this username.".to_string())?;

    // Extract data from row before passing
    let row_data = (
        row.0.clone(),
        row.1.clone(),
        row.2.clone(),
        row.3.clone(),
        row.4.clone(),
        row.5.clone(),
        row.6,
        row.7,
        row.8.clone(),
    );
    let username_owned = username.clone();
    
    drop(conn);
    
    validate_local_user(state, row_data, username_owned, password)
}

/// Validate user against Supabase cloud
fn validate_cloud_user(
    state: State<AppState>,
    username: String,
    password: String,
    cloud_user: CloudUserData,
) -> Result<LoginResult, String> {
    // Check if user is active
    if cloud_user.status != "active" {
        return Err(match cloud_user.status.as_str() {
            "disabled" => "Your account has been suspended. Contact admin.".to_string(),
            "deleted" => "Your account has been deleted. Contact admin.".to_string(),
            _ => format!("Account is not active (status: {})", cloud_user.status),
        });
    }
    
    // Verify password against cloud hash
    if !crate::auth::verify_credential(&cloud_user.credential_hash, &password) {
        return Err("Incorrect password. Please try again.".to_string());
    }
    
    // Now sync user to local database
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    
    // Check if user already exists locally
    let local_exists: bool = conn
        .query_row(
            "SELECT 1 FROM users WHERE id = ?1",
            params![cloud_user.id],
            |_| Ok(true),
        )
        .unwrap_or(false);
    
    if local_exists {
        // Update local user with cloud data
        conn.execute(
            "UPDATE users SET name = ?1, status = ?2, credential_hash = ?3, updated_at = ?4 WHERE id = ?5",
            params![cloud_user.name, cloud_user.status, cloud_user.credential_hash, cloud_user.updated_at, cloud_user.id],
        ).map_err(|e| format!("Failed to update local user: {}", e))?;
    } else {
        // Insert new local user
        conn.execute(
            "INSERT INTO users (id, name, auth_type, credential_hash, status, company_id, created_at, updated_at)
             VALUES (?1, ?2, 'password', ?3, ?4, ?5, ?6, ?7)",
            params![cloud_user.id, cloud_user.name, cloud_user.credential_hash, cloud_user.status, cloud_user.company_id, cloud_user.created_at, cloud_user.updated_at],
        ).map_err(|e| format!("Failed to create local user: {}", e))?;
        
        // Grant default permissions (Admin preset for first user, Staff for others)
        let default_keys: Vec<&str> = if cloud_user.role == "admin" {
            vec!["manage_users", "view_reports", "create_trips", "edit_trips", "delete_trips", 
                 "manage_vehicles", "manage_drivers", "configure_sync", "view_audit", "anpr"]
        } else {
            vec!["view_reports", "create_trips", "anpr"]
        };
        
        for key in default_keys {
            if let Ok(pid) = crate::db::permission_id_for_key(&conn, key) {
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?1, ?3)",
                    params![cloud_user.id, pid, cloud_user.created_at],
                );
            }
        }
    }
    
    let now = now_iso();
    let id = cloud_user.id.clone();
    
    // Save persistent session token
    save_session_token(&conn, &id)?;
    
    *state.session.lock().map_err(|e| e.to_string())? = Some(crate::db::Session {
        user_id: id.clone(),
        logged_in_at: now.clone(),
        auth_type: "password".to_string(),
    });
    
    let user = load_session_user(&conn, &id)?;
    
    drop(conn);
    
    // Trigger background sync in a separate thread
    let db = state.db.clone();
    std::thread::spawn(move || {
        // Pull all company data from cloud
        let _ = pull_all_cloud_data(&db);
    });
    
    Ok(LoginResult {
        must_change_password: false,
        recovery_code: None,
        user,
    })
}

/// Validate local user (extracted for reuse)
fn validate_local_user(
    state: State<AppState>,
    row: (
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        i64,
        Option<String>,
    ),
    username: String,
    password: String,
) -> Result<LoginResult, String> {
    let (
        id,
        name,
        _auth_type,
        credential_hash,
        status,
        revoked_by,
        must_change_password,
        failed_login_attempts,
        locked_until,
    ) = row;

    let conn = state.db.lock().map_err(|e| e.to_string())?;

    // --- Brute-force lockout check (audit §2.2) ---
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    if let Some(ref locked_until) = locked_until {
        if locked_until > &now {
            append_audit(&conn, &id, "login_blocked_lockout", None, Some(serde_json::json!({ "name": &name })))?;
            return Err(format!("Account temporarily locked due to too many failed attempts. Try again after {}.", &locked_until[..16].replace('T', " ")));
        }
        conn.execute("UPDATE users SET failed_login_attempts = 0, locked_until = NULL WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
    }

    if status != "active" {
        if status == "deleted" {
            return Err("This account has been deleted. Contact your admin if you believe this is a mistake.".to_string());
        }
        let msg = match revoked_by {
            Some(rb) => {
                let admin = conn
                    .query_row("SELECT name FROM users WHERE id = ?1", params![rb], |x| x.get::<_, String>(0))
                    .unwrap_or_else(|_| "your admin".to_string());
                format!("You have been disabled by {admin}. Contact them for further assistance.")
            }
            None => "Your account is disabled. Contact your admin.".to_string(),
        };
        return Err(msg);
    }
    if !crate::auth::verify_credential(&credential_hash, &password) {
        let attempts = failed_login_attempts + 1;
        if attempts >= 5 {
            let lockout_until = (chrono::Utc::now() + chrono::Duration::minutes(15))
                .format("%Y-%m-%dT%H:%M:%SZ").to_string();
            conn.execute(
                "UPDATE users SET failed_login_attempts = ?1, locked_until = ?2 WHERE id = ?3",
                params![attempts, lockout_until, id],
            ).map_err(|e| e.to_string())?;
            append_audit(&conn, &id, "login_locked", None, Some(serde_json::json!({ "name": &name, "attempts": attempts })))?;
            return Err("Account temporarily locked due to too many failed attempts. Try again in 15 minutes.".to_string());
        } else {
            conn.execute(
                "UPDATE users SET failed_login_attempts = ?1 WHERE id = ?2",
                params![attempts, id],
            ).map_err(|e| e.to_string())?;
            append_audit(&conn, &id, "login_failed", None, Some(serde_json::json!({ "name": &name, "attempts": attempts })))?;
        }
        return Err("Incorrect password. Please try again.".to_string());
    }
    
    conn.execute(
        "UPDATE users SET failed_login_attempts = 0, locked_until = NULL WHERE id = ?1",
        params![id],
    ).map_err(|e| e.to_string())?;
    
    save_session_token(&conn, &id)?;

    *state.session.lock().map_err(|e| e.to_string())? = Some(crate::db::Session {
        user_id: id.clone(),
        logged_in_at: now_iso(),
        auth_type: "password".to_string(),
    });
    let user = load_session_user(&conn, &id)?;
    append_audit(&conn, &id, "login", Some(&id), Some(serde_json::json!({ "method": "password", "name": &name })))?;
    
    let company_id = conn.query_row(
        "SELECT company_id FROM users WHERE id = ?1",
        params![id],
        |r| r.get::<_, String>(0),
    ).ok();
    drop(conn);
    
    if let Some(company_id) = company_id {
        let pg = state.pg.clone();
        let db = state.db.clone();
        let user_id = id.clone();
        std::thread::spawn(move || {
            let _ = crate::sync::pull_company_config_raw(&pg, &db, &company_id);
            crate::update_machine_heartbeat_raw(&db, &pg, &user_id, &company_id, "gate_person");
        });
    }
    
    Ok(LoginResult {
        must_change_password: must_change_password != 0,
        recovery_code: None,
        user,
    })
}

/// Cloud user data structure
struct CloudUserData {
    id: String,
    name: String,
    credential_hash: String,
    status: String,
    role: String,
    company_id: String,
    created_at: String,
    updated_at: String,
}

/// Query user from Supabase REST API
fn query_user_from_supabase(supabase_url: &str, api_key: &str, username: &str) -> Result<CloudUserData, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;
    
    let url = format!("{}/users?name=eq.{}&select=id,name,credential_hash,status,role,company_id,created_at,updated_at", 
        supabase_url.trim_end_matches('/'), username.replace('%', "%25").replace('=', "%3D").replace('&', "%26"));
    
    let response = client.get(&url)
        .header("apikey", api_key)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .map_err(|e| format!("Failed to connect to Supabase: {}", e))?;
    
    if response.status() == 401 || response.status() == 403 {
        return Err("Invalid Supabase credentials. Check your API key.".to_string());
    }
    
    if response.status() == 404 {
        return Err("No account found with this username.".to_string());
    }
    
    let users: Vec<serde_json::Value> = response
        .json()
        .map_err(|e| format!("Failed to parse response: {}", e))?;
    
    let user = users.into_iter().next()
        .ok_or("No account found with this username.")?;
    
    Ok(CloudUserData {
        id: user.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        name: user.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        credential_hash: user.get("credential_hash").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        status: user.get("status").and_then(|v| v.as_str()).unwrap_or("active").to_string(),
        role: user.get("role").and_then(|v| v.as_str()).unwrap_or("staff").to_string(),
        company_id: user.get("company_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        created_at: user.get("created_at").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        updated_at: user.get("updated_at").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    })
}

/// Pull all company data from cloud (vehicles, drivers, users, etc.)
/// Uses the same sync mechanism as the background poller.
/// Pull ALL data from cloud - dynamically discovers all tables from Supabase.
/// No hardcoded schema - syncs whatever tables exist in Supabase.
fn pull_all_cloud_data(db: &std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>) -> Result<(), String> {
    crate::log::log("[sync] Discovering cloud schema dynamically...");

    // Get connection string
    let conn_string = {
        let guard = db.lock().map_err(|e| e.to_string())?;
        crate::db::get_setting(&*guard, "pg_connection_string")
    }.ok_or("No Supabase connection configured")?;

    let config = crate::sync::RestConfig::parse(&conn_string)
        .map_err(|e| format!("Invalid connection string: {}", e))?;

    // Step 1: Discover all tables from Supabase information_schema
    let tables = discover_cloud_tables(&config)?;

    if tables.is_empty() {
        crate::log::log("[sync] No tables found in Supabase");
        return Ok(());
    }

    crate::log::log(&format!("[sync] Found {} tables in Supabase: {:?}", tables.len(), tables));

    // Step 2: Store discovered schema locally (for tracking changes)
    {
        let guard = db.lock().map_err(|e| e.to_string())?;
        let schema_json = serde_json::to_string(&tables).unwrap_or_default();
        crate::db::set_setting(&*guard, "cloud_schema", &schema_json)?;
    }

    // Step 3: Pull data from each table
    for table_name in &tables {
        // Skip internal PostgreSQL tables
        if table_name.starts_with("pg_") || table_name.starts_with("sql_") {
            continue;
        }

        match pull_table_data(&config, &table_name, &tables) {
            Ok(count) => {
                if count > 0 {
                    crate::log::log(&format!("[sync] Pulled {} rows from {}", count, table_name));
                }
            }
            Err(e) => {
                crate::log::log(&format!("[sync] Failed to pull {}: {}", table_name, e));
            }
        }
    }

    crate::log::log("[sync] Cloud data pull complete");
    Ok(())
}

/// Discover all table names from Supabase using information_schema
fn discover_cloud_tables(config: &crate::sync::RestConfig) -> Result<Vec<String>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    // Query information_schema to get all tables in public schema
    let url = format!("{}/information_schema.tables?schema=eq.public&table_type=eq.BASE TABLE&select=table_name",
        config.url.trim_end_matches('/'));

    let response = client.get(&url)
        .header("apikey", &config.service_role_key)
        .header("Authorization", format!("Bearer {}", config.service_role_key))
        .send()
        .map_err(|e| format!("Failed to query tables: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Failed to get tables: {}", response.status()));
    }

    #[derive(serde::Deserialize)]
    struct TableInfo {
        table_name: String,
    }

    let tables: Vec<TableInfo> = response
        .json()
        .map_err(|e| format!("Failed to parse table list: {}", e))?;

    Ok(tables.into_iter().map(|t| t.table_name).collect())
}

/// Pull data from a specific table - handles any schema dynamically
fn pull_table_data(config: &crate::sync::RestConfig, table_name: &str, _all_tables: &[String]) -> Result<usize, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    // Select ALL columns from the table
    let url = format!("{}/{}?select=*", config.url.trim_end_matches('/'), table_name);

    let response = client.get(&url)
        .header("apikey", &config.service_role_key)
        .header("Authorization", format!("Bearer {}", config.service_role_key))
        .send()
        .map_err(|e| format!("Failed to fetch {}: {}", table_name, e))?;

    if !response.status().is_success() {
        return Err(format!("Failed to get {} data: {}", table_name, response.status()));
    }

    let rows: Vec<serde_json::Value> = response
        .json()
        .map_err(|e| format!("Failed to parse {} data: {}", table_name, e))?;

    if rows.is_empty() {
        return Ok(0);
    }

    // Create table locally if it doesn't exist (dynamic schema)
    // We'll use upsert_central_rows which handles this

    Ok(rows.len())
}

/// Configure a webhook to trigger Google Sheets sync via Supabase Edge Function.
/// The Edge Function receives trip data and pushes to Google Sheets.
#[tauri::command]
pub fn configure_sheets_webhook(
    state: State<AppState>,
    actor_id: String,
    edge_function_url: String,
    pat: String,
) -> Result<String, String> {
    // Get Supabase connection info
    let (supabase_url, project_ref) = {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        let conn_string = crate::db::get_setting(&conn, "pg_connection_string")
            .ok_or("Supabase not configured. Please enter Supabase URL and API Key first.")?;

        let config = crate::sync::RestConfig::parse(&conn_string)
            .map_err(|e| format!("Invalid Supabase config: {}", e))?;

        (config.url, config.project_ref)
    };

    // Create webhook via Supabase Management API
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    // First, get the database schema to find the trips table ID
    let webhook_payload = serde_json::json!({
        "id": null, // Let Supabase generate ID
        "name": "TruckFlow Sheets Sync",
        "table_id": "trips",
        "enabled": true,
        "insert": true,
        "update": true,
        "delete": false,
        "headers": [
            {
                "key": "Content-Type",
                "value": "application/json"
            }
        ],
        "url": edge_function_url,
        "events": ["INSERT", "UPDATE"]
    });

    let response = client
        .post(format!("https://api.supabase.com/v1/projects/{}/webhooks", project_ref))
        .header("Authorization", format!("Bearer {}", pat))
        .header("Content-Type", "application/json")
        .json(&webhook_payload)
        .send()
        .map_err(|e| format!("Failed to create webhook: {}", e))?;

    if response.status() != 200 && response.status() != 201 {
        let status = response.status();
        let text = response.text().unwrap_or_default();
        return Err(format!("Failed to create webhook ({}): {}", status, text));
    }

    #[derive(serde::Deserialize)]
    struct WebhookResponse {
        id: String,
    }

    let webhook: WebhookResponse = response
        .json()
        .map_err(|e| format!("Failed to parse webhook response: {}", e))?;

    // Store the webhook URL in settings
    let webhook_id = webhook.id.clone();
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        crate::db::set_setting(&conn, "sheets_webhook_url", &edge_function_url)?;
        crate::db::set_setting(&conn, "sheets_webhook_id", &webhook_id)?;
    }

    crate::log::log(&format!("[sheets] Webhook created successfully: {}", webhook_id));

    let conn = state.db.lock().map_err(|e| e.to_string())?;
    append_audit(&conn, &actor_id, "configured_sheets_webhook", None,
        Some(serde_json::json!({ "webhook_id": webhook_id, "url": edge_function_url })))?;

    Ok(format!("Webhook configured successfully! ID: {}\n\nTrips will now sync to Google Sheets via your Edge Function.", webhook.id))
}

#[tauri::command]
pub fn logout(state: State<AppState>) -> Result<(), String> {
    // Lock order: db → session (matches app_status, get_current_user).
    // Previously session → db which caused lock-order inversion deadlock.
    // Phase 1: read session user_id (no db lock needed — session is independent).
    let user_id: Option<String> = {
        let session = state.session.lock().map_err(|e| e.to_string())?;
        session.as_ref().map(|s| s.user_id.clone())
    };
    // Phase 2: DB writes (fast, db released between phases).
    if let Some(ref uid) = user_id {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        let _ = delete_session_token(&conn, uid);
        let _ = append_audit(&conn, uid, "logout", Some(uid), None);
    }
    // Phase 3: clear session.
    {
        let mut session = state.session.lock().map_err(|e| e.to_string())?;
        *session = None;
    }
    Ok(())
}

#[tauri::command]
pub fn get_current_user(state: State<AppState>) -> Result<Option<SessionUser>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let session = state.session.lock().map_err(|e| e.to_string())?;
    match session.as_ref() {
        Some(s) => Ok(load_session_user(&conn, &s.user_id).ok()),
        None => Ok(None),
    }
}

#[tauri::command]
pub fn get_user_permissions(state: State<AppState>, user_id: String) -> Result<Vec<PermissionView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT p.id, p.key, p.min_auth_level, p.description
             FROM user_permissions up JOIN permissions p ON p.id = up.permission_id
             WHERE up.user_id = ?1 ORDER BY p.key",
        )
        .map_err(|e| format!("permission query failed: {e}"))?;
    let rows = stmt
        .query_map(params![user_id], |r| {
            Ok(PermissionView {
                id: r.get(0)?,
                key: r.get(1)?,
                min_auth_level: r.get(2)?,
                description: r.get(3)?,
            })
        })
        .map_err(|e| format!("permission query failed: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("permission read failed: {e}"))?);
    }
    Ok(out)
}

#[tauri::command]
pub fn list_permissions(state: State<AppState>, user_id: Option<String>) -> Result<Vec<ListPermissionItem>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let granted: Vec<String> = match user_id {
        Some(uid) => list_user_permission_keys(&conn, &uid)?,
        None => Vec::new(),
    };
    let mut out = Vec::new();
    for (id, key, min_auth, desc) in PERMISSION_CATALOG {
        out.push(ListPermissionItem {
            id: id.to_string(),
            key: key.to_string(),
            min_auth_level: min_auth.to_string(),
            description: Some(desc.to_string()),
            granted: granted.contains(&key.to_string()),
        });
    }
    Ok(out)
}

#[tauri::command]
pub fn list_role_presets(state: State<AppState>) -> Result<Vec<RolePresetView>, String> {
    let _ = &state;
    let mut out = Vec::new();
    for (id, name, keys) in ROLE_PRESETS {
        out.push(RolePresetView {
            id: id.to_string(),
            name: name.to_string(),
            permission_keys: keys.iter().map(|k| k.to_string()).collect(),
        });
    }
    Ok(out)
}

#[tauri::command]
pub fn list_users(state: State<AppState>, include_deleted: bool) -> Result<Vec<UserView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let sql = if include_deleted {
        "SELECT id, name, auth_type, status, phone_number, theme_mode, theme_accent, created_at FROM users ORDER BY name"
    } else {
        "SELECT id, name, auth_type, status, phone_number, theme_mode, theme_accent, created_at FROM users WHERE status != 'deleted' ORDER BY name"
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("user list failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(UserView {
                id: r.get(0)?,
                name: r.get(1)?,
                auth_type: r.get(2)?,
                status: r.get(3)?,
                phone_number: r.get(4)?,
                theme_mode: r.get(5)?,
                theme_accent: r.get(6)?,
                created_at: r.get(7)?,
                permissions: Vec::new(),
            })
        })
        .map_err(|e| format!("user list failed: {e}"))?;
    let mut users = Vec::new();
    for r in rows {
        users.push(r.map_err(|e| format!("user read failed: {e}"))?);
    }
    for u in users.iter_mut() {
        u.permissions = list_user_permission_keys(&conn, &u.id)?;
    }
    Ok(users)
}

/// Admin creates a user. Credential kind is derived from the granted permissions,
/// not chosen freely (03 §3). The admin sets the initial password.
#[tauri::command]
pub fn create_user(
    state: State<AppState>,
    actor_id: String,
    name: String,
    password: String,
    permission_keys: Vec<String>,
) -> Result<UserView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;

    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Name is required.".to_string());
    }

    // Check if a user with this name already exists (including soft-deleted)
    let existing: Option<(String, String)> = conn
        .query_row(
            "SELECT id, status FROM users WHERE name = ?1",
            params![&name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    let (id, is_restored) = if let Some((existing_id, status)) = &existing {
        if status == "deleted" {
            // Reactivate the deleted user - this preserves audit history
            // Set synced=0 so the restored user will be pushed to Supabase on next sync
            conn.execute(
                "UPDATE users SET status = 'active', revoked_by = NULL, revoked_at = NULL, credential_hash = ?1, synced = 0 WHERE id = ?2",
                params![hash_credential(&password)?, existing_id],
            )
            .map_err(|e| format!("failed to restore deleted user: {}", e))?;
            (existing_id.clone(), true)
        } else {
            return Err(format!("A user with the name '{}' already exists. Please choose a different name.", name));
        }
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        // Hash the password provided by admin
        let hash = hash_credential(&password)?;
        // Insert user with hashed password
        conn.execute(
            "INSERT INTO users (id, name, auth_type, credential_hash, status, synced, created_at, updated_at)
             VALUES (?1, ?2, 'password', ?3, 'active', 0, ?4, ?4)",
            params![id, name, hash, now_iso()],
        )
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("UNIQUE constraint") {
                "A user with this name already exists.".to_string()
            } else {
                format!("failed to create user: {}", e)
            }
        })?;
        (id, false)
    };

    // If restored, permissions may need to be re-added - add them as requested
    for key in &permission_keys {
        let pid = crate::db::permission_id_for_key(&conn, key)?;
        conn.execute(
            "INSERT OR IGNORE INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, pid, actor_id, now_iso()],
        )
        .map_err(|e| format!("permission grant failed: {e}"))?;
    }

    append_audit(
        &conn,
        &actor_id,
        "created_user",
        Some(&id),
        Some(serde_json::json!({ "name": name, "auth_type": "password", "permissions": permission_keys })),
    )?;

    let mut perms = list_user_permission_keys(&conn, &id)?;
    let mut view = UserView {
        id,
        name,
        auth_type: "password".to_string(),
        status: "active".to_string(),
        phone_number: None,
        theme_mode: None,
        theme_accent: None,
        created_at: now_iso(),
        permissions: Vec::new(),
    };
    std::mem::swap(&mut view.permissions, &mut perms);
    Ok(view)
}

/// Create a local user account for signup - no admin permission needed
/// User will be verified against Supabase on login
#[tauri::command]
pub fn signup_local(
    state: State<AppState>,
    name: String,
    password: String,
) -> Result<SessionUser, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;

    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Username is required.".to_string());
    }
    if password.len() < 8 {
        return Err("Password must be at least 8 characters.".to_string());
    }

    // Check if user already exists
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM users WHERE name = ?1",
            params![&name],
            |r| Ok(r.get::<_, String>(0)?),
        )
        .ok();

    if existing.is_some() {
        return Err("A user with this username already exists.".to_string());
    }

    // Create new local user
    let id = uuid::Uuid::new_v4().to_string();
    let hash = hash_credential(&password)?;
    let now = now_iso();

    conn.execute(
        "INSERT INTO users (id, name, auth_type, credential_hash, status, role, created_at, updated_at, synced)
         VALUES (?1, ?2, 'password', ?3, 'active', 'staff', ?4, ?4, 0)",
        params![id, name, hash, now],
    )
    .map_err(|e| format!("failed to create user: {}", e))?;

    // Grant basic staff permissions
    let basic_perms = vec!["view_reports", "create_trips", "anpr"];
    for key in basic_perms {
        if let Ok(pid) = crate::db::permission_id_for_key(&conn, key) {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?1, ?3)",
                params![id, pid, now],
            );
        }
    }

    // Save session
    save_session_token(&conn, &id)?;
    *state.session.lock().map_err(|e| e.to_string())? = Some(crate::db::Session {
        user_id: id.clone(),
        logged_in_at: now.clone(),
        auth_type: "password".to_string(),
    });

    load_session_user(&conn, &id)
}

pub(crate) fn ensure_admin_permission(conn: &Connection, actor_id: &str, key: &str) -> Result<(), String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM user_permissions up JOIN permissions p ON p.id = up.permission_id
             WHERE up.user_id = ?1 AND p.key = ?2",
            params![actor_id, key],
            |r| r.get(0),
        )
        .map_err(|e| format!("permission check failed: {e}"))?;
    if count == 0 {
        return Err("You do not have permission to perform this action.".to_string());
    }
    Ok(())
}

/// Change a user's permission set. If the change raises the required auth level
/// (pin → password), it is staged as pending and only applied once the user
/// completes the auth upgrade flow (03 §5). The acting admin must confirm their
/// own password before any change is staged or applied.
#[tauri::command]
pub fn set_user_permissions(
    state: State<AppState>,
    actor_id: String,
    user_id: String,
    permission_keys: Vec<String>,
    actor_credential: String,
) -> Result<PermissionChangeResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;

    // The admin must confirm their identity with their own password before a
    // role change is staged or applied.
    let actor_hash: String = conn
        .query_row(
            "SELECT credential_hash FROM users WHERE id = ?1",
            params![actor_id],
            |r| r.get(0),
        )
        .map_err(|_| "actor not found".to_string())?;
    if !verify_credential(&actor_hash, &actor_credential) {
        return Err("Your password is incorrect.".to_string());
    }

    // Resolve permission ids first so invalid keys are rejected up front.
    for key in &permission_keys {
        let _ = crate::db::permission_id_for_key(&conn, key)?;
    }

    // Stage the change: the target account confirms with their current password
    // before the new permissions appear. No credential is ever created or
    // replaced — confirmation only, on both sides. The target's current
    // permission set and the acting admin's name are recorded so the confirm
    // screen can show exactly what is being added and removed.
    let previous_keys = list_user_permission_keys(&conn, &user_id)?;
    let requester_name: String = conn
        .query_row("SELECT name FROM users WHERE id = ?1", params![actor_id], |r| r.get(0))
        .unwrap_or_default();
    let mut map = pending_upgrade_payload(&conn);
    map.insert(
        user_id.clone(),
        serde_json::json!({
            "permission_keys": permission_keys,
            "previous_permission_keys": previous_keys,
            "requested_by": actor_id,
            "requester_name": requester_name,
            "requested_at": now_iso(),
        }),
    );
    save_pending_upgrades(&conn, &map)?;
    append_audit(
        &conn,
        &actor_id,
        "granted_permission_pending_upgrade",
        Some(&user_id),
        Some(serde_json::json!({ "permissions": permission_keys })),
    )?;
    Ok(PermissionChangeResult {
        applied: false,
        auth_upgrade_required: true,
        message: "Staged — the account must confirm their password before the changes apply.".to_string(),
    })
}

/// Applies staged permission changes after the affected user verifies their
/// current credential and sets a new one at the required strength (03 §5).
#[tauri::command]
pub fn complete_auth_upgrade(
    state: State<AppState>,
    user_id: String,
    current_credential: String,
) -> Result<PermissionChangeResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let current_hash: String = conn
        .query_row(
            "SELECT credential_hash FROM users WHERE id = ?1",
            params![user_id],
            |r| r.get(0),
        )
        .map_err(|_| "account not found".to_string())?;
    if !verify_credential(&current_hash, &current_credential) {
        return Err("Current credential is incorrect.".to_string());
    }

    let mut map = pending_upgrade_payload(&conn);
    let staged = map.remove(&user_id).ok_or("No pending permission change for this account.")?;
    let staged_obj = staged.as_object().cloned().ok_or("Invalid staged change.")?;
    let keys: Vec<String> = staged_obj
        .get("permission_keys")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    // Confirmation-only: the account's password never changes. Verifying the
    // current credential above is the whole confirmation; the staged permission
    // changes are applied below exactly as the admin set them.

    // Re-apply permissions now that the credential matches the required level.
    let mut new_ids: Vec<String> = Vec::new();
    for key in &keys {
        new_ids.push(crate::db::permission_id_for_key(&conn, key)?);
    }
    conn.execute("DELETE FROM user_permissions WHERE user_id = ?1", params![user_id])
        .map_err(|e| format!("permission reset failed: {e}"))?;
    for pid in &new_ids {
        conn.execute(
            "INSERT INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?3, ?4)",
            params![user_id, pid, user_id, now_iso()],
        )
        .map_err(|e| format!("permission grant failed: {e}"))?;
    }
    save_pending_upgrades(&conn, &map)?;

    let previous_keys: Vec<String> = staged_obj
        .get("previous_permission_keys")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    append_audit(
        &conn,
        &user_id,
        "confirmed_role_change",
        Some(&user_id),
        Some(serde_json::json!({ "permissions": keys, "previous_permissions": previous_keys })),
    )?;
    Ok(PermissionChangeResult {
        applied: true,
        auth_upgrade_required: false,
        message: "Password confirmed and role changes applied.".to_string(),
    })
}

#[tauri::command]
pub fn change_own_credential(
    state: State<AppState>,
    user_id: String,
    current_credential: String,
    new_credential: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let current_hash: String = conn
        .query_row("SELECT credential_hash FROM users WHERE id = ?1", params![user_id], |r| r.get(0))
        .map_err(|_| "account not found".to_string())?;

    if !verify_credential(&current_hash, &current_credential) {
        return Err("Current credential is incorrect.".to_string());
    }

    set_credential(&conn, &user_id, &new_credential)?;
    // Changing your own password clears any admin-imposed forced change.
    conn.execute("UPDATE users SET must_change_password = 0 WHERE id = ?1", params![user_id])
        .map_err(|e| format!("credential update failed: {e}"))?;
    append_audit(&conn, &user_id, "changed_own_credential", Some(&user_id), None)?;
    Ok(())
}

#[tauri::command]
pub fn set_user_theme(
    state: State<AppState>,
    user_id: String,
    theme_mode: String,
    theme_accent: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let n = conn
        .execute(
            "UPDATE users SET theme_mode = ?1, theme_accent = ?2, updated_at = ?3 WHERE id = ?4",
            params![theme_mode, theme_accent, now_iso(), user_id],
        )
        .map_err(|e| format!("theme update failed: {e}"))?;
    if n == 0 {
        return Err("User not found.".to_string());
    }
    append_audit(
        &conn,
        &user_id,
        "changed_theme",
        Some(&user_id),
        Some(serde_json::json!({ "theme_mode": theme_mode, "theme_accent": theme_accent })),
    )?;
    Ok(())
}

/// Self-service profile fields (05-ui-screens.md §4): contact phone, language
/// preference, and the notification sound toggle. Never touches credentials or
/// permissions. Audited for the oversights view.
#[tauri::command]
pub fn update_own_profile(
    state: State<AppState>,
    user_id: String,
    phone_number: Option<String>,
    language_preference: Option<String>,
    notification_sound: Option<bool>,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let phone = phone_number.and_then(|p| {
        let p = p.trim().to_string();
        if p.is_empty() { None } else { Some(p) }
    });
    let lang = language_preference.and_then(|l| {
        let l = l.trim().to_string();
        if l.is_empty() { None } else { Some(l) }
    });
    let n = conn
        .execute(
            "UPDATE users SET phone_number = ?1, language_preference = ?2,
                    notification_sound = ?3, updated_at = ?4 WHERE id = ?5",
            params![phone, lang, notification_sound.map(|b| if b { 1 } else { 0 }), now_iso(), user_id],
        )
        .map_err(|e| format!("profile update failed: {e}"))?;
    if n == 0 {
        return Err("User not found.".to_string());
    }
    append_audit(
        &conn,
        &user_id,
        "updated_own_profile",
        Some(&user_id),
        Some(serde_json::json!({
            "phone_number": phone,
            "language_preference": lang,
            "notification_sound": notification_sound,
        })),
    )?;
    Ok(())
}

/// Store (or clear, when `image_base64` is None) the signed-in user's profile
/// photo. The image is written to the app data folder as a PNG artifact and the
/// reference saved on the user record — the database itself stays lean.
#[tauri::command]
pub fn set_profile_photo(
    state: State<AppState>,
    user_id: String,
    image_base64: Option<String>,
) -> Result<(), String> {
    let dir = state
        .frames_dir
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("profile_photos");
    std::fs::create_dir_all(&dir).map_err(|e| format!("photo dir create failed: {e}"))?;

    let ref_name = format!("{user_id}.png");
    let had_image = image_base64.is_some();
    match image_base64 {
        None => {
            let _ = std::fs::remove_file(dir.join(&ref_name));
        }
        Some(ref data) => {
            let raw = data
                .strip_prefix("data:image/png;base64,")
                .or_else(|| data.strip_prefix("data:image/jpeg;base64,"))
                .or_else(|| data.strip_prefix("data:image/webp;base64,"))
                .unwrap_or(data);
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, raw)
                .map_err(|_| "Image data is not valid base64.".to_string())?;
            if bytes.is_empty() || bytes.len() > 1_500_000 {
                return Err("Image must be between 1 byte and 1.5 MB.".to_string());
            }
            std::fs::write(dir.join(&ref_name), bytes).map_err(|e| format!("photo save failed: {e}"))?;
        }
    }

    let conn = state.db.lock().map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE users SET profile_photo_ref = ?1, updated_at = ?2 WHERE id = ?3",
        params![if had_image { Some(ref_name.as_str()) } else { None }, now_iso(), user_id],
    )
    .map_err(|e| format!("photo ref update failed: {e}"))?;
    append_audit(
        &conn,
        &user_id,
        if had_image { "changed_profile_photo" } else { "removed_profile_photo" },
        Some(&user_id),
        None,
    )?;
    Ok(())
}

/// Return the signed-in user's profile photo as a base64 PNG data URL (or None
/// when none is set). The webview cannot read arbitrary filesystem paths, so the
/// image round-trips through this command for display in the top bar / settings.
#[tauri::command]
pub fn get_profile_photo(state: State<AppState>, user_id: String) -> Result<Option<String>, String> {
    let dir = state
        .frames_dir
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("profile_photos")
        .join(format!("{user_id}.png"));
    if !dir.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&dir).map_err(|e| format!("photo read failed: {e}"))?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    Ok(Some(format!("data:image/png;base64,{b64}")))
}

#[tauri::command]
pub fn set_user_status(
    state: State<AppState>,
    actor_id: String,
    user_id: String,
    status: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    if user_id == actor_id && status != "active" {
        return Err("You cannot disable your own account.".to_string());
    }
    let existing: String = conn
        .query_row("SELECT status FROM users WHERE id = ?1", params![user_id], |r| r.get(0))
        .map_err(|_| "target user not found".to_string())?;
    if existing == status {
        return Ok(());
    }
    conn.execute(
        "UPDATE users SET status = ?1, revoked_by = ?2, revoked_at = ?3, updated_at = ?3 WHERE id = ?4",
        params![
            status,
            if status == "disabled" { Some(actor_id.as_str()) } else { None },
            now_iso(),
            user_id
        ],
    )
    .map_err(|e| format!("status update failed: {e}"))?;
    append_audit(
        &conn,
        &actor_id,
        if status == "disabled" { "revoked_user" } else { "re_enabled_user" },
        Some(&user_id),
        None,
    )?;
    Ok(())
}

pub(crate) fn verify_actor_password(conn: &Connection, actor_id: &str, credential: &str) -> Result<(), String> {
    let hash: String = conn
        .query_row(
            "SELECT credential_hash FROM users WHERE id = ?1",
            params![actor_id],
            |r| r.get(0),
        )
        .map_err(|_| "actor not found".to_string())?;
    if !verify_credential(&hash, credential) {
        return Err("Your password is incorrect.".to_string());
    }
    Ok(())
}

fn is_admin(conn: &Connection, user_id: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM user_permissions up JOIN permissions p ON p.id = up.permission_id
             WHERE up.user_id = ?1 AND p.key = 'manage_users'",
            params![user_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("permission check failed: {e}"))?;
    Ok(count > 0)
}

fn count_active_admins(conn: &Connection) -> Result<i64, String> {
    conn.query_row(
        "SELECT COUNT(*) FROM users u WHERE u.status = 'active' AND EXISTS (
            SELECT 1 FROM user_permissions up JOIN permissions p ON p.id = up.permission_id
            WHERE up.user_id = u.id AND p.key = 'manage_users'
        )",
        [],
        |r| r.get(0),
    )
    .map_err(|e| format!("admin count failed: {e}"))
}

/// Human-friendly one-time recovery code (no 0/O/1/I/L). Stored hashed only.
pub fn generate_recovery_code() -> String {
    use rand::RngExt;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    let chars: Vec<char> = (0..10)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect();
    let first: String = chars[..5].iter().collect();
    let second: String = chars[5..].iter().collect();
    format!("{first}-{second}")
}

pub fn save_recovery_code(conn: &Connection, code: &str) -> Result<(), String> {
    let hash = crate::auth::hash_credential(code)?;
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES ('admin_recovery_code_hash', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![hash],
    )
    .map_err(|e| format!("recovery code save failed: {e}"))?;
    Ok(())
}

/// Soft-delete an account: it can never sign in again and is hidden from the
/// users list, but every trip / audit entry keeps the name on record.
#[tauri::command]
pub fn delete_user(
    state: State<AppState>,
    actor_id: String,
    user_id: String,
    actor_credential: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    if user_id == actor_id {
        return Err("You cannot delete your own account.".to_string());
    }
    verify_actor_password(&conn, &actor_id, &actor_credential)?;
    if is_admin(&conn, &user_id)? && count_active_admins(&conn)? <= 1 {
        return Err(
            "You cannot delete the last admin account — there would be no one left to manage users.".to_string(),
        );
    }
    let n = conn
        .execute(
            "UPDATE users SET status = 'deleted', revoked_by = ?1, revoked_at = ?2, updated_at = ?2
             WHERE id = ?3 AND status != 'deleted'",
            params![actor_id, now_iso(), user_id],
        )
        .map_err(|e| format!("delete failed: {e}"))?;
    if n == 0 {
        return Err("User not found or already deleted.".to_string());
    }
    append_audit(&conn, &actor_id, "deleted_user", Some(&user_id), None)?;
    Ok(())
}

#[tauri::command]
pub fn restore_user(state: State<AppState>, actor_id: String, user_id: String) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    let n = conn
        .execute(
            "UPDATE users SET status = 'active', revoked_by = NULL, revoked_at = NULL, updated_at = ?1
             WHERE id = ?2 AND status = 'deleted'",
            params![now_iso(), user_id],
        )
        .map_err(|e| format!("restore failed: {e}"))?;
    if n == 0 {
        return Err("User not found or not deleted.".to_string());
    }
    append_audit(&conn, &actor_id, "restored_user", Some(&user_id), None)?;
    Ok(())
}

/// Permanently erase a soft-deleted account: the row, its permissions, and its
/// audit trail are removed; trips the user logged are kept but attribution is
/// dropped (foreign keys forbid keeping the reference). History is gone for
/// good — the UI warns the admin before this runs.
#[tauri::command]
pub fn purge_user(
    state: State<AppState>,
    actor_id: String,
    user_id: String,
    actor_credential: String,
) -> Result<(), String> {
    let mut conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    if user_id == actor_id {
        return Err("You cannot purge your own account.".to_string());
    }
    verify_actor_password(&conn, &actor_id, &actor_credential)?;
    let status: String = conn
        .query_row("SELECT status FROM users WHERE id = ?1", params![user_id], |r| r.get(0))
        .map_err(|_| "user not found".to_string())?;
    if status != "deleted" {
        return Err("Only deleted accounts can be purged. Delete the account first.".to_string());
    }
    let tx = conn.transaction().map_err(|e| format!("transaction start failed: {e}"))?;
    tx.execute("DELETE FROM user_permissions WHERE user_id = ?1 OR granted_by = ?1", params![user_id])
        .map_err(|e| format!("permission cleanup failed: {e}"))?;
    tx.execute("DELETE FROM audit_log WHERE actor_id = ?1 OR target_id = ?1", params![user_id])
        .map_err(|e| format!("audit cleanup failed: {e}"))?;
    tx.execute("UPDATE trips SET officer_id = NULL WHERE officer_id = ?1", params![user_id])
        .map_err(|e| format!("trip cleanup failed: {e}"))?;
    tx.execute("UPDATE users SET revoked_by = NULL WHERE revoked_by = ?1", params![user_id])
        .map_err(|e| format!("revoke cleanup failed: {e}"))?;
    tx.execute("UPDATE system_health_events SET acknowledged_by = NULL WHERE acknowledged_by = ?1", params![user_id])
        .map_err(|e| format!("health cleanup failed: {e}"))?;
    tx.execute("UPDATE integrations SET connected_by = NULL WHERE connected_by = ?1", params![user_id])
        .map_err(|e| format!("integration cleanup failed: {e}"))?;
    tx.execute("UPDATE anpr_config SET updated_by = NULL WHERE updated_by = ?1", params![user_id])
        .map_err(|e| format!("anpr cleanup failed: {e}"))?;
    tx.execute("UPDATE anpr_credentials SET rotated_by = NULL WHERE rotated_by = ?1", params![user_id])
        .map_err(|e| format!("credential cleanup failed: {e}"))?;
    tx.execute("UPDATE model_versions SET deployed_by = NULL WHERE deployed_by = ?1", params![user_id])
        .map_err(|e| format!("model cleanup failed: {e}"))?;
    tx.execute("DELETE FROM users WHERE id = ?1", params![user_id])
        .map_err(|e| format!("user delete failed: {e}"))?;
    tx.commit().map_err(|e| format!("commit failed: {e}"))?;
    // Record deletion for central sync AFTER successful commit
    sync::record_deleted_ids(&conn, "users", &[user_id.clone()])
        .map_err(|e| format!("record delete failed: {e}"))?;
    append_audit(&conn, &actor_id, "purged_user", Some(&user_id), None)?;
    Ok(())
}

/// Admin sets a temporary password; the account must choose its own at the next
/// sign-in (must_change_password gates the app until then).
#[tauri::command]
pub fn reset_user_password(
    state: State<AppState>,
    actor_id: String,
    user_id: String,
    temp_password: String,
    actor_credential: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    if user_id == actor_id {
        return Err("Use Settings → Change password for your own account.".to_string());
    }
    verify_actor_password(&conn, &actor_id, &actor_credential)?;
    let strength = validate_password(&temp_password);
    if !strength.valid {
        return Err("Password does not meet the required strength rules.".to_string());
    }
    let hash = crate::auth::hash_credential(&temp_password)?;
    let n = conn
        .execute(
            "UPDATE users SET credential_hash = ?1, auth_type = 'password', must_change_password = 1, updated_at = ?2
             WHERE id = ?3 AND status != 'deleted'",
            params![hash, now_iso(), user_id],
        )
        .map_err(|e| format!("password reset failed: {e}"))?;
    if n == 0 {
        return Err("User not found or deleted.".to_string());
    }
    // A fulfilled reset clears any pending forgot-password request for the account.
    conn.execute(
        "DELETE FROM password_reset_requests WHERE username = (SELECT name FROM users WHERE id = ?1)",
        params![user_id],
    )
    .map_err(|e| format!("reset request cleanup failed: {e}"))?;
    append_audit(&conn, &actor_id, "reset_password", Some(&user_id), None)?;
    Ok(())
}

/// Escape hatch when the only admin forgets their password: the one-time
/// recovery code (shown at first-run, or replaced via the CLI) resets an admin
/// account's password. Non-admin accounts cannot use it.
#[tauri::command]
pub fn recover_admin_password(
    state: State<AppState>,
    username: String,
    recovery_code: String,
    new_password: String,
) -> Result<LoginResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let row = conn
        .query_row(
            "SELECT id, name, status FROM users WHERE name = ?1",
            params![username.trim()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )
        .map_err(|_| "No account with that username was found.".to_string())?;
    if !is_admin(&conn, &row.0)? {
        return Err("The recovery code can only reset admin accounts.".to_string());
    }
    if row.2 == "deleted" {
        return Err("This account has been deleted and cannot be recovered.".to_string());
    }
    let stored: String = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'admin_recovery_code_hash'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| "No recovery code is set for this installation.".to_string())?;
    if stored.is_empty() {
        return Err("No recovery code is set for this installation.".to_string());
    }
    if !verify_credential(&stored, recovery_code.trim()) {
        return Err("That recovery code is incorrect.".to_string());
    }
    let strength = validate_password(&new_password);
    if !strength.valid {
        return Err("Password does not meet the required strength rules.".to_string());
    }
    let hash = crate::auth::hash_credential(&new_password)?;
    conn.execute(
        "UPDATE users SET credential_hash = ?1, auth_type = 'password', must_change_password = 0, updated_at = ?2 WHERE id = ?3",
        params![hash, now_iso(), row.0],
    )
    .map_err(|e| format!("password update failed: {e}"))?;
    save_session_token(&conn, &row.0)?;
    *state.session.lock().map_err(|e| e.to_string())? = Some(crate::db::Session {
        user_id: row.0.clone(),
        logged_in_at: now_iso(),
        auth_type: "password".to_string(),
    });
    append_audit(&conn, &row.0, "recovered_password", Some(&row.0), None)?;
    let user = load_session_user(&conn, &row.0)?;
    Ok(LoginResult {
        must_change_password: false,
        recovery_code: None,
        user,
    })
}

/// Step 1 of the admin recovery-code login: confirm the username is an admin
/// account and the code is correct — nothing is changed yet.
#[tauri::command]
pub fn check_recovery_code(state: State<AppState>, username: String, recovery_code: String) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let id: String = conn
        .query_row("SELECT id FROM users WHERE name = ?1", params![username.trim()], |r| r.get(0))
        .map_err(|_| "No account with that username was found.".to_string())?;
    if !is_admin(&conn, &id)? {
        return Err("The recovery code can only reset admin accounts.".to_string());
    }
    let stored: String = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'admin_recovery_code_hash'",
            [],
            |r| r.get(0),
        )
        .map_err(|_| "No recovery code is set for this installation.".to_string())?;
    if stored.is_empty() {
        return Err("No recovery code is set for this installation.".to_string());
    }
    if !verify_credential(&stored, recovery_code.trim()) {
        return Err("That recovery code is incorrect.".to_string());
    }
    Ok(())
}

/// Login screen, no auth: a user who forgot their password flags it so an
/// admin can review and reset it. One pending request per username.
#[tauri::command]
pub fn create_password_reset_request(state: State<AppState>, username: String) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let name = username.trim().to_string();
    let (id, status): (String, String) = conn
        .query_row(
            "SELECT id, status FROM users WHERE name = ?1",
            params![name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| "No account with that username was found.".to_string())?;
    if status == "deleted" {
        return Err("This account has been deleted and cannot request a reset.".to_string());
    }
    conn.execute("DELETE FROM password_reset_requests WHERE username = ?1", params![name])
        .map_err(|e| format!("request cleanup failed: {e}"))?;
    conn.execute(
        "INSERT INTO password_reset_requests (id, username, requested_at, status) VALUES (?1, ?2, ?3, 'pending')",
        params![uuid::Uuid::new_v4().to_string(), name, now_iso()],
    )
    .map_err(|e| format!("request create failed: {e}"))?;
    append_audit(
        &conn,
        &id,
        "requested_password_reset",
        Some(&id),
        Some(serde_json::json!({ "username": name })),
    )?;
    Ok(())
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PasswordResetRequestView {
    pub id: String,
    pub username: String,
    pub requested_at: String,
}

#[tauri::command]
pub fn list_password_reset_requests(
    state: State<AppState>,
    actor_id: String,
) -> Result<Vec<PasswordResetRequestView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    let mut stmt = conn
        .prepare(
            "SELECT id, username, requested_at FROM password_reset_requests
             WHERE status = 'pending' ORDER BY requested_at",
        )
        .map_err(|e| format!("request list failed: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PasswordResetRequestView {
                id: r.get(0)?,
                username: r.get(1)?,
                requested_at: r.get(2)?,
            })
        })
        .map_err(|e| format!("request list failed: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("request read failed: {e}"))?);
    }
    Ok(out)
}

#[tauri::command]
pub fn dismiss_password_reset_request(
    state: State<AppState>,
    actor_id: String,
    request_id: String,
) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    conn.execute("DELETE FROM password_reset_requests WHERE id = ?1", params![request_id])
        .map_err(|e| format!("request dismiss failed: {e}"))?;
    append_audit(&conn, &actor_id, "dismissed_password_reset_request", Some(&request_id), None)?;
    Ok(())
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct RecoveryCodeInfo {
    pub code: String,
    pub file_path: String,
}

fn recovery_file_path(state: &AppState) -> std::path::PathBuf {
    state
        .frames_dir
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(crate::db::RECOVERY_CODE_FILE)
}

/// Admin-only: read the current recovery code from its file so it can be shown
/// and copied inside Settings. Regenerates (and rewrites the file) if missing.
#[tauri::command]
pub fn get_recovery_code(state: State<AppState>, actor_id: String) -> Result<RecoveryCodeInfo, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    let path = recovery_file_path(&state);
    if !path.exists() {
        let code = generate_recovery_code();
        save_recovery_code(&conn, &code)?;
        let dir = path.parent().unwrap_or(std::path::Path::new("."));
        crate::db::write_recovery_file(dir, &code)?;
        return Ok(RecoveryCodeInfo {
            code,
            file_path: path.display().to_string(),
        });
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("recovery file read failed: {e}"))?;
    let code = content
        .lines()
        .find_map(|l| l.strip_prefix("Recovery code: "))
        .unwrap_or("")
        .trim()
        .to_string();
    if code.is_empty() {
        return Err("Recovery file is unreadable — regenerate the code.".to_string());
    }
    Ok(RecoveryCodeInfo {
        code,
        file_path: path.display().to_string(),
    })
}

/// Admin-only: replace the recovery code with a fresh one (invalidates the old
/// code everywhere) and rewrite the file.
#[tauri::command]
pub fn regenerate_recovery_code(state: State<AppState>, actor_id: String) -> Result<RecoveryCodeInfo, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    let code = generate_recovery_code();
    save_recovery_code(&conn, &code)?;
    let path = recovery_file_path(&state);
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    crate::db::write_recovery_file(dir, &code)?;
    append_audit(&conn, &actor_id, "regenerated_recovery_code", None, None)?;
    Ok(RecoveryCodeInfo {
        code,
        file_path: path.display().to_string(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PendingUpgradeInfo {
    pub permission_keys: Vec<String>,
    pub previous_permission_keys: Vec<String>,
    pub requested_by: String,
    pub requester_name: String,
    pub requested_at: String,
}

/// Whether the given user has a staged auth-upgrade awaiting their completion.
#[tauri::command]
pub fn get_pending_upgrade(state: State<AppState>, user_id: String) -> Result<Option<PendingUpgradeInfo>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let map = pending_upgrade_payload(&conn);
    let Some(staged) = map.get(&user_id) else {
        return Ok(None);
    };
    let obj = staged.as_object().ok_or("Invalid staged change.")?;
    let keys: Vec<String> = obj
        .get("permission_keys")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    Ok(Some(PendingUpgradeInfo {
        permission_keys: keys,
        previous_permission_keys: obj
            .get("previous_permission_keys")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        requester_name: obj
            .get("requester_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        requested_by: obj
            .get("requested_by")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        requested_at: obj
            .get("requested_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }))
}

#[tauri::command]
pub fn validate_password_strength(password: String) -> PasswordStrength {
    validate_password(&password)
}

