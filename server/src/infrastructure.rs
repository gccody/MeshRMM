use crate::*;

const RESERVED_TENANT_SLUGS: &[&str] = &[
    "admin",
    "api",
    "auth",
    "downloads",
    "status",
    "support",
    "www",
];

pub(crate) fn validate_identifier(value: &str, label: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    valid
        .then_some(())
        .ok_or_else(|| Error::RustError(format!("invalid {label}")))
}

pub(crate) fn validate_name<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    let value = value.trim();
    if !value.is_empty() && value.len() <= 120 && !value.chars().any(char::is_control) {
        Ok(value)
    } else {
        Err(Error::RustError(format!("invalid {label}")))
    }
}

pub(crate) fn validate_slug(value: &str) -> Result<String> {
    let slug = value.trim().to_ascii_lowercase();
    let valid = (2..=63).contains(&slug.len())
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !RESERVED_TENANT_SLUGS.contains(&slug.as_str());
    valid
        .then_some(slug)
        .ok_or_else(|| Error::RustError("invalid or reserved company slug".into()))
}

pub(crate) fn validate_email(value: &str) -> Result<String> {
    let email = value.trim().to_ascii_lowercase();
    let valid = email.len() <= 254
        && !email.chars().any(char::is_control)
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && !domain.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
    valid
        .then_some(email)
        .ok_or_else(|| Error::RustError("invalid company administrator email".into()))
}

pub(crate) fn tenant_root_domain(environment: &Env) -> Result<String> {
    Ok(environment
        .var("TENANT_ROOT_DOMAIN")?
        .to_string()
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase())
}

pub(crate) fn request_hostname(request: &Request) -> Result<String> {
    request
        .url()?
        .host_str()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| Error::RustError("request hostname is missing".into()))
}

pub(crate) fn is_legacy_control_plane_request(
    request: &Request,
    environment: &Env,
) -> Result<bool> {
    let hostname = request_hostname(request)?;
    let legacy_api_host = public_api_url(environment)?
        .parse::<url::Url>()
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
    Ok(legacy_api_host.as_deref() == Some(hostname.as_str())
        || matches!(hostname.as_str(), "localhost" | "127.0.0.1"))
}

fn tenant_slug_from_hostname<'a>(hostname: &'a str, root_domain: &str) -> Option<&'a str> {
    let suffix = format!(".{root_domain}");
    let slug = hostname.strip_suffix(&suffix)?;
    (!slug.is_empty() && !slug.contains('.')).then_some(slug)
}

pub(crate) async fn company_for_request(
    db: &D1Database,
    request: &Request,
    environment: &Env,
    workos_organization_id: Option<&str>,
) -> Result<Option<TenantCompany>> {
    let hostname = request_hostname(request)?;
    let root_domain = tenant_root_domain(environment)?;
    if let Some(slug) = tenant_slug_from_hostname(&hostname, &root_domain) {
        return query!(
            db,
            "SELECT id, workos_organization_id, status FROM companies WHERE slug = ?1 COLLATE NOCASE",
            slug
        )?
        .first::<TenantCompany>(None)
        .await;
    }

    if !is_legacy_control_plane_request(request, environment)? {
        return Ok(None);
    }
    let Some(workos_organization_id) = workos_organization_id else {
        return Ok(None);
    };
    query!(
        db,
        "SELECT id, workos_organization_id, status FROM companies WHERE workos_organization_id = ?1",
        workos_organization_id
    )?
    .first::<TenantCompany>(None)
    .await
}

pub(crate) async fn request_tenant_company(
    db: &D1Database,
    request: &Request,
    environment: &Env,
) -> Result<Option<TenantCompany>> {
    let hostname = request_hostname(request)?;
    let root_domain = tenant_root_domain(environment)?;
    let Some(slug) = tenant_slug_from_hostname(&hostname, &root_domain) else {
        return Ok(None);
    };
    query!(
        db,
        "SELECT id, workos_organization_id, status FROM companies WHERE slug = ?1 COLLATE NOCASE",
        slug
    )?
    .first::<TenantCompany>(None)
    .await
}

pub(crate) async fn canonical_company_url(
    db: &D1Database,
    environment: &Env,
    company_id: &str,
) -> Result<String> {
    #[derive(Deserialize)]
    struct CompanySlug {
        slug: Option<String>,
    }
    let company = query!(db, "SELECT slug FROM companies WHERE id = ?1", company_id)?
        .first::<CompanySlug>(None)
        .await?
        .ok_or_else(|| Error::RustError("company has not been provisioned".into()))?;
    match company.slug {
        Some(slug) => Ok(format!(
            "https://{slug}.{}",
            tenant_root_domain(environment)?
        )),
        None => public_api_url(environment),
    }
}

pub(crate) fn validate_dashboard_idle_timeout(value: u32) -> Result<u32> {
    if (MIN_DASHBOARD_IDLE_TIMEOUT_MINUTES..=MAX_DASHBOARD_IDLE_TIMEOUT_MINUTES).contains(&value) {
        Ok(value)
    } else {
        Err(Error::RustError(format!(
            "dashboard idle timeout must be between {MIN_DASHBOARD_IDLE_TIMEOUT_MINUTES} and {MAX_DASHBOARD_IDLE_TIMEOUT_MINUTES} minutes"
        )))
    }
}

pub(crate) fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub(crate) fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let left = Sha256::digest(left);
    let right = Sha256::digest(right);
    bool::from(left.ct_eq(&right))
}

pub(crate) fn bearer_token(request: &Request) -> Result<String> {
    request
        .headers()
        .get("Authorization")?
        .and_then(|value| value.strip_prefix("Bearer ").map(str::to_owned))
        .ok_or_else(|| Error::RustError("missing bearer token".into()))
}

pub(crate) fn public_api_url(environment: &Env) -> Result<String> {
    Ok(environment
        .var("PUBLIC_API_URL")?
        .to_string()
        .trim_end_matches('/')
        .to_owned())
}

pub(crate) fn update_manifest_url(environment: &Env) -> Result<String> {
    let dashboard = environment
        .var("DASHBOARD_ORIGIN")?
        .to_string()
        .trim_end_matches('/')
        .to_owned();
    Ok(format!("{dashboard}/downloads/update-manifest.json"))
}

pub(crate) fn agent_event_websocket_url(api_url: &str) -> Result<String> {
    let host = api_url
        .strip_prefix("https://")
        .ok_or_else(|| Error::RustError("Agent event URL must use HTTPS".into()))?;
    Ok(format!("wss://{host}/v1/agents/events"))
}

pub(crate) fn expected_request_origin(request: &Request, environment: &Env) -> Result<String> {
    if is_legacy_control_plane_request(request, environment)? {
        return Ok(environment.var("DASHBOARD_ORIGIN")?.to_string());
    }
    Ok(format!("https://{}", request_hostname(request)?))
}

pub(crate) fn now_ms_i64() -> Result<i64> {
    i64::try_from(Date::now().as_millis()).map_err(|_| Error::RustError("clock overflow".into()))
}

pub(crate) fn session_idle_timeout(environment: &Env) -> Result<u64> {
    let timeout = environment
        .var("REMOTE_SESSION_IDLE_TIMEOUT_SECONDS")
        // Preserve deployments that still inject the former variable name
        // outside wrangler.jsonc while changing its semantics to idle time.
        .or_else(|_| environment.var("REMOTE_SESSION_TTL_SECONDS"))
        .ok()
        .and_then(|value| value.to_string().parse().ok())
        .unwrap_or(DEFAULT_SESSION_IDLE_TIMEOUT_SECONDS);
    if (60..=MAX_SESSION_IDLE_TIMEOUT_SECONDS).contains(&timeout) {
        Ok(timeout)
    } else {
        Err(Error::RustError(
            "REMOTE_SESSION_IDLE_TIMEOUT_SECONDS must be between 60 and 3600".into(),
        ))
    }
}

pub(crate) async fn generate_ice_servers(
    environment: &Env,
    ttl_seconds: u64,
) -> Result<Vec<IceServer>> {
    let key_id = environment.secret("TURN_KEY_ID")?.to_string();
    let api_token = environment.secret("TURN_KEY_API_TOKEN")?.to_string();
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {api_token}"))?;
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(
            serde_json::to_string(&serde_json::json!({ "ttl": ttl_seconds }))?.into(),
        ));
    let request = Request::new_with_init(
        &format!(
            "https://rtc.live.cloudflare.com/v1/turn/keys/{key_id}/credentials/generate-ice-servers"
        ),
        &init,
    )?;
    let mut response = Fetch::Request(request).send().await?;
    if !(200..300).contains(&response.status_code()) {
        return Err(Error::RustError(format!(
            "TURN credential service returned HTTP {}",
            response.status_code()
        )));
    }
    let mut response: TurnResponse = response.json().await?;
    for server in &mut response.ice_servers {
        server.urls.retain(|url| supported_ice_url(url));
    }
    response
        .ice_servers
        .retain(|server| !server.urls.is_empty());
    if response.ice_servers.is_empty() {
        return Err(Error::RustError(
            "TURN credential service returned no WebRTC-compatible ICE servers".into(),
        ));
    }
    Ok(response.ice_servers)
}

pub(crate) fn supported_ice_url(url: &str) -> bool {
    url == "stun:stun.cloudflare.com:3478" || url == "turn:turn.cloudflare.com:3478?transport=udp"
}

pub(crate) fn object_stub(environment: &Env, binding: &str, name: &str) -> Result<Stub> {
    environment
        .durable_object(binding)?
        .id_from_name(name)?
        .get_stub()
}

pub(crate) async fn forward_to_object(
    environment: &Env,
    binding: &str,
    name: &str,
    request: Request,
    internal_url: &str,
    internal_headers: &[(&str, &str)],
) -> Result<Response> {
    let headers = request.headers().clone();
    for (name, value) in internal_headers {
        headers.set(name, value)?;
    }
    let mut init = RequestInit::new();
    init.with_method(request.method()).with_headers(headers);
    let internal = Request::new_with_init(internal_url, &init)?;
    object_stub(environment, binding, name)?
        .fetch_with_request(internal)
        .await
}

pub(crate) fn internal_json_request<T: Serialize>(url: &str, value: &T) -> Result<Request> {
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(serde_json::to_string(value)?.into()));
    Request::new_with_init(url, &init)
}

pub(crate) async fn ensure_success(mut response: Response, action: &str) -> Result<()> {
    if (200..300).contains(&response.status_code()) {
        return Ok(());
    }
    let status = response.status_code();
    let body = response.text().await.unwrap_or_default();
    Err(Error::RustError(format!(
        "failed to {action}: HTTP {status}: {body}"
    )))
}

pub(crate) fn api_error(status: u16, message: &str) -> Result<Response> {
    Ok(Response::from_json(&ApiError {
        error: message.to_owned(),
    })?
    .with_status(status))
}

pub(crate) fn workos_auth_error(error: Error) -> Result<Response> {
    let detail = error.to_string();
    let reason = if detail.contains("no organization") {
        "missing_organization"
    } else if detail.contains("authorization header") || detail.contains("bearer") {
        "missing_bearer_token"
    } else if detail.contains("unsupported WorkOS token algorithm") {
        "unsupported_algorithm"
    } else if detail.contains("missing a key ID") {
        "missing_key_id"
    } else if detail.contains("JWKS returned HTTP") {
        "jwks_request_failed"
    } else if detail.contains("signing key was not found") {
        "signing_key_not_found"
    } else if detail.contains("invalid WorkOS signing key") {
        "invalid_signing_key"
    } else if detail.contains("invalid WorkOS access token claims") {
        "client_or_issuer_mismatch"
    } else if detail.contains("invalid WorkOS access token") {
        if detail.contains("InvalidSignature") || detail.contains("Invalid signature") {
            "invalid_signature"
        } else if detail.contains("InvalidIssuer") || detail.contains("Invalid issuer") {
            "invalid_issuer"
        } else if detail.contains("ExpiredSignature") || detail.contains("Expired signature") {
            "expired_token"
        } else if detail.contains("MissingRequiredClaim")
            || detail.contains("Missing required claim")
        {
            "missing_required_claim"
        } else if detail.contains("JSON") || detail.contains("Json") {
            "invalid_claims_shape"
        } else {
            "invalid_signature_or_standard_claims"
        }
    } else {
        "unexpected_validation_error"
    };

    console_error!(
        "{}",
        serde_json::json!({
            "event": "workos_auth_rejected",
            "reason": reason,
        })
    );

    if detail.contains("platform owner access is required")
        || detail.contains("platform owner access is restricted")
    {
        api_error(403, "platform owner access is required")
    } else if detail.contains("does not match the company hostname") {
        api_error(
            403,
            "this account cannot access the requested company hostname",
        )
    } else if detail.contains("company is not active") {
        api_error(403, "this company is not active")
    } else if detail.contains("company has not been provisioned") {
        api_error(404, "company hostname was not found")
    } else if reason == "missing_organization" {
        api_error(
            401,
            "select a WorkOS organization before accessing company data",
        )
    } else {
        api_error(
            401,
            "your WorkOS session could not be verified; sign out and sign in again",
        )
    }
}
