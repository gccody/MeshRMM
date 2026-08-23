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
        "SELECT id, name, dashboard_idle_timeout_minutes, slug, status FROM companies WHERE id = ?1",
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
    let db = environment.d1("DB")?;
    let company_exists = query!(
        &db,
        "SELECT id, name, dashboard_idle_timeout_minutes, slug, status FROM companies WHERE id = ?1",
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
        "UPDATE companies SET dashboard_idle_timeout_minutes = ?1 WHERE id = ?2",
        timeout,
        identity.company_id
    )?
    .run()
    .await?;
    audit(
        &db,
        &identity,
        "company.settings.update",
        "company",
        &identity.company_id,
        &serde_json::json!({ "dashboard_idle_timeout_minutes": timeout }).to_string(),
    )
    .await?;
    account_for_identity(environment, identity).await
}
