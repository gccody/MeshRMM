//! User accounts: lookups, validation and the changes several routes share.
use sea_query::{Expr, ExprTrait, Func, LockType, Query};

use crate::{
    db::{
        self, Executor,
        tables::{
            PasswordResets, UserPasskeys, UserRecoveryCodes, UserRoles, UserSessions, UserTotp,
            Users,
        },
    },
    http::ApiError,
};

pub const MAX_EMAIL_LENGTH: usize = 254;
pub const MAX_DISPLAY_NAME_LENGTH: usize = 120;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub password_hash: Option<String>,
    pub password_changed_at: Option<i64>,
    pub disabled: bool,
    /// The SSO provider's subject, once the user has signed in with SSO.
    pub oidc_subject: Option<String>,
    pub scim_external_id: Option<String>,
    /// SCIM created or changed the account.
    pub scim_managed: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_sign_in_at: Option<i64>,
}

const COLUMNS: [Users; 12] = [
    Users::Id,
    Users::Email,
    Users::DisplayName,
    Users::PasswordHash,
    Users::PasswordChangedAt,
    Users::Disabled,
    Users::OidcSubject,
    Users::ScimExternalId,
    Users::ScimManaged,
    Users::CreatedAt,
    Users::UpdatedAt,
    Users::LastSignInAt,
];

pub fn select() -> sea_query::SelectStatement {
    Query::select()
        .columns(COLUMNS)
        .from(Users::Table)
        .to_owned()
}

pub async fn by_id(executor: &mut impl Executor, id: &str) -> db::Result<Option<User>> {
    executor
        .fetch_optional(&select().and_where(Expr::col(Users::Id).eq(id)).to_owned())
        .await
}

/// Looks up an account by an email address already passed through
/// [`normalize_email`].
pub async fn by_email(executor: &mut impl Executor, email: &str) -> db::Result<Option<User>> {
    executor
        .fetch_optional(
            &select()
                .and_where(Expr::col(Users::Email).eq(email))
                .to_owned(),
        )
        .await
}

pub async fn count(executor: &mut impl Executor) -> db::Result<i64> {
    let (count,): (i64,) = executor
        .fetch_one(
            &Query::select()
                .expr(Func::count(Expr::col(Users::Id)))
                .from(Users::Table)
                .to_owned(),
        )
        .await?;
    Ok(count)
}

pub struct NewUser<'a> {
    pub id: &'a str,
    pub email: &'a str,
    pub display_name: &'a str,
    pub password_hash: Option<&'a str>,
    pub role_ids: &'a [String],
    pub now_ms: i64,
}

/// Inserts a user and their roles.
pub async fn insert(executor: &mut impl Executor, user: NewUser<'_>) -> db::Result<()> {
    executor
        .execute(
            &Query::insert()
                .into_table(Users::Table)
                .columns([
                    Users::Id,
                    Users::Email,
                    Users::DisplayName,
                    Users::PasswordHash,
                    Users::PasswordChangedAt,
                    Users::CreatedAt,
                    Users::UpdatedAt,
                ])
                .values_panic([
                    user.id.into(),
                    user.email.into(),
                    user.display_name.into(),
                    user.password_hash.map(str::to_owned).into(),
                    user.password_hash.map(|_| user.now_ms).into(),
                    user.now_ms.into(),
                    user.now_ms.into(),
                ])
                .to_owned(),
        )
        .await?;
    set_roles(executor, user.id, user.role_ids).await
}

/// Replaces the roles a user holds.
pub async fn set_roles(
    executor: &mut impl Executor,
    user_id: &str,
    role_ids: &[String],
) -> db::Result<()> {
    executor
        .execute(
            &Query::delete()
                .from_table(UserRoles::Table)
                .and_where(Expr::col(UserRoles::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    if role_ids.is_empty() {
        return Ok(());
    }
    let mut insert = Query::insert();
    insert
        .into_table(UserRoles::Table)
        .columns([UserRoles::UserId, UserRoles::RoleId]);
    for role_id in role_ids {
        insert.values_panic([user_id.into(), role_id.as_str().into()]);
    }
    executor.execute(&insert).await?;
    Ok(())
}

/// Sets a new password, ends every session except `keep_session_id`, and
/// voids outstanding reset links.
pub async fn set_password(
    executor: &mut impl Executor,
    user_id: &str,
    password_hash: &str,
    keep_session_id: Option<&str>,
    now_ms: i64,
) -> db::Result<()> {
    executor
        .execute(
            &Query::update()
                .table(Users::Table)
                .values([
                    (Users::PasswordHash, password_hash.into()),
                    (Users::PasswordChangedAt, now_ms.into()),
                    (Users::UpdatedAt, now_ms.into()),
                ])
                .and_where(Expr::col(Users::Id).eq(user_id))
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::update()
                .table(PasswordResets::Table)
                .value(PasswordResets::UsedAt, now_ms)
                .and_where(Expr::col(PasswordResets::UserId).eq(user_id))
                .and_where(Expr::col(PasswordResets::UsedAt).is_null())
                .to_owned(),
        )
        .await?;
    end_sessions(executor, user_id, keep_session_id).await?;
    Ok(())
}

/// Ends a user's sessions, except `keep_session_id`. Returns how many ended.
pub async fn end_sessions(
    executor: &mut impl Executor,
    user_id: &str,
    keep_session_id: Option<&str>,
) -> db::Result<u64> {
    let mut delete = Query::delete();
    delete
        .from_table(UserSessions::Table)
        .and_where(Expr::col(UserSessions::UserId).eq(user_id));
    if let Some(keep) = keep_session_id {
        delete.and_where(Expr::col(UserSessions::Id).ne(keep));
    }
    executor.execute(&delete).await
}

/// Locks the user's row until the transaction ends, so changes that check
/// what else the user has (such as removing a second factor) run one at a
/// time. (SQLite's write lock already does; sea-query leaves FOR UPDATE out
/// there.)
pub async fn lock(executor: &mut impl Executor, user_id: &str) -> db::Result<()> {
    executor
        .fetch_optional::<(String,), _>(
            &Query::select()
                .column(Users::Id)
                .from(Users::Table)
                .and_where(Expr::col(Users::Id).eq(user_id))
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Removes every second factor: the authenticator app, passkeys and
/// recovery codes.
pub async fn remove_two_factor(executor: &mut impl Executor, user_id: &str) -> db::Result<()> {
    executor
        .execute(
            &Query::delete()
                .from_table(UserTotp::Table)
                .and_where(Expr::col(UserTotp::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::delete()
                .from_table(UserPasskeys::Table)
                .and_where(Expr::col(UserPasskeys::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    remove_recovery_codes(executor, user_id).await
}

pub async fn remove_recovery_codes(executor: &mut impl Executor, user_id: &str) -> db::Result<()> {
    executor
        .execute(
            &Query::delete()
                .from_table(UserRecoveryCodes::Table)
                .and_where(Expr::col(UserRecoveryCodes::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Whether the user has a confirmed authenticator app.
pub async fn has_totp(executor: &mut impl Executor, user_id: &str) -> db::Result<bool> {
    let row: Option<(String,)> = executor
        .fetch_optional(
            &Query::select()
                .column(UserTotp::UserId)
                .from(UserTotp::Table)
                .and_where(Expr::col(UserTotp::UserId).eq(user_id))
                .and_where(Expr::col(UserTotp::ConfirmedAt).is_not_null())
                .to_owned(),
        )
        .await?;
    Ok(row.is_some())
}

pub async fn passkey_count(executor: &mut impl Executor, user_id: &str) -> db::Result<i64> {
    let (count,): (i64,) = executor
        .fetch_one(
            &Query::select()
                .expr(Func::count(Expr::col(UserPasskeys::Id)))
                .from(UserPasskeys::Table)
                .and_where(Expr::col(UserPasskeys::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    Ok(count)
}

/// The second factors a user has set up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecondFactors {
    pub totp: bool,
    pub passkeys: i64,
}

impl SecondFactors {
    /// Two-factor authentication is on: a password alone doesn't sign in.
    pub fn any(self) -> bool {
        self.totp || self.passkeys > 0
    }
}

pub async fn second_factors(
    executor: &mut impl Executor,
    user_id: &str,
) -> db::Result<SecondFactors> {
    Ok(SecondFactors {
        totp: has_totp(executor, user_id).await?,
        passkeys: passkey_count(executor, user_id).await?,
    })
}

/// Trims and lowercases an email address and checks it looks like one. The
/// server can't prove an address works; invitations and resets do that.
pub fn normalize_email(raw: &str) -> Result<String, ApiError> {
    let email = raw.trim().to_lowercase();
    let valid = email.len() <= MAX_EMAIL_LENGTH
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && !domain.is_empty()
                && !domain.contains('@')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
    if valid {
        Ok(email)
    } else {
        Err(ApiError::bad_request("enter a valid email address"))
    }
}

pub fn validate_display_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty()
        || name.chars().count() > MAX_DISPLAY_NAME_LENGTH
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(format!(
            "the name must be 1 to {MAX_DISPLAY_NAME_LENGTH} characters with no control characters"
        )));
    }
    Ok(name.to_owned())
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_trimmed_lowercased_and_checked() {
        assert_eq!(
            normalize_email("  Ada@Example.COM ").unwrap(),
            "ada@example.com"
        );
        for invalid in [
            "",
            "ada",
            "@example.com",
            "ada@",
            "ada@@example.com",
            "ada lovelace@example.com",
            "ada@.example.com",
            "ada@example.com.",
        ] {
            assert!(
                normalize_email(invalid).is_err(),
                "{invalid:?} was accepted"
            );
        }
        let long = format!("{}@example.com", "a".repeat(250));
        assert!(normalize_email(&long).is_err());
    }

    #[test]
    fn display_names_are_trimmed_and_bounded() {
        assert_eq!(validate_display_name("  Ada  ").unwrap(), "Ada");
        assert!(validate_display_name("   ").is_err());
        assert!(validate_display_name("Ada\u{7}").is_err());
        assert!(validate_display_name(&"é".repeat(120)).is_ok());
        assert!(validate_display_name(&"é".repeat(121)).is_err());
    }
}
