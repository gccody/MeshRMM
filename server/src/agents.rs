//! Enrolled devices and how their Agents authenticate.
//!
//! An Agent authenticates with a bearer token whose SHA-256 is its row's
//! `auth_token_hash`. A rotation stages a new token as the pending one; the
//! first request that uses it promotes it and retires the old token, so an
//! Agent that never received the new token keeps working with the old one.
use axum::http::{HeaderMap, StatusCode, header};
use sea_query::{Expr, ExprTrait, Query};
use subtle::ConstantTimeEq;

use crate::{
    db::{Executor, tables::Agents},
    http::{ApiError, AppState},
    secrets::{InstanceKey, token_hash},
    time::now_ms,
};

pub const MAX_NAME_CHARS: usize = 120;

/// A device ID, run ID or other server-generated ID from a request path:
/// a UUID in its usual lowercase hyphenated form, which is also safe to use
/// as a file name.
pub fn parse_id(value: &str, what: &str) -> Result<String, ApiError> {
    uuid::Uuid::parse_str(value)
        .ok()
        .map(|id| id.hyphenated().to_string())
        .filter(|id| id == value)
        .ok_or_else(|| ApiError::bad_request(format!("invalid {what} ID")))
}

/// A computer's name as the Agent reports it: trimmed, on one line, and at
/// most [`MAX_NAME_CHARS`] characters.
pub fn normalize_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(char::is_control))
    .then(|| name.to_owned())
}

/// The bearer token in an `Authorization` header.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// Whether two token hashes are equal, in time that doesn't depend on where
/// they differ.
pub fn hashes_match(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).into()
}

/// The Agent credential an installer and an endpoint's private recovery key
/// produce. Neither secret is stored, so only the endpoint holding both can
/// recover the credential by redeeming the installer again.
pub fn enrollment_token(install_token: &str, redemption_key: &str) -> String {
    token_hash(&format!(
        "meshrmm-enrollment-v1:{install_token}:{redemption_key}"
    ))
}

/// An Agent that proved its credential.
#[derive(Debug, Clone)]
pub struct AuthenticatedAgent {
    pub device_id: String,
    pub name: String,
    /// The device was deleted, and the Agent is expected to uninstall itself.
    pub deletion_requested: bool,
}

#[derive(sqlx::FromRow)]
struct CredentialRow {
    name: String,
    auth_token_hash: String,
    pending_auth_token_hash: Option<String>,
    deletion_requested_at: Option<i64>,
}

fn unauthenticated() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "Agent authentication failed")
        .with_code("agent_unauthenticated")
}

/// Checks the request's bearer token against the device's credential, and
/// promotes a pending credential the Agent has started using.
pub async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    device_id: &str,
) -> Result<AuthenticatedAgent, ApiError> {
    let device_id = parse_id(device_id, "device").map_err(|_| unauthenticated())?;
    let supplied = token_hash(bearer_token(headers).ok_or_else(unauthenticated)?);
    let database = &state.database;
    let row: CredentialRow = database
        .fetch_optional(
            &Query::select()
                .columns([
                    Agents::Name,
                    Agents::AuthTokenHash,
                    Agents::PendingAuthTokenHash,
                    Agents::DeletionRequestedAt,
                ])
                .from(Agents::Table)
                .and_where(Expr::col(Agents::Id).eq(device_id.as_str()))
                .to_owned(),
        )
        .await?
        .ok_or_else(unauthenticated)?;
    let pending = row
        .pending_auth_token_hash
        .as_deref()
        .is_some_and(|pending| hashes_match(&supplied, pending));
    if !pending && !hashes_match(&supplied, &row.auth_token_hash) {
        return Err(unauthenticated());
    }
    if pending {
        database
            .execute(
                &Query::update()
                    .table(Agents::Table)
                    .value(Agents::AuthTokenHash, supplied.as_str())
                    .value(Agents::PendingAuthTokenHash, Option::<String>::None)
                    .value(Agents::PendingAuthTokenEncrypted, Option::<Vec<u8>>::None)
                    .value(Agents::UpdatedAt, now_ms())
                    .and_where(Expr::col(Agents::Id).eq(device_id.as_str()))
                    .and_where(Expr::col(Agents::PendingAuthTokenHash).eq(supplied.as_str()))
                    .to_owned(),
            )
            .await?;
        tracing::info!(device_id, "Agent started using its rotated credential");
    }
    Ok(AuthenticatedAgent {
        device_id,
        name: row.name,
        deletion_requested: row.deletion_requested_at.is_some(),
    })
}

/// What the device's rotated credential is sealed for.
pub fn rotation_context(device_id: &str) -> String {
    format!("agent-rotation:{device_id}")
}

/// A device's staged rotation: the pending credential's hash, and the
/// credential sealed with the instance key.
#[derive(Debug, sqlx::FromRow)]
pub struct PendingCredential {
    pub pending_auth_token_hash: Option<String>,
    pub pending_auth_token_encrypted: Option<Vec<u8>>,
}

impl PendingCredential {
    /// The staged credential, while it is still the pending one.
    pub fn token(&self, key: &InstanceKey, device_id: &str) -> anyhow::Result<Option<String>> {
        let (Some(hash), Some(sealed)) = (
            &self.pending_auth_token_hash,
            &self.pending_auth_token_encrypted,
        ) else {
            return Ok(None);
        };
        let token = String::from_utf8(key.decrypt(&rotation_context(device_id), sealed)?)?;
        Ok((token_hash(&token) == *hash).then_some(token))
    }
}

/// The device's staged rotation, if it is enrolled and not deleted.
pub async fn pending_credential(
    executor: &mut impl Executor,
    device_id: &str,
) -> crate::db::Result<Option<PendingCredential>> {
    executor
        .fetch_optional(
            &Query::select()
                .columns([
                    Agents::PendingAuthTokenHash,
                    Agents::PendingAuthTokenEncrypted,
                ])
                .from(Agents::Table)
                .and_where(Expr::col(Agents::Id).eq(device_id))
                .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
                .to_owned(),
        )
        .await
}

/// Whether the device is enrolled and not deleted.
pub async fn is_active(executor: &mut impl Executor, device_id: &str) -> crate::db::Result<bool> {
    let row: Option<(String,)> = executor
        .fetch_optional(
            &active()
                .and_where(Expr::col(Agents::Id).eq(device_id))
                .to_owned(),
        )
        .await?;
    Ok(row.is_some())
}

/// The IDs of devices that are enrolled and not deleted.
pub fn active() -> sea_query::SelectStatement {
    Query::select()
        .column(Agents::Id)
        .from(Agents::Table)
        .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
        .to_owned()
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn ids_must_be_canonical_uuids() {
        let id = "0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a";
        assert_eq!(parse_id(id, "device").unwrap(), id);
        for invalid in [
            "",
            "..",
            "device-1",
            "0B5E8F5C-58A4-4A5E-9D7E-3C0C4D1C2B3A",
            "0b5e8f5c58a44a5e9d7e3c0c4d1c2b3a",
            "urn:uuid:0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a",
            "{0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a}",
        ] {
            assert!(parse_id(invalid, "device").is_err(), "{invalid}");
        }
    }

    #[test]
    fn names_are_trimmed_single_line_and_bounded() {
        assert_eq!(normalize_name("  DESKTOP-1 ").as_deref(), Some("DESKTOP-1"));
        assert_eq!(normalize_name(&"é".repeat(120)), Some("é".repeat(120)));
        assert_eq!(normalize_name(&"a".repeat(121)), None);
        assert_eq!(normalize_name("   "), None);
        assert_eq!(normalize_name("a\nb"), None);
    }

    #[test]
    fn bearer_tokens_need_the_scheme() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer_token(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(bearer_token(&headers), Some("abc"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer_token(&headers), None);
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn enrollment_tokens_depend_on_both_secrets() {
        let token = enrollment_token("installer", "key");
        assert_eq!(token.len(), 64);
        assert_eq!(token, enrollment_token("installer", "key"));
        assert_ne!(token, enrollment_token("installer", "other"));
        assert_ne!(token, enrollment_token("other", "key"));
        assert!(hashes_match(&token, &enrollment_token("installer", "key")));
        assert!(!hashes_match(&token, &token[..63]));
    }
}
