//! Single sign-on: signing in through the OpenID Connect provider, and the
//! administrator's settings for it.
//!
//! Setting up SSO decides who can sign in as whom and which roles they get,
//! including the Administrator role, so only administrators may change it.
use std::collections::BTreeSet;

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use sea_query::{Expr, ExprTrait, Query as Sql};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{double_option, require_administrator};
use crate::{
    audit::{self, Actor, Target},
    auth::{
        Authorized,
        limits::ip_key,
        oidc::{self, Identity, MAX_GROUP_NAME_LENGTH, Provider},
        session::{self, AuthMethod, Client},
    },
    db::{
        Executor, Transaction,
        tables::{OidcGroupRoles, OidcProvider, UserOidcGroups, Users},
    },
    http::{ApiError, AppState, JsonBody, client_ip::ClientIp},
    rbac,
    time::now_ms,
    users::{self, MAX_DISPLAY_NAME_LENGTH, NewUser, User, new_id},
};

/// Holds the browser's half of a pending SSO sign-in. Lax, so the browser
/// sends it on the provider's redirect back.
const BINDING_COOKIE: &str = "__Host-meshrmm-sso";
const BINDING_MAX_AGE_SECONDS: u64 = 10 * 60;
const DEFAULT_SCOPES: &str = "openid email profile";
const MAX_DISPLAY_NAME: usize = 80;
const MAX_GROUP_MAPPINGS: usize = 200;

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    #[serde(default)]
    next: Option<String>,
}

/// Why an SSO sign-in failed, as the sign-in page's `sso_error`.
#[derive(Debug, Clone, Copy)]
enum Failure {
    /// SSO isn't set up or is turned off.
    Unavailable,
    /// The provider or its answer failed; the server log says why.
    Failed,
    /// The sign-in took too long, or the callback came to another browser.
    Expired,
    /// The user cancelled or the provider refused them.
    Denied,
    /// No account matches and the provider may not create one.
    NoAccount,
    /// The provider gave no usable email address.
    NoEmail,
    /// The provider didn't vouch for the email address.
    EmailUnverified,
    /// The matching account is linked to another identity at the provider.
    Conflict,
    Disabled,
    RateLimited,
}

impl Failure {
    fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
            Self::Expired => "expired",
            Self::Denied => "denied",
            Self::NoAccount => "no_account",
            Self::NoEmail => "no_email",
            Self::EmailUnverified => "email_unverified",
            Self::Conflict => "conflict",
            Self::Disabled => "account_disabled",
            Self::RateLimited => "rate_limited",
        }
    }
}

impl From<ApiError> for Failure {
    fn from(_: ApiError) -> Self {
        Self::Failed
    }
}

impl From<sqlx::Error> for Failure {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(%error, "database error during SSO sign-in");
        Self::Failed
    }
}

fn binding_cookie(value: &str, max_age_seconds: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{BINDING_COOKIE}={value}; Path=/; Max-Age={max_age_seconds}; HttpOnly; Secure; SameSite=Lax"
    ))
    .expect("a hex token is a valid header value")
}

fn binding(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == BINDING_COOKIE)
        .map(|(_, value)| value)
        .filter(|value| !value.is_empty())
}

/// A redirect to `location`, with each `Set-Cookie` value.
fn see_other(location: &str, cookies: &[HeaderValue]) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    if let Ok(location) = HeaderValue::from_str(location) {
        headers.insert(header::LOCATION, location);
    }
    for cookie in cookies {
        headers.append(header::SET_COOKIE, cookie.clone());
    }
    response
}

fn to_login(state: &AppState, failure: Failure) -> Response {
    see_other(
        &format!(
            "{}/login?sso_error={}",
            state.config.public_origin(),
            failure.code()
        ),
        &[binding_cookie("", 0)],
    )
}

/// A website path to return to: absolute on this origin, not an API route.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.starts_with("/\\")
                && path != "/v1"
                && !path.starts_with("/v1/")
                && path.len() <= 2048
                && !path.chars().any(|c| c.is_control() || c == '\\') =>
        {
            path.to_owned()
        }
        _ => "/".to_owned(),
    }
}

/// `GET /v1/auth/sso/start?next=/path`: sends the browser to the provider.
pub async fn start(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Query(query): Query<StartQuery>,
) -> Response {
    match begin(&state, ip, query.next.as_deref()).await {
        Ok(response) => response,
        Err(failure) => to_login(&state, failure),
    }
}

async fn begin(
    state: &AppState,
    ip: std::net::IpAddr,
    next: Option<&str>,
) -> Result<Response, Failure> {
    if state.auth.ceremony_starts.hit(&ip_key(ip)).is_err() {
        return Err(Failure::RateLimited);
    }
    let provider = oidc::enabled(&mut &state.database)
        .await?
        .ok_or(Failure::Unavailable)?;
    let started = oidc::begin(
        &state.config,
        &state.instance_key,
        &provider,
        safe_next(next),
        |pending| state.auth.sso_sign_ins.start(pending),
    )
    .await
    .map_err(|error| {
        tracing::warn!(
            error = format!("{error:#}"),
            "could not start an SSO sign-in"
        );
        Failure::Failed
    })?;
    Ok(see_other(
        started.authorize_url.as_str(),
        &[binding_cookie(&started.binding, BINDING_MAX_AGE_SECONDS)],
    ))
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// `GET /v1/auth/sso/callback`: where the provider sends the browser back.
/// Signs in and continues to the page the sign-in started from.
pub async fn callback(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    match finish(&state, ip, &headers, query).await {
        Ok((cookie, next)) => see_other(
            &format!("{}{next}", state.config.public_origin()),
            &[cookie, binding_cookie("", 0)],
        ),
        Err(failure) => to_login(&state, failure),
    }
}

async fn finish(
    state: &AppState,
    ip: std::net::IpAddr,
    headers: &HeaderMap,
    query: CallbackQuery,
) -> Result<(HeaderValue, String), Failure> {
    let pending = query
        .state
        .as_deref()
        .and_then(|token| state.auth.sso_sign_ins.take(token))
        .ok_or(Failure::Expired)?;
    if !binding(headers).is_some_and(|binding| pending.bound_to(binding)) {
        return Err(Failure::Expired);
    }
    if let Some(error) = &query.error {
        tracing::info!(
            error,
            description = query.error_description.as_deref().unwrap_or_default(),
            "the SSO provider refused a sign-in"
        );
        return Err(Failure::Denied);
    }
    let code = query.code.as_deref().ok_or(Failure::Failed)?;
    let provider = oidc::enabled(&mut &state.database)
        .await?
        .ok_or(Failure::Unavailable)?;
    let next = pending.next.clone();
    let identity = oidc::complete(&state.config, &state.instance_key, &provider, pending, code)
        .await
        .map_err(|error| {
            tracing::warn!(error = format!("{error:#}"), "an SSO sign-in failed");
            Failure::Failed
        })?;
    let mut transaction = state.database.begin().await?;
    let user = match account_for(&mut transaction, &provider, &identity, ip).await {
        Ok(user) => user,
        Err(failure) => {
            tracing::info!(
                subject = identity.subject,
                email = identity.email.as_deref().unwrap_or_default(),
                reason = failure.code(),
                "an SSO sign-in matched no usable account"
            );
            return Err(failure);
        }
    };
    if user.disabled {
        transaction.rollback().await?;
        audit::record(
            &mut &state.database,
            &Actor::anonymous(ip),
            "auth.sign_in_failed",
            Target::user(&user.id),
            json!({ "reason": "account_disabled", "method": "oidc" }),
        )
        .await?;
        return Err(Failure::Disabled);
    }
    sync_groups(
        &mut transaction,
        &user.id,
        provider
            .groups_claim
            .as_ref()
            .and(identity.groups.as_deref()),
    )
    .await?;
    let client = Client::new(ip, headers);
    let cookie =
        session::start(&mut transaction, &user.id, AuthMethod::Oidc, false, &client).await?;
    audit::record(
        &mut transaction,
        &Actor::user(&user.id, &user.email, Some(ip)),
        "auth.sign_in",
        Target::user(&user.id),
        json!({ "method": "oidc", "groups": identity.groups }),
    )
    .await?;
    transaction.commit().await?;
    // The user's groups may have changed their roles.
    state.presence.recheck_access();
    Ok((cookie, next))
}

/// The account `identity` signs in to: the one linked to its subject, or
/// the one with its email address (which then gets linked), or a new one if
/// the provider may create accounts.
async fn account_for(
    transaction: &mut Transaction,
    provider: &Provider,
    identity: &Identity,
    ip: std::net::IpAddr,
) -> Result<User, Failure> {
    let linked: Option<User> = transaction
        .fetch_optional(
            &users::select()
                .and_where(Expr::col(Users::OidcSubject).eq(identity.subject.as_str()))
                .to_owned(),
        )
        .await?;
    if let Some(user) = linked {
        return Ok(user);
    }
    let email = identity
        .email
        .as_deref()
        .and_then(|email| users::normalize_email(email).ok())
        .ok_or(Failure::NoEmail)?;
    if provider.require_verified_email && !identity.email_verified {
        return Err(Failure::EmailUnverified);
    }
    let actor = Actor {
        user_id: None,
        label: format!("SSO ({})", provider.display_name),
        ip: Some(ip),
    };
    if let Some(user) = users::by_email(transaction, &email).await? {
        if user.oidc_subject.is_some() {
            return Err(Failure::Conflict);
        }
        transaction
            .execute(
                &Sql::update()
                    .table(Users::Table)
                    .value(Users::OidcSubject, identity.subject.as_str())
                    .and_where(Expr::col(Users::Id).eq(user.id.as_str()))
                    .to_owned(),
            )
            .await?;
        audit::record(
            transaction,
            &actor,
            "user.sso_link",
            Target::user(&user.id),
            json!({ "email": user.email }),
        )
        .await?;
        return Ok(user);
    }
    if !provider.auto_provision {
        return Err(Failure::NoAccount);
    }
    let display_name = identity
        .name
        .as_deref()
        .and_then(|name| users::validate_display_name(name).ok())
        .unwrap_or_else(|| {
            let local = email.split('@').next().unwrap_or(&email);
            local.chars().take(MAX_DISPLAY_NAME_LENGTH).collect()
        });
    let id = new_id();
    let role_ids = provider.default_role_id.iter().cloned().collect::<Vec<_>>();
    users::insert(
        transaction,
        NewUser {
            id: &id,
            email: &email,
            display_name: &display_name,
            password_hash: None,
            role_ids: &role_ids,
            now_ms: now_ms(),
        },
    )
    .await?;
    transaction
        .execute(
            &Sql::update()
                .table(Users::Table)
                .value(Users::OidcSubject, identity.subject.as_str())
                .and_where(Expr::col(Users::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        transaction,
        &actor,
        "user.provision",
        Target::user(&id),
        json!({ "source": "sso", "email": email, "role_ids": role_ids }),
    )
    .await?;
    users::by_id(transaction, &id).await?.ok_or(Failure::Failed)
}

/// Replaces the user's SSO groups with `groups`, or clears them when the
/// provider has no groups claim.
async fn sync_groups(
    executor: &mut impl Executor,
    user_id: &str,
    groups: Option<&[String]>,
) -> crate::db::Result<()> {
    executor
        .execute(
            &Sql::delete()
                .from_table(UserOidcGroups::Table)
                .and_where(Expr::col(UserOidcGroups::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    let Some(groups) = groups.filter(|groups| !groups.is_empty()) else {
        return Ok(());
    };
    let mut insert = Sql::insert();
    insert
        .into_table(UserOidcGroups::Table)
        .columns([UserOidcGroups::UserId, UserOidcGroups::GroupName]);
    for group in groups {
        insert.values_panic([user_id.into(), group.as_str().into()]);
    }
    executor.execute(&insert).await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRole {
    group: String,
    role_id: String,
}

#[derive(Debug, Serialize)]
pub struct ProviderView {
    enabled: bool,
    display_name: String,
    issuer_url: String,
    client_id: String,
    /// The secret itself is never sent back.
    has_client_secret: bool,
    scopes: String,
    auto_provision: bool,
    default_role_id: Option<String>,
    require_verified_email: bool,
    groups_claim: Option<String>,
    group_roles: Vec<GroupRole>,
    updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct SsoSettings {
    /// What to register with the provider as the redirect URI.
    redirect_uri: String,
    provider: Option<ProviderView>,
}

async fn view(state: &AppState, executor: &mut impl Executor) -> Result<SsoSettings, ApiError> {
    let provider = match oidc::load(executor).await? {
        Some(provider) => Some(ProviderView {
            has_client_secret: provider.client_secret_encrypted.is_some(),
            group_roles: oidc::group_roles(executor)
                .await?
                .into_iter()
                .map(|(group, role_id)| GroupRole { group, role_id })
                .collect(),
            enabled: provider.enabled,
            display_name: provider.display_name,
            issuer_url: provider.issuer_url,
            client_id: provider.client_id,
            scopes: provider.scopes,
            auto_provision: provider.auto_provision,
            default_role_id: provider.default_role_id,
            require_verified_email: provider.require_verified_email,
            groups_claim: provider.groups_claim,
            updated_at: provider.updated_at,
        }),
        None => None,
    };
    Ok(SsoSettings {
        redirect_uri: oidc::redirect_uri(&state.config),
        provider,
    })
}

/// `GET /v1/settings/sso`
pub async fn get_settings(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<SsoSettings>, ApiError> {
    require_administrator(&actor)?;
    Ok(Json(view(&state, &mut &state.database).await?))
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsoUpdate {
    enabled: bool,
    display_name: String,
    issuer_url: String,
    client_id: String,
    /// Missing keeps the stored secret; `null` removes it (a public client).
    #[serde(default, deserialize_with = "double_option")]
    client_secret: Option<Option<String>>,
    #[serde(default)]
    scopes: Option<String>,
    #[serde(default)]
    auto_provision: bool,
    #[serde(default)]
    default_role_id: Option<String>,
    #[serde(default = "default_true")]
    require_verified_email: bool,
    #[serde(default)]
    groups_claim: Option<String>,
    #[serde(default)]
    group_roles: Vec<GroupRole>,
}

fn check(valid: bool, message: &'static str) -> Result<(), ApiError> {
    if valid {
        Ok(())
    } else {
        Err(ApiError::bad_request(message))
    }
}

fn printable(text: &str) -> bool {
    !text.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Validates the scopes and makes sure `openid` is one of them.
fn normalize_scopes(raw: Option<&str>) -> Result<String, ApiError> {
    let mut scopes = raw
        .unwrap_or(DEFAULT_SCOPES)
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    check(
        scopes
            .iter()
            .all(|scope| scope.len() <= 128 && !scope.contains(['"', '\\'])),
        "scopes are space-separated words",
    )?;
    if !scopes.iter().any(|scope| scope == "openid") {
        scopes.insert(0, "openid".to_owned());
    }
    scopes.dedup();
    check(scopes.len() <= 20, "list at most 20 scopes")?;
    Ok(scopes.join(" "))
}

/// A validated [`SsoUpdate`], trimmed and normalized.
struct ProviderSettings<'a> {
    display_name: &'a str,
    issuer_url: &'a str,
    client_id: &'a str,
    scopes: String,
    groups_claim: Option<&'a str>,
    group_roles: BTreeSet<GroupRole>,
}

impl<'a> ProviderSettings<'a> {
    fn validate(request: &'a SsoUpdate) -> Result<Self, ApiError> {
        let display_name = request.display_name.trim();
        check(
            !display_name.is_empty()
                && display_name.chars().count() <= MAX_DISPLAY_NAME
                && !display_name.chars().any(char::is_control),
            "the provider name must be 1 to 80 characters",
        )?;
        let issuer_url = request.issuer_url.trim();
        oidc::parse_issuer(issuer_url).map_err(ApiError::bad_request)?;
        let client_id = request.client_id.trim();
        check(
            !client_id.is_empty() && client_id.len() <= 512 && printable(client_id),
            "enter the client ID the provider gave you",
        )?;
        let scopes = normalize_scopes(request.scopes.as_deref())?;
        let groups_claim = request
            .groups_claim
            .as_deref()
            .map(str::trim)
            .filter(|claim| !claim.is_empty());
        check(
            groups_claim.is_none_or(|claim| claim.len() <= 256 && printable(claim)),
            "the groups claim must be a claim name",
        )?;
        let mut group_roles = request
            .group_roles
            .iter()
            .map(|mapping| GroupRole {
                group: mapping.group.trim().to_owned(),
                role_id: mapping.role_id.clone(),
            })
            .collect::<BTreeSet<_>>();
        check(
            group_roles.iter().all(|mapping| {
                !mapping.group.is_empty()
                    && mapping.group.chars().count() <= MAX_GROUP_NAME_LENGTH
                    && !mapping.group.chars().any(char::is_control)
            }),
            "group names must be 1 to 256 characters",
        )?;
        check(
            group_roles.len() <= MAX_GROUP_MAPPINGS,
            "map at most 200 groups",
        )?;
        if groups_claim.is_none() {
            group_roles.clear();
        }
        Ok(Self {
            display_name,
            issuer_url,
            client_id,
            scopes,
            groups_claim,
            group_roles,
        })
    }
}

/// `PUT /v1/settings/sso`: sets up the provider. When it is turned on, its
/// discovery document must load.
pub async fn put_settings(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(request): JsonBody<SsoUpdate>,
) -> Result<Json<SsoSettings>, ApiError> {
    require_administrator(&actor)?;
    let settings = ProviderSettings::validate(&request)?;
    if request.enabled {
        oidc::discover(settings.issuer_url).await.map_err(|error| {
            ApiError::bad_request(format!("{error:#}")).with_code("discovery_failed")
        })?;
    }
    let mut transaction = state.database.begin().await?;
    check_roles_exist(&mut transaction, &settings.group_roles, &request).await?;
    let previous = oidc::load(&mut transaction).await?;
    let secret = client_secret(&state, &request, previous.as_ref())?;
    let issuer_changed = previous
        .as_ref()
        .is_some_and(|provider| provider.issuer_url != settings.issuer_url);
    if issuer_changed {
        // Subjects are only unique per provider.
        unlink_everyone(&mut transaction).await?;
    }
    store_provider(&mut transaction, &request, &settings, secret).await?;
    store_group_roles(&mut transaction, &settings).await?;
    // Unlinking or remapping can take away the role someone held as an
    // administrator through an SSO group.
    super::ensure_administrator_remains(&mut transaction).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "settings.sso_update",
        Target::settings(),
        json!({
            "enabled": request.enabled,
            "display_name": settings.display_name,
            "issuer_url": settings.issuer_url,
            "client_id": settings.client_id,
            "client_secret_changed": request.client_secret.is_some(),
            "scopes": settings.scopes,
            "auto_provision": request.auto_provision,
            "default_role_id": request.default_role_id,
            "require_verified_email": request.require_verified_email,
            "groups_claim": settings.groups_claim,
            "group_roles": settings.group_roles,
            "accounts_unlinked": issuer_changed,
        }),
    )
    .await?;
    let settings = view(&state, &mut transaction).await?;
    transaction.commit().await?;
    state.presence.recheck_access();
    Ok(Json(settings))
}

/// Checks that the mapped and default roles all exist.
async fn check_roles_exist(
    executor: &mut impl Executor,
    group_roles: &BTreeSet<GroupRole>,
    request: &SsoUpdate,
) -> Result<(), ApiError> {
    let mut role_ids = group_roles
        .iter()
        .map(|mapping| mapping.role_id.clone())
        .collect::<BTreeSet<_>>();
    role_ids.extend(request.default_role_id.iter().cloned());
    let role_ids = role_ids.into_iter().collect::<Vec<_>>();
    if rbac::load_roles(executor, Some(&role_ids)).await?.len() != role_ids.len() {
        return Err(ApiError::bad_request("one of the roles doesn't exist"));
    }
    Ok(())
}

/// The sealed client secret to store: the previous one unless the request
/// replaces or removes it.
fn client_secret(
    state: &AppState,
    request: &SsoUpdate,
    previous: Option<&Provider>,
) -> Result<Option<Vec<u8>>, ApiError> {
    Ok(match &request.client_secret {
        None => previous.and_then(|provider| provider.client_secret_encrypted.clone()),
        Some(None) => None,
        Some(Some(secret)) if secret.is_empty() => None,
        Some(Some(secret)) => {
            check(secret.len() <= 4096, "the client secret is too long")?;
            Some(
                state
                    .instance_key
                    .encrypt(oidc::SECRET_CONTEXT, secret.as_bytes()),
            )
        }
    })
}

async fn store_provider(
    executor: &mut impl Executor,
    request: &SsoUpdate,
    settings: &ProviderSettings<'_>,
    secret: Option<Vec<u8>>,
) -> crate::db::Result<()> {
    executor
        .execute(&Sql::delete().from_table(OidcProvider::Table).to_owned())
        .await?;
    executor
        .execute(
            &Sql::insert()
                .into_table(OidcProvider::Table)
                .columns([
                    OidcProvider::Id,
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
                .values_panic([
                    1.into(),
                    request.enabled.into(),
                    settings.display_name.into(),
                    settings.issuer_url.into(),
                    settings.client_id.into(),
                    secret.into(),
                    settings.scopes.as_str().into(),
                    request.auto_provision.into(),
                    request.default_role_id.clone().into(),
                    request.require_verified_email.into(),
                    settings.groups_claim.map(str::to_owned).into(),
                    now_ms().into(),
                ])
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Replaces the group mappings. Without a groups claim, everyone's stored
/// groups go too.
async fn store_group_roles(
    executor: &mut impl Executor,
    settings: &ProviderSettings<'_>,
) -> crate::db::Result<()> {
    executor
        .execute(&Sql::delete().from_table(OidcGroupRoles::Table).to_owned())
        .await?;
    if !settings.group_roles.is_empty() {
        let mut insert = Sql::insert();
        insert
            .into_table(OidcGroupRoles::Table)
            .columns([OidcGroupRoles::GroupName, OidcGroupRoles::RoleId]);
        for mapping in &settings.group_roles {
            insert.values_panic([
                mapping.group.as_str().into(),
                mapping.role_id.as_str().into(),
            ]);
        }
        executor.execute(&insert).await?;
    }
    if settings.groups_claim.is_none() {
        executor
            .execute(&Sql::delete().from_table(UserOidcGroups::Table).to_owned())
            .await?;
    }
    Ok(())
}

/// Forgets every account's SSO identity and groups.
async fn unlink_everyone(executor: &mut impl Executor) -> crate::db::Result<()> {
    executor
        .execute(
            &Sql::update()
                .table(Users::Table)
                .value(Users::OidcSubject, Option::<String>::None)
                .and_where(Expr::col(Users::OidcSubject).is_not_null())
                .to_owned(),
        )
        .await?;
    executor
        .execute(&Sql::delete().from_table(UserOidcGroups::Table).to_owned())
        .await?;
    Ok(())
}

/// `DELETE /v1/settings/sso`: removes the provider. Accounts keep working
/// with their passwords and passkeys; roles from SSO groups end.
pub async fn delete_settings(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<StatusCode, ApiError> {
    require_administrator(&actor)?;
    let mut transaction = state.database.begin().await?;
    unlink_everyone(&mut transaction).await?;
    transaction
        .execute(&Sql::delete().from_table(OidcGroupRoles::Table).to_owned())
        .await?;
    let removed = transaction
        .execute(&Sql::delete().from_table(OidcProvider::Table).to_owned())
        .await?;
    if removed == 0 {
        return Err(ApiError::not_found("SSO isn't set up"));
    }
    super::ensure_administrator_remains(&mut transaction).await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "settings.sso_delete",
        Target::settings(),
        json!({}),
    )
    .await?;
    transaction.commit().await?;
    state.presence.recheck_access();
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_website_paths_are_followed() {
        assert_eq!(safe_next(Some("/toolbox?tab=files")), "/toolbox?tab=files");
        assert_eq!(safe_next(None), "/");
        for unsafe_path in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "/v1/agents",
            "/v1",
            "toolbox",
            "/a\nb",
        ] {
            assert_eq!(safe_next(Some(unsafe_path)), "/", "{unsafe_path:?}");
        }
    }

    #[test]
    fn scopes_always_include_openid() {
        assert_eq!(normalize_scopes(None).unwrap(), "openid email profile");
        assert_eq!(
            normalize_scopes(Some("email  groups")).unwrap(),
            "openid email groups"
        );
        assert!(normalize_scopes(Some("a\"b")).is_err());
    }

    #[test]
    fn the_binding_cookie_is_found() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("__Host-meshrmm-session=a; __Host-meshrmm-sso=b"),
        );
        assert_eq!(binding(&headers), Some("b"));
        assert_eq!(binding(&HeaderMap::new()), None);
    }
}
