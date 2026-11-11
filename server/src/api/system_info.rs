//! Devices' hardware and software: each device's latest snapshot, its
//! applications and their changes, asking an Agent for a new snapshot, and
//! searching devices and the fleet's applications. Seeing any of it needs
//! `devices.view`, and so does asking for a refresh, which only makes the
//! Agent collect again.
use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
};
use meshrmm_protocol_types::AgentCommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    agents::parse_id,
    auth::Authorized,
    http::{ApiError, AppState},
    rbac::Permission,
    system_info::{
        self, ChangeRecord, Snapshot,
        changes::StoredApplication,
        search::{
            self, CompanyFilter, DeviceFilter, DevicePage, InstalledPage, Scope, SoftwarePage,
        },
    },
};

/// The most application changes one request returns.
const MAX_CHANGES: u64 = 1000;
const DEFAULT_CHANGES: u64 = 200;
/// The most applications one page of the fleet search returns.
const MAX_SOFTWARE: u64 = 500;
const DEFAULT_SOFTWARE: u64 = 200;
/// The most devices one page returns: a device search's snapshots are
/// several KiB each.
const MAX_DEVICES: u64 = 200;
const DEFAULT_DEVICES: u64 = 100;

/// The active device named by a path, if the actor may see it.
async fn device(state: &AppState, actor: &Authorized, id: &str) -> Result<String, ApiError> {
    let id = parse_id(id, "device")?;
    actor
        .require_device(&mut &state.database, Permission::DevicesView, &id)
        .await?;
    Ok(id)
}

/// A search text: trimmed, `None` when empty, and refused when too long.
pub(super) fn search_text(text: Option<String>, name: &str) -> Result<Option<String>, ApiError> {
    let text = text
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    if text
        .as_ref()
        .is_some_and(|text| text.chars().count() > search::MAX_SEARCH_CHARS)
    {
        return Err(ApiError::bad_request(format!(
            "{name} can be at most {} characters",
            search::MAX_SEARCH_CHARS
        )));
    }
    Ok(text)
}

/// The devices in `company` (`none` for no company, or a company's ID)
/// with the tag `tag`, of those the actor may see.
fn scope(
    actor: &Authorized,
    company: Option<String>,
    tag: Option<String>,
) -> Result<Scope, ApiError> {
    Ok(Scope {
        within: actor.companies(Permission::DevicesView),
        company: company_filter(company)?,
        tag: tag
            .as_deref()
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(|tag| parse_id(tag, "tag"))
            .transpose()?,
    })
}

/// `none` for devices in no company, or a company's ID.
fn company_filter(company: Option<String>) -> Result<Option<CompanyFilter>, ApiError> {
    match company.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some("none") => Ok(Some(CompanyFilter::None)),
        Some(id) => Ok(Some(CompanyFilter::Company(parse_id(id, "company")?))),
    }
}

fn parameters<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(parameters)| parameters)
        .map_err(|rejection| ApiError::bad_request(rejection.body_text()))
}

/// `GET /v1/agents/{id}/system-info`: the device's latest snapshot, without
/// its applications. 404 with `not_collected` until the device sent one.
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Snapshot>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let id = device(&state, &actor, &id).await?;
    system_info::load(&state.database, &id)
        .await?
        .map(Json)
        .ok_or_else(|| {
            ApiError::not_found("the device hasn't sent its system information yet")
                .with_code("not_collected")
        })
}

/// `POST /v1/agents/{id}/system-info/refresh`: asks the online Agent for a
/// new snapshot. It arrives within seconds, and the device list's summary
/// of the device changes when it does.
pub async fn refresh(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    actor.require(Permission::DevicesView)?;
    let id = device(&state, &actor, &id).await?;
    state
        .agents
        .send(&id, AgentCommand::CollectSystemInfo)
        .map_err(|unsent| {
            unsent.answer(
                ApiError::conflict(
                    "the device is offline; it sends its system information when it next connects, if what the server has is old",
                )
                .with_code("device_offline"),
            )
        })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "device_id": id, "status": "requested" })),
    ))
}

#[derive(Debug, Serialize)]
pub struct ApplicationList {
    applications: Vec<StoredApplication>,
}

/// `GET /v1/agents/{id}/applications`: the applications in the device's
/// latest snapshot, by name.
pub async fn applications(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<ApplicationList>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let id = device(&state, &actor, &id).await?;
    Ok(Json(ApplicationList {
        applications: system_info::applications(&state.database, &id).await?,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeParameters {
    limit: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ChangeList {
    changes: Vec<ChangeRecord>,
}

/// `GET /v1/agents/{id}/application-changes?limit=`: what was installed,
/// removed and updated on the device, newest first.
pub async fn changes(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    query: Result<Query<ChangeParameters>, QueryRejection>,
) -> Result<Json<ChangeList>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let parameters = parameters(query)?;
    let id = device(&state, &actor, &id).await?;
    let limit = parameters
        .limit
        .unwrap_or(DEFAULT_CHANGES)
        .clamp(1, MAX_CHANGES);
    Ok(Json(ChangeList {
        changes: system_info::recent_changes(&state.database, &id, limit).await?,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceParameters {
    q: Option<String>,
    os: Option<String>,
    model: Option<String>,
    serial: Option<String>,
    ip: Option<String>,
    application: Option<String>,
    company: Option<String>,
    tag: Option<String>,
    /// 0 for only the count.
    limit: Option<u64>,
    offset: Option<u64>,
}

/// `GET /v1/system-info?q=&os=&model=&serial=&ip=&application=&company=&tag=&limit=&offset=`:
/// a page of the snapshots of the enrolled devices that match every given
/// filter, by device name, and how many devices match. Each text matches
/// anywhere, ignoring case; `q` matches the name, operating system, model,
/// serial number, IP addresses or host name.
pub async fn search_devices(
    State(state): State<AppState>,
    actor: Authorized,
    query: Result<Query<DeviceParameters>, QueryRejection>,
) -> Result<Json<DevicePage>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let parameters = parameters(query)?;
    let filter = DeviceFilter {
        query: search_text(parameters.q, "q")?,
        os: search_text(parameters.os, "os")?,
        model: search_text(parameters.model, "model")?,
        serial_number: search_text(parameters.serial, "serial")?,
        ip: search_text(parameters.ip, "ip")?,
        application: search_text(parameters.application, "application")?,
        scope: scope(&actor, parameters.company, parameters.tag)?,
    };
    let limit = parameters.limit.unwrap_or(DEFAULT_DEVICES).min(MAX_DEVICES);
    Ok(Json(
        search::devices(
            &state.database,
            &filter,
            limit,
            parameters.offset.unwrap_or(0),
        )
        .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoftwareParameters {
    q: Option<String>,
    company: Option<String>,
    tag: Option<String>,
    limit: Option<u64>,
    offset: Option<u64>,
}

/// `GET /v1/software?q=&company=&tag=&limit=&offset=`: a page of the
/// applications on enrolled devices whose name contains `q`, by name, with
/// how many devices have each version; without `q`, those on the most
/// devices. Says how many matched, and `truncated` when more follow.
pub async fn software(
    State(state): State<AppState>,
    actor: Authorized,
    query: Result<Query<SoftwareParameters>, QueryRejection>,
) -> Result<Json<SoftwarePage>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let parameters = parameters(query)?;
    let query = search_text(parameters.q, "q")?;
    let scope = scope(&actor, parameters.company, parameters.tag)?;
    let limit = parameters
        .limit
        .unwrap_or(DEFAULT_SOFTWARE)
        .clamp(1, MAX_SOFTWARE);
    Ok(Json(
        search::software(
            &state.database,
            query.as_deref(),
            &scope,
            limit,
            parameters.offset.unwrap_or(0),
        )
        .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledParameters {
    name: String,
    version: Option<String>,
    /// Only devices whose copy has no version.
    #[serde(default)]
    unversioned: bool,
    company: Option<String>,
    tag: Option<String>,
    /// 0 for only the count.
    limit: Option<u64>,
    offset: Option<u64>,
}

/// `GET /v1/software/devices?name=&version=&unversioned=&company=&tag=&limit=&offset=`:
/// a page of the copies of the application named exactly `name` on enrolled
/// devices, in `version` if given, by device name, and how many there are.
pub async fn software_devices(
    State(state): State<AppState>,
    actor: Authorized,
    query: Result<Query<InstalledParameters>, QueryRejection>,
) -> Result<Json<InstalledPage>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let parameters = parameters(query)?;
    if parameters.name.is_empty() || parameters.name.chars().count() > 256 {
        return Err(ApiError::bad_request("name must be an application's name"));
    }
    let version = match (parameters.version.as_deref(), parameters.unversioned) {
        (Some(_), true) => {
            return Err(ApiError::bad_request(
                "give either version or unversioned, not both",
            ));
        }
        (Some(version), false) => Some(Some(version)),
        (None, true) => Some(None),
        (None, false) => None,
    };
    let scope = scope(&actor, parameters.company, parameters.tag)?;
    let limit = parameters.limit.unwrap_or(DEFAULT_DEVICES).min(MAX_DEVICES);
    Ok(Json(
        search::installed_on(
            &state.database,
            &parameters.name,
            version,
            &scope,
            limit,
            parameters.offset.unwrap_or(0),
        )
        .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_texts_are_trimmed_and_bounded() {
        assert_eq!(
            search_text(Some("  ubuntu ".into()), "q")
                .unwrap()
                .as_deref(),
            Some("ubuntu")
        );
        assert_eq!(search_text(Some("   ".into()), "q").unwrap(), None);
        assert_eq!(search_text(None, "q").unwrap(), None);
        assert!(search_text(Some("x".repeat(search::MAX_SEARCH_CHARS + 1)), "q").is_err());
    }

    #[test]
    fn company_filters_are_none_or_an_id() {
        assert_eq!(company_filter(None).unwrap(), None);
        assert_eq!(
            company_filter(Some("none".into())).unwrap(),
            Some(CompanyFilter::None)
        );
        let id = "0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a";
        assert_eq!(
            company_filter(Some(id.into())).unwrap(),
            Some(CompanyFilter::Company(id.into()))
        );
        assert!(company_filter(Some("acme".into())).is_err());
    }
}
