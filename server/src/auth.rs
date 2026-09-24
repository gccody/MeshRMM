//! Dashboard and platform-console authorization with WorkOS access tokens.
use crate::*;
use jsonwebtoken::errors::ErrorKind;
use std::sync::Mutex;

/// WorkOS signing keys are reused for this long before they are fetched again.
const JWKS_TTL_MS: f64 = 10.0 * 60.0 * 1000.0;
/// A token signed with an unknown key refetches the keys at most this often,
/// so random key IDs cannot make every request call WorkOS.
const JWKS_REFRESH_INTERVAL_MS: f64 = 30.0 * 1000.0;
/// While WorkOS is unreachable, expired cached keys still verify tokens this long.
const JWKS_STALE_LIMIT_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

/// Why a request could not be authorized. Only token problems tell the
/// dashboard to sign in again; outages are reported as retryable.
#[derive(Debug)]
pub(crate) enum AuthError {
    /// The bearer token is missing, malformed, expired or not signed by
    /// WorkOS. The label is logged, never returned.
    InvalidToken(&'static str),
    MissingOrganization,
    PlatformOwnerRequired,
    WrongCompany,
    CompanyInactive,
    CompanyNotFound,
    /// WorkOS, D1 or the Worker's configuration failed, so the token could
    /// not be checked. The detail is logged, never returned.
    Unavailable(String),
}

impl From<Error> for AuthError {
    fn from(error: Error) -> Self {
        Self::Unavailable(error.to_string())
    }
}

impl AuthError {
    fn reason(&self) -> &'static str {
        match self {
            Self::InvalidToken(reason) => reason,
            Self::MissingOrganization => "missing_organization",
            Self::PlatformOwnerRequired => "platform_owner_required",
            Self::WrongCompany => "organization_hostname_mismatch",
            Self::CompanyInactive => "company_inactive",
            Self::CompanyNotFound => "company_not_found",
            Self::Unavailable(_) => "authorization_unavailable",
        }
    }

    fn status_and_message(&self) -> (u16, &'static str) {
        match self {
            Self::InvalidToken(_) => (
                401,
                "your WorkOS session could not be verified; sign out and sign in again",
            ),
            Self::MissingOrganization => (
                401,
                "select a WorkOS organization before accessing company data",
            ),
            Self::PlatformOwnerRequired => (403, "platform owner access is required"),
            Self::WrongCompany => (
                403,
                "this account cannot access the requested company hostname",
            ),
            Self::CompanyInactive => (403, "this company is not active"),
            Self::CompanyNotFound => (404, "company hostname was not found"),
            Self::Unavailable(_) => (
                503,
                "sign-in could not be checked right now; try again in a moment",
            ),
        }
    }
}

pub(crate) fn workos_auth_error(error: AuthError) -> Result<Response> {
    let reason = error.reason();
    if let AuthError::Unavailable(detail) = &error {
        console_error!(
            "{}",
            serde_json::json!({
                "event": "workos_auth_unavailable",
                "reason": reason,
                "detail": detail,
            })
        );
    } else {
        console_error!(
            "{}",
            serde_json::json!({
                "event": "workos_auth_rejected",
                "reason": reason,
            })
        );
    }
    let (status, message) = error.status_and_message();
    let mut response = api_error(status, message)?;
    if status == 503 {
        response.headers_mut().set("Retry-After", "5")?;
    }
    Ok(response)
}

fn token_error_reason(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidSignature => "invalid_signature",
        ErrorKind::InvalidIssuer => "invalid_issuer",
        ErrorKind::ExpiredSignature => "expired_token",
        ErrorKind::ImmatureSignature => "token_not_yet_valid",
        ErrorKind::MissingRequiredClaim(_) => "missing_required_claim",
        ErrorKind::InvalidClaimFormat(_) | ErrorKind::Json(_) => "invalid_claims_shape",
        _ => "invalid_signature_or_standard_claims",
    }
}

#[derive(Clone)]
struct CachedJwks {
    client_id: String,
    keys: JwkSet,
    fetched_at_ms: f64,
}

/// One key set per isolate. Never hold the lock across an await.
static JWKS_CACHE: Mutex<Option<CachedJwks>> = Mutex::new(None);

#[derive(Debug, PartialEq)]
enum CachedKey {
    Found,
    /// Fetch the keys again.
    Refresh,
    /// The key is unknown and the keys were fetched too recently to retry.
    Unknown,
}

fn cached_key(cache: Option<&CachedJwks>, client_id: &str, key_id: &str, now_ms: f64) -> CachedKey {
    let Some(cache) = cache.filter(|cache| cache.client_id == client_id) else {
        return CachedKey::Refresh;
    };
    let age = now_ms - cache.fetched_at_ms;
    if !(0.0..JWKS_TTL_MS).contains(&age) {
        CachedKey::Refresh
    } else if cache.keys.find(key_id).is_some() {
        CachedKey::Found
    } else if age < JWKS_REFRESH_INTERVAL_MS {
        CachedKey::Unknown
    } else {
        CachedKey::Refresh
    }
}

/// Keys still usable after a failed fetch: known, and cached recently enough.
fn stale_keys(
    cache: Option<&CachedJwks>,
    client_id: &str,
    key_id: &str,
    now_ms: f64,
) -> Option<JwkSet> {
    cache
        .filter(|cache| {
            cache.client_id == client_id
                && now_ms - cache.fetched_at_ms < JWKS_STALE_LIMIT_MS
                && cache.keys.find(key_id).is_some()
        })
        .map(|cache| cache.keys.clone())
}

fn read_jwks_cache() -> Option<CachedJwks> {
    JWKS_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

async fn fetch_jwks(client_id: &str) -> std::result::Result<JwkSet, AuthError> {
    let jwks_url = format!("https://api.workos.com/sso/jwks/{client_id}");
    let mut response = Fetch::Url(
        jwks_url
            .parse()
            .map_err(|error: url::ParseError| AuthError::Unavailable(error.to_string()))?,
    )
    .send()
    .await?;
    if !(200..300).contains(&response.status_code()) {
        return Err(AuthError::Unavailable(format!(
            "WorkOS JWKS returned HTTP {}",
            response.status_code()
        )));
    }
    Ok(response.json().await?)
}

/// The WorkOS key that signed a token, from the isolate's cache when possible.
async fn workos_signing_key(
    client_id: &str,
    key_id: &str,
) -> std::result::Result<DecodingKey, AuthError> {
    let cache = read_jwks_cache();
    let now_ms = Date::now().as_millis() as f64;
    let keys = match cached_key(cache.as_ref(), client_id, key_id, now_ms) {
        CachedKey::Found => cache.map(|cache| cache.keys).unwrap_or_default(),
        CachedKey::Unknown => return Err(AuthError::InvalidToken("signing_key_not_found")),
        CachedKey::Refresh => match fetch_jwks(client_id).await {
            Ok(keys) => {
                *JWKS_CACHE.lock().unwrap_or_else(|error| error.into_inner()) = Some(CachedJwks {
                    client_id: client_id.to_owned(),
                    keys: keys.clone(),
                    fetched_at_ms: Date::now().as_millis() as f64,
                });
                keys
            }
            Err(error) => {
                let Some(keys) = stale_keys(cache.as_ref(), client_id, key_id, now_ms) else {
                    return Err(error);
                };
                console_warn!(
                    "{}",
                    serde_json::json!({
                        "event": "workos_jwks_stale",
                        "detail": format!("{error:?}"),
                    })
                );
                keys
            }
        },
    };
    let jwk = keys
        .find(key_id)
        .ok_or(AuthError::InvalidToken("signing_key_not_found"))?;
    DecodingKey::from_jwk(jwk)
        .map_err(|_| AuthError::Unavailable("invalid WorkOS signing key".into()))
}

pub(crate) async fn authorize_workos_user(
    request: &Request,
    environment: &Env,
) -> std::result::Result<Identity, AuthError> {
    let claims = authorize_workos_claims(request, environment).await?;
    let workos_organization_id = claims
        .org_id
        .as_deref()
        .ok_or(AuthError::MissingOrganization)?;
    validate_identifier(workos_organization_id, "WorkOS organization ID")
        .map_err(|_| AuthError::InvalidToken("invalid_organization"))?;
    let db = environment.d1("DB")?;
    let company = company_for_request(&db, request, environment, Some(workos_organization_id))
        .await?
        .ok_or(AuthError::CompanyNotFound)?;
    if company.status != "active" && company.status != "awaiting_admin" {
        return Err(AuthError::CompanyInactive);
    }
    if company.workos_organization_id.as_deref() != Some(workos_organization_id) {
        return Err(AuthError::WrongCompany);
    }
    if company.status == "awaiting_admin" {
        query!(
            &db,
            "UPDATE companies SET status = 'active', provisioning_error = NULL, updated_at = ?1 WHERE id = ?2 AND status = 'awaiting_admin'",
            now_ms_i64()?,
            company.id
        )?
        .run()
        .await?;
    }
    Ok(Identity {
        user_id: claims.sub,
        company_id: company.id,
        role: claims.role,
        roles: claims.roles,
        permissions: claims.permissions,
    })
}

async fn authorize_workos_claims(
    request: &Request,
    environment: &Env,
) -> std::result::Result<WorkOsClaims, AuthError> {
    let token =
        bearer_token(request).map_err(|_| AuthError::InvalidToken("missing_bearer_token"))?;
    let client_id = environment.var("WORKOS_CLIENT_ID")?.to_string();
    validate_identifier(&client_id, "WorkOS client ID")?;
    let header =
        decode_header(&token).map_err(|_| AuthError::InvalidToken("invalid_token_header"))?;
    if header.alg != Algorithm::RS256 {
        return Err(AuthError::InvalidToken("unsupported_algorithm"));
    }
    let key_id = header
        .kid
        .ok_or(AuthError::InvalidToken("missing_key_id"))?;
    let decoding_key = workos_signing_key(&client_id, &key_id).await?;

    let issuer = environment
        .var("WORKOS_ISSUER")
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "https://api.workos.com".to_owned());
    let issuer_with_slash = format!("{}/", issuer.trim_end_matches('/'));
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_aud = false;
    validation.set_required_spec_claims(&["exp", "iss", "sub"]);
    validation.set_issuer(&[issuer.as_str(), issuer_with_slash.as_str()]);
    let claims = decode::<WorkOsClaims>(&token, &decoding_key, &validation)
        .map_err(|error| AuthError::InvalidToken(token_error_reason(error.kind())))?
        .claims;
    if claims.client_id != client_id
        || claims.iss.trim_end_matches('/') != issuer.trim_end_matches('/')
        || claims.exp * 1000 <= Date::now().as_millis()
    {
        return Err(AuthError::InvalidToken("client_or_issuer_mismatch"));
    }
    Ok(claims)
}

pub(crate) async fn authorize_platform_owner(
    request: &Request,
    environment: &Env,
) -> std::result::Result<String, AuthError> {
    let hostname = request_hostname(request)?;
    let expected_hostname = format!("admin.{}", tenant_root_domain(environment)?);
    if hostname != expected_hostname && !matches!(hostname.as_str(), "localhost" | "127.0.0.1") {
        return Err(AuthError::PlatformOwnerRequired);
    }
    let claims = authorize_workos_claims(request, environment).await?;
    let owners = environment.var("PLATFORM_OWNER_USER_IDS")?.to_string();
    if !owners
        .split(',')
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
        .any(|owner| owner == claims.sub)
    {
        return Err(AuthError::PlatformOwnerRequired);
    }
    Ok(claims.sub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::errors::Error as JwtError;

    fn key_set(key_ids: &[&str]) -> JwkSet {
        let keys: Vec<_> = key_ids
            .iter()
            .map(|kid| serde_json::json!({"kty": "RSA", "kid": kid, "alg": "RS256", "n": "AQAB", "e": "AQAB"}))
            .collect();
        serde_json::from_value(serde_json::json!({ "keys": keys })).unwrap()
    }

    fn cache(fetched_at_ms: f64) -> CachedJwks {
        CachedJwks {
            client_id: "client".into(),
            keys: key_set(&["known"]),
            fetched_at_ms,
        }
    }

    #[test]
    fn only_token_problems_ask_the_user_to_sign_in_again() {
        for error in [
            AuthError::InvalidToken("expired_token"),
            AuthError::MissingOrganization,
        ] {
            assert_eq!(error.status_and_message().0, 401, "{error:?}");
        }
        let outage = AuthError::from(Error::RustError("D1_ERROR: network lost".into()));
        assert_eq!(outage.status_and_message().0, 503);
        assert!(!outage.status_and_message().1.contains("sign in"));
        assert_eq!(AuthError::PlatformOwnerRequired.status_and_message().0, 403);
        assert_eq!(AuthError::WrongCompany.status_and_message().0, 403);
        assert_eq!(AuthError::CompanyInactive.status_and_message().0, 403);
        assert_eq!(AuthError::CompanyNotFound.status_and_message().0, 404);
    }

    #[test]
    fn token_validation_errors_keep_their_reasons() {
        for (kind, reason) in [
            (ErrorKind::InvalidSignature, "invalid_signature"),
            (ErrorKind::ExpiredSignature, "expired_token"),
            (ErrorKind::InvalidIssuer, "invalid_issuer"),
            (
                ErrorKind::MissingRequiredClaim("exp".into()),
                "missing_required_claim",
            ),
            (
                ErrorKind::InvalidToken,
                "invalid_signature_or_standard_claims",
            ),
        ] {
            assert_eq!(token_error_reason(JwtError::from(kind).kind()), reason);
        }
    }

    #[test]
    fn signing_keys_come_from_the_cache_until_it_expires() {
        let now = 1_000_000.0;
        assert_eq!(cached_key(None, "client", "known", now), CachedKey::Refresh);
        let fresh = cache(now - 1_000.0);
        assert_eq!(
            cached_key(Some(&fresh), "client", "known", now),
            CachedKey::Found
        );
        assert_eq!(
            cached_key(Some(&fresh), "other-client", "known", now),
            CachedKey::Refresh
        );
        let expired = cache(now - JWKS_TTL_MS);
        assert_eq!(
            cached_key(Some(&expired), "client", "known", now),
            CachedKey::Refresh
        );
        let future = cache(now + 1_000.0);
        assert_eq!(
            cached_key(Some(&future), "client", "known", now),
            CachedKey::Refresh
        );
    }

    #[test]
    fn unknown_signing_keys_refetch_at_most_every_interval() {
        let now = 1_000_000.0;
        let recent = cache(now - 1_000.0);
        assert_eq!(
            cached_key(Some(&recent), "client", "rotated", now),
            CachedKey::Unknown
        );
        let older = cache(now - JWKS_REFRESH_INTERVAL_MS);
        assert_eq!(
            cached_key(Some(&older), "client", "rotated", now),
            CachedKey::Refresh
        );
    }

    #[test]
    fn known_keys_outlive_a_workos_outage_for_a_day() {
        let now = 100_000_000.0;
        let expired = cache(now - JWKS_TTL_MS * 2.0);
        assert!(stale_keys(Some(&expired), "client", "known", now).is_some());
        assert!(stale_keys(Some(&expired), "client", "rotated", now).is_none());
        assert!(stale_keys(Some(&expired), "other-client", "known", now).is_none());
        let too_old = cache(now - JWKS_STALE_LIMIT_MS);
        assert!(stale_keys(Some(&too_old), "client", "known", now).is_none());
    }
}
