use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};

use crate::models::PasswordStrength;

pub fn hash_credential(plain: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("credential hashing failed: {e}"))
}

pub fn verify_credential(hash: &str, plain: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

pub fn validate_password(pw: &str) -> PasswordStrength {
    let length = pw.chars().count() >= 8;
    let uppercase = pw.chars().any(|c| c.is_ascii_uppercase());
    let lowercase = pw.chars().any(|c| c.is_ascii_lowercase());
    let digit = pw.chars().any(|c| c.is_ascii_digit());
    let symbol = pw.chars().any(|c| c.is_ascii_punctuation() || (!c.is_ascii_alphanumeric() && !c.is_whitespace()));
    PasswordStrength {
        length,
        uppercase,
        lowercase,
        digit,
        symbol,
        valid: length && uppercase && lowercase && digit && symbol,
    }
}
