//! Passkeys (WebAuthn credentials): storing them, and the relying party
//! that registers and checks them.
//!
//! A passkey always verifies its user (a PIN or biometric), so it is a
//! second factor after a password and also signs in on its own.
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::Serialize;
use webauthn_rs::{
    Webauthn, WebauthnBuilder,
    prelude::{AuthenticationResult, Passkey},
};

use crate::{
    config::Config,
    db::{self, Executor, tables::UserPasskeys},
    http::ApiError,
    time::now_ms,
    users::new_id,
};

pub const MAX_NAME_LENGTH: usize = 120;
/// More than anyone needs, and a bound on what sign-in has to load.
pub const MAX_PER_USER: usize = 20;

/// The relying party for this server: its host is the RP ID and its origin
/// the only one allowed to use the passkeys.
pub fn relying_party(config: &Config, instance_name: &str) -> Result<Webauthn, ApiError> {
    let unavailable = || {
        ApiError::conflict("passkeys need the server's public URL to use a domain name")
            .with_code("passkeys_unavailable")
    };
    let host = config.public_url.host_str().ok_or_else(unavailable)?;
    let origin = url::Url::parse(&config.public_origin()).map_err(|_| unavailable())?;
    WebauthnBuilder::new(host, &origin)
        .map(|builder| builder.rp_name(instance_name))
        .and_then(WebauthnBuilder::build)
        .map_err(|error| {
            tracing::warn!(%error, "passkeys are unavailable with this public URL");
            unavailable()
        })
}

#[derive(sqlx::FromRow)]
struct PasskeyRow {
    id: String,
    user_id: String,
    name: String,
    passkey_json: String,
    created_at: i64,
    last_used_at: Option<i64>,
}

/// A user's passkey as stored.
#[derive(Debug, Clone)]
pub struct StoredPasskey {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub passkey: Passkey,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl StoredPasskey {
    fn from_row(row: PasskeyRow) -> Option<Self> {
        match serde_json::from_str(&row.passkey_json) {
            Ok(passkey) => Some(Self {
                id: row.id,
                user_id: row.user_id,
                name: row.name,
                passkey,
                created_at: row.created_at,
                last_used_at: row.last_used_at,
            }),
            Err(error) => {
                tracing::error!(passkey_id = row.id, %error, "a stored passkey is unreadable");
                None
            }
        }
    }
}

/// What the account page shows about a passkey.
#[derive(Debug, Serialize)]
pub struct PasskeyView {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl From<&StoredPasskey> for PasskeyView {
    fn from(stored: &StoredPasskey) -> Self {
        Self {
            id: stored.id.clone(),
            name: stored.name.clone(),
            created_at: stored.created_at,
            last_used_at: stored.last_used_at,
        }
    }
}

fn select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            UserPasskeys::Id,
            UserPasskeys::UserId,
            UserPasskeys::Name,
            UserPasskeys::PasskeyJson,
            UserPasskeys::CreatedAt,
            UserPasskeys::LastUsedAt,
        ])
        .from(UserPasskeys::Table)
        .to_owned()
}

pub async fn for_user(
    executor: &mut impl Executor,
    user_id: &str,
) -> db::Result<Vec<StoredPasskey>> {
    let rows: Vec<PasskeyRow> = executor
        .fetch_all(
            &select()
                .and_where(Expr::col(UserPasskeys::UserId).eq(user_id))
                .order_by(UserPasskeys::CreatedAt, Order::Asc)
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(StoredPasskey::from_row)
        .collect())
}

pub async fn by_credential_id(
    executor: &mut impl Executor,
    credential_id: &[u8],
) -> db::Result<Option<StoredPasskey>> {
    let row: Option<PasskeyRow> = executor
        .fetch_optional(
            &select()
                .and_where(Expr::col(UserPasskeys::CredentialId).eq(credential_id.to_vec()))
                .to_owned(),
        )
        .await?;
    Ok(row.and_then(StoredPasskey::from_row))
}

/// Stores a newly registered passkey and returns it.
pub async fn insert(
    executor: &mut impl Executor,
    user_id: &str,
    name: &str,
    passkey: Passkey,
) -> Result<StoredPasskey, ApiError> {
    let json = serde_json::to_string(&passkey).map_err(anyhow::Error::from)?;
    let stored = StoredPasskey {
        id: new_id(),
        user_id: user_id.to_owned(),
        name: name.to_owned(),
        created_at: now_ms(),
        last_used_at: None,
        passkey,
    };
    let inserted = executor
        .execute(
            &Query::insert()
                .into_table(UserPasskeys::Table)
                .columns([
                    UserPasskeys::Id,
                    UserPasskeys::UserId,
                    UserPasskeys::CredentialId,
                    UserPasskeys::Name,
                    UserPasskeys::PasskeyJson,
                    UserPasskeys::CreatedAt,
                ])
                .values_panic([
                    stored.id.as_str().into(),
                    user_id.into(),
                    stored.passkey.cred_id().to_vec().into(),
                    name.into(),
                    json.into(),
                    stored.created_at.into(),
                ])
                .to_owned(),
        )
        .await;
    match inserted {
        Ok(_) => Ok(stored),
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            Err(ApiError::conflict("this passkey is already registered")
                .with_code("passkey_exists"))
        }
        Err(error) => Err(error.into()),
    }
}

/// Records a successful sign-in: when, and the authenticator's new
/// signature counter and backup state.
pub async fn record_use(
    executor: &mut impl Executor,
    stored: &mut StoredPasskey,
    result: &AuthenticationResult,
) -> Result<(), ApiError> {
    let now = now_ms();
    let mut update = Query::update();
    update
        .table(UserPasskeys::Table)
        .value(UserPasskeys::LastUsedAt, now)
        .and_where(Expr::col(UserPasskeys::Id).eq(stored.id.as_str()));
    if stored.passkey.update_credential(result) == Some(true) {
        let json = serde_json::to_string(&stored.passkey).map_err(anyhow::Error::from)?;
        update.value(UserPasskeys::PasskeyJson, json);
    }
    executor.execute(&update).await?;
    stored.last_used_at = Some(now);
    Ok(())
}

pub fn validate_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_LENGTH
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(format!(
            "the passkey name must be 1 to {MAX_NAME_LENGTH} characters with no control characters"
        )));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(public_url: &str) -> Config {
        Config::from_toml(&format!(
            "public_url = \"{public_url}\"\ndata_dir = \"/tmp/meshrmm-test\"\ntls.mode = \"proxy\""
        ))
        .unwrap()
    }

    #[test]
    fn the_relying_party_is_the_public_host() {
        assert!(relying_party(&config("https://rmm.example.com"), "Acme").is_ok());
        assert!(relying_party(&config("https://rmm.example.com:8443"), "Acme").is_ok());
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(validate_name("  YubiKey ").unwrap(), "YubiKey");
        assert!(validate_name(" ").is_err());
        assert!(validate_name("a\u{0}b").is_err());
        assert!(validate_name(&"x".repeat(MAX_NAME_LENGTH + 1)).is_err());
    }
}
