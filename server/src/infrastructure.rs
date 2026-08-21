use crate::*;

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

pub(crate) fn agent_event_websocket_url(environment: &Env) -> Result<String> {
    let api_url = public_api_url(environment)?;
    let host = api_url
        .strip_prefix("https://")
        .ok_or_else(|| Error::RustError("PUBLIC_API_URL must use HTTPS".into()))?;
    Ok(format!("wss://{host}/v1/agents/events"))
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

    if reason == "missing_organization" {
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
