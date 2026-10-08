//! Administering users: listing, roles, disabling, deleting, and helping
//! users who are locked out.
use std::collections::{BTreeSet, HashMap};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use sea_query::{Expr, ExprTrait, Func, LockType, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    RoleRef, create_reset, ensure_within, password_resets::ADMIN_RESET_TTL_MS,
    require_administrator,
};
use crate::{
    audit::{self, Target},
    auth::Authorized,
    db::{
        self, Executor,
        tables::{
            EffectiveUserRoles, OidcGroupRoles, ScimGroupMembers, ScimGroups,
            Settings as SettingsTable, UserOidcGroups, UserPasskeys, UserRoles, UserTotp, Users,
        },
    },
    http::{ApiError, AppState, JsonBody},
    mail::{self, Delivery},
    rbac::{self, ADMINISTRATOR_ROLE_ID, Permission, Role},
    settings,
    time::{HOUR_MS, now_ms},
    users::{self, User},
};

#[derive(Debug, Serialize)]
pub struct UserView {
    id: String,
    email: String,
    display_name: String,
    disabled: bool,
    has_password: bool,
    two_factor_enabled: bool,
    passkeys: i64,
    /// The user has signed in with SSO, which linked the account.
    sso_linked: bool,
    /// SCIM created or changed the account.
    scim_managed: bool,
    /// Roles assigned to the user.
    roles: Vec<RoleRef>,
    /// Roles the user holds through an identity provider group, which only
    /// the provider changes.
    group_roles: Vec<GroupRoleRef>,
    created_at: i64,
    last_sign_in_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct GroupRoleRef {
    id: String,
    name: String,
    /// `scim` or `sso`.
    source: &'static str,
    group: String,
}

/// Users with their roles and two-factor status: everyone, or one user.
async fn views(executor: &mut impl Executor, id: Option<&str>) -> db::Result<Vec<UserView>> {
    let mut select = users::select();
    select.order_by(Users::Email, Order::Asc);
    let mut memberships = Query::select();
    memberships
        .columns([UserRoles::UserId, UserRoles::RoleId])
        .from(UserRoles::Table);
    let mut totp = Query::select();
    totp.column(UserTotp::UserId)
        .from(UserTotp::Table)
        .and_where(Expr::col(UserTotp::ConfirmedAt).is_not_null());
    if let Some(id) = id {
        select.and_where(Expr::col(Users::Id).eq(id));
        memberships.and_where(Expr::col(UserRoles::UserId).eq(id));
        totp.and_where(Expr::col(UserTotp::UserId).eq(id));
    }
    let mut passkeys = Query::select();
    passkeys
        .column(UserPasskeys::UserId)
        .expr(Func::count(Expr::col(UserPasskeys::Id)))
        .from(UserPasskeys::Table)
        .group_by_col(UserPasskeys::UserId);
    let mut scim_groups = Query::select();
    scim_groups
        .column((ScimGroupMembers::Table, ScimGroupMembers::UserId))
        .column((ScimGroups::Table, ScimGroups::RoleId))
        .column((ScimGroups::Table, ScimGroups::DisplayName))
        .from(ScimGroupMembers::Table)
        .inner_join(
            ScimGroups::Table,
            Expr::col((ScimGroups::Table, ScimGroups::Id))
                .equals((ScimGroupMembers::Table, ScimGroupMembers::GroupId)),
        )
        .and_where(Expr::col((ScimGroups::Table, ScimGroups::RoleId)).is_not_null());
    let mut sso_groups = Query::select();
    sso_groups
        .column((UserOidcGroups::Table, UserOidcGroups::UserId))
        .column((OidcGroupRoles::Table, OidcGroupRoles::RoleId))
        .column((UserOidcGroups::Table, UserOidcGroups::GroupName))
        .from(UserOidcGroups::Table)
        .inner_join(
            OidcGroupRoles::Table,
            Expr::col((OidcGroupRoles::Table, OidcGroupRoles::GroupName))
                .equals((UserOidcGroups::Table, UserOidcGroups::GroupName)),
        );
    if let Some(id) = id {
        passkeys.and_where(Expr::col(UserPasskeys::UserId).eq(id));
        scim_groups
            .and_where(Expr::col((ScimGroupMembers::Table, ScimGroupMembers::UserId)).eq(id));
        sso_groups.and_where(Expr::col((UserOidcGroups::Table, UserOidcGroups::UserId)).eq(id));
    }
    let found: Vec<User> = executor.fetch_all(&select).await?;
    let memberships: Vec<(String, String)> = executor.fetch_all(&memberships).await?;
    let with_totp: Vec<(String,)> = executor.fetch_all(&totp).await?;
    let with_totp = with_totp
        .into_iter()
        .map(|(id,)| id)
        .collect::<BTreeSet<_>>();
    let passkeys: Vec<(String, i64)> = executor.fetch_all(&passkeys).await?;
    let passkeys = passkeys.into_iter().collect::<HashMap<_, _>>();
    let scim_groups: Vec<(String, String, String)> = executor.fetch_all(&scim_groups).await?;
    let sso_groups: Vec<(String, String, String)> = executor.fetch_all(&sso_groups).await?;
    let group_memberships = scim_groups
        .into_iter()
        .map(|row| ("scim", row))
        .chain(sso_groups.into_iter().map(|row| ("sso", row)))
        .collect::<Vec<_>>();
    let roles = rbac::load_roles(executor, None)
        .await?
        .into_iter()
        .map(|role| (role.id.clone(), role))
        .collect::<HashMap<_, _>>();
    Ok(found
        .into_iter()
        .map(|user| UserView {
            roles: memberships
                .iter()
                .filter(|(user_id, _)| *user_id == user.id)
                .filter_map(|(_, role_id)| roles.get(role_id).map(RoleRef::from))
                .collect(),
            group_roles: group_memberships
                .iter()
                .filter(|(_, (user_id, _, _))| *user_id == user.id)
                .filter_map(|(source, (_, role_id, group))| {
                    roles.get(role_id).map(|role| GroupRoleRef {
                        id: role.id.clone(),
                        name: role.name.clone(),
                        source,
                        group: group.clone(),
                    })
                })
                .collect(),
            passkeys: passkeys.get(&user.id).copied().unwrap_or_default(),
            two_factor_enabled: with_totp.contains(&user.id) || passkeys.contains_key(&user.id),
            sso_linked: user.oidc_subject.is_some(),
            scim_managed: user.scim_managed,
            has_password: user.password_hash.is_some(),
            id: user.id,
            email: user.email,
            display_name: user.display_name,
            disabled: user.disabled,
            created_at: user.created_at,
            last_sign_in_at: user.last_sign_in_at,
        })
        .collect())
}

/// `GET /v1/users`
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Vec<UserView>>, ApiError> {
    actor.require(Permission::UsersManage)?;
    Ok(Json(views(&mut &state.database, None).await?))
}

/// `GET /v1/users/{id}`
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<UserView>, ApiError> {
    actor.require(Permission::UsersManage)?;
    views(&mut &state.database, Some(&id))
        .await?
        .pop()
        .map(Json)
        .ok_or_else(no_such_user)
}

fn no_such_user() -> ApiError {
    ApiError::not_found("no such user")
}

/// Loads the user `actor` wants to change, and checks the actor holds every
/// permission the user does: otherwise managing users would let someone take
/// over a more powerful account.
async fn manageable(
    executor: &mut impl Executor,
    actor: &Authorized,
    id: &str,
) -> Result<(User, Vec<Role>), ApiError> {
    actor.require(Permission::UsersManage)?;
    let user = users::by_id(executor, id).await?.ok_or_else(no_such_user)?;
    let roles = rbac::user_roles(executor, &user.id).await?;
    if roles.iter().any(Role::is_administrator) {
        require_administrator(actor)?;
    }
    ensure_within(actor, &rbac::permissions_of(&roles), "manage this user")?;
    Ok((user, roles))
}

/// Locked-out users get help from someone else. Their own account changes go
/// through `/v1/account`, which asks for the current password; these routes
/// don't, so allowing them on oneself would let a borrowed session take the
/// account over.
fn not_self(actor: &Authorized, user: &User) -> Result<(), ApiError> {
    if user.id == actor.user.id {
        return Err(ApiError::forbidden(
            "change your own sign-in methods from your account page",
        ));
    }
    Ok(())
}

/// Fails unless an enabled administrator would remain. Call inside the
/// transaction that removes one, before committing.
pub(crate) async fn ensure_administrator_remains(
    executor: &mut impl Executor,
) -> Result<(), ApiError> {
    // Two transactions removing different administrators would each still
    // count the other. Locking one shared row first makes the second wait
    // and then count what the first committed. (SQLite's write lock already
    // serializes them; sea-query leaves FOR UPDATE out there.)
    let _: Option<(i64,)> = executor
        .fetch_optional(
            &Query::select()
                .column(SettingsTable::Id)
                .from(SettingsTable::Table)
                .and_where(Expr::col(SettingsTable::Id).eq(1))
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    let (count,): (i64,) = executor
        .fetch_one(
            &Query::select()
                .expr(Func::count(Expr::col((Users::Table, Users::Id))))
                .from(Users::Table)
                .inner_join(
                    EffectiveUserRoles::Table,
                    Expr::col((EffectiveUserRoles::Table, EffectiveUserRoles::UserId))
                        .equals((Users::Table, Users::Id)),
                )
                .and_where(
                    Expr::col((EffectiveUserRoles::Table, EffectiveUserRoles::RoleId))
                        .eq(ADMINISTRATOR_ROLE_ID),
                )
                .and_where(Expr::col((Users::Table, Users::Disabled)).eq(false))
                .to_owned(),
        )
        .await?;
    if count == 0 {
        return Err(ApiError::conflict(
            "at least one enabled user must keep the Administrator role",
        )
        .with_code("last_administrator"));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserUpdate {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    role_ids: Option<Vec<String>>,
}

/// Checks that every role exists and that `actor` may grant it.
pub(super) async fn grantable_roles(
    executor: &mut impl Executor,
    actor: &Authorized,
    role_ids: &[String],
) -> Result<Vec<Role>, ApiError> {
    let unique = role_ids.iter().cloned().collect::<BTreeSet<_>>();
    let unique = unique.into_iter().collect::<Vec<_>>();
    let roles = rbac::load_roles(executor, Some(&unique)).await?;
    if roles.len() != unique.len() {
        return Err(ApiError::bad_request("one of the roles doesn't exist"));
    }
    if roles.iter().any(Role::is_administrator) {
        require_administrator(actor)?;
    }
    ensure_within(actor, &rbac::permissions_of(&roles), "grant these roles")?;
    Ok(roles)
}

/// `PATCH /v1/users/{id}`: renames, disables or enables a user, or replaces
/// their roles. Disabling ends their sessions.
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<UserUpdate>,
) -> Result<Json<UserView>, ApiError> {
    let mut transaction = state.database.begin().await?;
    let (user, _) = manageable(&mut transaction, &actor, &id).await?;
    let now = now_ms();
    let mut changes = serde_json::Map::new();
    let mut values = Vec::new();
    if let Some(name) = &request.display_name {
        let name = users::validate_display_name(name)?;
        changes.insert("display_name".into(), json!(name));
        values.push((Users::DisplayName, name.into()));
    }
    if let Some(disabled) = request.disabled
        && disabled != user.disabled
    {
        if disabled && user.id == actor.user.id {
            return Err(ApiError::bad_request("you can't disable your own account"));
        }
        changes.insert("disabled".into(), json!(disabled));
        values.push((Users::Disabled, disabled.into()));
        if disabled {
            users::end_sessions(&mut transaction, &user.id, None).await?;
        }
    }
    if !values.is_empty() {
        values.push((Users::UpdatedAt, now.into()));
        transaction
            .execute(
                &Query::update()
                    .table(Users::Table)
                    .values(values)
                    .and_where(Expr::col(Users::Id).eq(user.id.as_str()))
                    .to_owned(),
            )
            .await?;
    }
    if let Some(role_ids) = &request.role_ids {
        let roles = grantable_roles(&mut transaction, &actor, role_ids).await?;
        let ids = roles.iter().map(|role| role.id.clone()).collect::<Vec<_>>();
        users::set_roles(&mut transaction, &user.id, &ids).await?;
        changes.insert("role_ids".into(), json!(ids));
    }
    ensure_administrator_remains(&mut transaction).await?;
    if !changes.is_empty() {
        audit::record(
            &mut transaction,
            &actor.actor(),
            "user.update",
            Target::user(&user.id),
            changes.into(),
        )
        .await?;
    }
    let view = views(&mut transaction, Some(&user.id))
        .await?
        .pop()
        .ok_or_else(no_such_user)?;
    transaction.commit().await?;
    Ok(Json(view))
}

/// `DELETE /v1/users/{id}`: removes a user and everything that signs them in.
/// Records they made (scripts, audit events) keep their ID.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = state.database.begin().await?;
    let (user, _) = manageable(&mut transaction, &actor, &id).await?;
    if user.id == actor.user.id {
        return Err(ApiError::bad_request("you can't delete your own account"));
    }
    transaction
        .execute(
            &Query::delete()
                .from_table(Users::Table)
                .and_where(Expr::col(Users::Id).eq(user.id.as_str()))
                .to_owned(),
        )
        .await?;
    ensure_administrator_remains(&mut transaction).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "user.delete",
        Target::user(&user.id),
        json!({ "email": user.email, "display_name": user.display_name }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/users/{id}/reset-two-factor`: removes the user's authenticator
/// app, passkeys and recovery codes and signs them out, for a user who lost
/// their device.
pub async fn reset_two_factor(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = state.database.begin().await?;
    let (user, _) = manageable(&mut transaction, &actor, &id).await?;
    not_self(&actor, &user)?;
    users::remove_two_factor(&mut transaction, &user.id).await?;
    users::end_sessions(&mut transaction, &user.id, None).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "user.two_factor_reset",
        Target::user(&user.id),
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct ResetLink {
    #[serde(flatten)]
    delivery: Delivery,
    expires_at: i64,
}

/// `POST /v1/users/{id}/password-reset`: makes a one-time link that sets the
/// user's password, emailed if email is set up and shown otherwise.
pub async fn password_reset(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<ResetLink>, ApiError> {
    let mut transaction = state.database.begin().await?;
    let (user, _) = manageable(&mut transaction, &actor, &id).await?;
    not_self(&actor, &user)?;
    if user.disabled {
        return Err(ApiError::conflict(
            "enable the user before resetting their password",
        ));
    }
    let (token, expires_at) = create_reset(
        &mut transaction,
        &user.id,
        Some(&actor.user.id),
        ADMIN_RESET_TTL_MS,
    )
    .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "user.password_reset_create",
        Target::user(&user.id),
        json!({}),
    )
    .await?;
    let settings = settings::load(&mut transaction).await?;
    transaction.commit().await?;
    let link = super::link(&state, "reset", &token);
    let (subject, body) =
        mail::password_reset_message(&settings.instance_name, &link, ADMIN_RESET_TTL_MS / HOUR_MS);
    let delivery = mail::deliver_link(&state, &settings, &user.email, &subject, body, link).await;
    Ok(Json(ResetLink {
        delivery,
        expires_at,
    }))
}

/// `POST /v1/users/{id}/sign-out`: ends every session the user has.
pub async fn sign_out(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = state.database.begin().await?;
    let (user, _) = manageable(&mut transaction, &actor, &id).await?;
    let ended = users::end_sessions(&mut transaction, &user.id, None).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "user.sign_out",
        Target::user(&user.id),
        json!({ "sessions": ended }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/users/{id}/unlink-sso`: forgets the user's SSO identity, so
/// their next SSO sign-in links the account again by email. For a user
/// recreated at the provider, who has a new identity there.
pub async fn unlink_sso(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = state.database.begin().await?;
    let (user, _) = manageable(&mut transaction, &actor, &id).await?;
    if user.oidc_subject.is_none() {
        return Err(ApiError::conflict("the user hasn't signed in with SSO"));
    }
    transaction
        .execute(
            &Query::update()
                .table(Users::Table)
                .value(Users::OidcSubject, Option::<String>::None)
                .and_where(Expr::col(Users::Id).eq(user.id.as_str()))
                .to_owned(),
        )
        .await?;
    transaction
        .execute(
            &Query::delete()
                .from_table(UserOidcGroups::Table)
                .and_where(Expr::col(UserOidcGroups::UserId).eq(user.id.as_str()))
                .to_owned(),
        )
        .await?;
    ensure_administrator_remains(&mut transaction).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "user.sso_unlink",
        Target::user(&user.id),
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
