use crate::*;

pub(crate) async fn create_handoff(request: &mut Request, environment: &Env) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) => identity,
        Err(error) => return workos_auth_error(error),
    };
    let body: HandoffRequest = request
        .json()
        .await
        .map_err(|_| Error::RustError("invalid remote handoff request".into()))?;
    validate_identifier(&body.device_id, "device ID")?;
    let db = environment.d1("DB")?;
    let permitted = query!(
        &db,
        "SELECT 1 AS permitted FROM agents WHERE id = ?1 AND company_id = ?2 AND deletion_requested_at IS NULL",
        body.device_id,
        identity.company_id
    )?
    .first::<i64>(Some("permitted"))
    .await?
    .is_some();
    if !permitted {
        return api_error(404, "Agent not found");
    }
    let handoff_token = random_token();
    let token_hash = sha256_hex(&handoff_token);
    let created_at = Date::now().as_millis();
    let expires_at = created_at + HANDOFF_TTL_MS;
    query!(
        &db,
        "DELETE FROM remote_handoffs WHERE expires_at <= ?1",
        now_ms_i64()?
    )?
    .run()
    .await?;
    query!(
        &db,
        "INSERT INTO remote_handoffs (token_hash, company_id, device_id, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        token_hash,
        identity.company_id,
        body.device_id,
        identity.user_id,
        i64::try_from(created_at).map_err(|_| Error::RustError("clock overflow".into()))?,
        i64::try_from(expires_at).map_err(|_| Error::RustError("clock overflow".into()))?
    )?
    .run()
    .await?;
    audit(
        &db,
        &identity,
        "remote.handoff_create",
        "agent",
        &body.device_id,
        "{}",
    )
    .await?;
    Response::from_json(&HandoffResponse {
        handoff_token,
        api_url: canonical_company_url(&db, environment, &identity.company_id).await?,
        expires_at_unix_ms: expires_at,
    })
}

pub(crate) async fn redeem_handoff(request: &Request, environment: &Env) -> Result<Response> {
    let supplied = match bearer_token(request) {
        Ok(token) => token,
        Err(_) => return api_error(401, "remote handoff token is required"),
    };
    let token_hash = sha256_hex(&supplied);
    let now = now_ms_i64()?;
    let db = environment.d1("DB")?;
    let request_tenant = request_tenant_company(&db, request, environment).await?;
    if request_tenant.is_none() && !is_legacy_control_plane_request(request, environment)? {
        return api_error(404, "company hostname was not found");
    }
    let handoff = if let Some(tenant) = request_tenant.as_ref() {
        query!(
            &db,
            "UPDATE remote_handoffs SET used_at = ?1 WHERE token_hash = ?2 AND company_id = ?3 AND used_at IS NULL AND expires_at > ?1 AND EXISTS (SELECT 1 FROM agents WHERE agents.id = remote_handoffs.device_id AND agents.deletion_requested_at IS NULL) RETURNING company_id, device_id, user_id",
            now,
            token_hash,
            tenant.id
        )?
        .first::<HandoffRow>(None)
        .await?
    } else {
        query!(
            &db,
            "UPDATE remote_handoffs SET used_at = ?1 WHERE token_hash = ?2 AND used_at IS NULL AND expires_at > ?1 AND EXISTS (SELECT 1 FROM agents WHERE agents.id = remote_handoffs.device_id AND agents.deletion_requested_at IS NULL) RETURNING company_id, device_id, user_id",
            now,
            token_hash
        )?
        .first::<HandoffRow>(None)
        .await?
    };
    let Some(handoff) = handoff else {
        return api_error(401, "remote handoff is invalid, expired, or already used");
    };
    let response = create_session_for_device(environment, &handoff.device_id).await?;
    let identity = Identity {
        user_id: handoff.user_id,
        company_id: handoff.company_id,
        role: None,
        roles: Vec::new(),
        permissions: Vec::new(),
    };
    audit(
        &db,
        &identity,
        "remote.session_create",
        "agent",
        &handoff.device_id,
        "{}",
    )
    .await?;
    Ok(response)
}

pub(crate) async fn create_session_for_device(
    environment: &Env,
    device_id: &str,
) -> Result<Response> {
    let session_id = Uuid::new_v4().to_string();
    let client_token = random_token();
    let agent_token = random_token();
    let idle_timeout_seconds = session_idle_timeout(environment)?;
    let idle_timeout_ms = idle_timeout_seconds * 1000;
    let expires_at_unix_ms = Date::now().as_millis() + idle_timeout_ms;
    let ice_servers = generate_ice_servers(environment, idle_timeout_seconds).await?;

    let init = SessionInit {
        client_token: &client_token,
        agent_token: &agent_token,
        expires_at_unix_ms,
        idle_timeout_ms,
    };
    let init_request = internal_json_request("https://session.internal/init", &init)?;
    let session_stub = object_stub(environment, "REMOTE_SESSION", &session_id)?;
    ensure_success(
        session_stub.fetch_with_request(init_request).await?,
        "initialize session",
    )
    .await?;

    let agent_request = AgentSessionRequest {
        session_id: RemoteSessionId::new(&session_id),
        signaling_token: agent_token,
        expires_at_unix_ms,
        ice_servers: ice_servers.clone(),
    };
    let notify_request = internal_json_request("https://agent.internal/request", &agent_request)?;
    let notify_response = object_stub(environment, "AGENT_COORDINATOR", device_id)?
        .fetch_with_request(notify_request)
        .await?;
    if !(200..300).contains(&notify_response.status_code()) {
        let cleanup = Request::new("https://session.internal/expire", Method::Post)?;
        let _ = session_stub.fetch_with_request(cleanup).await;
        return api_error(409, "target Agent is not connected");
    }

    console_log!(
        "event=remote_session_created session_id={} device_id={} expires_at_ms={}",
        session_id,
        device_id,
        expires_at_unix_ms
    );
    Response::from_json(&SessionBootstrap {
        session_id: RemoteSessionId::new(session_id),
        signaling_token: client_token,
        expires_at_unix_ms,
        ice_servers,
    })
}
