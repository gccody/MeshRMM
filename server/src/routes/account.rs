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
        "SELECT id, name FROM companies WHERE id = ?1",
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

pub(crate) async fn bootstrap_company(
    request: &mut Request,
    environment: &Env,
) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) if identity.is_admin() => identity,
        Ok(_) => return api_error(403, "company administrator access is required"),
        Err(error) => return workos_auth_error(error),
    };
    let body: BootstrapCompanyRequest = request
        .json()
        .await
        .map_err(|_| Error::RustError("invalid company request".into()))?;
    let name = validate_name(&body.name, "company name")?;
    let now = now_ms_i64()?;
    let db = environment.d1("DB")?;
    query!(
        &db,
        "INSERT INTO companies (id, name, created_at) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO NOTHING",
        identity.company_id,
        name,
        now
    )?
    .run()
    .await?;
    audit(
        &db,
        &identity,
        "company.bootstrap",
        "company",
        &identity.company_id,
        "{}",
    )
    .await?;
    account_for_identity(environment, identity).await
}
