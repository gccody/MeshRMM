//! `GET /v1/audit`: the audit log, newest first, a page at a time.
use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};

use crate::{
    audit::{self, Event, Filter},
    auth::Authorized,
    http::{ApiError, AppState},
    rbac::Permission,
};

const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQuery {
    #[serde(default)]
    limit: Option<u64>,
    /// The `next` cursor from the previous page.
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    actor_user_id: Option<String>,
    #[serde(default)]
    target_type: Option<String>,
    #[serde(default)]
    target_id: Option<String>,
    /// An action such as `user.update`, or a prefix such as `user.`.
    #[serde(default)]
    action: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuditPage {
    events: Vec<Event>,
    /// Pass as `before` for the next page; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
    Query(query): Query<AuditQuery>,
) -> Result<Json<AuditPage>, ApiError> {
    actor.require(Permission::AuditView)?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let before = query
        .before
        .as_deref()
        .map(|cursor| {
            cursor
                .split_once(':')
                .and_then(|(time, id)| Some((time.parse().ok()?, id.to_owned())))
                .ok_or_else(|| ApiError::bad_request("invalid cursor"))
        })
        .transpose()?;
    let target = match (query.target_type, query.target_id) {
        (Some(kind), Some(id)) => Some((kind, id)),
        (None, None) => None,
        _ => {
            return Err(ApiError::bad_request(
                "filter by target with both target_type and target_id",
            ));
        }
    };
    let filter = Filter {
        actor_user_id: query.actor_user_id,
        target,
        action: query.action,
        before,
    };
    let events = audit::list(&mut &state.database, &filter, limit).await?;
    let next = (events.len() as u64 == limit)
        .then(|| {
            events
                .last()
                .map(|event| format!("{}:{}", event.created_at, event.id))
        })
        .flatten();
    Ok(Json(AuditPage { events, next }))
}
