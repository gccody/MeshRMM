//! Invitations: an administrator invites an email address with roles, and
//! the recipient accepts by choosing a name and password.
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{RoleRef, signed_in_response, users::grantable_roles};
use crate::{
    audit::{self, Actor, Target},
    auth::{
        Authorized, password,
        session::{self, AuthMethod, Client},
    },
    db::{
        self, Executor,
        tables::{InvitationRoles, Invitations},
    },
    http::{ApiError, AppState, JsonBody, client_ip::ClientIp},
    mail::{self, Delivery},
    rbac::{self, Permission},
    secrets::{new_token, token_hash},
    settings,
    time::{DAY_MS, now_ms},
    users::{self, NewUser, new_id},
};

pub const INVITATION_TTL_MS: i64 = 7 * DAY_MS;

#[derive(Debug, Serialize)]
pub struct InvitationView {
    id: String,
    email: String,
    roles: Vec<RoleRef>,
    created_by_user_id: Option<String>,
    created_at: i64,
    expires_at: i64,
    /// Renew it for a working link.
    expired: bool,
}

#[derive(sqlx::FromRow)]
struct InvitationRow {
    id: String,
    email: String,
    created_by_user_id: Option<String>,
    created_at: i64,
    expires_at: i64,
}

/// Invitations not yet accepted or revoked, expired or not. An expired one
/// can be renewed.
fn open() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            Invitations::Id,
            Invitations::Email,
            Invitations::CreatedByUserId,
            Invitations::CreatedAt,
            Invitations::ExpiresAt,
        ])
        .from(Invitations::Table)
        .and_where(Expr::col(Invitations::AcceptedAt).is_null())
        .and_where(Expr::col(Invitations::RevokedAt).is_null())
        .to_owned()
}

/// Invitations whose link works: open and unexpired.
fn pending() -> sea_query::SelectStatement {
    open()
        .and_where(Expr::col(Invitations::ExpiresAt).gt(now_ms()))
        .to_owned()
}

async fn role_ids(executor: &mut impl Executor, invitation_id: &str) -> db::Result<Vec<String>> {
    let rows: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .column(InvitationRoles::RoleId)
                .from(InvitationRoles::Table)
                .and_where(Expr::col(InvitationRoles::InvitationId).eq(invitation_id))
                .to_owned(),
        )
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

async fn view(executor: &mut impl Executor, row: InvitationRow) -> db::Result<InvitationView> {
    let ids = role_ids(executor, &row.id).await?;
    let roles = rbac::load_roles(executor, Some(&ids)).await?;
    Ok(InvitationView {
        id: row.id,
        email: row.email,
        roles: roles.iter().map(RoleRef::from).collect(),
        created_by_user_id: row.created_by_user_id,
        created_at: row.created_at,
        expired: row.expires_at <= now_ms(),
        expires_at: row.expires_at,
    })
}

/// `GET /v1/invitations`: invitations not yet accepted or revoked, newest
/// first.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Vec<InvitationView>>, ApiError> {
    actor.require(Permission::UsersManage)?;
    let mut database = &state.database;
    let rows: Vec<InvitationRow> = database
        .fetch_all(
            &open()
                .order_by(Invitations::CreatedAt, Order::Desc)
                .to_owned(),
        )
        .await?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        views.push(view(&mut database, row).await?);
    }
    Ok(Json(views))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvitationRequest {
    email: String,
    role_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CreatedInvitation {
    invitation: InvitationView,
    #[serde(flatten)]
    delivery: Delivery,
}

/// `POST /v1/invitations`: invites an email address with roles. The link is
/// emailed if email is set up, and returned to pass on otherwise.
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<InvitationRequest>,
) -> Result<(StatusCode, Json<CreatedInvitation>), ApiError> {
    actor.require(Permission::UsersManage)?;
    let email = users::normalize_email(&request.email)?;
    let mut transaction = state.database.begin().await?;
    let roles = grantable_roles(&mut transaction, &actor, &request.role_ids).await?;
    if users::by_email(&mut transaction, &email).await?.is_some() {
        return Err(ApiError::conflict("a user with this email already exists"));
    }
    let existing: Option<InvitationRow> = transaction
        .fetch_optional(
            &open()
                .and_where(Expr::col(Invitations::Email).eq(email.as_str()))
                .to_owned(),
        )
        .await?;
    if existing.is_some() {
        return Err(ApiError::conflict(
            "this email already has an invitation; renew or revoke it",
        ));
    }
    let id = new_id();
    let token = new_token();
    let now = now_ms();
    transaction
        .execute(
            &Query::insert()
                .into_table(Invitations::Table)
                .columns([
                    Invitations::Id,
                    Invitations::TokenHash,
                    Invitations::Email,
                    Invitations::CreatedByUserId,
                    Invitations::CreatedAt,
                    Invitations::ExpiresAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    token_hash(&token).into(),
                    email.as_str().into(),
                    actor.user.id.as_str().into(),
                    now.into(),
                    (now + INVITATION_TTL_MS).into(),
                ])
                .to_owned(),
        )
        .await?;
    if !roles.is_empty() {
        let mut insert = Query::insert();
        insert
            .into_table(InvitationRoles::Table)
            .columns([InvitationRoles::InvitationId, InvitationRoles::RoleId]);
        for role in &roles {
            insert.values_panic([id.as_str().into(), role.id.as_str().into()]);
        }
        transaction.execute(&insert).await?;
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "invitation.create",
        Target::invitation(&id),
        json!({
            "email": email,
            "role_ids": roles.iter().map(|role| role.id.as_str()).collect::<Vec<_>>(),
        }),
    )
    .await?;
    let settings = settings::load(&mut transaction).await?;
    transaction.commit().await?;
    let invitation = InvitationView {
        id,
        email: email.clone(),
        roles: roles.iter().map(RoleRef::from).collect(),
        created_by_user_id: Some(actor.user.id.clone()),
        created_at: now,
        expires_at: now + INVITATION_TTL_MS,
        expired: false,
    };
    let delivery = send(&state, &settings, &actor, &email, &token).await;
    Ok((
        StatusCode::CREATED,
        Json(CreatedInvitation {
            invitation,
            delivery,
        }),
    ))
}

async fn send(
    state: &AppState,
    settings: &settings::Settings,
    actor: &Authorized,
    email: &str,
    token: &str,
) -> Delivery {
    let link = super::link(state, "invite", token);
    let (subject, body) =
        mail::invitation_message(&settings.instance_name, &actor.user.email, &link);
    mail::deliver_link(state, settings, email, &subject, body, link).await
}

/// A pending invitation the actor may manage: its roles must be ones they
/// could grant, or renewing it would hand the link to more access.
async fn manageable(
    executor: &mut impl Executor,
    actor: &Authorized,
    id: &str,
) -> Result<InvitationRow, ApiError> {
    actor.require(Permission::UsersManage)?;
    let row: InvitationRow = executor
        .fetch_optional(
            &open()
                .and_where(Expr::col(Invitations::Id).eq(id))
                .to_owned(),
        )
        .await?
        .ok_or_else(|| ApiError::not_found("no such open invitation"))?;
    let ids = role_ids(executor, &row.id).await?;
    grantable_roles(executor, actor, &ids).await?;
    Ok(row)
}

/// `DELETE /v1/invitations/{id}`: revokes a pending invitation.
pub async fn revoke(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = state.database.begin().await?;
    let row = manageable(&mut transaction, &actor, &id).await?;
    transaction
        .execute(
            &Query::update()
                .table(Invitations::Table)
                .value(Invitations::RevokedAt, now_ms())
                .and_where(Expr::col(Invitations::Id).eq(row.id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "invitation.revoke",
        Target::invitation(&row.id),
        json!({ "email": row.email }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/invitations/{id}/renew`: replaces the link (the old one stops
/// working) and restarts the expiry, then sends or returns it again.
pub async fn renew(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<CreatedInvitation>, ApiError> {
    let mut transaction = state.database.begin().await?;
    let mut row = manageable(&mut transaction, &actor, &id).await?;
    let token = new_token();
    row.expires_at = now_ms() + INVITATION_TTL_MS;
    transaction
        .execute(
            &Query::update()
                .table(Invitations::Table)
                .values([
                    (Invitations::TokenHash, token_hash(&token).into()),
                    (Invitations::ExpiresAt, row.expires_at.into()),
                ])
                .and_where(Expr::col(Invitations::Id).eq(row.id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "invitation.renew",
        Target::invitation(&row.id),
        json!({ "email": row.email }),
    )
    .await?;
    let settings = settings::load(&mut transaction).await?;
    let email = row.email.clone();
    let invitation = view(&mut transaction, row).await?;
    transaction.commit().await?;
    let delivery = send(&state, &settings, &actor, &email, &token).await;
    Ok(Json(CreatedInvitation {
        invitation,
        delivery,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvitationToken {
    token: String,
}

#[derive(Debug, Serialize)]
pub struct InvitationDetails {
    email: String,
    instance_name: String,
    expires_at: i64,
}

fn invalid_link() -> ApiError {
    ApiError::not_found("this invitation link is invalid, used or expired")
        .with_code("invalid_token")
}

async fn find(executor: &mut impl Executor, token: &str) -> Result<InvitationRow, ApiError> {
    executor
        .fetch_optional(
            &pending()
                .and_where(Expr::col(Invitations::TokenHash).eq(token_hash(token)))
                .to_owned(),
        )
        .await?
        .ok_or_else(invalid_link)
}

/// `POST /v1/auth/invitation`: who an invitation link is for.
pub async fn lookup(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    JsonBody(request): JsonBody<InvitationToken>,
) -> Result<Json<InvitationDetails>, ApiError> {
    let mut database = &state.database;
    super::limit_token_attempts(&state, ip)?;
    let row = find(&mut database, &request.token).await?;
    let settings = settings::load(&mut database).await?;
    Ok(Json(InvitationDetails {
        email: row.email,
        instance_name: settings.instance_name,
        expires_at: row.expires_at,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acceptance {
    token: String,
    display_name: String,
    password: String,
}

/// `POST /v1/auth/invitation/accept`: creates the invited account and signs
/// it in.
pub async fn accept(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(request): JsonBody<Acceptance>,
) -> Result<Response, ApiError> {
    let display_name = users::validate_display_name(&request.display_name)?;
    let settings = settings::load(&mut &state.database).await?;
    password::check_policy(&request.password, settings.password_min_length)?;
    // The token is checked before the costly hash, so guessing is limited.
    super::limit_token_attempts(&state, ip)?;
    find(&mut &state.database, &request.token).await?;
    let hash = password::hash(&request.password).await?;
    let mut transaction = state.database.begin().await?;
    let row = find(&mut transaction, &request.token).await?;
    if users::by_email(&mut transaction, &row.email)
        .await?
        .is_some()
    {
        return Err(ApiError::conflict(
            "an account with this email already exists; sign in instead",
        ));
    }
    let now = now_ms();
    let claimed = transaction
        .execute(
            &Query::update()
                .table(Invitations::Table)
                .value(Invitations::AcceptedAt, now)
                .and_where(Expr::col(Invitations::Id).eq(row.id.as_str()))
                .and_where(Expr::col(Invitations::AcceptedAt).is_null())
                .and_where(Expr::col(Invitations::RevokedAt).is_null())
                .to_owned(),
        )
        .await?;
    if claimed != 1 {
        return Err(invalid_link());
    }
    let user_id = new_id();
    // Roles deleted since the invitation are simply not granted.
    let role_ids = role_ids(&mut transaction, &row.id).await?;
    users::insert(
        &mut transaction,
        NewUser {
            id: &user_id,
            email: &row.email,
            display_name: &display_name,
            password_hash: Some(&hash),
            role_ids: &role_ids,
            now_ms: now,
        },
    )
    .await?;
    audit::record(
        &mut transaction,
        &Actor::user(&user_id, &row.email, Some(ip)),
        "invitation.accept",
        Target::invitation(&row.id),
        json!({ "user_id": user_id }),
    )
    .await?;
    let cookie = session::start(
        &mut transaction,
        &user_id,
        AuthMethod::Password,
        false,
        &Client::new(ip, &headers),
    )
    .await?;
    transaction.commit().await?;
    Ok(signed_in_response(
        StatusCode::CREATED,
        cookie,
        settings.require_two_factor,
    ))
}
