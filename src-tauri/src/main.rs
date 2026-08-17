// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

/// Offline recovery tools, run with physical access to the machine (the app
/// must be closed):
///
///   truckflow reset-admin <username> <new-password>
///       Reset any account's password directly in the database.
///
///   truckflow set-recovery-code
///       Generate and print a brand-new one-time recovery code (replaces the
///       first-run code if it was lost). The printed code is the only time it
///       is shown.
///
///   truckflow db-path
///       Print the resolved database path (honours TRUCKFLOW_DB).
///
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        truckflow_lib::run();
        return ExitCode::SUCCESS;
    }
    match args[1].as_str() {
        "db-path" => {
            println!("{}", truckflow_lib::db::default_db_path().display());
            ExitCode::SUCCESS
        }
        "reset-admin" => {
            if args.len() != 4 {
                eprintln!("usage: truckflow reset-admin <username> <new-password>");
                return ExitCode::FAILURE;
            }
            match cli_reset_admin(&args[2], &args[3]) {
                Ok(msg) => {
                    println!("{msg}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "set-recovery-code" => {
            if args.len() != 2 {
                eprintln!("usage: truckflow set-recovery-code");
                return ExitCode::FAILURE;
            }
            match cli_set_recovery_code() {
                Ok(msg) => {
                    println!("{msg}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("unknown command: {} (expected reset-admin | set-recovery-code | db-path)", args[1]);
            ExitCode::FAILURE
        }
    }
}

fn cli_reset_admin(username: &str, new_password: &str) -> Result<String, String> {
    let path = truckflow_lib::db::default_db_path();
    let conn = truckflow_lib::db::open_db(&path).map_err(|e| format!("cannot open database at {}: {e}", path.display()))?;
    let id: String = conn
        .query_row(
            "SELECT id FROM users WHERE name = ?1",
            rusqlite::params![username.trim()],
            |r| r.get(0),
        )
        .map_err(|_| format!("no account named \"{username}\" was found"))?;
    let strength = truckflow_lib::auth::validate_password(new_password);
    if !strength.valid {
        return Err("new password does not meet the strength rules".to_string());
    }
    let hash = truckflow_lib::auth::hash_credential(new_password)?;
    conn.execute(
        "UPDATE users SET credential_hash = ?1, auth_type = 'password', must_change_password = 0, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![hash, truckflow_lib::db::now_iso(), id],
    )
    .map_err(|e| format!("password update failed: {e}"))?;
    Ok(format!("Password for \"{username}\" has been reset. Close this window and sign in with the new password."))
}

fn cli_set_recovery_code() -> Result<String, String> {
    let path = truckflow_lib::db::default_db_path();
    let conn = truckflow_lib::db::open_db(&path).map_err(|e| format!("cannot open database at {}: {e}", path.display()))?;
    let code = truckflow_lib::commands::generate_recovery_code();
    truckflow_lib::commands::save_recovery_code(&conn, &code)?;
    Ok(format!(
        "New recovery code: {code}\nWrite this down and keep it safe — it is only shown once. Close this window after saving it."
    ))
}


