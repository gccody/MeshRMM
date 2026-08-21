use crate::*;

pub(crate) async fn list_agents(request: &Request, environment: &Env) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) => identity,
        Err(error) => return workos_auth_error(error),
    };
    company_presence::snapshot(environment, &identity.company_id).await
}

pub(crate) async fn create_agent_event_subscription(
    request: &Request,
    environment: &Env,
) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) => identity,
        Err(error) => return workos_auth_error(error),
    };
    let db = environment.d1("DB")?;
    ensure_company_exists(&db, &identity.company_id).await?;
    let now = now_ms_i64()?;
    query!(
        &db,
        "DELETE FROM agent_event_subscriptions WHERE expires_at <= ?1",
        now
    )?
    .run()
    .await?;
    let subscription_token = random_token();
    let token_hash = sha256_hex(&subscription_token);
    let expires_at = Date::now().as_millis() + AGENT_EVENT_SUBSCRIPTION_TTL_MS;
    let expires_at_i64 =
        i64::try_from(expires_at).map_err(|_| Error::RustError("clock overflow".into()))?;
    query!(
        &db,
        "INSERT INTO agent_event_subscriptions (token_hash, company_id, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        token_hash,
        identity.company_id,
        identity.user_id,
        now,
        expires_at_i64
    )?
    .run()
    .await?;
    Response::from_json(&AgentEventSubscription {
        subscription_token,
        websocket_url: agent_event_websocket_url(environment)?,
        expires_at_unix_ms: expires_at,
    })
}

pub(crate) async fn subscribe_agent_events(
    request: Request,
    environment: &Env,
) -> Result<Response> {
    if request
        .headers()
        .get("Upgrade")?
        .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
    {
        return api_error(426, "WebSocket upgrade required");
    }
    let expected_origin = environment.var("DASHBOARD_ORIGIN")?.to_string();
    if request.headers().get("Origin")?.as_deref() != Some(expected_origin.as_str()) {
        return api_error(403, "dashboard origin is not allowed");
    }
    let supplied = request
        .url()?
        .query_pairs()
        .find_map(|(key, value)| (key == "token").then(|| value.into_owned()))
        .unwrap_or_default();
    if supplied.len() != 64 || !supplied.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return api_error(401, "Agent event subscription is invalid or expired");
    }
    let now = now_ms_i64()?;
    let token_hash = sha256_hex(&supplied);
    let db = environment.d1("DB")?;
    let subscription = query!(
        &db,
        "UPDATE agent_event_subscriptions SET used_at = ?1 WHERE token_hash = ?2 AND used_at IS NULL AND expires_at > ?1 RETURNING company_id",
        now,
        token_hash
    )?
    .first::<AgentEventSubscriptionRow>(None)
    .await?;
    let Some(subscription) = subscription else {
        return api_error(401, "Agent event subscription is invalid or expired");
    };
    forward_to_object(
        environment,
        "COMPANY_PRESENCE",
        &subscription.company_id,
        request,
        "https://presence.internal/subscribe",
        &[("X-Pulse-Company-Id", subscription.company_id.as_str())],
    )
    .await
}
