//! `meshrmm-server admin ...`: account repairs for operators locked out of
//! the website. Each prints a one-time link instead of taking a password on
//! the command line, where it would land in shell history.
use anyhow::{Context, bail};
use sea_query::{Expr, ExprTrait, Query};
use serde_json::json;

use crate::{
    api::create_reset,
    audit::{self, Actor, Target},
    db::tables::Users,
    http::AppState,
    rbac::{self, ADMINISTRATOR_ROLE_ID},
    time::{HOUR_MS, now_ms},
    users::{self, NewUser, User, new_id},
};

/// How long a link printed by the CLI works.
const LINK_TTL_MS: i64 = 24 * HOUR_MS;

fn reset_link(state: &AppState, token: &str) -> String {
    format!("{}/reset#token={token}", state.config.public_origin())
}

async fn find_user(state: &AppState, email: &str) -> anyhow::Result<User> {
    let email = users::normalize_email(email)
        .map_err(|_| anyhow::anyhow!("{email:?} isn't a valid email address"))?;
    users::by_email(&mut &state.database, &email)
        .await?
        .with_context(|| format!("no user has the email {email}"))
}

/// Creates a user with `roles` (role IDs or names; the Administrator role if
/// none) and prints a link to choose their password.
pub async fn create_user(
    state: &AppState,
    email: &str,
    display_name: &str,
    roles: &[String],
) -> anyhow::Result<String> {
    let email = users::normalize_email(email)
        .map_err(|_| anyhow::anyhow!("{email:?} isn't a valid email address"))?;
    let display_name = users::validate_display_name(display_name)
        .map_err(|_| anyhow::anyhow!("the name must be 1 to 120 characters"))?;
    let all_roles = rbac::load_roles(&mut &state.database, None).await?;
    let role_ids = if roles.is_empty() {
        vec![ADMINISTRATOR_ROLE_ID.to_owned()]
    } else {
        roles
            .iter()
            .map(|wanted| {
                all_roles
                    .iter()
                    .find(|role| role.id == *wanted || role.name.eq_ignore_ascii_case(wanted))
                    .map(|role| role.id.clone())
                    .with_context(|| format!("no role is called {wanted:?}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    };
    let mut transaction = state.database.begin().await?;
    if users::by_email(&mut transaction, &email).await?.is_some() {
        bail!("a user with the email {email} already exists; use reset-password");
    }
    let user_id = new_id();
    users::insert(
        &mut transaction,
        NewUser {
            id: &user_id,
            email: &email,
            display_name: &display_name,
            password_hash: None,
            role_ids: &role_ids,
            now_ms: now_ms(),
        },
    )
    .await?;
    let (token, _) = create_reset(&mut transaction, &user_id, None, LINK_TTL_MS).await?;
    audit::record(
        &mut transaction,
        &Actor::cli(),
        "user.create",
        Target::user(&user_id),
        json!({ "email": email, "role_ids": role_ids }),
    )
    .await?;
    transaction.commit().await?;
    Ok(format!(
        "Created {email}. Open this link within 24 hours to choose a password:\n{}",
        reset_link(state, &token)
    ))
}

/// Prints a link that sets a new password for the user and enables the
/// account if it was disabled.
pub async fn reset_password(state: &AppState, email: &str) -> anyhow::Result<String> {
    let user = find_user(state, email).await?;
    let mut transaction = state.database.begin().await?;
    if user.disabled {
        transaction
            .execute(
                &Query::update()
                    .table(Users::Table)
                    .values([
                        (Users::Disabled, false.into()),
                        (Users::UpdatedAt, now_ms().into()),
                    ])
                    .and_where(Expr::col(Users::Id).eq(user.id.as_str()))
                    .to_owned(),
            )
            .await?;
    }
    let (token, _) = create_reset(&mut transaction, &user.id, None, LINK_TTL_MS).await?;
    audit::record(
        &mut transaction,
        &Actor::cli(),
        "user.password_reset_create",
        Target::user(&user.id),
        json!({ "enabled": user.disabled }),
    )
    .await?;
    transaction.commit().await?;
    let enabled = if user.disabled {
        " (the account was disabled and is now enabled)"
    } else {
        ""
    };
    Ok(format!(
        "Open this link within 24 hours to set a new password for {}{enabled}:\n{}",
        user.email,
        reset_link(state, &token)
    ))
}

/// Removes the user's authenticator app and recovery codes and signs them
/// out everywhere.
pub async fn reset_two_factor(state: &AppState, email: &str) -> anyhow::Result<String> {
    let user = find_user(state, email).await?;
    let mut transaction = state.database.begin().await?;
    users::remove_two_factor(&mut transaction, &user.id).await?;
    users::end_sessions(&mut transaction, &user.id, None).await?;
    audit::record(
        &mut transaction,
        &Actor::cli(),
        "user.two_factor_reset",
        Target::user(&user.id),
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    Ok(format!(
        "Removed two-factor authentication from {} and signed them out. They can sign in with their password and set it up again.",
        user.email
    ))
}
