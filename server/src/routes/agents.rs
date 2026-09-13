use crate::*;

pub(crate) async fn create_agent_installer(
    request: &mut Request,
    environment: &Env,
) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) if identity.has_permission("agents:manage") => identity,
        Ok(_) => return api_error(403, "company administrator access is required"),
        Err(error) => return workos_auth_error(error),
    };
    let body: CreateAgentInstallerRequest = request
        .json()
        .await
        .map_err(|_| Error::RustError("invalid Agent installer request".into()))?;
    if body.platform != "windows-x64" {
        return api_error(400, "unsupported Agent installer platform");
    }

    let db = environment.d1("DB")?;
    ensure_company_exists(&db, &identity.company_id).await?;
    let now = now_ms_i64()?;
    query!(
        &db,
        "DELETE FROM agent_install_tokens WHERE expires_at <= ?1",
        now
    )?
    .run()
    .await?;
    let install_id = Uuid::new_v4().to_string();
    let install_token = random_token();
    let token_hash = sha256_hex(&install_token);
    let expires_at = Date::now().as_millis() + AGENT_INSTALL_TTL_MS;
    let expires_at_i64 =
        i64::try_from(expires_at).map_err(|_| Error::RustError("clock overflow".into()))?;
    query!(
        &db,
        "INSERT INTO agent_install_tokens (id, token_hash, company_id, created_by_user_id, platform, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        install_id,
        token_hash,
        identity.company_id,
        identity.user_id,
        body.platform,
        now,
        expires_at_i64
    )?
    .run()
    .await?;
    audit(
        &db,
        &identity,
        "agent_installer.issue",
        "agent_installer",
        &install_id,
        &serde_json::json!({ "platform": body.platform }).to_string(),
    )
    .await?;
    Response::from_json(&AgentInstallerBootstrap {
        server: canonical_company_url(&db, environment, &identity.company_id).await?,
        install_token,
        expires_at_unix_ms: expires_at,
    })
}

pub(crate) async fn redeem_agent_installer(
    request: &mut Request,
    environment: &Env,
) -> Result<Response> {
    let supplied = match bearer_token(request) {
        Ok(token) => token,
        Err(_) => return api_error(401, "Agent installer token is required"),
    };
    let mut body: RedeemAgentInstallerRequest = request
        .json()
        .await
        .map_err(|_| Error::RustError("invalid Agent installer redemption".into()))?;
    let name = validate_name(&body.name, "computer name")?.to_owned();
    let now = now_ms_i64()?;
    let db = environment.d1("DB")?;
    let token_hash = sha256_hex(&supplied);
    let request_tenant = request_tenant_company(&db, request, environment).await?;
    if request_tenant.is_none() && !is_legacy_control_plane_request(request, environment)? {
        return api_error(404, "company hostname was not found");
    }
    // Older already-downloaded installers retain one-shot enrollment. Only
    // current installers can recover a claim using their persisted private key.
    if body.redemption_key.is_empty() {
        body.redemption_key = random_token();
    }
    if body.redemption_key.len() < 32 || body.redemption_key.len() > 128 {
        return api_error(
            400,
            "installer recovery key is required; download a current installer",
        );
    }
    let key_hash = sha256_hex(&body.redemption_key);
    let tenant_id = request_tenant
        .as_ref()
        .map(|tenant| tenant.id.as_str())
        .unwrap_or("");
    let candidate_id = Uuid::new_v4().to_string();
    // The credential can be recovered only with both the installer and the
    // endpoint's private recovery key. Neither secret is stored in D1.
    let agent_token = sha256_hex(&format!(
        "meshrmm-enrollment-v1:{supplied}:{}",
        body.redemption_key
    ));
    let agent_token_hash = sha256_hex(&agent_token);
    let claim = query!(
        &db,
        "UPDATE agent_install_tokens SET used_at = COALESCE(used_at, ?1), device_id = COALESCE(device_id, ?2), computer_name = ?3, redemption_key_hash = ?4 WHERE token_hash = ?5 AND expires_at > ?1 AND (?6 = '' OR company_id = ?6) AND EXISTS (SELECT 1 FROM companies WHERE companies.id = agent_install_tokens.company_id AND companies.status IN ('active', 'awaiting_admin')) AND (used_at IS NULL OR (redemption_key_hash = ?4 AND computer_name = ?3))",
        now,
        candidate_id,
        name,
        key_hash,
        token_hash,
        tenant_id
    )?;
    let insert = query!(
        &db,
        "INSERT OR IGNORE INTO agents (id, company_id, name, auth_token_hash, created_by_user_id, created_at, updated_at) SELECT device_id, company_id, computer_name, ?1, created_by_user_id, ?2, ?2 FROM agent_install_tokens WHERE token_hash = ?3 AND redemption_key_hash = ?4 AND computer_name = ?5 AND expires_at > ?2 AND (?6 = '' OR company_id = ?6) AND EXISTS (SELECT 1 FROM companies WHERE companies.id = agent_install_tokens.company_id AND companies.status IN ('active', 'awaiting_admin'))",
        agent_token_hash,
        now,
        token_hash,
        key_hash,
        name,
        tenant_id
    )?;
    db.batch(vec![claim, insert]).await?;
    let ticket = query!(&db,
        "SELECT t.id, t.company_id, t.created_by_user_id, t.device_id FROM agent_install_tokens t JOIN agents a ON a.id = t.device_id WHERE t.token_hash = ?1 AND t.redemption_key_hash = ?2 AND t.computer_name = ?3 AND t.expires_at > ?4 AND a.deletion_requested_at IS NULL AND a.auth_token_hash = ?5 AND (?6 = '' OR t.company_id = ?6) AND EXISTS (SELECT 1 FROM companies WHERE companies.id = t.company_id AND companies.status IN ('active', 'awaiting_admin'))",
        token_hash, key_hash, name, now, agent_token_hash, tenant_id
    )?.first::<AgentInstallTokenRow>(None).await?;
    let Some(ticket) = ticket else {
        return api_error(
            401,
            "Agent installer is invalid, expired, or claimed by another endpoint",
        );
    };
    let device_id = ticket.device_id;
    let identity = Identity {
        user_id: ticket.created_by_user_id,
        company_id: ticket.company_id,
        role: None,
        roles: Vec::new(),
        permissions: Vec::new(),
    };
    audit(
        &db,
        &identity,
        "agent_installer.redeem",
        "agent",
        &device_id,
        &serde_json::json!({ "installer_id": ticket.id }).to_string(),
    )
    .await?;
    let created = company_presence::PresenceMutation::Upsert {
        agent_id: device_id.clone(),
        name: Some(name.clone()),
        connected: Some(false),
    };
    if let Err(error) = company_presence::publish(environment, &identity.company_id, &created).await
    {
        console_error!("event=agent_created_publish_failed error={}", error);
    }
    Response::from_json(&AgentConfig {
        server: canonical_company_url(&db, environment, &identity.company_id).await?,
        device_id,
        agent_token,
        update_manifest_url: update_manifest_url(environment)?,
        frames_per_second: 60,
        bitrate_bits_per_second: 12_000_000,
        json_logs: false,
    })
}

pub(crate) async fn delete_agent(
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
    let now = now_ms_i64()?;
    let result = query!(
        &db,
        "UPDATE agents SET deletion_requested_at = ?1, updated_at = ?1 WHERE id = ?2 AND company_id = ?3 AND deletion_requested_at IS NULL",
        now,
        device_id,
        identity.company_id
    )?
    .run()
    .await?;
    if result
        .meta()?
        .and_then(|meta| meta.changes)
        .unwrap_or_default()
        == 0
    {
        return api_error(404, "Agent not found");
    }
    audit(&db, &identity, "agent.delete", "agent", device_id, "{}").await?;
    let deleted = company_presence::PresenceMutation::Delete {
        agent_id: device_id.to_owned(),
    };
    if let Err(error) = company_presence::publish(environment, &identity.company_id, &deleted).await
    {
        console_error!("event=agent_deleted_publish_failed error={}", error);
    }
    if let Err(error) = crate::agent_coordinator::request_uninstall(environment, device_id).await {
        console_error!("event=agent_uninstall_notify_failed error={}", error);
    }
    Response::empty().map(|response| response.with_status(204))
}

pub(crate) async fn rotate_agent_token(
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
    let agent_token = random_token();
    let token_hash = sha256_hex(&agent_token);
    let now = now_ms_i64()?;
    let db = environment.d1("DB")?;
    let result = query!(
        &db,
        "UPDATE agents SET pending_auth_token_hash = ?1, updated_at = ?2 WHERE id = ?3 AND company_id = ?4 AND deletion_requested_at IS NULL AND pending_auth_token_hash IS NULL",
        token_hash,
        now,
        device_id,
        identity.company_id
    )?
    .run()
    .await?;
    if result
        .meta()?
        .and_then(|meta| meta.changes)
        .unwrap_or_default()
        == 0
    {
        return api_error(
            409,
            "Agent not found or a credential rotation is already pending",
        );
    }
    audit(
        &db,
        &identity,
        "agent.rotate_token",
        "agent",
        device_id,
        "{}",
    )
    .await?;
    let command = meshrmm_protocol_types::AgentCommand::RotateToken { token: agent_token };
    let request = internal_json_request("https://agent.internal/rotate-token", &command)?;
    let response = object_stub(environment, "AGENT_COORDINATOR", device_id)?
        .fetch_with_request(request)
        .await?;
    if response.status_code() != 200 {
        return api_error(
            409,
            "Agent must be online to receive its new credential; current credential remains valid",
        );
    }
    Response::from_json(
        &serde_json::json!({ "device_id": device_id, "status": "rotation_pending" }),
    )
    .map(|response| response.with_status(202))
}
