//! Remote terminals: the website asks for one, then opens its socket, and
//! the Agent connects the device's side. See [`crate::terminals`].
use axum::{
    Json,
    extract::{Path, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use meshrmm_protocol_types::{
    MAX_TERMINAL_MESSAGE_BYTES, RunAs, TerminalShell, render_connection_notification,
    valid_terminal_size,
};
use serde::{Deserialize, Serialize};

use crate::{
    agents::{self, parse_id},
    auth::{Authorized, SignedIn, session},
    http::{ApiError, AppState, JsonBody, client_ip::ClientIp},
    settings,
    terminals::{
        MAX_TERMINALS_PER_DEVICE, MAX_TERMINALS_PER_USER, Refusal, Requested, permission, relay,
    },
};

/// Room around the largest terminal message for the frame itself.
const MAX_SOCKET_MESSAGE_BYTES: usize = MAX_TERMINAL_MESSAGE_BYTES + 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenTerminal {
    shell: TerminalShell,
    run_as: RunAs,
    columns: u16,
    rows: u16,
}

#[derive(Debug, Serialize)]
pub struct TerminalView {
    id: String,
    device_id: String,
    shell: TerminalShell,
    run_as: RunAs,
}

/// `POST /v1/agents/{id}/terminals`: reserves a terminal on an online
/// device. The browser opens its socket next.
pub async fn open(
    State(state): State<AppState>,
    actor: Authorized,
    Path(device_id): Path<String>,
    JsonBody(request): JsonBody<OpenTerminal>,
) -> Result<(StatusCode, Json<TerminalView>), ApiError> {
    actor.require(permission(request.run_as))?;
    let device_id = parse_id(&device_id, "device")?;
    let platform = agents::active_platform(&mut &state.database, &device_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Device not found"))?;
    if !request.shell.runs_on(platform) {
        return Err(ApiError::bad_request(format!(
            "{} isn't a shell {} devices have",
            request.shell.label(),
            platform.label()
        ))
        .with_code("shell_unsupported"));
    }
    if !valid_terminal_size(request.columns, request.rows) {
        return Err(
            ApiError::bad_request("a terminal is 1 to 1000 columns by 1 to 500 rows")
                .with_code("invalid_size"),
        );
    }
    if !state.agents.is_connected(&device_id) {
        return Err(ApiError::conflict("the device is offline").with_code("device_offline"));
    }
    // A terminal as the signed-in user runs in their session, like a session
    // on their desktop; one as SYSTEM or root is as invisible to them as
    // background mode. Each follows its notification policy.
    let settings = settings::load(&mut &state.database).await?;
    let notify = match request.run_as {
        RunAs::User => settings.connection_notification,
        RunAs::System => settings.background_connection_notification,
    };
    let notification = notify.then(|| {
        render_connection_notification(
            &settings.connection_notification_message,
            &actor.user.display_name,
        )
    });
    let id = uuid::Uuid::new_v4().to_string();
    let reserved = state.terminals.reserve(Requested {
        id: id.clone(),
        device_id: device_id.clone(),
        user_id: actor.user.id.clone(),
        actor: actor.actor(),
        shell: request.shell,
        run_as: request.run_as,
        columns: request.columns,
        rows: request.rows,
        notification,
    });
    match reserved {
        Ok(()) => Ok((
            StatusCode::CREATED,
            Json(TerminalView {
                id,
                device_id,
                shell: request.shell,
                run_as: request.run_as,
            }),
        )),
        Err(Refusal::DeviceLimit) => Err(ApiError::conflict(format!(
            "the device already has {MAX_TERMINALS_PER_DEVICE} terminals open; close one first"
        ))
        .with_code("too_many_terminals")),
        Err(Refusal::UserLimit) => Err(ApiError::conflict(format!(
            "you already have {MAX_TERMINALS_PER_USER} terminals open; close one first"
        ))
        .with_code("too_many_terminals")),
    }
}

/// `GET /v1/terminals/{id}`: the browser's socket for a terminal its user
/// asked for. See [`relay`] for what it carries and how it closes.
pub async fn connect(
    State(state): State<AppState>,
    signed_in: SignedIn,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    // A website session only: the session cookie keeps the socket's
    // access checked while it stays open.
    let actor = Authorized::new(signed_in)?;
    // Browsers send any site's cookies with a WebSocket handshake, so only
    // the website's own pages may open one.
    let origin = state.config.public_origin();
    if headers
        .get(header::ORIGIN)
        .is_none_or(|value| value.as_bytes() != origin.as_bytes())
    {
        return Err(
            ApiError::forbidden("this socket must be opened by the MeshRMM website")
                .with_code("cross_site_request"),
        );
    }
    let id = parse_id(&id, "terminal")?;
    let token = session::token(&headers).expect("an authorized request has a session cookie");
    let (claim, requested, agent) = state
        .terminals
        .claim_browser(&id, &actor.user.id)
        .ok_or_else(|| {
            ApiError::not_found("the terminal has ended or is open elsewhere")
                .with_code("terminal_not_found")
        })?;
    // Dropping the claim forgets the terminal.
    actor.require(permission(requested.run_as))?;
    Ok(upgrade
        .max_message_size(MAX_SOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| relay::serve(state, claim, requested, agent, token, ip, socket)))
}

/// `GET /v1/agents/{id}/terminals/{terminal_id}`: the Agent's socket for a
/// terminal the server asked it to open.
pub async fn agent_connect(
    State(state): State<AppState>,
    Path((device_id, terminal_id)): Path<(String, String)>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let agent = agents::authenticate(&state, &headers, &device_id).await?;
    let terminal_id = parse_id(&terminal_id, "terminal")?;
    let waiting = (!agent.deletion_requested)
        .then(|| state.terminals.claim_agent(&terminal_id, &agent.device_id))
        .flatten()
        .ok_or_else(|| {
            ApiError::not_found("the terminal has ended").with_code("terminal_not_found")
        })?;
    Ok(upgrade
        .max_message_size(MAX_SOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            // The browser's task may have given up waiting; dropping the
            // socket tells the Agent.
            let _ = waiting.send(socket);
        }))
}
