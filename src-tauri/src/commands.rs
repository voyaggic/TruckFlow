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



/// Save a session token to the DB and to a file. Returns the raw token.
fn save_session_token(conn: &Connection, user_id: &str) -> Result<String, String> {
    let token = crate::db::generate_session_token();
    let token_hash = crate::db::hash_session_token(&token);
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

    let token_hash = crate::db::hash_session_token(&token);
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
        "UPDATE users SET credential_hash = ?1, auth_type = 'password', updated_at = ?2, synced = 0 WHERE id = ?3",
        params![hash, now_iso(), user_id],
    )
    .map_err(|e| format!("credential update failed: {e}"))?;
    let _ = sync::write_to_sync_log(conn, "users", user_id, "UPDATE", Some(&serde_json::json!({
        "id": user_id, "updated_at": now_iso()
    })));
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
    let mut session = match state.session.lock() {
        Ok(s) => s,
        Err(poisoned) => poisoned.into_inner(),
    };
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

    *match state.session.lock() { Ok(s) => s, Err(p) => p.into_inner() } = Some(crate::db::Session {
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

    *match state.session.lock() { Ok(s) => s, Err(p) => p.into_inner() } = Some(crate::db::Session {
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

    // Create the ORGANIZATION (the account owner). Deliberately NOT inserted
    // into `companies` — that table is exclusively for client companies whose
    // trucks discharge trips; the org must never appear in the admin's
    // Companies tab or be deletable from it.
    let organization_id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO organizations (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
        params![organization_id, company_name, now],
    )
    .map_err(|e| format!("organization creation failed: {e}"))?;
    let _ = crate::sync::write_to_sync_log(&conn, "organizations", &organization_id, "INSERT", Some(&serde_json::json!({
        "id": organization_id, "name": company_name, "created_at": now, "updated_at": now
    })));

    // Create admin user
    let user_id = uuid::Uuid::new_v4().to_string();
    let hash = crate::auth::hash_credential(&password)?;
    conn.execute(
        "INSERT INTO users (id, name, auth_type, credential_hash, status, organization_id, created_at, updated_at)
         VALUES (?1, ?2, 'password', ?3, 'active', ?4, ?5, ?5)",
        params![user_id, admin_name, hash, organization_id, now, now],
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

    append_audit(&conn, &user_id, "created_user", Some(&user_id), Some(serde_json::json!({ "name": admin_name, "preset": "Admin", "organization": company_name })))?;
    append_audit(&conn, &user_id, "first_admin_created", Some(&user_id), Some(serde_json::json!({ "name": admin_name, "organization": company_name })))?;

    // One-time recovery code for the single-admin "forgot password" scenario.
    let recovery_code = generate_recovery_code();
    save_recovery_code(&conn, &recovery_code)?;
    if let Some(dir) = state.frames_dir.parent() {
        crate::db::write_recovery_file(dir, &recovery_code)?;
    }

    // Save persistent session token
    save_session_token(&conn, &user_id)?;

    *match state.session.lock() { Ok(s) => s, Err(p) => p.into_inner() } = Some(crate::db::Session {
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

    // Build connection string — normalize so the saved URL carries /rest/v1
    let normalized_base = crate::sync::normalize_supabase_rest_url(&supabase_url);
    let conn_string = format!("REST|{}|{}", normalized_base, api_key);

    // Create HTTP client
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    // First, create all tables in Supabase if they don't exist
    // No PAT available at signup, so table creation is best-effort
    let _ = create_cloud_tables(&client, &supabase_url, &api_key, "");

    // Push company to Supabase (best-effort — local data is always created)
    let base_url = normalized_base.clone();
    {
        let company_data = serde_json::json!({
            "id": company_id,
            "name": company_name,
            "created_at": now,
            "updated_at": now
        });
        let url = format!("{}/organizations", base_url);
        let resp = client.post(&url)
            .header("apikey", &api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("Prefer", "resolution=merge-duplicates")
            .json(&company_data)
            .send();
        match resp {
            Ok(r) if !r.status().is_success() => {
                let status = r.status();
                let err = r.text().unwrap_or_default();
                crate::log::log(&format!("[setup] Warning: company not pushed to Supabase: {} {}", status, err));
            }
            Err(e) => crate::log::log(&format!("[setup] Warning: company push failed: {}", e)),
            _ => crate::log::log("[setup] Company pushed to Supabase"),
        }
    }

    // Push admin user to Supabase (best-effort)
    {
        let user_data = serde_json::json!({
            "id": user_id,
            "name": admin_name,
            "auth_type": "password",
            "credential_hash": hash,
            "status": "active",
            "organization_id": company_id,
            "created_at": now,
            "updated_at": now
        });
        let url = format!("{}/users", base_url);
        let resp = client.post(&url)
            .header("apikey", &api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("Prefer", "resolution=merge-duplicates")
            .json(&user_data)
            .send();
        match resp {
            Ok(r) if !r.status().is_success() => {
                let status = r.status();
                let err = r.text().unwrap_or_default();
                crate::log::log(&format!("[setup] Warning: user not pushed to Supabase: {} {}", status, err));
            }
            Err(e) => crate::log::log(&format!("[setup] Warning: user push failed: {}", e)),
            _ => crate::log::log("[setup] User pushed to Supabase"),
        }
    }

    // Push permissions to cloud (best-effort)
    for (perm_id, key, min_auth, desc) in PERMISSION_CATALOG {
        let perm_data = serde_json::json!({
            "id": perm_id,
            "key": key,
            "description": desc,
            "min_auth_level": min_auth
        });
        let url = format!("{}/permissions", base_url);
        let resp = client.post(&url)
            .header("apikey", &api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("Prefer", "resolution=merge-duplicates")
            .json(&perm_data)
            .send();
        if let Ok(r) = resp {
            if !r.status().is_success() {
                crate::log::log(&format!("[setup] Warning: permission {} not synced: {}", key, r.status()));
            }
        }
    }

    // Get admin_keys for local permission grant
    let admin_keys: Vec<&str> = ROLE_PRESETS
        .iter()
        .find(|(_, n, _)| *n == "Admin")
        .map(|(_, _, keys)| keys.to_vec())
        .unwrap_or_default();

    // Push role presets to cloud (best-effort)
    for (preset_id, name, keys) in ROLE_PRESETS {
        let preset_data = serde_json::json!({
            "id": preset_id,
            "name": name,
            "permission_ids": keys.join(",")
        });
        let url = format!("{}/role_presets", base_url);
        let resp = client.post(&url)
            .header("apikey", &api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("Prefer", "resolution=merge-duplicates")
            .json(&preset_data)
            .send();
        if let Ok(r) = resp {
            if !r.status().is_success() {
                crate::log::log(&format!("[setup] Warning: role preset {} not synced: {}", name, r.status()));
            }
        }
    }

    // Push admin user_permissions to cloud (best-effort)
    for key in &admin_keys {
        if let Some((perm_id, _, _, _)) = PERMISSION_CATALOG.iter().find(|(id, k, _, _)| *k == *key) {
            let up_data = serde_json::json!({
                "user_id": user_id,
                "permission_id": perm_id,
                "granted_by": user_id,
                "granted_at": now
            });
            let url = format!("{}/user_permissions", base_url);
            let resp = client.post(&url)
                .header("apikey", &api_key)
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .header("Prefer", "resolution=merge-duplicates")
                .json(&up_data)
                .send();
            if let Ok(r) = resp {
                if !r.status().is_success() {
                    crate::log::log(&format!("[setup] Warning: user_permission {} not synced", key));
                }
            }
        }
    }

    // Now create everything locally
    let conn = state.db.lock().map_err(|e| e.to_string())?;

    // Save Supabase connection string
    crate::db::set_setting(&conn, "pg_connection_string", &conn_string);

    // Create the ORGANIZATION locally (synced=0 default → background sync pushes
    // to cloud). NOT inserted into `companies` — that table holds client
    // companies only; the account owner org must never be editable/deletable
    // from the admin Companies tab.
    conn.execute(
        "INSERT INTO organizations (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
        params![company_id, company_name, now],
    ).map_err(|e| format!("organization creation failed: {}", e))?;
    let _ = crate::sync::write_to_sync_log(&conn, "organizations", &company_id, "INSERT", Some(&serde_json::json!({
        "id": company_id, "name": company_name, "created_at": now, "updated_at": now
    })));
    let _ = sync::write_to_sync_log(&conn, "companies", &company_id, "INSERT", Some(&serde_json::json!({
        "id": company_id, "name": company_name, "status": "active", "created_at": now, "updated_at": now
    })));

    // Create admin user locally (synced=0 default → background sync pushes to cloud)
    conn.execute(
        "INSERT INTO users (id, name, auth_type, credential_hash, status, organization_id, created_at, updated_at)
         VALUES (?1, ?2, 'password', ?3, 'active', ?4, ?5, ?5)",
        params![user_id, admin_name, hash, company_id, now, now],
    ).map_err(|e| format!("admin creation failed: {}", e))?;
    let _ = sync::write_to_sync_log(&conn, "users", &user_id, "INSERT", Some(&serde_json::json!({
        "id": user_id, "name": admin_name, "status": "active", "created_at": now, "updated_at": now
    })));

    // Grant admin permissions locally (synced=0 default → background sync pushes to cloud)
    for key in &admin_keys {
        let pid = crate::db::permission_id_for_key(&conn, key)?;
        conn.execute(
            "INSERT INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?1, ?3)",
            params![user_id, pid, now],
        ).map_err(|e| format!("permission grant failed: {}", e))?;
        let _ = sync::write_to_sync_log(&conn, "user_permissions", &format!("{}:{}", user_id, pid), "INSERT", Some(&serde_json::json!({
            "user_id": user_id, "permission_id": pid, "granted_by": user_id
        })));
    }

    append_audit(&conn, &user_id, "created_user", Some(&user_id), Some(serde_json::json!({ "name": admin_name, "preset": "Admin", "organization": company_name })))?;
    append_audit(&conn, &user_id, "first_admin_created", Some(&user_id), Some(serde_json::json!({ "name": admin_name, "organization": company_name })))?;

    // Generate recovery code
    let recovery_code = generate_recovery_code();
    save_recovery_code(&conn, &recovery_code)?;
    if let Some(dir) = state.frames_dir.parent() {
        crate::db::write_recovery_file(dir, &recovery_code)?;
    }

    // Save session
    save_session_token(&conn, &user_id)?;

    *match state.session.lock() { Ok(s) => s, Err(p) => p.into_inner() } = Some(crate::db::Session {
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

/// Create all required tables in Supabase via the Management API.
/// Requires a Personal Access Token (PAT). If no PAT is available, returns Ok(())
/// and logs a warning — the tables can be created later via the "Create Tables" button.
fn create_cloud_tables(client: &reqwest::blocking::Client, supabase_url: &str, _api_key: &str, pat: &str) -> Result<(), String> {
    let base_url = supabase_url.trim_end_matches('/');

    // Extract project_ref from Supabase URL (https://xxx.supabase.co/rest/v1 → xxx)
    let project_ref = if let Some(start) = base_url.find("://") {
        let after_proto = &base_url[start + 3..];
        if let Some(dot_pos) = after_proto.find(".supabase.co") {
            after_proto[..dot_pos].to_string()
        } else {
            crate::log::log("[setup] Could not extract project_ref from URL, skipping table creation");
            return Ok(());
        }
    } else {
        crate::log::log("[setup] Invalid Supabase URL format, skipping table creation");
        return Ok(());
    };

    // Read the authoritative SQL from the setup file (embedded at compile time)
    let sql = include_str!("../../docs/SUPABASE_SETUP.sql");

    // Use the Management API (api.supabase.com) which requires a PAT.
    // The service_role key does NOT work with the Management API.
    // During signup we don't have a PAT, so table creation is best-effort.
    // Tables will be created properly when the user connects via the Sync panel
    // and provides a PAT through create_postgres_tables().
    let mgmt_url = format!("https://api.supabase.com/v1/projects/{}/database/query", project_ref);
    crate::log::log(&format!("[setup] Attempting table creation via Management API: {}", mgmt_url));

    let auth_token = if !pat.is_empty() { pat } else { _api_key };
    crate::log::log(&format!("[setup] Using {} for Management API auth", if !pat.is_empty() { "PAT" } else { "api_key (may fail)" }));

    let response = match client.post(&mgmt_url)
        .header("Authorization", format!("Bearer {}", auth_token))
        .header("Content-Type", "application/json")
        .body(serde_json::json!({ "query": sql }).to_string())
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            crate::log::log(&format!("[setup] Management API request failed: {}. Tables will be created when PAT is provided via Sync panel.", e));
            return Ok(());
        }
    };

    let status = response.status();
    if status.is_success() {
        crate::log::log("[setup] Tables created successfully via Management API");
        // Notify PostgREST to refresh its schema cache
        let notify_url = format!("https://api.supabase.com/v1/projects/{}/database/query", project_ref);
        let _ = client.post(&notify_url)
            .header("Authorization", format!("Bearer {}", auth_token))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "query": "NOTIFY pgrst, 'reload schema cache';" }).to_string())
            .send();
        Ok(())
    } else {
        let err = response.text().unwrap_or_default();
        crate::log::log(&format!("[setup] Management API returned {}: {}. Tables will be created when PAT is provided via Sync panel.", status, err));
        // Non-fatal: tables will be created later via create_postgres_tables()
        Ok(())
    }
}

/// Check if the required tables exist in Supabase using Management API (bypasses PostgREST cache)
fn check_tables_exist(client: &reqwest::blocking::Client, supabase_url: &str, _api_key: &str, pat: &str) -> Result<bool, String> {
    let base_url = supabase_url.trim_end_matches('/');

    // Extract project_ref from Supabase URL
    let project_ref = if let Some(start) = base_url.find("://") {
        let after_proto = &base_url[start + 3..];
        if let Some(dot_pos) = after_proto.find(".supabase.co") {
            after_proto[..dot_pos].to_string()
        } else {
            return Err("Invalid Supabase URL format".to_string());
        }
    } else {
        return Err("Invalid Supabase URL format".to_string());
    };

    // Use Management API to check if users table exists in information_schema
    // This bypasses PostgREST's schema cache completely
    let mgmt_url = format!("https://api.supabase.com/v1/projects/{}/database/query", project_ref);
    let auth_token = if !pat.is_empty() { pat } else { _api_key };

    let check_sql = "SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'users' LIMIT 1";

    let response = client.post(&mgmt_url)
        .header("Authorization", format!("Bearer {}", auth_token))
        .header("Content-Type", "application/json")
        .body(serde_json::json!({ "query": check_sql }).to_string())
        .send()
        .map_err(|e| format!("Failed to check tables: {}", e))?;

    if response.status().is_success() {
        let body = response.text().unwrap_or_default();
        // If we get rows back, users table exists
        if body.contains("\"users\"") || body.starts_with('[') && body != "[]" {
            crate::log::log("[check_tables] Users table exists in PostgreSQL");
            Ok(true)
        } else {
            crate::log::log("[check_tables] Users table does NOT exist in PostgreSQL");
            Ok(false)
        }
    } else {
        // Management API failed - be conservative and assume tables exist to avoid blocking login
        crate::log::log(&format!("[check_tables] Management API error: {}", response.status()));
        Ok(true)
    }
}

#[tauri::command]
pub fn login_password(
    state: State<AppState>,
    username: String,
    password: String,
    supabase_url: String,
    api_key: String,
    pat: String,
) -> Result<LoginResult, String> {
    // Store PAT in settings if provided (for Management API access during login)
    if !pat.is_empty() {
        if let Ok(conn) = state.db.lock() {
            let _ = crate::db::set_setting(&conn, "supabase_pat", &pat);
        }
    }
    
    // Check if this is a first-time login (no local data at all)
    // First-time = users table is completely empty
    let is_first_time_login = {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap_or(0);
        count == 0
    };
    
    // If Supabase URL provided, try cloud authentication first
    if !supabase_url.is_empty() && !api_key.is_empty() {
        // Save Supabase connection to local settings.
        // normalize_supabase_rest_url guarantees the saved URL carries /rest/v1 —
        // a bare project URL here was the root cause of the sync 404s.
        {
            let conn = state.db.lock().map_err(|e| e.to_string())?;
            let conn_string = format!(
                "REST|{}|{}",
                crate::sync::normalize_supabase_rest_url(&supabase_url),
                api_key
            );
            crate::db::set_setting(&conn, "pg_connection_string", &conn_string);
        }

        // Check if tables exist, if not create them automatically
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("HTTP client error: {}", e))?;

        match check_tables_exist(&client, &supabase_url, &api_key, &pat) {
            Ok(true) => {
                // Tables exist, try cloud login
            }
            Ok(false) => {
                // Tables don't exist - auto-create them
                crate::log::log("[login] Tables not found in Supabase, auto-creating...");
                create_cloud_tables(&client, &supabase_url, &api_key, &pat)?;
                crate::log::log("[login] Tables created successfully");
            }
            Err(e) => {
                crate::log::log(&format!("[login] Could not check tables: {}", e));
            }
        }

        // First-time login: MUST use cloud, no local fallback allowed
        let normalized_url = crate::sync::normalize_supabase_rest_url(&supabase_url);
        if is_first_time_login {
            match query_user_from_supabase(&normalized_url, &api_key, &username) {
                Ok(cloud_user) => {
                    return validate_cloud_user(state, username, password, cloud_user, &supabase_url, &api_key, &pat);
                }
                Err(e) => {
                    crate::log::log(&format!("[login] First-time login - cloud user not found: {}", e));
                    // Distinguish connectivity failures from a genuinely missing
                    // account: with no local data there is no offline fallback,
                    // so the user must know WHY the login failed.
                    if e.starts_with("Failed to connect") {
                        return Err(format!(
                            "No local account found and the cloud is unreachable. Check your Internet connection and try again. ({})",
                            e
                        ));
                    }
                    return Err("No account found with this username. Please check your Supabase credentials.".to_string());
                }
            }
        }

        // Returning user: try cloud first, fall back to local only if offline
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        let local_user_exists: bool = conn
            .query_row(
                "SELECT 1 FROM users WHERE name = ?1",
                params![username.trim()],
                |_| Ok(true),
            )
            .unwrap_or(false);
        drop(conn);

        if local_user_exists {
            // User exists locally - try cloud auth first
            match query_user_from_supabase(&normalized_url, &api_key, &username) {
                Ok(cloud_user) => {
                    // Cloud auth successful - validate with cloud data
                    return validate_cloud_user(state, username, password, cloud_user, &supabase_url, &api_key, &pat);
                }
                Err(e) => {
                    // Offline fallback ONLY for connectivity failures. While the
                    // Internet is up, the cloud is authoritative: a deleted/disabled
                    // account or a changed password must NOT authenticate from the
                    // stale local copy. 401/403 (bad key) and 404 (no such user)
                    // surface their own specific errors instead.
                    if !e.starts_with("Failed to connect") {
                        crate::log::log(&format!("[login] Cloud auth rejected (not a connectivity failure): {}", e));
                        return Err(e);
                    }
                    crate::log::log(&format!("[login] Cloud unreachable, falling back to local: {}", e));
                    // Cloud unreachable but user has local data - allow offline login
                }
            }
        } else {
            // User doesn't exist locally but is a returning install (other users exist)
            // Must be in cloud to login
            match query_user_from_supabase(&normalized_url, &api_key, &username) {
                Ok(cloud_user) => {
                    return validate_cloud_user(state, username, password, cloud_user, &supabase_url, &api_key, &pat);
                }
                Err(e) => {
                    crate::log::log(&format!("[login] Cloud user not found: {}", e));
                    if e.starts_with("Failed to connect") {
                        return Err(format!(
                            "This account is not on this PC and the cloud is unreachable. Check your Internet connection and try again. ({})",
                            e
                        ));
                    }
                    return Err("No account found with this username.".to_string());
                }
            }
        }
    } else {
        // No Supabase credentials - local-only auth
        // Only allowed for returning users with local data
        if is_first_time_login {
            return Err("No Supabase credentials provided. This appears to be a first-time login. Please enter your Supabase URL and API Key.".to_string());
        }
        
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

        return validate_local_user(state, row_data, username_owned, password);
    }

    // Returning user with local data - try local authentication (offline mode)
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
    supabase_url: &str,
    api_key: &str,
    pat: &str,
) -> Result<LoginResult, String> {
    // Check if user is active
    if cloud_user.status != "active" {
        return Err(match cloud_user.status.as_str() {
            "disabled" => "Your account has been suspended. Contact admin.".to_string(),
            "deleted" => "Your account has been deleted. Contact admin.".to_string(),
            _ => format!("Account is not active (status: {})", cloud_user.status),
        });
    }

    // Fetch actual permissions from cloud for this user (needs db conn for pg_connection_string).
    // The lock is held for the whole validation so the brute-force counters
    // below are consistent.
    let conn = state.db.lock().map_err(|e| e.to_string())?;

    // Brute-force lockout, mirroring the offline path: because auth goes
    // through the service_role key (bypassing Supabase Auth's built-in rate
    // limiting), THIS code is the only rate limiter for cloud logins.
    {
        let lock_row: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT failed_login_attempts, locked_until FROM users WHERE id = ?1",
                params![cloud_user.id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .ok();
        if let Some((attempts, locked_until)) = lock_row {
            let now_ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            if let Some(ref lu) = locked_until {
                if lu.as_str() > now_ts.as_str() {
                    append_audit(&conn, &cloud_user.id, "login_blocked_lockout", None,
                        Some(serde_json::json!({ "name": &cloud_user.name })))?;
                    return Err(format!(
                        "Account temporarily locked due to too many failed attempts. Try again after {}.",
                        lu[..16].replace('T', " ")));
                }
            }
            // Verify password against cloud hash
            if !crate::auth::verify_credential(&cloud_user.credential_hash, &password) {
                let new_attempts = attempts + 1;
                if new_attempts >= 5 {
                    let lockout_until = (chrono::Utc::now() + chrono::Duration::minutes(15))
                        .format("%Y-%m-%dT%H:%M:%SZ").to_string();
                    conn.execute(
                        "UPDATE users SET failed_login_attempts = ?1, locked_until = ?2 WHERE id = ?3",
                        params![new_attempts, lockout_until, cloud_user.id],
                    ).map_err(|e| format!("lockout update failed: {e}"))?;
                    append_audit(&conn, &cloud_user.id, "login_locked", None,
                        Some(serde_json::json!({ "name": &cloud_user.name, "attempts": new_attempts })))?;
                    return Err("Account temporarily locked due to too many failed attempts. Try again in 15 minutes.".to_string());
                }
                conn.execute(
                    "UPDATE users SET failed_login_attempts = ?1 WHERE id = ?2",
                    params![new_attempts, cloud_user.id],
                ).map_err(|e| format!("attempt update failed: {e}"))?;
                append_audit(&conn, &cloud_user.id, "login_failed", None,
                    Some(serde_json::json!({ "name": &cloud_user.name, "attempts": new_attempts })))?;
                return Err("Incorrect password. Please try again.".to_string());
            }
        } else if !crate::auth::verify_credential(&cloud_user.credential_hash, &password) {
            // Fresh machine: no local row to count against yet.
            return Err("Incorrect password. Please try again.".to_string());
        }
    }

    let cloud_permission_keys = fetch_cloud_permissions_for_user(&conn, supabase_url, api_key, &cloud_user.id, pat)?;
    crate::log::log(&format!("[validate_cloud_user] Cloud permissions for {}: {:?}", cloud_user.id, cloud_permission_keys));

    // Empty cloud permission list must not silently strip a working account:
    // PostgREST schema-cache misses and missing PATs both yield []. In that
    // case keep the local set (it was pushed by the admin's PC and pulled here)
    // instead of replacing it with nothing.
    let permissions_authoritative = !cloud_permission_keys.is_empty();
    let effective_permission_keys = if permissions_authoritative {
        cloud_permission_keys
    } else {
        let local_keys = list_user_permission_keys(&conn, &cloud_user.id).unwrap_or_default();
        crate::log::log(&format!(
            "[validate_cloud_user] Cloud returned no permissions for {} — keeping local set ({})",
            cloud_user.id,
            local_keys.len()
        ));
        local_keys
    };

    // Now sync user to local database
    // conn is already locked from above

    // Successful cloud login — clear any brute-force lockout state.
    let _ = conn.execute(
        "UPDATE users SET failed_login_attempts = 0, locked_until = NULL WHERE id = ?1",
        params![cloud_user.id],
    );

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
        let now = crate::db::now_iso();
        conn.execute(
            "UPDATE users SET name = ?1, status = ?2, credential_hash = ?3, updated_at = ?4 WHERE id = ?5",
            params![cloud_user.name, cloud_user.status, cloud_user.credential_hash, now, cloud_user.id],
        ).map_err(|e| format!("Failed to update local user: {}", e))?;
    } else {
        // Insert new local user (only use columns that exist in local schema)
        let now = crate::db::now_iso();
        conn.execute(
            "INSERT INTO users (id, name, auth_type, credential_hash, status, created_at, updated_at)
             VALUES (?1, ?2, 'password', ?3, ?4, ?5, ?6)",
            params![cloud_user.id, cloud_user.name, cloud_user.credential_hash, cloud_user.status, now, now],
        ).map_err(|e| format!("Failed to create local user: {}", e))?;
    }

    // Sync permissions so the CLOUD set is authoritative at login:
    // grants apply, and REVOCATIONS made by the admin on another PC take
    // effect here too (previously only additions propagated — downgrades and
    // role changes never reached other machines).
    // When the cloud list is empty (schema cache miss / no PAT) the local set
    // is kept — see `effective_permission_keys` above.
    {
        let now = crate::db::now_iso();
        let mut desired_pids: Vec<String> = Vec::new();
        for key in &effective_permission_keys {
            if let Ok(pid) = crate::db::permission_id_for_key(&conn, key) {
                conn.execute(
                    "INSERT OR IGNORE INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?1, ?3)",
                    params![cloud_user.id, pid, now],
                ).map_err(|e| format!("Failed to grant permission {}: {}", key, e))?;
                desired_pids.push(pid);
            }
        }
        if permissions_authoritative {
            // Remove local grants the cloud no longer has.
            let existing_pids: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT permission_id FROM user_permissions WHERE user_id = ?1",
                ).map_err(|e| format!("permission read failed: {e}"))?;
                let rows = stmt.query_map(params![cloud_user.id], |r| r.get::<_, String>(0))
                    .map_err(|e| format!("permission read failed: {e}"))?;
                rows.filter_map(|r| r.ok()).collect()
            };
            for pid in existing_pids {
                if !desired_pids.contains(&pid) {
                    conn.execute(
                        "DELETE FROM user_permissions WHERE user_id = ?1 AND permission_id = ?2",
                        params![cloud_user.id, pid],
                    ).map_err(|e| format!("Failed to revoke permission {}: {}", pid, e))?;
                }
            }
        }
    }

    // Sync cloud settings (pg_connection_string, sheets_webhook_url, etc.) to local
    // Only fills in empty values — does not overwrite manually set values
    if let Err(e) = sync_cloud_settings_to_local(&conn, supabase_url, api_key) {
        crate::log::log(&format!("[validate_cloud_user] Failed to sync cloud settings: {}", e));
    }

    let now = crate::db::now_iso();
    let id = cloud_user.id.clone();

    // Save persistent session token
    save_session_token(&conn, &id)?;

    *match state.session.lock() { Ok(s) => s, Err(p) => p.into_inner() } = Some(crate::db::Session {
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

    *match state.session.lock() { Ok(s) => s, Err(p) => p.into_inner() } = Some(crate::db::Session {
        user_id: id.clone(),
        logged_in_at: now_iso(),
        auth_type: "password".to_string(),
    });
    let user = load_session_user(&conn, &id)?;
    append_audit(&conn, &id, "login", Some(&id), Some(serde_json::json!({ "method": "password", "name": &name })))?;
    
    // Company config is keyed by organization for org-owned settings;
    // fall back to the legacy client-company link for older rows.
    let company_id = conn.query_row(
        "SELECT COALESCE(organization_id, company_id) FROM users WHERE id = ?1",
        params![id],
        |r| r.get::<_, String>(0),
    ).ok();
    drop(conn);
    
    if let Some(company_id) = company_id {
        let pg = state.pg.get();
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
}

/// Cloud permission entry from Supabase
#[derive(serde::Deserialize)]
struct CloudPermissionRow {
    key: String,
}

/// Query user from Supabase REST API
fn query_user_from_supabase(supabase_url: &str, api_key: &str, username: &str) -> Result<CloudUserData, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    // Accept raw or /rest/v1-normalized URLs: callers mix both shapes, so
    // normalize here to guarantee exactly one /rest/v1 prefix.
    let base_url = crate::sync::normalize_supabase_rest_url(supabase_url);
    let encoded_username = username.replace('%', "%25").replace('=', "%3D").replace('&', "%26");
    let url = format!("{}/users?name=eq.{}&select=id,name,credential_hash,status",
        base_url, encoded_username);

    crate::log::log(&format!("[query_user_from_supabase] URL: {}", url));

    let response = client.get(&url)
        .header("apikey", api_key)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .map_err(|e| format!("Failed to connect to Supabase: {}", e))?;

    crate::log::log(&format!("[query_user_from_supabase] Status: {}", response.status()));

    if response.status() == 401 || response.status() == 403 {
        return Err("Invalid Supabase credentials. Check your API key.".to_string());
    }

    if response.status() == 404 {
        let body = response.text().unwrap_or_default();
        crate::log::log(&format!("[query_user_from_supabase] 404 body: {}", body));
        // Check if this is a PostgREST schema cache issue (PGRST205)
        if body.contains("PGRST205") || body.contains("Could not find the table") {
            return Err("Cloud tables not yet available. Please connect via the Sync panel first to create tables and sync data.".to_string());
        }
        return Err("No account found with this username.".to_string());
    }

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        crate::log::log(&format!("[query_user_from_supabase] Error body: {}", body));
        return Err(format!("Supabase error ({}): {}", status, body));
    }

    let users: Vec<serde_json::Value> = response
        .json()
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    crate::log::log(&format!("[query_user_from_supabase] Found {} users", users.len()));

    let user = users.into_iter().next()
        .ok_or("No account found with this username.")?;
    
    Ok(CloudUserData {
        id: user.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        name: user.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        credential_hash: user.get("credential_hash").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        status: user.get("status").and_then(|v| v.as_str()).unwrap_or("active").to_string(),
    })
}

/// Notify PostgREST to reload its schema cache via Management API
fn notify_postgrest_reload(client: &reqwest::blocking::Client, project_ref: &str, pat: &str) -> Result<(), String> {
    if pat.is_empty() {
        return Err("PAT required for PostgREST reload".to_string());
    }

    let mgmt_url = format!(
        "https://api.supabase.com/v1/projects/{}/database/query",
        project_ref
    );

    let response = client.post(&mgmt_url)
        .header("Authorization", format!("Bearer {}", pat))
        .header("Content-Type", "application/json")
        .body(serde_json::json!({ "query": "NOTIFY pgrst, 'reload schema cache';" }).to_string())
        .send()
        .map_err(|e| format!("Failed to notify PostgREST: {}", e))?;

    let status = response.status();
    if status.is_success() {
        crate::log::log("[notify_postgrest] PostgREST schema reload triggered");
        Ok(())
    } else {
        let body = response.text().unwrap_or_default();
        Err(format!("PostgREST notify failed ({}): {}", status, body))
    }
}

/// Fetch permission keys for a user from Supabase cloud.
/// Uses the Management API SQL endpoint to bypass PostgREST entirely.
/// PostgREST may not have user_permissions/permissions tables in its schema cache,
/// so we query the database directly via api.supabase.com.
/// Returns a list of permission keys like ["manage_users", "view_reports"].
fn fetch_cloud_permissions_for_user(
    conn: &rusqlite::Connection,
    _supabase_url: &str,
    _api_key: &str,
    user_id: &str,
    pat_param: &str,
) -> Result<Vec<String>, String> {
    // Use PAT from login parameter, falling back to stored setting
    let pat = if !pat_param.is_empty() {
        pat_param.to_string()
    } else {
        crate::db::get_setting(conn, "supabase_pat")
            .unwrap_or_default()
    };
    let conn_string = crate::db::get_setting(conn, "pg_connection_string")
        .ok_or("pg_connection_string not set")?;

    let config = crate::sync::RestConfig::parse(&conn_string)
        .map_err(|e| format!("Invalid connection string: {}", e))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    // Strategy 1: Try PostgREST directly (fast, works when schema cache is fresh)
    let rest_url = format!(
        "{}/user_permissions?user_id=eq.{}&select=permission_id",
        config.url.trim_end_matches('/'), user_id
    );
    crate::log::log(&format!("[fetch_cloud_permissions] Trying PostgREST: {}", rest_url));

    let postgrest_result = client.get(&rest_url)
        .header("apikey", &config.service_role_key)
        .header("Authorization", format!("Bearer {}", config.service_role_key))
        .send();

    let postgrest_ok = match postgrest_result {
        Ok(resp) if resp.status().is_success() => {
            // PostgREST works — parse permission_ids and resolve to keys via local permissions table
            let body = resp.text().unwrap_or_default();
            #[derive(serde::Deserialize)]
            struct UpRow { permission_id: String }
            if let Ok(rows) = serde_json::from_str::<Vec<UpRow>>(&body) {
                let perm_conn = conn;
                let mut keys = Vec::new();
                for row in rows {
                    if let Ok(key) = perm_conn.query_row(
                        "SELECT key FROM permissions WHERE id = ?1",
                        params![row.permission_id],
                        |r| r.get::<_, String>(0),
                    ) {
                        keys.push(key);
                    }
                }
                crate::log::log(&format!("[fetch_cloud_permissions] PostgREST OK, {} keys: {:?}", keys.len(), keys));
                return Ok(keys);
            }
            false
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            crate::log::log(&format!("[fetch_cloud_permissions] PostgREST failed ({}): {}", status, &body[..body.len().min(200)]));
            
            // If 404, try to notify PostgREST to reload schema and retry once
            if status == 404 {
                crate::log::log("[fetch_cloud_permissions] PostgREST 404 - attempting schema reload...");
                if let Ok(()) = notify_postgrest_reload(&client, &config.project_ref, &pat) {
                    crate::log::log("[fetch_cloud_permissions] PostgREST notified, retrying...");
                    // Retry the query once after notify
                    if let Ok(retry_resp) = client.get(&rest_url)
                        .header("apikey", &config.service_role_key)
                        .header("Authorization", format!("Bearer {}", config.service_role_key))
                        .send()
                    {
                        if retry_resp.status().is_success() {
                            let body = retry_resp.text().unwrap_or_default();
                            #[derive(serde::Deserialize)]
                            struct UpRow { permission_id: String }
                            if let Ok(rows) = serde_json::from_str::<Vec<UpRow>>(&body) {
                                let perm_conn = conn;
                                let mut keys = Vec::new();
                                for row in rows {
                                    if let Ok(key) = perm_conn.query_row(
                                        "SELECT key FROM permissions WHERE id = ?1",
                                        params![row.permission_id],
                                        |r| r.get::<_, String>(0),
                                    ) {
                                        keys.push(key);
                                    }
                                }
                                crate::log::log(&format!("[fetch_cloud_permissions] PostgREST retry OK, {} keys: {:?}", keys.len(), keys));
                                return Ok(keys);
                            }
                        }
                    }
                }
            }
            false
        }
        Err(e) => {
            crate::log::log(&format!("[fetch_cloud_permissions] PostgREST error: {}", e));
            false
        }
    };

    // Strategy 2: Try Management API SQL endpoint (requires PAT)
    if pat.is_empty() {
        crate::log::log("[fetch_cloud_permissions] PostgREST failed and no PAT available, returning empty");
        return Ok(Vec::new());
    }

    let mgmt_url = format!(
        "https://api.supabase.com/v1/projects/{}/database/query",
        config.project_ref
    );
    let sql_query = format!(
        "SELECT p.key FROM user_permissions up \
         JOIN permissions p ON p.id = up.permission_id \
         WHERE up.user_id = '{}'",
        user_id
    );
    let sql_payload = serde_json::json!({ "query": &sql_query });

    crate::log::log(&format!(
        "[fetch_cloud_permissions] Trying Management API: {}, user_id: {}",
        mgmt_url, user_id
    ));

    let response = client.post(&mgmt_url)
        .header("Authorization", format!("Bearer {}", pat))
        .header("Content-Type", "application/json")
        .body(sql_payload.to_string())
        .send()
        .map_err(|e| format!("Management API request failed: {}", e))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        crate::log::log(&format!("[fetch_cloud_permissions] Management API failed ({}): {}", status, body));
        return Ok(Vec::new());
    }

    let response_text = response.text().unwrap_or_default();
    crate::log::log(&format!("[fetch_cloud_permissions] Raw response: {}", &response_text[..response_text.len().min(500)]));

    #[derive(serde::Deserialize)]
    struct PermKeyRow { key: String }

    let rows: Vec<PermKeyRow> = serde_json::from_str(&response_text)
        .map_err(|e| format!("Failed to parse permission keys response: {} (body: {})", e, &response_text[..response_text.len().min(200)]))?;

    let keys: Vec<String> = rows.into_iter().map(|r| r.key).collect();
    crate::log::log(&format!("[fetch_cloud_permissions] Found {} permission keys: {:?}", keys.len(), keys));
    Ok(keys)
}

/// Shared row type for cloud app_settings queries.
#[derive(serde::Deserialize)]
struct CloudSettingRow {
    key: String,
    value: String,
}

/// Sync app_settings from cloud to local.
/// For each setting key, only overwrite local value if it's currently empty.
/// This preserves manually entered connection strings while filling in cloud values for new installs.
fn sync_cloud_settings_to_local(conn: &rusqlite::Connection, supabase_url: &str, api_key: &str) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let base_url = supabase_url.trim_end_matches('/');

    // Try PostgREST first (fast, but may fail if table not in schema cache)
    let url = format!("{}/rest/v1/app_settings?select=key,value", base_url);
    let rows: Vec<CloudSettingRow> = match client.get(&url)
        .header("apikey", api_key)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
    {
        Ok(resp) if resp.status().is_success() => {
            resp.json().unwrap_or_default()
        }
        _ => {
            // PostgREST failed (likely table not in schema cache). Fall back to Management API.
            crate::log::log("[sync_cloud_settings] PostgREST failed, trying Management API");
            fetch_settings_via_mgmt_api(conn)?
        }
    };

    for row in rows {
        let local_val = crate::db::get_setting(conn, &row.key);
        if local_val.is_none() || local_val.as_ref().map(|s| s.is_empty()).unwrap_or(false) {
            if !row.value.is_empty() {
                crate::log::log(&format!("[sync_cloud_settings] Setting {} from cloud", row.key));
                crate::db::set_setting(conn, &row.key, &row.value)?;
            }
        }
    }

    Ok(())
}

/// Fetch app_settings via the Management API SQL endpoint (bypasses PostgREST).
fn fetch_settings_via_mgmt_api(
    conn: &rusqlite::Connection,
) -> Result<Vec<CloudSettingRow>, String> {
    let pat = crate::db::get_setting(conn, "supabase_pat")
        .filter(|p| !p.is_empty())
        .ok_or("PAT not available for Management API")?;
    let conn_string = crate::db::get_setting(conn, "pg_connection_string")
        .ok_or("pg_connection_string not set")?;
    let config = crate::sync::RestConfig::parse(&conn_string)
        .map_err(|e| format!("Invalid connection string: {}", e))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let mgmt_url = format!(
        "https://api.supabase.com/v1/projects/{}/database/query",
        config.project_ref
    );
    let payload = serde_json::json!({ "query": "SELECT key, value FROM app_settings" });

    let response = client.post(&mgmt_url)
        .header("Authorization", format!("Bearer {}", pat))
        .header("Content-Type", "application/json")
        .body(payload.to_string())
        .send()
        .map_err(|e| format!("Management API request failed: {}", e))?;

    let resp_status = response.status();
    if !resp_status.is_success() {
        let body = response.text().unwrap_or_default();
        crate::log::log(&format!("[sync_cloud_settings] Management API failed: {} {}", resp_status, body));
        return Ok(Vec::new());
    }

    let text = response.text().unwrap_or_default();
    // Management API may return rows directly as an array or wrapped in {"data": [...]
    let rows: Vec<CloudSettingRow> = serde_json::from_str(&text)
        .or_else(|_| {
            #[derive(serde::Deserialize)]
            struct Wrapper { data: Vec<CloudSettingRow> }
            serde_json::from_str::<Wrapper>(&text).map(|w| w.data)
        })
        .unwrap_or_default();

    crate::log::log(&format!("[sync_cloud_settings] Management API returned {} settings", rows.len()));
    Ok(rows)
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

    // Step 1: Discover all tables from Supabase (PostgREST → Management API → fallback)
    let tables = discover_cloud_tables(&config, db)?;

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

        match pull_table_data(&config, &table_name, &tables, db) {
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
fn discover_cloud_tables(config: &crate::sync::RestConfig, conn: &std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>) -> Result<Vec<String>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    // Strategy 1: Try PostgREST (information_schema is exposed by PostgREST)
    let url = format!("{}/information_schema.tables?schema=eq.public&table_type=eq.BASE TABLE&select=table_name",
        config.url.trim_end_matches('/'));

    let postgrest_result = client.get(&url)
        .header("apikey", &config.service_role_key)
        .header("Authorization", format!("Bearer {}", config.service_role_key))
        .send();

    if let Ok(resp) = postgrest_result {
        if resp.status().is_success() {
            #[derive(serde::Deserialize)]
            struct TableInfo { table_name: String }
            if let Ok(tables) = resp.json::<Vec<TableInfo>>() {
                let names: Vec<String> = tables.into_iter().map(|t| t.table_name).collect();
                if !names.is_empty() {
                    crate::log::log(&format!("[discover_cloud_tables] PostgREST: found {} tables", names.len()));
                    return Ok(names);
                }
            }
        }
    }

    // Strategy 2: Try Management API (requires PAT)
    let pat = conn.lock().ok().and_then(|g| crate::db::get_setting(&g, "supabase_pat"));
    if let Some(pat_val) = pat.filter(|p| !p.is_empty()) {
        let mgmt_url = format!("https://api.supabase.com/v1/projects/{}/database/query", config.project_ref);
        let check_sql = "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY table_name";
        let payload = serde_json::json!({ "query": check_sql });
        if let Ok(resp) = client.post(&mgmt_url)
            .header("Authorization", format!("Bearer {}", pat_val))
            .header("Content-Type", "application/json")
            .body(payload.to_string())
            .send()
        {
            if resp.status().is_success() {
                if let Ok(text) = resp.text() {
                    #[derive(serde::Deserialize)]
                    struct TableNameRow { table_name: String }
                    if let Ok(rows) = serde_json::from_str::<Vec<TableNameRow>>(&text) {
                        let names: Vec<String> = rows.into_iter().map(|r| r.table_name).collect();
                        crate::log::log(&format!("[discover_cloud_tables] Management API: found {} tables", names.len()));
                        return Ok(names);
                    }
                }
            }
        }
    }

    // Strategy 3: Return the known table list as fallback
    // These are the tables created by SUPABASE_SETUP.sql
    crate::log::log("[discover_cloud_tables] Using hardcoded table list as fallback");
    Ok(vec![
        "organizations".to_string(), "users".to_string(), "permissions".to_string(), "user_permissions".to_string(),
        "companies".to_string(), "drivers".to_string(), "vehicles".to_string(),
        "trips".to_string(), "field_definitions".to_string(), "audit_log".to_string(),
        "system_health_events".to_string(), "integrations".to_string(),
        "anpr_config".to_string(), "camera_sources".to_string(),
        "model_versions".to_string(), "training_candidates".to_string(),
        "role_presets".to_string(), "sync_log".to_string(),
        "pc_identity".to_string(), "offline_queue".to_string(),
        "app_settings".to_string(),
    ])
}

/// Pull data from a specific table - handles any schema dynamically
fn pull_table_data(
    config: &crate::sync::RestConfig,
    table_name: &str,
    _all_tables: &[String],
    db: &std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>,
) -> Result<usize, String> {
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

    // Persist rows to local SQLite using upsert_central_rows
    let guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
    let count = crate::sync::upsert_central_rows(&guard, table_name, &rows)?;
    drop(guard);

    crate::log::log(&format!("[pull_table_data] Persisted {} rows to local {}", count, table_name));
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
        let session = match state.session.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
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
        let mut session = match state.session.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        *session = None;
    }
    Ok(())
}

#[tauri::command]
pub fn get_current_user(state: State<AppState>) -> Result<Option<SessionUser>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    // Recover from poisoned mutex — a background thread panic shouldn't log the user out
    let session = match state.session.lock() {
        Ok(s) => s,
        Err(poisoned) => poisoned.into_inner(),
    };
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
            let _ = sync::write_to_sync_log(&conn, "users", existing_id, "UPDATE", Some(&serde_json::json!({
                "id": existing_id, "status": "active", "synced": 0
            })));
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
        let _ = sync::write_to_sync_log(&conn, "users", &id, "INSERT", Some(&serde_json::json!({
            "id": id, "name": name, "status": "active", "created_at": now_iso(), "updated_at": now_iso()
        })));
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
        let _ = sync::write_to_sync_log(&conn, "user_permissions", &format!("{}:{}", id, pid), "INSERT", Some(&serde_json::json!({
            "user_id": id, "permission_id": pid, "granted_by": actor_id
        })));
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
        "INSERT INTO users (id, name, auth_type, credential_hash, status, created_at, updated_at, synced)
         VALUES (?1, ?2, 'password', ?3, 'active', ?4, ?4, 0)",
        params![id, name, hash, now],
    )
    .map_err(|e| format!("failed to create user: {}", e))?;

    let _ = sync::write_to_sync_log(&conn, "users", &id, "INSERT", Some(&serde_json::json!({
        "id": id, "name": name, "status": "active", "created_at": now, "updated_at": now, "synced": 0
    })));

    // Grant basic staff permissions
    let basic_perms = vec!["view_reports", "create_trips", "anpr"];
    for key in basic_perms {
        if let Ok(pid) = crate::db::permission_id_for_key(&conn, key) {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?1, ?3)",
                params![id, pid, now],
            );
            let _ = sync::write_to_sync_log(&conn, "user_permissions", &format!("{}:{}", id, pid), "INSERT", Some(&serde_json::json!({
                "user_id": id, "permission_id": pid, "granted_by": id, "granted_at": now
            })));
        }
    }

    // Save session
    save_session_token(&conn, &id)?;
    *match state.session.lock() { Ok(s) => s, Err(p) => p.into_inner() } = Some(crate::db::Session {
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
    let _ = sync::write_to_sync_log(&conn, "user_permissions", &user_id, "DELETE", Some(&serde_json::json!({
        "user_id": user_id, "reason": "complete_auth_upgrade"
    })));
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
        "UPDATE users SET status = ?1, revoked_by = ?2, revoked_at = ?3, updated_at = ?3, synced = 0 WHERE id = ?4",
        params![
            status,
            if status == "disabled" { Some(actor_id.as_str()) } else { None },
            now_iso(),
            user_id
        ],
    )
    .map_err(|e| format!("status update failed: {e}"))?;
    let _ = sync::write_to_sync_log(&conn, "users", &user_id, "UPDATE", Some(&serde_json::json!({
        "id": user_id, "status": status, "updated_at": now_iso()
    })));
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
            "UPDATE users SET status = 'deleted', revoked_by = ?1, revoked_at = ?2, updated_at = ?2, synced = 0
             WHERE id = ?3 AND status != 'deleted'",
            params![actor_id, now_iso(), user_id],
        )
        .map_err(|e| format!("delete failed: {e}"))?;
    if n == 0 {
        return Err("User not found or already deleted.".to_string());
    }
    let _ = sync::write_to_sync_log(&conn, "users", &user_id, "UPDATE", Some(&serde_json::json!({
        "id": user_id, "status": "deleted", "updated_at": now_iso()
    })));
    append_audit(&conn, &actor_id, "deleted_user", Some(&user_id), None)?;
    Ok(())
}

#[tauri::command]
pub fn restore_user(state: State<AppState>, actor_id: String, user_id: String) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;
    let n = conn
        .execute(
            "UPDATE users SET status = 'active', revoked_by = NULL, revoked_at = NULL, updated_at = ?1, synced = 0
             WHERE id = ?2 AND status = 'deleted'",
            params![now_iso(), user_id],
        )
        .map_err(|e| format!("restore failed: {e}"))?;
    if n == 0 {
        return Err("User not found or not deleted.".to_string());
    }
    let _ = sync::write_to_sync_log(&conn, "users", &user_id, "UPDATE", Some(&serde_json::json!({
        "id": user_id, "status": "active", "updated_at": now_iso()
    })));
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
    let _ = sync::write_to_sync_log(&conn, "users", &user_id, "DELETE", Some(&serde_json::json!({
        "id": user_id
    })));
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
            "UPDATE users SET credential_hash = ?1, auth_type = 'password', must_change_password = 1, updated_at = ?2, synced = 0
             WHERE id = ?3 AND status != 'deleted'",
            params![hash, now_iso(), user_id],
        )
        .map_err(|e| format!("password reset failed: {e}"))?;
    if n == 0 {
        return Err("User not found or deleted.".to_string());
    }
    let _ = sync::write_to_sync_log(&conn, "users", &user_id, "UPDATE", Some(&serde_json::json!({
        "id": user_id, "updated_at": now_iso()
    })));
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
        "UPDATE users SET credential_hash = ?1, auth_type = 'password', must_change_password = 0, updated_at = ?2, synced = 0 WHERE id = ?3",
        params![hash, now_iso(), row.0],
    )
    .map_err(|e| format!("password update failed: {e}"))?;
    save_session_token(&conn, &row.0)?;
    *match state.session.lock() { Ok(s) => s, Err(p) => p.into_inner() } = Some(crate::db::Session {
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

