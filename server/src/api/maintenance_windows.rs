//! Maintenance windows: putting devices in maintenance, one or many, ending
//! it, and listing the windows. See docs/maintenance-windows.md.
//!
//! Every route checks which devices the caller may act on in one place:
//! [`devices_for`] for a target, [`window_for`] for one window, and
//! [`visible`] for a list, so per-company access applies to all of them
//! there.
use axum::{
    Json,
    extract::{Path, Query as QueryParameters, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::double_option;
use crate::{
    access::CompanySet,
    agents::parse_id,
    audit::{self, Target as AuditTarget},
    auth::Authorized,
    http::{ApiError, AppState, JsonBody},
    maintenance_windows::{
        self, Automation, Device, MAX_LEAD_MS, MAX_LENGTH_MS, Plan, Setter,
        store::{self, Change, Ender, Filter, Showing, Window, WindowState},
    },
    rbac::Permission,
    targets::{Choice, TagMatch, Target, TargetKind},
    time::{MINUTE_MS, now_ms},
};

const DEFAULT_LIMIT: u64 = 200;
const MAX_LIMIT: u64 = 1000;
/// How many of the windows it set a start answers with.
const ANSWERED_WINDOWS: usize = 100;
/// How far in the past a start may be and still count as now, for clocks a
/// little behind the server's.
const PAST_START_SLACK_MS: i64 = MINUTE_MS;

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_maintenance")
}

fn window_not_found() -> ApiError {
    ApiError::not_found("Maintenance window not found")
}

/// Who may see windows.
fn require_view(actor: &Authorized) -> Result<(), ApiError> {
    actor.require(Permission::DevicesView)
}

/// Who may start, change and end them.
fn require_change(actor: &Authorized) -> Result<(), ApiError> {
    actor.require(Permission::DevicesView)?;
    actor.require(Permission::DevicesMaintenance)
}

/// The companies whose devices `actor` may put in maintenance.
fn changeable(actor: &Authorized) -> CompanySet {
    actor
        .companies(Permission::DevicesMaintenance)
        .intersection(&actor.companies(Permission::DevicesView))
}

/// The devices `target` covers that `actor` may act on. A device or company
/// chosen by ID that isn't there, or that the actor may not act on, is
/// `404`; every device needs the permissions in every company.
async fn devices_for(
    state: &AppState,
    actor: &Authorized,
    target: &Target,
) -> Result<Vec<Device>, ApiError> {
    target.check_references(&mut &state.database).await?;
    let within = changeable(actor);
    match (target, &within) {
        (_, CompanySet::All) => {}
        (Target::All, _) => {
            return Err(ApiError::forbidden(
                "you can put every device in maintenance only with permissions for every company; choose companies, tags or devices instead",
            )
            .with_code("permission_denied"));
        }
        (Target::Companies(ids), CompanySet::Only(reached))
            if ids.iter().any(|id| !reached.contains(id)) =>
        {
            return Err(ApiError::not_found("a chosen company no longer exists"));
        }
        _ => {}
    }
    let devices = maintenance_windows::resolve(&mut &state.database, target, &within).await?;
    if let Target::Devices(ids) = target
        && devices.len() < ids.len()
    {
        return Err(ApiError::not_found("a chosen device no longer exists"));
    }
    if devices.is_empty() {
        return Err(
            ApiError::bad_request("there are no devices to put in maintenance")
                .with_code("no_devices"),
        );
    }
    Ok(devices)
}

/// The window `id` if `actor` may see its device; `404` otherwise.
async fn window_for(
    state: &AppState,
    actor: &Authorized,
    id: &str,
    now: i64,
) -> Result<Window, ApiError> {
    let id = parse_id(id, "maintenance window")?;
    let window = store::get(&mut &state.database, &id, now)
        .await?
        .ok_or_else(window_not_found)?;
    if !visible(actor, &window) {
        return Err(window_not_found());
    }
    Ok(window)
}

/// Whether `actor` may see a window, by its device's company.
fn visible(actor: &Authorized, window: &Window) -> bool {
    actor.covers(Permission::DevicesView, window.company_id.as_deref())
}

/// The window `id` if `actor` may change it: `404` like a missing one when
/// they may see it but not put its device in maintenance.
async fn changeable_window(
    state: &AppState,
    actor: &Authorized,
    id: &str,
    now: i64,
) -> Result<Window, ApiError> {
    let window = window_for(state, actor, id, now).await?;
    if !changeable(actor).covers(window.company_id.as_deref()) {
        return Err(window_not_found());
    }
    Ok(window)
}

/// A request's target: its kind and the IDs it uses.
fn target_of(
    kind: TargetKind,
    device_ids: Vec<String>,
    company_ids: Vec<String>,
    tag_ids: Vec<String>,
    tag_match: TagMatch,
) -> Result<Target, ApiError> {
    if kind == TargetKind::Outdated {
        return Err(invalid(
            "choose devices, companies, tags or every device to put in maintenance",
        ));
    }
    Target::from_request(
        kind,
        Choice {
            device_ids,
            company_ids,
            tag_ids,
            tag_match,
            packages: Vec::new(),
        },
    )
    .map_err(|error| invalid(error.message().to_owned()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParameters {
    /// `open` (in force or upcoming, the default), `current`, `upcoming`,
    /// `past` or `all`.
    #[serde(default)]
    state: Showing,
    device: Option<String>,
    /// A company ID, or `none` for devices in no company.
    company: Option<String>,
    policy: Option<String>,
    limit: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct WindowList {
    windows: Vec<Window>,
    /// More windows match than were listed.
    truncated: bool,
}

/// `GET /v1/maintenance-windows`: open windows by when they start, or past
/// ones latest first.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
    QueryParameters(parameters): QueryParameters<ListParameters>,
) -> Result<Json<WindowList>, ApiError> {
    require_view(&actor)?;
    let filter = Filter {
        showing: parameters.state,
        device_id: parameters
            .device
            .as_deref()
            .map(|id| parse_id(id, "device"))
            .transpose()?,
        company: match parameters.company.as_deref() {
            None => None,
            Some("none") => Some(None),
            Some(id) => Some(Some(parse_id(id, "company")?)),
        },
        policy_id: parameters
            .policy
            .as_deref()
            .map(|id| parse_id(id, "policy"))
            .transpose()?,
        within: actor.companies(Permission::DevicesView),
    };
    let limit = parameters
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let mut windows = store::list(&mut &state.database, &filter, now_ms(), limit + 1).await?;
    windows.retain(|window| visible(&actor, window));
    let truncated = windows.len() as u64 > limit;
    windows.truncate(limit as usize);
    Ok(Json(WindowList { windows, truncated }))
}

/// `GET /v1/maintenance-windows/{id}`.
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Window>, ApiError> {
    require_view(&actor)?;
    Ok(Json(window_for(&state, &actor, &id, now_ms()).await?))
}

/// `GET /v1/agents/{id}/maintenance-windows`: the device's open windows and
/// its latest past ones.
pub async fn device_windows(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_view(&actor)?;
    let id = parse_id(&id, "device")?;
    // A device out of reach answers as a missing one.
    actor
        .require_device(&mut &state.database, Permission::DevicesView, &id)
        .await?;
    let now = now_ms();
    let filter = |showing| Filter {
        showing,
        device_id: Some(id.clone()),
        ..Filter::default()
    };
    let mut database = &state.database;
    let mut open = store::list(&mut database, &filter(Showing::Open), now, MAX_LIMIT).await?;
    let mut past = store::list(&mut database, &filter(Showing::Past), now, 10).await?;
    open.retain(|window| visible(&actor, window));
    past.retain(|window| visible(&actor, window));
    Ok(Json(json!({ "open": open, "past": past })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    target: TargetKind,
    #[serde(default)]
    device_ids: Vec<String>,
    #[serde(default)]
    company_ids: Vec<String>,
    #[serde(default)]
    tag_ids: Vec<String>,
    #[serde(default)]
    tag_match: TagMatch,
    /// When the windows start, in Unix milliseconds; now when missing.
    #[serde(default)]
    starts_at: Option<i64>,
    /// When they end; with neither this nor `duration_minutes`, they last
    /// until someone ends them.
    #[serde(default)]
    ends_at: Option<i64>,
    #[serde(default)]
    duration_minutes: Option<i64>,
    reason: String,
    #[serde(default)]
    automation: Automation,
}

/// Checks when a window starts and ends, as asked at `now`.
fn plan(request: &StartRequest, now: i64) -> Result<Plan, ApiError> {
    let reason = maintenance_windows::reason(&request.reason)?;
    let starts_at = match request.starts_at {
        None => now,
        Some(at) if at < now - PAST_START_SLACK_MS => {
            return Err(invalid(
                "a window can't start in the past; leave the start out to start now",
            ));
        }
        Some(at) if at > now + MAX_LEAD_MS => {
            return Err(invalid("a window can start at most a year ahead"));
        }
        Some(at) => at.max(now),
    };
    let ends_at = match (request.ends_at, request.duration_minutes) {
        (Some(_), Some(_)) => return Err(invalid("give an end or a duration, not both")),
        (Some(at), None) => Some(at),
        (None, Some(minutes)) if minutes >= 1 => {
            Some(starts_at.saturating_add(minutes.saturating_mul(MINUTE_MS)))
        }
        (None, Some(_)) => return Err(invalid("a window lasts at least a minute")),
        (None, None) => None,
    };
    if let Some(end) = ends_at {
        check_end(starts_at, end, now)?;
    }
    Ok(Plan {
        starts_at,
        ends_at,
        reason,
        automation: request.automation,
    })
}

/// A window from `starts_at` may end at `end`, set at `now`.
fn check_end(starts_at: i64, end: i64, now: i64) -> Result<(), ApiError> {
    if end <= starts_at.max(now) {
        return Err(invalid("a window must end after it starts, and after now"));
    }
    if end - starts_at > MAX_LENGTH_MS {
        return Err(invalid("a window lasts at most a year"));
    }
    Ok(())
}

/// `POST /v1/maintenance-windows`: puts the devices a target covers in
/// maintenance, now or later, a window each, and answers with the first
/// [`ANSWERED_WINDOWS`] of them.
pub async fn start(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<StartRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_change(&actor)?;
    let now = now_ms();
    let plan = plan(&request, now)?;
    let target = target_of(
        request.target,
        request.device_ids,
        request.company_ids,
        request.tag_ids,
        request.tag_match,
    )?;
    let devices = devices_for(&state, &actor, &target).await?;
    let label = user_label(&actor);
    let audit_actor = actor.actor();
    let setter = Setter {
        user_id: Some(&actor.user.id),
        label: &label,
        actor: &audit_actor,
        policy: None,
    };
    let mut transaction = state.database.begin().await?;
    let ids = maintenance_windows::write(&mut transaction, &devices, &plan, &setter, now).await?;
    transaction.commit().await?;
    let device_ids = devices
        .into_iter()
        .map(|device| device.id)
        .collect::<Vec<_>>();
    maintenance_windows::changed(&state, &device_ids, now).await;
    let mut windows = Vec::new();
    for id in ids.iter().take(ANSWERED_WINDOWS) {
        if let Some(window) = store::get(&mut &state.database, id, now).await? {
            windows.push(window);
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({ "created": ids.len(), "windows": windows })),
    ))
}

/// The name windows show as who set or ended them.
fn user_label(actor: &Authorized) -> String {
    if actor.user.display_name.trim().is_empty() {
        actor.user.email.clone()
    } else {
        actor.user.display_name.clone()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndRequest {
    target: TargetKind,
    #[serde(default)]
    device_ids: Vec<String>,
    #[serde(default)]
    company_ids: Vec<String>,
    #[serde(default)]
    tag_ids: Vec<String>,
    #[serde(default)]
    tag_match: TagMatch,
    /// Also cancel the windows that haven't started.
    #[serde(default)]
    upcoming: bool,
}

/// Ends `windows` at `now` as `actor`, each with an audit event. Returns how
/// many were still open.
async fn end_windows(
    state: &AppState,
    actor: &Authorized,
    windows: &[Window],
    now: i64,
) -> Result<usize, ApiError> {
    let label = user_label(actor);
    let ender = Ender {
        user_id: Some(&actor.user.id),
        label: &label,
    };
    let mut transaction = state.database.begin().await?;
    let mut ended = Vec::new();
    for window in windows {
        if !store::end(&mut transaction, &window.id, ender, now).await? {
            continue;
        }
        let cancelled = window.state == WindowState::Upcoming;
        audit::record(
            &mut transaction,
            &actor.actor(),
            if cancelled {
                "maintenance_window.cancel"
            } else {
                "maintenance_window.end"
            },
            AuditTarget::device(&window.device_id),
            json!({
                "name": window.device_name,
                "window_id": window.id,
                "reason": window.reason,
                "starts_at": window.starts_at,
                "planned_ends_at": window.ends_at,
                "ended_at": now,
            }),
        )
        .await?;
        ended.push(window.device_id.clone());
    }
    transaction.commit().await?;
    let count = ended.len();
    ended.sort_unstable();
    ended.dedup();
    maintenance_windows::changed(state, &ended, now).await;
    Ok(count)
}

/// `POST /v1/maintenance-windows/end`: ends the windows in force on the
/// devices a target covers, and with `upcoming` cancels those to come.
pub async fn end_many(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<EndRequest>,
) -> Result<Json<Value>, ApiError> {
    require_change(&actor)?;
    let now = now_ms();
    let target = target_of(
        request.target,
        request.device_ids,
        request.company_ids,
        request.tag_ids,
        request.tag_match,
    )?;
    let devices = devices_for(&state, &actor, &target).await?;
    let ids = devices
        .into_iter()
        .map(|device| device.id)
        .collect::<Vec<_>>();
    let mut windows = store::open_on(&mut &state.database, &ids, now, request.upcoming).await?;
    windows.retain(|window| visible(&actor, window));
    let ended = end_windows(&state, &actor, &windows, now).await?;
    Ok(Json(json!({ "ended": ended })))
}

/// `POST /v1/maintenance-windows/{id}/end`: ends the window now, or cancels
/// it if it hasn't started.
pub async fn end(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Window>, ApiError> {
    require_change(&actor)?;
    let now = now_ms();
    let window = changeable_window(&state, &actor, &id, now).await?;
    if !matches!(window.state, WindowState::Current | WindowState::Upcoming)
        || end_windows(&state, &actor, std::slice::from_ref(&window), now).await? == 0
    {
        return Err(over());
    }
    Ok(Json(
        store::get(&mut &state.database, &window.id, now)
            .await?
            .ok_or_else(window_not_found)?,
    ))
}

fn over() -> ApiError {
    ApiError::conflict("this maintenance window is over").with_code("window_over")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeRequest {
    /// A new end, or `null` to last until someone ends it.
    #[serde(default, deserialize_with = "double_option")]
    ends_at: Option<Option<i64>>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    automation: Option<Automation>,
}

/// `PATCH /v1/maintenance-windows/{id}`: changes an open window's end,
/// reason or what it holds back.
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<ChangeRequest>,
) -> Result<Json<Window>, ApiError> {
    require_change(&actor)?;
    let now = now_ms();
    let window = changeable_window(&state, &actor, &id, now).await?;
    if !matches!(window.state, WindowState::Current | WindowState::Upcoming) {
        return Err(over());
    }
    if let Some(Some(end)) = request.ends_at {
        check_end(window.starts_at, end, now)?;
    }
    let change = Change {
        ends_at: request.ends_at,
        reason: request
            .reason
            .as_deref()
            .map(maintenance_windows::reason)
            .transpose()?,
        automation: request.automation,
    };
    let mut transaction = state.database.begin().await?;
    if !store::update(&mut transaction, &window.id, &change, now).await? {
        return Err(over());
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "maintenance_window.update",
        AuditTarget::device(&window.device_id),
        json!({
            "name": window.device_name,
            "window_id": window.id,
            "reason": change.reason.as_deref().unwrap_or(&window.reason),
            "previous_reason": change.reason.as_ref().map(|_| &window.reason),
            "ends_at": change.ends_at.unwrap_or(window.ends_at),
            "previous_ends_at": change.ends_at.map(|_| window.ends_at),
            "automation": change.automation.unwrap_or(window.automation),
            "previous_automation": change.automation.map(|_| window.automation),
        }),
    )
    .await?;
    transaction.commit().await?;
    maintenance_windows::changed(&state, std::slice::from_ref(&window.device_id), now).await;
    Ok(Json(
        store::get(&mut &state.database, &window.id, now)
            .await?
            .ok_or_else(window_not_found)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        starts_at: Option<i64>,
        ends_at: Option<i64>,
        duration: Option<i64>,
    ) -> StartRequest {
        StartRequest {
            target: TargetKind::All,
            device_ids: Vec::new(),
            company_ids: Vec::new(),
            tag_ids: Vec::new(),
            tag_match: TagMatch::Any,
            starts_at,
            ends_at,
            duration_minutes: duration,
            reason: "  Replacing the disk ".into(),
            automation: Automation::Scans,
        }
    }

    #[test]
    fn windows_start_now_or_later_and_end_after_they_start() {
        let now = 1_000 * MINUTE_MS;
        let plan = plan(&request(None, None, None), now).unwrap();
        assert_eq!((plan.starts_at, plan.ends_at), (now, None));
        assert_eq!(plan.reason, "Replacing the disk");
        let later = plan_of(Some(now + 60 * MINUTE_MS), None, Some(30), now);
        assert_eq!(later.ends_at, Some(now + 90 * MINUTE_MS));
        // A start a few seconds back is now.
        assert_eq!(plan_of(Some(now - 5_000), None, None, now).starts_at, now);
        assert!(super::plan(&request(Some(now - 2 * MINUTE_MS), None, None), now).is_err());
        assert!(super::plan(&request(None, Some(now), None), now).is_err());
        assert!(super::plan(&request(None, Some(now + 1), Some(5)), now).is_err());
        assert!(super::plan(&request(None, None, Some(0)), now).is_err());
        assert!(super::plan(&request(None, Some(now + MAX_LENGTH_MS + 1), None), now).is_err());
        assert!(super::plan(&request(Some(now + MAX_LEAD_MS + 1), None, None), now).is_err());
        let mut blank = request(None, None, None);
        blank.reason = " ".into();
        assert!(super::plan(&blank, now).is_err());
    }

    fn plan_of(
        starts_at: Option<i64>,
        ends_at: Option<i64>,
        duration: Option<i64>,
        now: i64,
    ) -> Plan {
        plan(&request(starts_at, ends_at, duration), now).unwrap()
    }
}
