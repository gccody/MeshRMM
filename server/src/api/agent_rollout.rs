//! The Agent's update check, and the staged rollout of new Agent releases
//! that decides its answer. See [`crate::agent_rollout`].
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use meshrmm_protocol_types::is_release_version;
use sea_query::SimpleExpr;
use serde::{Deserialize, Serialize};

use super::{agent::read_body, settings::save};
use crate::{
    agent_rollout::{self, EVERYONE, Progress, Rollout, STAGED},
    agents,
    auth::Authorized,
    db::tables::Settings as SettingsTable,
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
};

/// An update check is a version and little else.
const MAX_CHECK_BYTES: usize = 4 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateCheck {
    /// The Agent's own version.
    version: String,
}

/// `POST /v1/agents/{id}/update-check`: the update manifest for this
/// device, which offers the Agent release the server ships when the rollout
/// offers it to the device, or when the device has it already, and nothing
/// otherwise. A deleted device is offered nothing, since it is uninstalling.
/// The release entries are the public manifest's, release signatures and
/// all, so the Agent checks them as before.
pub async fn check(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let agent = agents::authenticate(&state, &headers, &id).await?;
    let body = read_body(
        &headers,
        body,
        MAX_CHECK_BYTES,
        "the update check is too large",
        state.agent_timeouts.upload_stall,
    )
    .await?;
    let check: UpdateCheck = serde_json::from_slice(&body)
        .map_err(|error| ApiError::bad_request(format!("invalid update check: {error}")))?;
    if !is_release_version(&check.version) || semver::Version::parse(&check.version).is_err() {
        return Err(ApiError::bad_request("invalid Agent version"));
    }
    // Without downloads there is nothing to offer, as the public manifest
    // answers then.
    let (Some(release), Some(manifest)) = (state.downloads.version(), state.downloads.manifest())
    else {
        return Err(ApiError::not_found("no such download"));
    };
    if agent.deletion_requested {
        return Ok(manifest_response(agent_rollout::empty_manifest()));
    }
    agent_rollout::record_version(&mut &state.database, &agent.device_id, &check.version).await?;
    let offered = agent_rollout::has_release(Some(&check.version), release)
        || Rollout::from_settings(&*state.settings.get(&state.database).await?)
            .offers(agent_rollout::bucket(&agent.device_id));
    Ok(manifest_response(if offered {
        manifest
    } else {
        agent_rollout::empty_manifest()
    }))
}

fn manifest_response(manifest: Vec<u8>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        Body::from(manifest),
    )
        .into_response()
}

/// The rollout as the website shows it.
#[derive(Debug, Serialize)]
pub struct RolloutView {
    /// The Agent release the server ships; `None` without downloads.
    release: Option<String>,
    /// `"everyone"` or `"staged"`.
    mode: &'static str,
    /// Where each new release starts in staged mode.
    first_percent: u8,
    /// The stage this release is at in staged mode.
    percent: u8,
    paused: bool,
    /// The share of devices without the release that are offered it now:
    /// 100 for everyone, 0 while paused.
    offered_percent: u8,
    /// Where the enrolled devices stand; `None` without a release.
    devices: Option<Progress>,
    updated_at: i64,
}

async fn view(state: &AppState) -> Result<RolloutView, ApiError> {
    let settings = state.settings.get(&state.database).await?;
    let rollout = Rollout::from_settings(&settings);
    let release = state.downloads.version().map(str::to_owned);
    let devices = match &release {
        Some(release) => {
            Some(agent_rollout::progress(&mut &state.database, release, &rollout).await?)
        }
        None => None,
    };
    Ok(RolloutView {
        release,
        mode: if rollout.staged { STAGED } else { EVERYONE },
        first_percent: rollout.first_percent,
        percent: rollout.percent,
        paused: rollout.paused,
        offered_percent: rollout.offered_percent(),
        devices,
        updated_at: settings.updated_at,
    })
}

/// `GET /v1/settings/agent-rollout`
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<RolloutView>, ApiError> {
    actor.require(Permission::SettingsManage)?;
    Ok(Json(view(&state).await?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Everyone,
    Staged,
}

/// Every field is optional; a missing one is left as it is.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RolloutUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    first_percent: Option<u8>,
    /// Staged mode only. Lower than now stops offering the release to the
    /// devices above it; those that have it keep it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    percent: Option<u8>,
    /// Staged mode only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    paused: Option<bool>,
}

fn valid_percent(percent: Option<u8>) -> Result<(), ApiError> {
    match percent {
        Some(percent) if !(1..=100).contains(&percent) => Err(ApiError::bad_request(
            "a rollout stage is 1 to 100 percent of devices",
        )),
        _ => Ok(()),
    }
}

/// `PATCH /v1/settings/agent-rollout`. Switching to staged mode starts the
/// release the server ships at the first-stage percentage, not paused,
/// unless the request says otherwise.
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<RolloutUpdate>,
) -> Result<Json<RolloutView>, ApiError> {
    actor.require(Permission::SettingsManage)?;
    valid_percent(request.first_percent)?;
    valid_percent(request.percent)?;
    let current = Rollout::from_settings(&*state.settings.get(&state.database).await?);
    let staged = request
        .mode
        .map_or(current.staged, |mode| mode == Mode::Staged);
    if !staged && (request.percent.is_some() || request.paused.is_some()) {
        return Err(ApiError::bad_request(
            "the stage and pausing apply in staged mode only",
        ));
    }
    let mut values: Vec<(SettingsTable, SimpleExpr)> = Vec::new();
    let mut percent = request.percent;
    let mut paused = request.paused;
    if let Some(mode) = request.mode {
        values.push((
            SettingsTable::AgentRolloutMode,
            match mode {
                Mode::Everyone => EVERYONE,
                Mode::Staged => STAGED,
            }
            .into(),
        ));
        if mode == Mode::Staged && !current.staged {
            percent = percent.or(Some(request.first_percent.unwrap_or(current.first_percent)));
            paused = paused.or(Some(false));
            if let Some(release) = state.downloads.version() {
                values.push((SettingsTable::AgentRolloutVersion, release.into()));
            }
        }
    }
    if let Some(first) = request.first_percent {
        values.push((
            SettingsTable::AgentRolloutFirstPercent,
            i64::from(first).into(),
        ));
    }
    if let Some(percent) = percent {
        values.push((
            SettingsTable::AgentRolloutPercent,
            i64::from(percent).into(),
        ));
    }
    if let Some(paused) = paused {
        values.push((SettingsTable::AgentRolloutPaused, paused.into()));
    }
    let metadata = serde_json::to_value(&request).unwrap_or_default();
    save(
        &state,
        &actor,
        "settings.agent_rollout_update",
        values,
        metadata,
    )
    .await?;
    Ok(Json(view(&state).await?))
}
