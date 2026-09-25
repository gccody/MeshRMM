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
    .metered_first::<i64>(Some("permitted"))
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
    .metered_run()
    .await?;
    query!(
        &db,
        "INSERT INTO remote_handoffs (token_hash, company_id, device_id, user_id, created_at, expires_at, start_in_background) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        token_hash,
        identity.company_id,
        body.device_id,
        identity.user_id,
        i64::try_from(created_at).map_err(|_| Error::RustError("clock overflow".into()))?,
        i64::try_from(expires_at).map_err(|_| Error::RustError("clock overflow".into()))?,
        body.start_in_background
    )?
    .metered_run()
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
        start_in_background: body.start_in_background,
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
            "UPDATE remote_handoffs SET used_at = ?1 WHERE token_hash = ?2 AND company_id = ?3 AND used_at IS NULL AND expires_at > ?1 AND EXISTS (SELECT 1 FROM companies WHERE companies.id = remote_handoffs.company_id AND companies.status IN ('active', 'awaiting_admin')) AND EXISTS (SELECT 1 FROM agents WHERE agents.id = remote_handoffs.device_id AND agents.deletion_requested_at IS NULL) RETURNING company_id, device_id, user_id, start_in_background",
            now,
            token_hash,
            tenant.id
        )?
        .metered_first::<HandoffRow>(None)
        .await?
    } else {
        query!(
            &db,
            "UPDATE remote_handoffs SET used_at = ?1 WHERE token_hash = ?2 AND used_at IS NULL AND expires_at > ?1 AND EXISTS (SELECT 1 FROM companies WHERE companies.id = remote_handoffs.company_id AND companies.status IN ('active', 'awaiting_admin')) AND EXISTS (SELECT 1 FROM agents WHERE agents.id = remote_handoffs.device_id AND agents.deletion_requested_at IS NULL) RETURNING company_id, device_id, user_id, start_in_background",
            now,
            token_hash
        )?
        .metered_first::<HandoffRow>(None)
        .await?
    };
    let Some(handoff) = handoff else {
        return api_error(401, "remote handoff is invalid, expired, or already used");
    };
    crate::usage::attribute_company(&handoff.company_id);
    validate_identifier(&handoff.user_id, "user ID")?;
    let mut profile_response = super::platform::workos_request(
        environment,
        Method::Get,
        &format!("/user_management/users/{}", handoff.user_id),
        None,
    )
    .await?;
    if !(200..300).contains(&profile_response.status_code()) {
        return api_error(502, "could not resolve the remote user's dashboard name");
    }
    let profile: serde_json::Value = profile_response.json().await?;
    let viewer_name = dashboard_user_name(&profile);
    let response = create_session_for_device(
        environment,
        &handoff.device_id,
        &viewer_name,
        handoff.start_in_background,
    )
    .await?;
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
    viewer_name: &str,
    start_in_background: bool,
) -> Result<Response> {
    // Resolve policy from the enrolled device's company, never from viewer input.
    let db = environment.d1("DB")?;
    #[derive(Deserialize)]
    struct MaintenancePolicy {
        company_id: String,
        blackout_message: String,
        #[serde(deserialize_with = "deserialize_sql_bool")]
        display_border: bool,
        #[serde(deserialize_with = "deserialize_sql_bool")]
        prevent_idle_lock: bool,
        #[serde(deserialize_with = "deserialize_sql_bool")]
        allow_idle_override: bool,
    }
    let policy = query!(&db,
        "SELECT c.id AS company_id, c.blackout_message, c.display_border, c.prevent_idle_lock, c.allow_idle_override FROM companies c JOIN agents a ON a.company_id = c.id WHERE a.id = ?1 AND a.deletion_requested_at IS NULL",
        device_id
    )?.metered_first::<MaintenancePolicy>(None).await?;
    let Some(policy) = policy else {
        return api_error(404, "agent not found");
    };
    let idle_policy = meshrmm_protocol_types::IdlePolicy {
        prevent_idle_lock: policy.prevent_idle_lock,
        allow_override: policy.allow_idle_override,
    };
    let session_id = Uuid::new_v4().to_string();
    let client_token = random_token();
    let agent_token = random_token();
    let idle_timeout_seconds = session_idle_timeout(environment)?;
    let idle_timeout_ms = idle_timeout_seconds * 1000;
    let expires_at_unix_ms = Date::now().as_millis() + idle_timeout_ms.max(15 * 60 * 1000);
    let ice_servers =
        generate_ice_servers(environment, idle_timeout_seconds, &policy.company_id).await?;
    // The session's Durable Object is named after the session, so its usage is
    // attributed through this record.
    query!(
        &db,
        "INSERT OR IGNORE INTO usage_object_owners (object_name, company_id, kind, created_at) VALUES (?1, ?2, 'remote_session', ?3)",
        session_id,
        policy.company_id,
        now_ms_i64()?
    )?
    .metered_run()
    .await?;

    let init = SessionInit {
        start_in_background,
        idle_policy,
        display_border: policy.display_border,
        blackout_message: &policy.blackout_message,
        viewer_name,
        session_id: &session_id,
        device_id,
        company_id: &policy.company_id,
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
        start_in_background,
        idle_policy,
        blackout_message: policy.blackout_message,
        viewer_name: viewer_name.to_owned(),
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
        return api_error(
            409,
            "target Agent is offline or already has an active remote session",
        );
    }

    console_log!(
        "event=remote_session_created session_id={} device_id={} expires_at_ms={}",
        session_id,
        device_id,
        expires_at_unix_ms
    );
    Response::from_json(&SessionBootstrap {
        start_in_background,
        idle_policy,
        display_border: policy.display_border,
        session_id: RemoteSessionId::new(session_id),
        signaling_token: client_token,
        expires_at_unix_ms,
        ice_servers,
    })
}

fn dashboard_user_name(profile: &serde_json::Value) -> String {
    let name = ["first_name", "last_name"]
        .iter()
        .filter_map(|key| profile[*key].as_str())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() {
        profile["email"]
            .as_str()
            .unwrap_or("Remote user")
            .to_owned()
    } else {
        name
    }
}

/// Company administrators can end an active or abandoned session without
/// removing the Agent or interrupting its signaling connection.
pub(crate) async fn close_agent_session(
    request: &Request,
    environment: &Env,
    device_id: &str,
) -> Result<Response> {
    validate_identifier(device_id, "device ID")?;
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) if identity.has_permission("agents:manage") => identity,
        Ok(_) => return api_error(403, "company administrator access is required"),
        Err(error) => return workos_auth_error(error),
    };
    let db = environment.d1("DB")?;
    let permitted = query!(&db,
        "SELECT 1 AS permitted FROM agents WHERE id = ?1 AND company_id = ?2 AND deletion_requested_at IS NULL",
        device_id, identity.company_id
    )?.metered_first::<i64>(Some("permitted")).await?.is_some();
    if !permitted {
        return api_error(404, "Agent not found");
    }
    let close = Request::new("https://agent.internal/close-session", Method::Post)?;
    let response = object_stub(environment, "AGENT_COORDINATOR", device_id)?
        .fetch_with_request(close)
        .await?;
    if !(200..300).contains(&response.status_code()) {
        return api_error(502, "The remote session could not be closed. Try again.");
    }
    audit(
        &db,
        &identity,
        "remote.session_close",
        "agent",
        device_id,
        "{}",
    )
    .await?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uses_dashboard_name_and_email_fallback() {
        assert_eq!(
            dashboard_user_name(
                &serde_json::json!({"first_name":"Ada", "last_name":"Lovelace", "email":"ada@example.com"})
            ),
            "Ada Lovelace"
        );
        assert_eq!(
            dashboard_user_name(&serde_json::json!({"first_name":null, "email":"ada@example.com"})),
            "ada@example.com"
        );
    }
}
