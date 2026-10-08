//! Single sign-on with an OpenID Connect provider: its settings, and the
//! authorization code flow (with PKCE and a nonce) that identifies a user.
//!
//! The provider's metadata and keys are fetched when a sign-in starts, so a
//! key rotation at the provider needs nothing from the operator.
use std::{sync::OnceLock, time::Duration};

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use openidconnect::{
    AccessToken, AuthorizationCode, ClientId, ClientSecret, CsrfToken, HttpRequest, HttpResponse,
    IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use sea_query::{Expr, ExprTrait, Order, Query};
use serde_json::Value;

use crate::{
    config::Config,
    db::{
        self, Executor,
        tables::{OidcGroupRoles, OidcProvider},
    },
    secrets::{InstanceKey, new_token, token_hash},
};

/// Where the provider sends the browser back to. Administrators register it
/// with the provider.
pub const CALLBACK_PATH: &str = "/v1/auth/sso/callback";
/// The encryption context of the stored client secret.
pub const SECRET_CONTEXT: &str = "oidc:client_secret";
pub const MAX_GROUPS: usize = 500;
pub const MAX_GROUP_NAME_LENGTH: usize = 256;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Discovery documents, key sets and tokens are small.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// The provider's settings: the single `oidc_provider` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Provider {
    pub enabled: bool,
    pub display_name: String,
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret_encrypted: Option<Vec<u8>>,
    pub scopes: String,
    pub auto_provision: bool,
    pub default_role_id: Option<String>,
    pub require_verified_email: bool,
    pub groups_claim: Option<String>,
    pub updated_at: i64,
}

pub async fn load(executor: &mut impl Executor) -> db::Result<Option<Provider>> {
    executor
        .fetch_optional(
            &Query::select()
                .columns([
                    OidcProvider::Enabled,
                    OidcProvider::DisplayName,
                    OidcProvider::IssuerUrl,
                    OidcProvider::ClientId,
                    OidcProvider::ClientSecretEncrypted,
                    OidcProvider::Scopes,
                    OidcProvider::AutoProvision,
                    OidcProvider::DefaultRoleId,
                    OidcProvider::RequireVerifiedEmail,
                    OidcProvider::GroupsClaim,
                    OidcProvider::UpdatedAt,
                ])
                .from(OidcProvider::Table)
                .and_where(Expr::col(OidcProvider::Id).eq(1))
                .to_owned(),
        )
        .await
}

/// The provider, if SSO is turned on.
pub async fn enabled(executor: &mut impl Executor) -> db::Result<Option<Provider>> {
    Ok(load(executor).await?.filter(|provider| provider.enabled))
}

/// Which SSO groups grant which roles, as `(group, role ID)` pairs.
pub async fn group_roles(executor: &mut impl Executor) -> db::Result<Vec<(String, String)>> {
    executor
        .fetch_all(
            &Query::select()
                .columns([OidcGroupRoles::GroupName, OidcGroupRoles::RoleId])
                .from(OidcGroupRoles::Table)
                .order_by(OidcGroupRoles::GroupName, Order::Asc)
                .to_owned(),
        )
        .await
}

pub fn redirect_uri(config: &Config) -> String {
    format!("{}{CALLBACK_PATH}", config.public_origin())
}

/// Checks an issuer URL. It must use HTTPS, except on the loopback interface
/// (a provider on the same machine, or a test).
pub fn parse_issuer(raw: &str) -> Result<IssuerUrl, &'static str> {
    let url = url::Url::parse(raw.trim()).map_err(|_| "the issuer must be a URL")?;
    let loopback = match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback)) {
        return Err("the issuer must be an https:// URL");
    }
    if url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() {
        return Err("the issuer URL can't have a query, fragment or credentials");
    }
    // Discovery compares the issuer the provider reports with this text, so
    // keep it exactly as entered.
    IssuerUrl::new(raw.trim().to_owned()).map_err(|_| "the issuer must be a URL")
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HttpError(String);

/// The client for every request to the provider. It never follows
/// redirects, so a provider's answer can't point the server elsewhere.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .user_agent(concat!("MeshRMM/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("the HTTP client's settings are valid")
    })
}

async fn send(request: HttpRequest) -> Result<HttpResponse, HttpError> {
    let request =
        reqwest::Request::try_from(request).map_err(|error| HttpError(error.to_string()))?;
    let url = request.url().clone();
    let mut response = http_client()
        .execute(request)
        .await
        .map_err(|error| HttpError(format!("could not reach {url}: {error}")))?;
    let mut builder = http::Response::builder().status(response.status());
    for (name, value) in response.headers() {
        builder = builder.header(name, value);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| HttpError(format!("{url} failed: {error}")))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(HttpError(format!("{url} sent too much data")));
        }
        body.extend_from_slice(&chunk);
    }
    builder
        .body(body)
        .map_err(|error| HttpError(error.to_string()))
}

/// Fetches the provider's metadata and signing keys.
pub async fn discover(issuer_url: &str) -> anyhow::Result<CoreProviderMetadata> {
    let issuer = parse_issuer(issuer_url).map_err(|message| anyhow!(message))?;
    let http = |request| send(request);
    CoreProviderMetadata::discover_async(issuer, &http)
        .await
        .map_err(|error| anyhow!(error_chain(&error)))
        .context("the provider's discovery document could not be used")
}

/// An error and its causes on one line, for messages administrators read.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

fn client(
    config: &Config,
    key: &InstanceKey,
    provider: &Provider,
    metadata: CoreProviderMetadata,
) -> anyhow::Result<
    CoreClient<
        openidconnect::EndpointSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointMaybeSet,
        openidconnect::EndpointMaybeSet,
    >,
> {
    let secret = provider
        .client_secret_encrypted
        .as_deref()
        .map(|sealed| key.decrypt(SECRET_CONTEXT, sealed))
        .transpose()?
        .map(|secret| ClientSecret::new(String::from_utf8_lossy(&secret).into_owned()));
    Ok(CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(provider.client_id.clone()),
        secret,
    )
    .set_redirect_uri(RedirectUrl::new(redirect_uri(config))?))
}

/// A sign-in waiting at the provider. The ceremony token is the OAuth
/// `state`; the browser that started it also holds `binding`, so a
/// callback URL replayed in another browser does nothing.
#[derive(Debug)]
pub struct Pending {
    binding_hash: String,
    pkce_verifier: String,
    nonce: String,
    /// Where on the website to go afterwards.
    pub next: String,
    metadata: CoreProviderMetadata,
    provider_updated_at: i64,
}

impl Pending {
    /// Whether `binding` (from the browser's cookie) belongs to this sign-in.
    pub fn bound_to(&self, binding: &str) -> bool {
        use subtle::ConstantTimeEq;
        self.binding_hash
            .as_bytes()
            .ct_eq(token_hash(binding).as_bytes())
            .into()
    }
}

/// A started sign-in: where to send the browser, and the value for its
/// binding cookie.
#[derive(Debug)]
pub struct Started {
    pub authorize_url: url::Url,
    pub binding: String,
}

/// Starts a sign-in with the provider. `store` keeps the pending state and
/// returns the token naming it, which becomes the OAuth `state`.
pub async fn begin(
    config: &Config,
    key: &InstanceKey,
    provider: &Provider,
    next: String,
    store: impl FnOnce(Pending) -> String,
) -> anyhow::Result<Started> {
    let metadata = discover(&provider.issuer_url).await?;
    let client = client(config, key, provider, metadata.clone())?;
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let nonce = Nonce::new_random();
    let binding = new_token();
    let state = store(Pending {
        binding_hash: token_hash(&binding),
        pkce_verifier: pkce_verifier.secret().clone(),
        nonce: nonce.secret().clone(),
        next,
        metadata,
        provider_updated_at: provider.updated_at,
    });
    let mut request = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            move || CsrfToken::new(state),
            move || nonce,
        )
        .set_pkce_challenge(pkce_challenge);
    for scope in provider.scopes.split_whitespace() {
        // `openid` is always requested.
        if scope != "openid" {
            request = request.add_scope(Scope::new(scope.to_owned()));
        }
    }
    let (authorize_url, _, _) = request.url();
    Ok(Started {
        authorize_url,
        binding,
    })
}

/// Who the provider says signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
    /// The groups claim, if the provider is set up to read one.
    pub groups: Option<Vec<String>>,
}

/// Exchanges the authorization code and verifies the ID token.
pub async fn complete(
    config: &Config,
    key: &InstanceKey,
    provider: &Provider,
    pending: Pending,
    code: &str,
) -> anyhow::Result<Identity> {
    if pending.provider_updated_at != provider.updated_at {
        bail!("the SSO settings changed during the sign-in");
    }
    let client = client(config, key, provider, pending.metadata)?;
    let http = |request| send(request);
    let response = client
        .exchange_code(AuthorizationCode::new(code.to_owned()))?
        .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier))
        .request_async(&http)
        .await
        .map_err(|error| anyhow!(error_chain(&error)))
        .context("the provider refused the authorization code")?;
    let id_token = response
        .id_token()
        .context("the provider's token response has no ID token")?;
    let verifier = client.id_token_verifier();
    let claims = id_token
        .claims(&verifier, &Nonce::new(pending.nonce))
        .map_err(|error| anyhow!(error_chain(&error)))
        .context("the ID token is invalid")?;
    if let Some(expected) = claims.access_token_hash() {
        let signing_algorithm = id_token
            .signing_alg()
            .map_err(|error| anyhow!(error_chain(&error)))?;
        let signing_key = id_token
            .signing_key(&verifier)
            .map_err(|error| anyhow!(error_chain(&error)))?;
        let actual = openidconnect::AccessTokenHash::from_token(
            response.access_token(),
            signing_algorithm,
            signing_key,
        )
        .map_err(|error| anyhow!(error_chain(&error)))?;
        if actual != *expected {
            bail!("the ID token doesn't match the access token");
        }
    }
    // The claims were verified above; this reads the ones the typed claims
    // don't carry, such as groups.
    let raw = id_token_payload(&id_token.to_string())?;
    let mut identity = identity_from_claims(&raw, provider.groups_claim.as_deref());
    identity.subject = claims.subject().as_str().to_owned();
    let wants_userinfo =
        identity.email.is_none() || (provider.groups_claim.is_some() && identity.groups.is_none());
    if wants_userinfo && let Some(userinfo_url) = client.user_info_url() {
        match userinfo(userinfo_url.url(), response.access_token()).await {
            Ok(info) if info.get("sub").and_then(Value::as_str) == Some(&identity.subject) => {
                let extra = identity_from_claims(&info, provider.groups_claim.as_deref());
                if identity.email.is_none() {
                    identity.email = extra.email;
                    identity.email_verified = extra.email_verified;
                }
                identity.name = identity.name.or(extra.name);
                identity.groups = identity.groups.or(extra.groups);
            }
            Ok(_) => tracing::warn!("ignored userinfo for a different subject"),
            Err(error) => tracing::warn!(error = format!("{error:#}"), "could not read userinfo"),
        }
    }
    if identity.email.is_none() && !provider.require_verified_email {
        // Microsoft Entra ID sends the sign-in name here when it has no
        // `email` claim.
        identity.email = raw
            .get("preferred_username")
            .and_then(Value::as_str)
            .filter(|name| name.contains('@'))
            .map(str::to_owned);
    }
    Ok(identity)
}

/// The JSON payload of a JWT, without checking anything.
fn id_token_payload(jwt: &str) -> anyhow::Result<Value> {
    let payload = jwt.split('.').nth(1).context("the ID token is malformed")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("the ID token is malformed")?;
    serde_json::from_slice(&bytes).context("the ID token is malformed")
}

async fn userinfo(url: &url::Url, access_token: &AccessToken) -> anyhow::Result<Value> {
    let request = http::Request::builder()
        .uri(url.as_str())
        .header(http::header::ACCEPT, "application/json")
        .header(
            http::header::AUTHORIZATION,
            format!("Bearer {}", access_token.secret()),
        )
        .body(Vec::new())?;
    let response = send(request).await?;
    if !response.status().is_success() {
        bail!("userinfo answered {}", response.status());
    }
    serde_json::from_slice(response.body()).context("userinfo isn't JSON")
}

/// Reads the email, name and groups from ID token or userinfo claims.
fn identity_from_claims(claims: &Value, groups_claim: Option<&str>) -> Identity {
    let text = |name: &str| {
        claims
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let flag = |name: &str| match claims.get(name) {
        Some(Value::Bool(set)) => *set,
        // Some providers (Amazon Cognito) send booleans as strings.
        Some(Value::String(set)) => set.eq_ignore_ascii_case("true") || set == "1",
        Some(Value::Number(set)) => set.as_i64() == Some(1),
        _ => false,
    };
    // Microsoft Entra ID has no `email_verified`; its optional `xms_edov`
    // claim says the email's domain is verified for the tenant.
    let email_verified = flag("email_verified") || flag("xms_edov");
    let name = text("name").or_else(|| {
        let given = text("given_name");
        let family = text("family_name");
        match (given, family) {
            (Some(given), Some(family)) => Some(format!("{given} {family}")),
            (given, family) => given.or(family),
        }
    });
    Identity {
        subject: text("sub").unwrap_or_default(),
        email: text("email"),
        email_verified,
        name,
        groups: groups_claim.and_then(|claim| groups(claims, claim)),
    }
}

/// The groups in `claim`: an array of names, or one name. A dotted claim
/// (`realm_access.roles`) reads a nested value.
fn groups(claims: &Value, claim: &str) -> Option<Vec<String>> {
    let value = claims.get(claim).or_else(|| {
        claim
            .split('.')
            .try_fold(claims, |value, part| value.get(part))
    })?;
    let names = match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(name) => Some(name.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            })
            .collect(),
        Value::String(name) => vec![name.clone()],
        _ => return None,
    };
    let mut names = names
        .into_iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty() && name.chars().count() <= MAX_GROUP_NAME_LENGTH)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names.truncate(MAX_GROUPS);
    Some(names)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn issuers_must_be_https_unless_on_loopback() {
        assert!(parse_issuer("https://login.example.com/realms/acme").is_ok());
        assert!(parse_issuer(" https://idp.example.com ").is_ok());
        assert!(parse_issuer("http://127.0.0.1:8080").is_ok());
        assert!(parse_issuer("http://localhost:8080/").is_ok());
        assert!(parse_issuer("http://idp.example.com").is_err());
        assert!(parse_issuer("https://idp.example.com/?tenant=1").is_err());
        assert!(parse_issuer("https://user:pass@idp.example.com").is_err());
        assert!(parse_issuer("idp.example.com").is_err());
    }

    #[test]
    fn claims_give_email_name_and_groups() {
        let claims = json!({
            "sub": "abc",
            "email": "Ada@Example.com",
            "email_verified": "true",
            "given_name": "Ada",
            "family_name": "Lovelace",
            "groups": ["admins", " techs ", "", "admins", 42],
            "realm_access": { "roles": ["ops"] },
        });
        let identity = identity_from_claims(&claims, Some("groups"));
        assert_eq!(identity.subject, "abc");
        assert_eq!(identity.email.as_deref(), Some("Ada@Example.com"));
        assert!(identity.email_verified);
        assert_eq!(identity.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(
            identity.groups,
            Some(vec!["42".into(), "admins".into(), "techs".into()])
        );
        assert_eq!(
            identity_from_claims(&claims, Some("realm_access.roles")).groups,
            Some(vec!["ops".into()])
        );
        assert_eq!(identity_from_claims(&claims, Some("missing")).groups, None);
        assert_eq!(identity_from_claims(&claims, None).groups, None);
        let bare = identity_from_claims(&json!({ "sub": "x", "email_verified": 0 }), None);
        assert!(!bare.email_verified && bare.email.is_none() && bare.name.is_none());
        let entra = identity_from_claims(&json!({ "sub": "x", "xms_edov": true }), None);
        assert!(entra.email_verified);
    }

    #[test]
    fn a_jwt_payload_is_decoded() {
        let jwt = format!(
            "e30.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"x"}"#)
        );
        assert_eq!(id_token_payload(&jwt).unwrap()["sub"], "x");
        assert!(id_token_payload("nonsense").is_err());
    }
}
