//! Password hashing with Argon2id and the password policy.
use std::sync::OnceLock;

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};

use crate::{http::ApiError, secrets::random_bytes};

/// Longer input only costs hashing time.
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// Hashes a password with Argon2id's default (OWASP-recommended) cost, off
/// the async runtime.
pub async fn hash(password: &str) -> anyhow::Result<String> {
    let password = password.to_owned();
    let _permit = argon2_permit().await;
    tokio::task::spawn_blocking(move || hash_blocking(&password)).await?
}

/// Each Argon2 run takes about 19 MiB and a core for tens of milliseconds, so
/// at most one per core runs at once; the rest wait instead of exhausting
/// memory.
async fn argon2_permit() -> tokio::sync::SemaphorePermit<'static> {
    static PERMITS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    PERMITS
        .get_or_init(|| {
            let cores = std::thread::available_parallelism().map_or(2, usize::from);
            tokio::sync::Semaphore::new(cores)
        })
        .acquire()
        .await
        .expect("the Argon2 semaphore is never closed")
}

fn hash_blocking(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::encode_b64(&random_bytes::<16>())
        .map_err(|error| anyhow::anyhow!("could not encode a salt: {error}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| anyhow::anyhow!("could not hash a password: {error}"))
}

/// Whether `password` matches `hash`. Without a hash (no such account, or
/// one with no password) it still spends the time of a real check, so the
/// response time doesn't reveal which accounts exist.
pub async fn verify(password: &str, hash: Option<&str>) -> bool {
    let password = password.to_owned();
    let hash = hash.map(str::to_owned);
    let _permit = argon2_permit().await;
    tokio::task::spawn_blocking(move || match hash {
        Some(hash) => verify_blocking(&password, &hash),
        None => {
            verify_blocking(&password, dummy_hash());
            false
        }
    })
    .await
    .unwrap_or(false)
}

fn verify_blocking(password: &str, hash: &str) -> bool {
    if password.len() > MAX_PASSWORD_BYTES {
        return false;
    }
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(error) => {
            tracing::error!(%error, "a stored password hash is malformed");
            false
        }
    }
}

fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_blocking(&crate::secrets::new_token()).expect("hashing a random password works")
    })
}

/// Checks a new password against the policy: at least `min_length`
/// characters, at most [`MAX_PASSWORD_BYTES`] bytes.
pub fn check_policy(password: &str, min_length: i64) -> Result<(), ApiError> {
    let length = password.chars().count();
    if i64::try_from(length).unwrap_or(i64::MAX) < min_length {
        return Err(ApiError::bad_request(format!(
            "the password must be at least {min_length} characters"
        ))
        .with_code("weak_password"));
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(ApiError::bad_request(format!(
            "the password must be at most {MAX_PASSWORD_BYTES} bytes"
        ))
        .with_code("weak_password"));
    }
    if password.trim().is_empty() {
        return Err(
            ApiError::bad_request("the password can't be only spaces").with_code("weak_password")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashes_verify_only_the_right_password() {
        let hash = hash("correct horse battery").await.unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify("correct horse battery", Some(&hash)).await);
        assert!(!verify("correct horse batter", Some(&hash)).await);
        assert!(!verify("correct horse battery", None).await);
        assert!(!verify("anything", Some("not a hash")).await);
        assert_ne!(hash, super::hash("correct horse battery").await.unwrap());
    }

    #[test]
    fn the_policy_counts_characters_and_caps_bytes() {
        assert!(check_policy("short", 12).is_err());
        assert!(check_policy("twelve chars", 12).is_ok());
        assert!(check_policy(&"é".repeat(12), 12).is_ok());
        assert!(check_policy(&" ".repeat(20), 12).is_err());
        assert!(check_policy(&"a".repeat(MAX_PASSWORD_BYTES + 1), 12).is_err());
    }
}
