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
use sha2::{Digest, Sha256};

use crate::{
    agents::{self, parse_id},
    audit::{self, Target},
    auth::Authorized,
    db::tables::Agents,
    http::{ApiError, AppState},
    rbac::Permission,
    realtime::presence::PresenceEvent,
    secrets::{hex, new_token, token_hash},
    time::now_ms,
};

/// `GET /v1/agents`: enrolled devices, online ones first, then by name, as
/// the presence snapshot the website's event socket starts from.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<PresenceEvent>, ApiError> {
    actor.require(Permission::DevicesView)?;
    Ok(Json(state.presence.snapshot().await?))
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
    if let Err(error) = state
        .sessions
        .end_for_device(&state, &id, "the device was removed")
        .await
    {
        tracing::warn!(device_id = id, status = %error.status(), "could not end a deleted device's remote session");
    }
    state.agents.send(&id, AgentCommand::Uninstall);
    state.presence.refresh(&id).await;
    Ok(StatusCode::NO_CONTENT)
}

const OFFLINE_FOR_ROTATION: &str =
    "the Agent must be online to receive its new credential; its current credential still works";

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
    let pending = agents::pending_credential(&mut transaction, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("Device not found"))?;
    if !state.agents.is_connected(&id) {
        return Err(ApiError::conflict(OFFLINE_FOR_ROTATION).with_code("device_offline"));
    }
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
    // A credential the Agent didn't get stays pending, and the next rotation
    // sends it again. Withdrawing it could race a rotation that did deliver it.
    if !state.agents.send(&id, AgentCommand::RotateToken { token }) {
        return Err(ApiError::conflict(OFFLINE_FOR_ROTATION).with_code("device_offline"));
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
    // The website revalidates itself; nothing in between may keep a copy.
    let cache_control = (
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    // A deleted device's image is gone, even if removing its file failed.
    if !agents::is_active(&mut &state.database, &id).await? {
        return Ok((StatusCode::NO_CONTENT, [cache_control]).into_response());
    }
    let path = state.storage.thumbnail(&id);
    let (bytes, modified) = match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let modified = tokio::fs::metadata(&path)
                .await
                .and_then(|metadata| metadata.modified())
                .ok();
            (bytes, modified)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((StatusCode::NO_CONTENT, [cache_control]).into_response());
        }
        Err(error) => return Err(anyhow::Error::from(error).into()),
    };
    let tag = hex(&Sha256::digest(&bytes)[..16]);
    let etag = HeaderValue::from_str(&format!("\"{tag}\"")).expect("hex is a valid header value");
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .and_then(requested_etag)
        .is_some_and(|requested| requested == tag);
    if unchanged {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [cache_control, (header::ETAG, etag)],
        )
            .into_response());
    }
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
