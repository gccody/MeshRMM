use crate::*;

#[derive(Debug, Deserialize)]
struct CreatePlatformCompanyRequest {
    name: String,
    slug: String,
    admin_email: String,
}

#[derive(Debug, Deserialize)]
struct AssignPlatformCompanyDomainRequest {
    slug: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct PlatformCompany {
    id: String,
    name: String,
    slug: Option<String>,
    workos_organization_id: Option<String>,
    status: String,
    initial_admin_email: Option<String>,
    provisioning_error: Option<String>,
    created_at: i64,
    updated_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ProvisioningCompany {
    id: String,
    name: String,
    slug: Option<String>,
    workos_organization_id: Option<String>,
    initial_admin_email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkOsOrganization {
    id: String,
}

#[derive(Debug, Deserialize)]
struct WorkOsPermission {
    slug: String,
}

#[derive(Debug, Deserialize)]
struct WorkOsRole {
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct WorkOsInvitation {
    id: String,
    state: String,
    #[serde(default)]
    organization_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkOsInvitationList {
    data: Vec<WorkOsInvitation>,
}

#[derive(Debug, Deserialize)]
struct WorkOsCorsOrigin {
    origin: String,
}

#[derive(Debug, Deserialize)]
struct WorkOsCorsOriginList {
    data: Vec<WorkOsCorsOrigin>,
    list_metadata: WorkOsListMetadata,
}

#[derive(Debug, Deserialize)]
struct WorkOsListMetadata {
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InvitationCompany {
    slug: Option<String>,
    status: String,
}

pub(crate) async fn resolve_workos_invitation(
    request: &Request,
    environment: &Env,
) -> Result<Response> {
    let hostname = request_hostname(request)?;
    let root_domain = tenant_root_domain(environment)?;
    if hostname != format!("auth.{root_domain}")
        && !matches!(hostname.as_str(), "localhost" | "127.0.0.1")
    {
        return api_error(404, "invitation was not found");
    }
    let url = request.url()?;
    let token = url
        .query_pairs()
        .find_map(|(key, value)| (key == "invitation_token").then(|| value.into_owned()))
        .filter(|value| {
            !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        });
    let Some(token) = token else {
        return api_error(400, "invitation token is required");
    };
    let encoded_token: String = url::form_urlencoded::byte_serialize(token.as_bytes()).collect();
    let mut workos_response = workos_request(
        environment,
        Method::Get,
        &format!("/user_management/invitations/by_token/{encoded_token}"),
        None,
    )
    .await?;
    if !(200..300).contains(&workos_response.status_code()) {
        return api_error(404, "invitation was not found or has expired");
    }
    let invitation: WorkOsInvitation = workos_response.json().await?;
    if invitation.state != "pending" {
        return api_error(410, "invitation is no longer pending");
    }
    let Some(organization_id) = invitation.organization_id else {
        return api_error(404, "invitation is not assigned to a company");
    };
    let db = environment.d1("DB")?;
    let company = query!(
        &db,
        "SELECT slug, status FROM companies WHERE workos_organization_id = ?1",
        organization_id
    )?
    .first::<InvitationCompany>(None)
    .await?;
    let Some(company) = company else {
        return api_error(404, "invitation company was not found");
    };
    if !matches!(company.status.as_str(), "active" | "awaiting_admin") {
        return api_error(403, "invitation company is not active");
    }
    let slug = match company.slug.and_then(|slug| validate_slug(&slug).ok()) {
        Some(slug) => slug,
        None => return api_error(409, "invitation company has no domain"),
    };
    let headers = Headers::new();
    headers.set(
        "Location",
        &format!("https://{slug}.{root_domain}/login?invitation_token={encoded_token}"),
    )?;
    headers.set("Cache-Control", "no-store")?;
    Ok(Response::empty()?.with_status(302).with_headers(headers))
}

pub(crate) async fn list_platform_companies(
    request: &Request,
    environment: &Env,
) -> Result<Response> {
    if let Err(error) = authorize_platform_owner(request, environment).await {
        return workos_auth_error(error);
    }
    let db = environment.d1("DB")?;
    let companies = query!(
        &db,
        "SELECT id, name, slug, workos_organization_id, status, initial_admin_email, provisioning_error, created_at, updated_at FROM companies ORDER BY created_at DESC"
    )
    .all()
    .await?
    .results::<PlatformCompany>()?;
    Response::from_json(&serde_json::json!({ "companies": companies }))
}

pub(crate) async fn assign_platform_company_domain(
    request: &mut Request,
    environment: &Env,
    company_id: &str,
) -> Result<Response> {
    validate_identifier(company_id, "company ID")?;
    let actor_user_id = match authorize_platform_owner(request, environment).await {
        Ok(user_id) => user_id,
        Err(error) => return workos_auth_error(error),
    };
    let body: AssignPlatformCompanyDomainRequest = match request.json().await {
        Ok(body) => body,
        Err(_) => return api_error(400, "invalid company domain request"),
    };
    let slug = match validate_slug(&body.slug) {
        Ok(slug) => slug,
        Err(_) => return api_error(400, "invalid or reserved company slug"),
    };
    let db = environment.d1("DB")?;
    let company = match load_platform_company(&db, company_id).await {
        Ok(company) => company,
        Err(_) => return api_error(404, "company was not found"),
    };
    if company.slug.is_some() {
        return api_error(409, "company slugs are immutable after assignment");
    }
    if company.workos_organization_id.is_none() {
        return api_error(409, "the legacy company has no WorkOS organization");
    }
    if query!(
        &db,
        "SELECT id FROM companies WHERE slug = ?1 COLLATE NOCASE",
        slug
    )?
    .first::<String>(Some("id"))
    .await?
    .is_some()
    {
        return api_error(409, "that company slug is already reserved");
    }
    configure_workos_cors_origin(environment, &slug).await?;
    let hostname = format!("{slug}.{}", tenant_root_domain(environment)?);
    let now = now_ms_i64()?;
    let statements = vec![
        query!(
            &db,
            "UPDATE companies SET slug = ?1, updated_at = ?2 WHERE id = ?3 AND slug IS NULL",
            slug,
            now,
            company_id
        )?,
        query!(
            &db,
            "INSERT INTO company_domains (hostname, company_id, kind, created_at) VALUES (?1, ?2, 'primary', ?3)",
            hostname,
            company_id,
            now
        )?,
        query!(
            &db,
            "INSERT INTO platform_audit_events (id, actor_user_id, action, company_id, metadata_json, created_at) VALUES (?1, ?2, 'company.domain_assign', ?3, ?4, ?5)",
            Uuid::new_v4().to_string(),
            actor_user_id,
            company_id,
            serde_json::json!({ "slug": slug }).to_string(),
            now
        )?,
    ];
    if let Err(error) = db.batch(statements).await {
        if error.to_string().contains("UNIQUE constraint failed") {
            return api_error(409, "that company slug is already reserved");
        }
        return Err(error);
    }
    Response::from_json(&load_platform_company(&db, company_id).await?)
}

pub(crate) async fn create_platform_company(
    request: &mut Request,
    environment: &Env,
) -> Result<Response> {
    let actor_user_id = match authorize_platform_owner(request, environment).await {
        Ok(user_id) => user_id,
        Err(error) => return workos_auth_error(error),
    };
    let body: CreatePlatformCompanyRequest = match request.json().await {
        Ok(body) => body,
        Err(_) => return api_error(400, "invalid company provisioning request"),
    };
    let name = match validate_name(&body.name, "company name") {
        Ok(name) => name.to_owned(),
        Err(_) => return api_error(400, "invalid company name"),
    };
    let slug = match validate_slug(&body.slug) {
        Ok(slug) => slug,
        Err(_) => return api_error(400, "invalid or reserved company slug"),
    };
    let admin_email = match validate_email(&body.admin_email) {
        Ok(email) => email,
        Err(_) => return api_error(400, "invalid company administrator email"),
    };
    let company_id = Uuid::new_v4().to_string();
    let operation_id = Uuid::new_v4().to_string();
    let hostname = format!("{slug}.{}", tenant_root_domain(environment)?);
    let now = now_ms_i64()?;
    let db = environment.d1("DB")?;
    let statements = vec![
        query!(
            &db,
            "INSERT INTO companies (id, name, created_at, dashboard_idle_timeout_minutes, slug, status, initial_admin_email, created_by_platform_user_id, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'provisioning', ?6, ?7, ?3)",
            company_id,
            name,
            now,
            DEFAULT_DASHBOARD_IDLE_TIMEOUT_MINUTES,
            slug,
            admin_email,
            actor_user_id
        )?,
        query!(
            &db,
            "INSERT INTO company_domains (hostname, company_id, kind, created_at) VALUES (?1, ?2, 'primary', ?3)",
            hostname,
            company_id,
            now
        )?,
        query!(
            &db,
            "INSERT INTO company_provisioning_operations (id, company_id, state, created_at, updated_at) VALUES (?1, ?2, 'pending', ?3, ?3)",
            operation_id,
            company_id,
            now
        )?,
        query!(
            &db,
            "INSERT INTO platform_audit_events (id, actor_user_id, action, company_id, metadata_json, created_at) VALUES (?1, ?2, 'company.create_requested', ?3, ?4, ?5)",
            Uuid::new_v4().to_string(),
            actor_user_id,
            company_id,
            serde_json::json!({ "slug": slug, "admin_email": admin_email }).to_string(),
            now
        )?,
    ];
    if let Err(error) = db.batch(statements).await {
        let detail = error.to_string();
        if detail.contains("UNIQUE constraint failed") {
            return api_error(409, "that company slug is already reserved");
        }
        return Err(error);
    }

    match provision_company(environment, &company_id, &operation_id).await {
        Ok(company) => Response::from_json(&company).map(|response| response.with_status(201)),
        Err(error) => {
            mark_provisioning_failed(&db, &company_id, &operation_id, &error.to_string()).await?;
            api_error(
                502,
                "the company was reserved, but WorkOS provisioning failed; retry it from the platform dashboard",
            )
        }
    }
}

pub(crate) async fn retry_platform_company(
    request: &Request,
    environment: &Env,
    company_id: &str,
) -> Result<Response> {
    validate_identifier(company_id, "company ID")?;
    let actor_user_id = match authorize_platform_owner(request, environment).await {
        Ok(user_id) => user_id,
        Err(error) => return workos_auth_error(error),
    };
    let db = environment.d1("DB")?;
    let operation_id = query!(
        &db,
        "SELECT id FROM company_provisioning_operations WHERE company_id = ?1 ORDER BY created_at DESC LIMIT 1",
        company_id
    )?
    .first::<String>(Some("id"))
    .await?;
    let Some(operation_id) = operation_id else {
        return api_error(404, "company provisioning operation was not found");
    };
    query!(
        &db,
        "INSERT INTO platform_audit_events (id, actor_user_id, action, company_id, created_at) VALUES (?1, ?2, 'company.provisioning_retry', ?3, ?4)",
        Uuid::new_v4().to_string(),
        actor_user_id,
        company_id,
        now_ms_i64()?
    )?
    .run()
    .await?;
    match provision_company(environment, company_id, &operation_id).await {
        Ok(company) => Response::from_json(&company),
        Err(error) => {
            mark_provisioning_failed(&db, company_id, &operation_id, &error.to_string()).await?;
            api_error(502, "WorkOS provisioning failed again")
        }
    }
}

pub(crate) async fn suspend_platform_company(
    request: &Request,
    environment: &Env,
    company_id: &str,
) -> Result<Response> {
    validate_identifier(company_id, "company ID")?;
    let actor_user_id = match authorize_platform_owner(request, environment).await {
        Ok(user_id) => user_id,
        Err(error) => return workos_auth_error(error),
    };
    let db = environment.d1("DB")?;
    let now = now_ms_i64()?;
    let result = query!(
        &db,
        "UPDATE companies SET status = 'suspended', updated_at = ?1 WHERE id = ?2 AND slug IS NOT NULL",
        now,
        company_id
    )?
    .run()
    .await?;
    if result
        .meta()?
        .and_then(|meta| meta.changes)
        .unwrap_or_default()
        == 0
    {
        return api_error(404, "company was not found");
    }
    query!(
        &db,
        "INSERT INTO platform_audit_events (id, actor_user_id, action, company_id, created_at) VALUES (?1, ?2, 'company.suspend', ?3, ?4)",
        Uuid::new_v4().to_string(),
        actor_user_id,
        company_id,
        now
    )?
    .run()
    .await?;
    Response::empty().map(|response| response.with_status(204))
}

pub(crate) async fn activate_platform_company(
    request: &Request,
    environment: &Env,
    company_id: &str,
) -> Result<Response> {
    validate_identifier(company_id, "company ID")?;
    let actor_user_id = match authorize_platform_owner(request, environment).await {
        Ok(user_id) => user_id,
        Err(error) => return workos_auth_error(error),
    };
    let db = environment.d1("DB")?;
    let now = now_ms_i64()?;
    let result = query!(
        &db,
        "UPDATE companies SET status = 'active', updated_at = ?1 WHERE id = ?2 AND status = 'suspended' AND slug IS NOT NULL AND workos_organization_id IS NOT NULL",
        now,
        company_id
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
            "only a fully provisioned suspended company can be activated",
        );
    }
    query!(
        &db,
        "INSERT INTO platform_audit_events (id, actor_user_id, action, company_id, created_at) VALUES (?1, ?2, 'company.activate', ?3, ?4)",
        Uuid::new_v4().to_string(),
        actor_user_id,
        company_id,
        now
    )?
    .run()
    .await?;
    Response::from_json(&load_platform_company(&db, company_id).await?)
}

async fn provision_company(
    environment: &Env,
    company_id: &str,
    operation_id: &str,
) -> Result<PlatformCompany> {
    let db = environment.d1("DB")?;
    let company = query!(
        &db,
        "SELECT id, name, slug, workos_organization_id, initial_admin_email FROM companies WHERE id = ?1",
        company_id
    )?
    .first::<ProvisioningCompany>(None)
    .await?
    .ok_or_else(|| Error::RustError("company was not found".into()))?;
    let slug = company
        .slug
        .as_deref()
        .ok_or_else(|| Error::RustError("company slug is missing".into()))?;
    let admin_email = company
        .initial_admin_email
        .as_deref()
        .ok_or_else(|| Error::RustError("company administrator email is missing".into()))?;
    let now = now_ms_i64()?;
    query!(
        &db,
        "UPDATE company_provisioning_operations SET state = 'creating_workos_organization', attempt_count = attempt_count + 1, last_error = NULL, updated_at = ?1 WHERE id = ?2",
        now,
        operation_id
    )?
    .run()
    .await?;

    let organization_id = match company.workos_organization_id {
        Some(id) => id,
        None => {
            let organization =
                find_or_create_workos_organization(environment, &company.id, &company.name, slug)
                    .await?;
            query!(
                &db,
                "UPDATE companies SET workos_organization_id = ?1, updated_at = ?2 WHERE id = ?3",
                organization.id,
                now_ms_i64()?,
                company.id
            )?
            .run()
            .await?;
            organization.id
        }
    };

    query!(
        &db,
        "UPDATE company_provisioning_operations SET state = 'configuring_workos_authorization', updated_at = ?1 WHERE id = ?2",
        now_ms_i64()?,
        operation_id
    )?
    .run()
    .await?;
    ensure_workos_company_admin_role(environment).await?;

    query!(
        &db,
        "UPDATE company_provisioning_operations SET state = 'configuring_workos_origin', updated_at = ?1 WHERE id = ?2",
        now_ms_i64()?,
        operation_id
    )?
    .run()
    .await?;
    configure_workos_cors_origin(environment, slug).await?;

    query!(
        &db,
        "UPDATE company_provisioning_operations SET state = 'inviting_admin', updated_at = ?1 WHERE id = ?2",
        now_ms_i64()?,
        operation_id
    )?
    .run()
    .await?;
    let invitation =
        find_or_create_workos_invitation(environment, &organization_id, admin_email).await?;
    let status = if invitation.state == "accepted" {
        "active"
    } else {
        "awaiting_admin"
    };
    let now = now_ms_i64()?;
    db.batch(vec![
        query!(
            &db,
            "UPDATE company_provisioning_operations SET state = 'complete', workos_invitation_id = ?1, last_error = NULL, updated_at = ?2 WHERE id = ?3",
            invitation.id,
            now,
            operation_id
        )?,
        query!(
            &db,
            "UPDATE companies SET status = ?1, provisioning_error = NULL, updated_at = ?2 WHERE id = ?3",
            status,
            now,
            company.id
        )?,
    ])
    .await?;
    load_platform_company(&db, &company.id).await
}

async fn ensure_workos_company_admin_role(environment: &Env) -> Result<()> {
    const CUSTOM_PERMISSIONS: &[(&str, &str, &str)] = &[
        (
            "agents:manage",
            "Manage MeshRMM Agents",
            "Enroll, rotate, and delete company Agents",
        ),
        (
            "company:settings:manage",
            "Manage MeshRMM company settings",
            "Change company-wide MeshRMM settings",
        ),
    ];
    const REQUIRED_PERMISSIONS: &[&str] = &[
        "agents:manage",
        "company:settings:manage",
        "widgets:users-table:manage",
        "widgets:domain-verification:manage",
        "widgets:sso:manage",
    ];

    for (slug, name, description) in CUSTOM_PERMISSIONS {
        let encoded: String = url::form_urlencoded::byte_serialize(slug.as_bytes()).collect();
        if workos_get_optional::<WorkOsPermission>(
            environment,
            &format!("/authorization/permissions/{encoded}"),
        )
        .await?
        .is_none()
        {
            let permission: WorkOsPermission = workos_json(
                environment,
                Method::Post,
                "/authorization/permissions",
                Some(&serde_json::json!({
                    "slug": slug,
                    "name": name,
                    "description": description
                })),
            )
            .await?;
            if permission.slug != *slug {
                return Err(Error::RustError(
                    "WorkOS created an unexpected permission".into(),
                ));
            }
        }
    }

    let mut role =
        match workos_get_optional::<WorkOsRole>(environment, "/authorization/roles/company_admin")
            .await?
        {
            Some(role) => role,
            None => {
                workos_json(
                    environment,
                    Method::Post,
                    "/authorization/roles",
                    Some(&serde_json::json!({
                        "slug": "company_admin",
                        "name": "Company Admin",
                        "description": "Highest-permission administrator for a MeshRMM company"
                    })),
                )
                .await?
            }
        };
    for permission in REQUIRED_PERMISSIONS {
        if role
            .permissions
            .iter()
            .any(|candidate| candidate == permission)
        {
            continue;
        }
        role = workos_json(
            environment,
            Method::Post,
            "/authorization/roles/company_admin/permissions",
            Some(&serde_json::json!({ "slug": permission })),
        )
        .await?;
    }
    Ok(())
}

async fn configure_workos_cors_origin(environment: &Env, slug: &str) -> Result<()> {
    let origin = format!("https://{slug}.{}", tenant_root_domain(environment)?);
    let mut after: Option<String> = None;
    loop {
        let path = match after.as_deref() {
            Some(cursor) => {
                let encoded: String =
                    url::form_urlencoded::byte_serialize(cursor.as_bytes()).collect();
                format!("/user_management/cors_origins?limit=100&after={encoded}")
            }
            None => "/user_management/cors_origins?limit=100".to_owned(),
        };
        let origins: WorkOsCorsOriginList =
            workos_json(environment, Method::Get, &path, None).await?;
        if origins
            .data
            .iter()
            .any(|candidate| candidate.origin == origin)
        {
            return Ok(());
        }
        after = origins.list_metadata.after;
        if after.is_none() {
            break;
        }
    }
    let _: WorkOsCorsOrigin = workos_json(
        environment,
        Method::Post,
        "/user_management/cors_origins",
        Some(&serde_json::json!({ "origin": origin })),
    )
    .await?;
    Ok(())
}

async fn find_or_create_workos_organization(
    environment: &Env,
    external_id: &str,
    name: &str,
    slug: &str,
) -> Result<WorkOsOrganization> {
    let encoded_external_id: String =
        url::form_urlencoded::byte_serialize(external_id.as_bytes()).collect();
    if let Some(organization) = workos_get_optional::<WorkOsOrganization>(
        environment,
        &format!("/organizations/external_id/{encoded_external_id}"),
    )
    .await?
    {
        return Ok(organization);
    }
    workos_json(
        environment,
        Method::Post,
        "/organizations",
        Some(&serde_json::json!({
            "name": name,
            "external_id": external_id,
            "metadata": { "meshrmm_slug": slug }
        })),
    )
    .await
}

async fn find_or_create_workos_invitation(
    environment: &Env,
    organization_id: &str,
    email: &str,
) -> Result<WorkOsInvitation> {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("organization_id", organization_id)
        .append_pair("email", email)
        .finish();
    let invitations: WorkOsInvitationList = workos_json(
        environment,
        Method::Get,
        &format!("/user_management/invitations?{query}"),
        None,
    )
    .await?;
    if let Some(invitation) = invitations
        .data
        .into_iter()
        .find(|invitation| matches!(invitation.state.as_str(), "pending" | "accepted"))
    {
        return Ok(invitation);
    }
    workos_json(
        environment,
        Method::Post,
        "/user_management/invitations",
        Some(&serde_json::json!({
            "email": email,
            "organization_id": organization_id,
            "role_slug": "company_admin"
        })),
    )
    .await
}

async fn workos_get_optional<T: for<'de> Deserialize<'de>>(
    environment: &Env,
    path: &str,
) -> Result<Option<T>> {
    let mut response = workos_request(environment, Method::Get, path, None).await?;
    if response.status_code() == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&response.status_code()) {
        return Err(workos_response_error(response, path).await);
    }
    Ok(Some(response.json().await?))
}

async fn workos_json<T: for<'de> Deserialize<'de>>(
    environment: &Env,
    method: Method,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<T> {
    let mut response = workos_request(environment, method, path, body).await?;
    if !(200..300).contains(&response.status_code()) {
        return Err(workos_response_error(response, path).await);
    }
    response.json().await
}

async fn workos_request(
    environment: &Env,
    method: Method,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<Response> {
    let api_key = environment.secret("WORKOS_API_KEY")?.to_string();
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {api_key}"))?;
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(method).with_headers(headers);
    if let Some(body) = body {
        init.with_body(Some(serde_json::to_string(body)?.into()));
    }
    let request = Request::new_with_init(&format!("https://api.workos.com{path}"), &init)?;
    Fetch::Request(request).send().await
}

async fn workos_response_error(mut response: Response, path: &str) -> Error {
    let status = response.status_code();
    let detail = response.text().await.unwrap_or_default();
    Error::RustError(format!(
        "WorkOS request {path} returned HTTP {status}: {}",
        detail.chars().take(1_000).collect::<String>()
    ))
}

async fn mark_provisioning_failed(
    db: &D1Database,
    company_id: &str,
    operation_id: &str,
    error: &str,
) -> Result<()> {
    let detail = error.chars().take(1_000).collect::<String>();
    let now = now_ms_i64()?;
    db.batch(vec![
        query!(
            db,
            "UPDATE companies SET status = 'failed', provisioning_error = ?1, updated_at = ?2 WHERE id = ?3",
            detail,
            now,
            company_id
        )?,
        query!(
            db,
            "UPDATE company_provisioning_operations SET state = 'failed', last_error = ?1, updated_at = ?2 WHERE id = ?3",
            detail,
            now,
            operation_id
        )?,
    ])
    .await?;
    Ok(())
}

async fn load_platform_company(db: &D1Database, company_id: &str) -> Result<PlatformCompany> {
    query!(
        db,
        "SELECT id, name, slug, workos_organization_id, status, initial_admin_email, provisioning_error, created_at, updated_at FROM companies WHERE id = ?1",
        company_id
    )?
    .first::<PlatformCompany>(None)
    .await?
    .ok_or_else(|| Error::RustError("company was not found".into()))
}
