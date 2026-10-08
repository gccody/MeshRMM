//! Remote sessions. The viewer redeems the website's handoff for a session,
//! signals through it, resumes and ends it, and uses its technician's
//! toolbox with the session's token. The website can close a device's
//! session.
use axum::{
    Json,
    extract::{Path, Query as QueryParameters, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use meshrmm_protocol_types::{
    FileDelivery, FileDeliveryDestination, ScriptRun, SessionBootstrap, StartFileDelivery,
    StartScriptRun, ToolboxFile, ToolboxListing, ToolboxScript,
};
use sea_query::{Expr, ExprTrait, Query};
use serde::Deserialize;
use serde_json::{Value, json};

use super::require_any;
use crate::{
    agents::{self, bearer_token, parse_id},
    audit::{self, Actor, Target},
    auth::Authorized,
    db::tables::{FileDeliveries, RemoteHandoffs, ScriptRuns},
    http::{ApiError, AppState, JsonBody, client_ip::ClientIp},
    rbac::Permission,
    realtime::sessions::{
        self, MAX_SIGNAL_BYTES, NewSession, Refusal, Role, SessionHandle, ended, user_permissions,
    },
    secrets::token_hash,
    time::now_ms,
    toolbox::{self, DeliveryRow, RunRow, Source, ToolboxUser},
    users,
};

fn invalid_handoff() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "the remote handoff is invalid, expired, or already used",
    )
    .with_code("handoff_invalid")
}

fn unauthenticated() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "remote session authentication failed",
    )
    .with_code("session_unauthenticated")
}

#[derive(sqlx::FromRow)]
struct HandoffRow {
    device_id: String,
    user_id: String,
    start_in_background: bool,
    reason: String,
}

/// `POST /v1/remote/handoffs/redeem`: the viewer trades the website's
/// one-time handoff token for a session with the device.
pub async fn redeem(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
) -> Result<Json<SessionBootstrap>, ApiError> {
    let hash = token_hash(bearer_token(&headers).ok_or_else(invalid_handoff)?);
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    let handoff: HandoffRow = transaction
        .fetch_optional(
            &Query::select()
                .columns([
                    RemoteHandoffs::DeviceId,
                    RemoteHandoffs::UserId,
                    RemoteHandoffs::StartInBackground,
                    RemoteHandoffs::Reason,
                ])
                .from(RemoteHandoffs::Table)
                .and_where(Expr::col(RemoteHandoffs::TokenHash).eq(hash.as_str()))
                .and_where(Expr::col(RemoteHandoffs::UsedAt).is_null())
                .and_where(Expr::col(RemoteHandoffs::ExpiresAt).gt(now))
                .to_owned(),
        )
        .await?
        .ok_or_else(invalid_handoff)?;
    let claimed = transaction
        .execute(
            &Query::update()
                .table(RemoteHandoffs::Table)
                .value(RemoteHandoffs::UsedAt, now)
                .and_where(Expr::col(RemoteHandoffs::TokenHash).eq(hash.as_str()))
                .and_where(Expr::col(RemoteHandoffs::UsedAt).is_null())
                .to_owned(),
        )
        .await?;
    if claimed == 0 {
        return Err(invalid_handoff());
    }
    transaction.commit().await?;
    let mut database = &state.database;
    if !agents::is_active(&mut database, &handoff.device_id).await? {
        return Err(ApiError::not_found("Device not found"));
    }
    let user = users::by_id(&mut database, &handoff.user_id)
        .await?
        .filter(|user| !user.disabled)
        .ok_or_else(|| {
            ApiError::forbidden("the technician's account is disabled").with_code("user_disabled")
        })?;
    // The website checked when it made the handoff; roles may have changed
    // since.
    let permissions = user_permissions(&mut database, &user.id).await?;
    let mut required = vec![Permission::SessionsConnect];
    if handoff.start_in_background {
        required.push(Permission::SessionsConnectBackground);
    }
    if let Some(missing) = required
        .into_iter()
        .find(|permission| !permissions.contains(permission))
    {
        return Err(
            ApiError::forbidden(format!("you don't have the {missing} permission"))
                .with_code("permission_denied"),
        );
    }
    let bootstrap = state
        .sessions
        .create(
            &state,
            NewSession {
                device_id: &handoff.device_id,
                user: &user,
                start_in_background: handoff.start_in_background,
                reason: &handoff.reason,
                actor: Actor::user(&user.id, &user.email, Some(ip)),
            },
        )
        .await?;
    Ok(Json(bootstrap))
}

/// The session's actor, for a viewer or Agent presenting its token.
async fn session<'a>(
    state: &AppState,
    session_id: &str,
    headers: &'a HeaderMap,
) -> Result<(SessionHandle, &'a str), ApiError> {
    let session_id = parse_id(session_id, "session")?;
    let token = bearer_token(headers).ok_or_else(unauthenticated)?;
    let handle = state
        .sessions
        .get(state, &session_id)
        .await?
        .ok_or_else(ended)?;
    Ok((handle, token))
}

#[derive(Debug, Deserialize)]
pub struct SignalQuery {
    role: String,
}

/// `GET /v1/remote/sessions/{id}/signal?role=client|agent`: the viewer's or
/// Agent's signaling socket.
pub async fn signal(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    QueryParameters(query): QueryParameters<SignalQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let role =
        Role::parse(&query.role).ok_or_else(|| ApiError::bad_request("invalid peer role"))?;
    let (handle, token) = session(&state, &session_id, &headers).await?;
    match handle.check(role, token).await {
        Ok(()) => {}
        Err(Refusal::Unauthorized) => return Err(unauthenticated()),
        Err(Refusal::Gone) => return Err(ended()),
    }
    let token = token.to_owned();
    Ok(upgrade
        .max_message_size(MAX_SIGNAL_BYTES * 2)
        .on_upgrade(move |socket| sessions::serve_peer(socket, handle, role, token)))
}

/// `POST /v1/remote/sessions/{id}/resume`: the viewer reconnects to its
/// session, and gets new ICE servers and a new deadline.
pub async fn resume(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionBootstrap>, ApiError> {
    let (handle, token) = session(&state, &session_id, &headers).await?;
    Ok(Json(handle.resume(token).await?))
}

/// `POST /v1/remote/sessions/{id}/end`: the viewer ends its session. Ending
/// one that already ended succeeds.
pub async fn end(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    match session(&state, &session_id, &headers).await {
        Ok((handle, token)) => handle.end(token).await?,
        Err(error) if error.status() == StatusCode::GONE => {}
        Err(error) => return Err(error),
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/agents/{id}/close-session`: ends the device's session from the
/// website. Anyone may close their own session; closing someone else's
/// needs `sessions.close_any`.
pub async fn close(
    State(state): State<AppState>,
    actor: Authorized,
    Path(device_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_any(
        &actor,
        &[Permission::SessionsConnect, Permission::SessionsCloseAny],
    )?;
    let device_id = parse_id(&device_id, "device")?;
    if !agents::is_active(&mut &state.database, &device_id).await? {
        return Err(ApiError::not_found("Device not found"));
    }
    let Some(session) = state.sessions.for_device(&state, &device_id).await? else {
        return Ok(Json(json!({ "closed": false })));
    };
    if session.user_id != actor.user.id {
        actor.require(Permission::SessionsCloseAny)?;
    }
    session
        .handle
        .expire("the session was closed from the website")
        .await;
    audit::record(
        &mut &state.database,
        &actor.actor(),
        "remote.session_close",
        Target::device(&device_id),
        json!({ "session_id": session.session_id, "user_id": session.user_id }),
    )
    .await?;
    Ok(Json(json!({ "closed": true })))
}

/// The technician of a live session, and its device, from the viewer's
/// session token. They use the toolbox with their current permissions.
async fn session_user(
    state: &AppState,
    session_id: &str,
    headers: &HeaderMap,
    ip: std::net::IpAddr,
) -> Result<(ToolboxUser, String), ApiError> {
    let (handle, token) = session(state, session_id, headers).await?;
    let identity = handle.identity(token).await?;
    let mut database = &state.database;
    let user = users::by_id(&mut database, &identity.user_id)
        .await?
        .filter(|user| !user.disabled)
        .ok_or_else(ended)?;
    let permissions = user_permissions(&mut database, &user.id).await?;
    Ok((
        ToolboxUser {
            actor: Actor::user(&user.id, &user.email, Some(ip)),
            user_id: user.id,
            permissions,
        },
        identity.device_id,
    ))
}

/// `GET /v1/remote/sessions/{id}/toolbox`: the scripts and files the
/// session's technician may use.
pub async fn session_toolbox(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ToolboxListing>, ApiError> {
    let (user, _) = session_user(&state, &session_id, &headers, ip).await?;
    let mut database = &state.database;
    let scripts = toolbox::visible_scripts(&mut database, &user)
        .await?
        .into_iter()
        .map(|row| {
            Ok(ToolboxScript {
                language: row.language()?,
                id: row.id,
                name: row.name,
                folder: row.folder,
                description: row.description,
                shared: row.shared,
            })
        })
        .collect::<Result<_, ApiError>>()?;
    let files = toolbox::visible_files(&mut database, &user)
        .await?
        .into_iter()
        .map(|row| ToolboxFile {
            size_bytes: row.size_bytes(),
            id: row.id,
            name: row.name,
            folder: row.folder,
            shared: row.shared,
        })
        .collect();
    Ok(Json(ToolboxListing { scripts, files }))
}

/// `POST /v1/remote/sessions/{id}/script-runs`: runs one of the technician's
/// scripts on the session's device.
pub async fn session_run_script(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    JsonBody(start): JsonBody<StartScriptRun>,
) -> Result<(StatusCode, Json<ScriptRun>), ApiError> {
    let (user, device_id) = session_user(&state, &session_id, &headers, ip).await?;
    let script_id = parse_id(&start.script_id, "script")?;
    let run = toolbox::start_script_run(
        &state,
        &user,
        &device_id,
        &script_id,
        start.run_as,
        Source::Session,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(run)))
}

/// `GET /v1/remote/sessions/{id}/script-runs/{run_id}`: a run the
/// technician started on the session's device, with its output.
pub async fn session_script_run(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Path((session_id, run_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ScriptRun>, ApiError> {
    let (user, device_id) = session_user(&state, &session_id, &headers, ip).await?;
    let run_id = parse_id(&run_id, "run")?;
    let row: RunRow = state
        .database
        .fetch_optional(
            &toolbox::run_select(true)
                .and_where(Expr::col(ScriptRuns::Id).eq(run_id.as_str()))
                .and_where(Expr::col(ScriptRuns::DeviceId).eq(device_id.as_str()))
                .and_where(Expr::col(ScriptRuns::RequestedByUserId).eq(user.user_id.as_str()))
                .to_owned(),
        )
        .await?
        .ok_or_else(|| ApiError::not_found("Run not found"))?;
    Ok(Json(row.into_run(now_ms())?))
}

/// `POST /v1/remote/sessions/{id}/file-deliveries`: sends one of the
/// technician's library files to the session's device.
pub async fn session_deliver_file(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    JsonBody(start): JsonBody<StartFileDelivery>,
) -> Result<(StatusCode, Json<FileDelivery>), ApiError> {
    let (user, device_id) = session_user(&state, &session_id, &headers, ip).await?;
    let file_id = parse_id(&start.file_id, "file")?;
    let destination = if start.background {
        FileDeliveryDestination::Public
    } else {
        FileDeliveryDestination::User
    };
    let delivery = toolbox::start_file_delivery(
        &state,
        &user,
        &device_id,
        &file_id,
        destination,
        Source::Session,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(delivery)))
}

/// `GET /v1/remote/sessions/{id}/file-deliveries/{delivery_id}`: a delivery
/// the technician started on the session's device.
pub async fn session_file_delivery(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Path((session_id, delivery_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<FileDelivery>, ApiError> {
    let (user, device_id) = session_user(&state, &session_id, &headers, ip).await?;
    let delivery_id = parse_id(&delivery_id, "delivery")?;
    let row: DeliveryRow = state
        .database
        .fetch_optional(
            &toolbox::delivery_select()
                .and_where(Expr::col(FileDeliveries::Id).eq(delivery_id.as_str()))
                .and_where(Expr::col(FileDeliveries::DeviceId).eq(device_id.as_str()))
                .and_where(Expr::col(FileDeliveries::RequestedByUserId).eq(user.user_id.as_str()))
                .to_owned(),
        )
        .await?
        .ok_or_else(|| ApiError::not_found("Delivery not found"))?;
    Ok(Json(row.into_delivery(now_ms())?))
}
