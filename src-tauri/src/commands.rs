use rusqlite::{params, Connection};
use serde::Serialize;
use tauri::State;

use crate::auth::{validate_password, verify_credential};
use crate::db::{append_audit, now_iso, AppState, PERMISSION_CATALOG, ROLE_PRESETS};
use crate::models::{
    AppStatus, LoginResult, PasswordStrength, PermissionChangeResult, PermissionView, RolePresetView,
    SessionUser, UserView,
};

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
                    profile_photo_ref, language_preference, notification_sound, must_change_password
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
    let session = state.session.lock().map_err(|e| e.to_string())?;
    let current_user = match session.as_ref() {
        Some(s) => load_session_user(&conn, &s.user_id).ok(),
        None => None,
    };
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
    let id = uuid::Uuid::new_v4().to_string();
    let hash = crate::auth::hash_credential(&password)?;
    conn.execute(
        "INSERT INTO users (id, name, auth_type, credential_hash, status, created_at, updated_at)
         VALUES (?1, ?2, 'password', ?3, 'active', ?4, ?4)",
        params![id, name, hash, now_iso()],
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
            params![id, pid, now_iso()],
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
pub fn login_password(state: State<AppState>, username: String, password: String) -> Result<LoginResult, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let row = conn
        .query_row(
            "SELECT id, name, auth_type, credential_hash, status, revoked_by, must_change_password FROM users WHERE name = ?1",
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
                ))
            },
        )
        .map_err(|_| "Incorrect username or password.".to_string())?;

    if row.4 != "active" {
        if row.4 == "deleted" {
            return Err(
                "This account has been deleted. Contact your admin if you believe this is a mistake.".to_string(),
            );
        }
        let msg = match row.5 {
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
    if !verify_credential(&row.3, &password) {
        return Err("Incorrect username or password.".to_string());
    }
    let id = row.0.clone();
    *state.session.lock().map_err(|e| e.to_string())? = Some(crate::db::Session {
        user_id: id.clone(),
        logged_in_at: now_iso(),
        auth_type: "password".to_string(),
    });
    let user = load_session_user(&conn, &id)?;
    append_audit(&conn, &id, "login", Some(&id), Some(serde_json::json!({ "method": "password", "name": row.1 })))?;
    Ok(LoginResult {
        must_change_password: row.6 != 0,
        recovery_code: None,
        user,
    })
}

#[tauri::command]
pub fn logout(state: State<AppState>) -> Result<(), String> {
    let mut session = state.session.lock().map_err(|e| e.to_string())?;
    if let Some(s) = session.as_ref() {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        let _ = append_audit(&conn, &s.user_id, "logout", Some(&s.user_id), None);
    }
    *session = None;
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
pub fn list_users(state: State<AppState>) -> Result<Vec<UserView>, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare("SELECT id, name, auth_type, status, phone_number, theme_mode, theme_accent, created_at FROM users ORDER BY name")
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
/// not chosen freely (03 §3).
#[tauri::command]
pub fn create_user(
    state: State<AppState>,
    actor_id: String,
    name: String,
    permission_keys: Vec<String>,
    initial_password: String,
) -> Result<UserView, String> {
    let conn = state.db.lock().map_err(|e| e.to_string())?;
    ensure_admin_permission(&conn, &actor_id, "manage_users")?;

    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Name is required.".to_string());
    }

    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO users (id, name, auth_type, credential_hash, status, created_at, updated_at)
         VALUES (?1, ?2, 'password', 'pending', 'active', ?3, ?3)",
        params![id, name, now_iso()],
    )
    .map_err(|e| format!("user creation failed: {e}"))?;

    if let Err(e) = set_credential(&conn, &id, &initial_password) {
        let _ = conn.execute("DELETE FROM users WHERE id = ?1", params![id]);
        return Err(e);
    }

    for key in &permission_keys {
        let pid = crate::db::permission_id_for_key(&conn, key)?;
        conn.execute(
            "INSERT INTO user_permissions (user_id, permission_id, granted_by, granted_at) VALUES (?1, ?2, ?3, ?4)",
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

