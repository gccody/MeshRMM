//! Enrolled devices, as the website manages them.
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use meshrmm_protocol_types::AgentCommand;
use sea_query::{Expr, ExprTrait, Query};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::{
    agents::{self, parse_id},
    audit::{self, Target},
    auth::Authorized,
    db::tables::Agents,
    http::{ApiError, AppState},
    rbac::Permission,
    realtime::Unsent,
    secrets::{new_token, token_hash},
    storage,
    time::now_ms,
};

/// `GET /v1/agents`: enrolled devices, online ones first, then by name, as
/// the presence snapshot the website's event socket starts from.
pub async fn list(State(state): State<AppState>, actor: Authorized) -> Result<Response, ApiError> {
    actor.require(Permission::DevicesView)?;
    let snapshot = state.presence.snapshot().await?;
    let companies = actor.companies(Permission::DevicesView);
    if companies.is_all() {
        return Ok(Json(&*snapshot).into_response());
    }
    Ok(Json(snapshot.within(&companies)).into_response())
}

/// `DELETE /v1/agents/{id}`: removes the device and tells its Agent to
/// uninstall itself, now or when it next connects. The Agent keeps its
/// credentials, including one a rotation just sent it, so it can still hear
/// the request.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::DevicesDelete)?;
    let id = parse_id(&id, "device")?;
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    actor
        .require_device(&mut transaction, Permission::DevicesDelete, &id)
        .await?;
    let (name,): (String,) = transaction
        .fetch_optional(
            &Query::select()
                .column(Agents::Name)
                .from(Agents::Table)
                .and_where(Expr::col(Agents::Id).eq(id.as_str()))
                .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
                .to_owned(),
        )
        .await?
        .ok_or_else(|| ApiError::not_found("Device not found"))?;
    transaction
        .execute(
            &Query::update()
                .table(Agents::Table)
                .value(Agents::DeletionRequestedAt, now)
                .value(Agents::UpdatedAt, now)
                .and_where(Expr::col(Agents::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "agent.delete",
        Target::device(&id),
        json!({ "name": name }),
    )
    .await?;
    transaction.commit().await?;
    if let Err(error) = state.storage.remove(&state.storage.thumbnail(&id)).await {
        tracing::warn!(device_id = id, %error, "could not remove a deleted device's thumbnail");
    }
    if let Err(error) = state.metrics.forget(&id).await {
        tracing::warn!(device_id = id, %error, "could not remove a deleted device's resource usage");
    }
    if let Err(error) = crate::system_info::forget(&state.database, &id).await {
        tracing::warn!(device_id = id, %error, "could not remove a deleted device's system information");
    }
    if let Err(error) = state
        .sessions
        .end_for_device(&state, &id, "the device was removed")
        .await
    {
        tracing::warn!(device_id = id, status = %error.status(), "could not end a deleted device's remote session");
    }
    crate::alerts::device_removed(&state, &id).await;
    crate::automation::device_removed(&state, &id).await;
    crate::maintenance_windows::device_removed(&state, &id).await;
    crate::updates::device_removed(&state, &id).await;
    crate::applications::device_removed(&state, &id).await;
    // An offline Agent is told to uninstall when it next connects. One too
    // far behind to take the command now is disconnected, so it reconnects
    // and is told then.
    if state.agents.send(&id, AgentCommand::Uninstall) == Err(Unsent::Full) {
        state.agents.disconnect(&id);
    }
    state.presence.refresh(&id).await;
    Ok(StatusCode::NO_CONTENT)
}

fn offline_for_rotation() -> ApiError {
    ApiError::conflict(
        "the Agent must be online to receive its new credential; its current credential still works",
    )
    .with_code("device_offline")
}

/// `POST /v1/agents/{id}/rotate-credential`: sends the online Agent a new
/// credential. The current one keeps working until the Agent uses the new
/// one. Rotating again before then sends the same new credential again, so
/// an Agent that saved it but could not reconnect is never locked out, and
/// a credential the Agent never got does no harm.
pub async fn rotate_credential(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    actor.require(Permission::DevicesRotateCredentials)?;
    let id = parse_id(&id, "device")?;
    let mut transaction = state.database.begin().await?;
    actor
        .require_device(&mut transaction, Permission::DevicesRotateCredentials, &id)
        .await?;
    let pending = agents::pending_credential(&mut transaction, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("Device not found"))?;
    // A place for the credential is held before it is staged, so an Agent
    // too far behind to take it now is not left with one it never got.
    let place = state
        .agents
        .reserve(&id)
        .map_err(|unsent| unsent.answer(offline_for_rotation()))?;
    let staged = pending.token(&state.instance_key, &id)?;
    let redelivered = staged.is_some();
    let token = match staged {
        Some(token) => token,
        None => {
            let token = new_token();
            let mut update = Query::update();
            update
                .table(Agents::Table)
                .value(Agents::PendingAuthTokenHash, token_hash(&token))
                .value(
                    Agents::PendingAuthTokenEncrypted,
                    state
                        .instance_key
                        .encrypt(&agents::rotation_context(&id), token.as_bytes()),
                )
                .value(Agents::UpdatedAt, now_ms())
                .and_where(Expr::col(Agents::Id).eq(id.as_str()))
                .and_where(Expr::col(Agents::DeletionRequestedAt).is_null());
            match &pending.pending_auth_token_hash {
                Some(hash) => {
                    update.and_where(Expr::col(Agents::PendingAuthTokenHash).eq(hash.as_str()))
                }
                None => update.and_where(Expr::col(Agents::PendingAuthTokenHash).is_null()),
            };
            if transaction.execute(&update).await? == 0 {
                return Err(ApiError::conflict(
                    "the device's credential changed during the rotation; try again",
                ));
            }
            token
        }
    };
    transaction.commit().await?;
    // A credential the Agent didn't get, because it went offline since,
    // stays pending, and the next rotation sends it again. Withdrawing it
    // could race a rotation that did deliver it.
    if !place.send(AgentCommand::RotateToken { token }) {
        return Err(offline_for_rotation());
    }
    audit::record(
        &mut &state.database,
        &actor.actor(),
        "agent.rotate_credential",
        Target::device(&id),
        json!({ "redelivered": redelivered }),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "device_id": id, "status": "rotation_pending" })),
    ))
}

/// The ETag an `If-None-Match` header names, without quotes or a weak prefix.
fn requested_etag(header: &str) -> Option<&str> {
    let etag = header.trim();
    let etag = etag.strip_prefix("W/").unwrap_or(etag);
    let etag = etag.trim_matches('"');
    (!etag.is_empty() && !etag.contains([',', '"', '*'])).then_some(etag)
}

/// `tag` quoted, as an `ETag` header value.
fn etag_value(tag: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{tag}\"")).expect("hex is a valid header value")
}

/// `GET /v1/agents/{id}/thumbnail`: the device's latest screen thumbnail.
/// Answers a matching `If-None-Match` with 304, so an unchanged image is not
/// sent again, and a device without an image with 204, which browsers do not
/// log as an error.
pub async fn thumbnail(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    actor.require(Permission::DevicesView)?;
    let id = parse_id(&id, "device")?;
    // A deleted device answers 204 below; one out of reach, 404.
    if !actor.everywhere(Permission::DevicesView) {
        actor
            .require_device(&mut &state.database, Permission::DevicesView, &id)
            .await?;
    }
    // The website revalidates itself; nothing in between may keep a copy.
    let cache_control = (
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    let no_image = || (StatusCode::NO_CONTENT, [cache_control.clone()]).into_response();
    // A deleted device's image is gone, even if removing its file failed.
    if !agents::is_active(&mut &state.database, &id).await? {
        return Ok(no_image());
    }
    let path = state.storage.thumbnail(&id);
    // A browser with the current image is answered from the file's metadata
    // alone, without reading it.
    let tag = match tokio::fs::metadata(&path).await {
        Ok(metadata) => storage::version(&metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(no_image()),
        Err(error) => return Err(anyhow::Error::from(error).into()),
    };
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .and_then(requested_etag)
        .is_some_and(|requested| requested == tag);
    if unchanged {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [cache_control, (header::ETAG, etag_value(&tag))],
        )
            .into_response());
    }
    // The ETag and Last-Modified sent come from the opened file, so they
    // describe its bytes even if a new image replaces it meanwhile.
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(no_image()),
        Err(error) => return Err(anyhow::Error::from(error).into()),
    };
    let metadata = file.metadata().await.map_err(anyhow::Error::from)?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.read_to_end(&mut bytes)
        .await
        .map_err(anyhow::Error::from)?;
    let etag = etag_value(&storage::version(&metadata));
    let modified = metadata.modified().ok();
    let mut response = (
        [
            cache_control,
            (header::ETAG, etag),
            (header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg")),
        ],
        Body::from(bytes),
    )
        .into_response();
    if let Some(modified) = modified
        && let Ok(value) = HeaderValue::from_str(&httpdate::fmt_http_date(modified))
    {
        response.headers_mut().insert(header::LAST_MODIFIED, value);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_none_match_names_one_etag() {
        assert_eq!(requested_etag("\"abc123\""), Some("abc123"));
        assert_eq!(requested_etag("W/\"abc123\""), Some("abc123"));
        assert_eq!(requested_etag(" abc123 "), Some("abc123"));
        assert_eq!(requested_etag("*"), None);
        assert_eq!(requested_etag("\"a\", \"b\""), None);
        assert_eq!(requested_etag("\"\""), None);
    }
}
