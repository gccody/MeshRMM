use crate::*;

pub(crate) async fn account(request: &Request, environment: &Env) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) => identity,
        Err(error) => return workos_auth_error(error),
    };
    account_for_identity(environment, identity).await
}

async fn account_for_identity(environment: &Env, identity: Identity) -> Result<Response> {
    let db = environment.d1("DB")?;
    let company = query!(
        &db,
        "SELECT id, name, dashboard_idle_timeout_minutes, blackout_message, display_border, slug, status FROM companies WHERE id = ?1",
        identity.company_id
    )?
    .first::<Company>(None)
    .await?;
    Response::from_json(&AccountResponse {
        user_id: identity.user_id,
        company,
        role: identity.role,
        roles: identity.roles,
        permissions: identity.permissions,
    })
}

pub(crate) async fn update_company_settings(
    request: &mut Request,
    environment: &Env,
) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) if identity.is_company_admin() => identity,
        Ok(_) => return api_error(403, "company administrator access is required"),
        Err(error) => return workos_auth_error(error),
    };
    let body: UpdateCompanySettingsRequest = match request.json().await {
        Ok(body) => body,
        Err(_) => return api_error(400, "invalid company settings request"),
    };
    let timeout = match validate_dashboard_idle_timeout(body.dashboard_idle_timeout_minutes) {
        Ok(timeout) => timeout,
        Err(_) => {
            return api_error(
                400,
                "dashboard idle timeout must be between 5 and 1440 minutes",
            );
        }
    };
    if body
        .blackout_message
        .as_deref()
        .is_some_and(|text| !meshrmm_protocol_types::valid_blackout_message(text))
    {
        return api_error(
            400,
            "blackout message must be nonempty, at most 2048 UTF-8 bytes, and contain no control characters except newlines",
        );
    }
    let db = environment.d1("DB")?;
    let company_exists = query!(
        &db,
        "SELECT id, name, dashboard_idle_timeout_minutes, blackout_message, display_border, slug, status FROM companies WHERE id = ?1",
        identity.company_id
    )?
    .first::<Company>(None)
    .await?
    .is_some();
    if !company_exists {
        return api_error(404, "company has not been provisioned");
    }
    query!(
        &db,
        "UPDATE companies SET dashboard_idle_timeout_minutes = ?1, blackout_message = COALESCE(?2, blackout_message), display_border = COALESCE(?4, display_border) WHERE id = ?3",
        timeout,
        body.blackout_message.clone(),
        identity.company_id,
        body.display_border.map(i32::from)
    )?
    .run()
    .await?;
    audit(
        &db,
        &identity,
        "company.settings.update",
        "company",
        &identity.company_id,
        &serde_json::json!({ "dashboard_idle_timeout_minutes": timeout, "blackout_message": body.blackout_message, "display_border": body.display_border }).to_string(),
    )
    .await?;
    account_for_identity(environment, identity).await
}
