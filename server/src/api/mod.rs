//! The `/v1` API used by the website, and by Agents and viewers.
mod account;
mod agent;
mod alerts;
mod api_tokens;
mod applications;
mod audit_log;
mod automation;
mod companies;
mod deployment_tokens;
mod devices;
mod enrollment;
mod events;
mod files;
mod handoffs;
mod instance;
mod invitations;
mod jobs;
mod metrics;
mod monitors;
mod notes;
mod notification_channels;
mod passkeys;
mod password_resets;
mod remote;
mod roles;
mod runs;
mod scim;
mod settings;
mod setup;
mod sign_in;
mod sso;
mod system_info;
mod tags;
mod terminals;
mod toolbox;
mod updates;
mod users;
mod wake;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderValue, Method, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{MethodRouter, delete, get, patch, post, put},
};
use serde::Serialize;
use tower_http::set_header::SetResponseHeaderLayer;

pub use self::deployment_tokens::EXPIRED_RETENTION_MS as DEPLOYMENT_TOKEN_RETENTION_MS;
pub use self::password_resets::create_reset;
pub(crate) use self::users::ensure_administrator_remains;
use crate::{
    auth::{Authorized, password, session::api_token_not_allowed},
    http::{ApiError, AppState, csrf},
    rbac::{Permissions, Role},
    users::User,
};

pub fn router(state: AppState) -> Router<AppState> {
    // What these routes change can end sessions or take permissions away, so
    // open sockets check their user's access again after each successful
    // write. A route that can change access belongs in one of them.
    let access = Router::new()
        .merge(sign_in_routes())
        .merge(account_routes())
        .merge(api_token_routes())
        .merge(administration_routes())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            recheck_access,
        ));
    Router::new()
        .merge(access)
        .merge(public_auth_routes())
        .merge(device_routes())
        .merge(toolbox_routes())
        .merge(remote_routes())
        .merge(alert_routes())
        .merge(monitor_routes())
        .merge(automation_routes())
        .merge(application_routes())
        .layer(middleware::from_fn_with_state(state, csrf::check))
        // Responses carry account data; browsers and proxies must not keep them.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
}

/// Starting and ending sessions. A sign-in other than through SSO also drops
/// the user's roles from SSO groups, and a completed password reset signs
/// the user out everywhere.
fn sign_in_routes() -> Router<AppState> {
    Router::new()
        .route("/setup", post(setup::complete))
        .route("/auth/sign-in", post(sign_in::password))
        .route("/auth/sign-in/second-factor", post(sign_in::second_factor))
        .route("/auth/sign-out", post(sign_in::sign_out))
        .route("/auth/passkey", post(passkeys::sign_in))
        .route("/auth/invitation/accept", post(invitations::accept))
        .route(
            "/auth/password-reset/complete",
            post(password_resets::complete),
        )
}

/// What anyone may ask before signing in, which changes no one's access.
/// The SSO callback is a `GET`, so it signals a sign-in itself.
fn public_auth_routes() -> Router<AppState> {
    Router::new()
        .route("/instance", get(instance::get))
        .route("/auth/passkey/options", post(passkeys::sign_in_options))
        .route("/auth/sso/start", get(sso::start))
        .route("/auth/sso/callback", get(sso::callback))
        .route("/auth/invitation", post(invitations::lookup))
        .route("/auth/password-reset", post(password_resets::request))
        .route("/auth/password-reset/lookup", post(password_resets::lookup))
}

fn account_routes() -> Router<AppState> {
    Router::new()
        .route("/account", get(account::get).patch(account::update))
        .route("/account/password", post(account::change_password))
        .route("/account/two-factor/totp", post(account::start_totp))
        .route(
            "/account/two-factor/totp/confirm",
            post(account::confirm_totp),
        )
        .route(
            "/account/two-factor/totp/disable",
            post(account::disable_totp),
        )
        .route(
            "/account/two-factor/recovery-codes",
            post(account::regenerate_recovery_codes),
        )
        .route(
            "/account/passkeys",
            get(passkeys::list).post(passkeys::register),
        )
        .route(
            "/account/passkeys/options",
            post(passkeys::registration_options),
        )
        .route("/account/passkeys/{id}", patch(passkeys::rename))
        .route("/account/passkeys/{id}/remove", post(passkeys::remove))
        .route("/account/sessions", get(account::sessions))
        .route("/account/sessions/{id}", delete(account::end_session))
        .route_layer(middleware::from_fn(website_only))
}

fn api_token_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api-tokens",
            get(api_tokens::list).post(api_tokens::create),
        )
        .route("/api-tokens/current", get(api_tokens::current))
        .route("/api-tokens/{id}", delete(api_tokens::revoke))
}

fn administration_routes() -> Router<AppState> {
    Router::new()
        .route("/users", get(users::list))
        // Enabling a user or changing their roles outlasts the token that did
        // it, and so does deleting a user whom SSO then creates again,
        // enabled. Tokens may read users and sign them out.
        .route(
            "/users/{id}",
            get(users::get).merge(website_only_route(
                patch(users::update).delete(users::delete),
            )),
        )
        .route(
            "/users/{id}/reset-two-factor",
            website_only_route(post(users::reset_two_factor)),
        )
        .route(
            "/users/{id}/password-reset",
            website_only_route(post(users::password_reset)),
        )
        .route("/users/{id}/sign-out", post(users::sign_out))
        .route(
            "/users/{id}/unlink-sso",
            website_only_route(post(users::unlink_sso)),
        )
        // An invited administrator's account would outlast the token that
        // invited them. The list holds no links, so tokens may read it, like
        // the user list.
        .route(
            "/invitations",
            get(invitations::list).merge(website_only_route(post(invitations::create))),
        )
        .route(
            "/invitations/{id}",
            website_only_route(delete(invitations::revoke)),
        )
        .route(
            "/invitations/{id}/renew",
            website_only_route(post(invitations::renew)),
        )
        // What a role grants, its members keep; tokens may read roles.
        .route(
            "/roles",
            get(roles::list).merge(website_only_route(post(roles::create))),
        )
        .route(
            "/roles/{id}",
            website_only_route(patch(roles::update).delete(roles::delete)),
        )
        .route("/permissions", get(roles::permissions))
        .route("/settings", get(settings::get).patch(settings::update))
        .route(
            "/settings/authentication",
            website_only_route(
                get(settings::get_authentication).patch(settings::update_authentication),
            ),
        )
        // Email carries every invitation and password reset link, so whoever
        // points it elsewhere can take over accounts.
        .route(
            "/settings/smtp",
            website_only_route(
                get(settings::get_smtp)
                    .put(settings::put_smtp)
                    .delete(settings::delete_smtp),
            ),
        )
        .route(
            "/settings/smtp/test",
            website_only_route(post(settings::test_smtp)),
        )
        .route("/settings/scim", website_only_route(get(scim::get)))
        .route(
            "/settings/scim/tokens",
            website_only_route(post(scim::create_token)),
        )
        .route(
            "/settings/scim/tokens/{id}",
            website_only_route(delete(scim::revoke_token)),
        )
        .route(
            "/settings/scim/groups/{id}",
            website_only_route(patch(scim::update_group)),
        )
        .route(
            "/settings/sso",
            website_only_route(
                get(sso::get_settings)
                    .put(sso::put_settings)
                    .delete(sso::delete_settings),
            ),
        )
        .route("/audit", get(audit_log::list))
}

fn device_routes() -> Router<AppState> {
    Router::new()
        .route("/agent-installers", post(enrollment::create))
        .route("/agent-installers/redeem", post(enrollment::redeem))
        // A deployment token enrolls devices until it expires, so only the
        // website makes and revokes them; tokens may read the list, which
        // holds no secrets.
        .route(
            "/deployment-tokens",
            get(deployment_tokens::list).merge(website_only_route(post(deployment_tokens::create))),
        )
        .route(
            "/deployment-tokens/{id}",
            website_only_route(delete(deployment_tokens::revoke)),
        )
        .route("/events", website_only_route(get(events::subscribe)))
        .route("/agents", get(devices::list))
        .route("/agents/{id}", delete(devices::delete))
        .route("/agents/{id}/company", put(companies::assign))
        .route("/agents/{id}/notes", get(notes::get).put(notes::save))
        .route("/agents/{id}/notes/{revision}", get(notes::revision))
        .route("/tags", get(tags::list).post(tags::create))
        .route("/tags/{id}", patch(tags::update).delete(tags::delete))
        .route("/device-tags", post(tags::change))
        .route("/companies", get(companies::list).post(companies::create))
        .route(
            "/companies/{id}",
            patch(companies::update).delete(companies::delete),
        )
        .route("/agents/{id}/connect", get(agent::connect))
        .route(
            "/agents/{id}/terminals",
            website_only_route(post(terminals::open)),
        )
        .route(
            "/agents/{id}/terminals/{terminal_id}",
            get(terminals::agent_connect),
        )
        .route(
            "/terminals/{id}",
            website_only_route(get(terminals::connect)),
        )
        .route("/agents/{id}/files", get(files::list))
        .route("/agents/{id}/files/roots", get(files::roots))
        .route("/agents/{id}/files/folders", post(files::create_folder))
        .route("/agents/{id}/files/rename", post(files::rename))
        .route("/agents/{id}/files/move", post(files::move_entries))
        .route("/agents/{id}/files/delete", post(files::delete))
        .route("/agents/{id}/files/downloads", post(files::create_download))
        .route("/agents/{id}/files/uploads", post(files::create_upload))
        .route(
            "/agents/{id}/file-transfers/{transfer_id}",
            get(files::agent_connect),
        )
        .route("/file-downloads/{id}", get(files::fetch_download))
        .route(
            "/file-downloads/{id}/progress",
            get(files::download_progress),
        )
        .route(
            "/file-uploads/{id}",
            get(files::upload_status)
                .put(files::send_upload)
                .delete(files::cancel_upload),
        )
        .route("/file-uploads/{id}/finish", post(files::finish_upload))
        .route("/agents/{id}/close-session", post(remote::close))
        .route("/agents/{id}/metrics", get(metrics::get))
        .route(
            "/agents/{id}/system-info",
            get(system_info::get).post(agent::report_system_info),
        )
        .route(
            "/agents/{id}/system-info/refresh",
            post(system_info::refresh),
        )
        .route("/agents/{id}/applications", get(system_info::applications))
        .route(
            "/agents/{id}/application-changes",
            get(system_info::changes),
        )
        .route("/system-info", get(system_info::search_devices))
        .route("/software", get(system_info::software))
        .route("/software/devices", get(system_info::software_devices))
        .route("/agents/{id}/updates", get(updates::get))
        .route("/agents/{id}/updates/scan", post(updates::scan))
        .route("/agents/{id}/updates/install", post(updates::install))
        .route("/updates/devices", get(updates::devices))
        .route("/update-runs", get(updates::list_runs))
        .route("/update-runs/{id}", get(updates::get_run))
        .route("/agents/{id}/wake", post(wake::wake_device))
        .route("/agents/{id}/wakes", get(wake::recent))
        .route("/wakes", get(wake::list).post(wake::wake_many))
        .route(
            "/agents/{id}/rotate-credential",
            post(devices::rotate_credential),
        )
        .route(
            "/agents/{id}/thumbnail",
            get(devices::thumbnail).put(agent::put_thumbnail),
        )
        .route("/agents/{id}/script-runs", post(runs::start_run))
        .route(
            "/agents/{id}/script-runs/{run_id}/result",
            post(agent::report_script_run),
        )
        .route("/agents/{id}/file-deliveries", post(runs::start_delivery))
        .route(
            "/agents/{id}/file-deliveries/{delivery_id}/content",
            get(agent::delivery_content),
        )
        .route(
            "/agents/{id}/file-deliveries/{delivery_id}/result",
            post(agent::report_file_delivery),
        )
        .route(
            "/agents/{id}/update-runs/{run_id}/result",
            post(agent::report_update_install),
        )
        .route(
            "/agents/{id}/application-runs/{run_id}/result",
            post(agent::report_application_run),
        )
}

fn application_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/applications",
            get(applications::list).post(applications::create),
        )
        .route(
            "/applications/{id}",
            get(applications::get)
                .put(applications::update)
                .delete(applications::delete),
        )
        .route("/agents/{id}/packages", get(applications::device_packages))
        .route("/agents/{id}/packages/scan", post(applications::scan))
        .route(
            "/agents/{id}/application-runs",
            post(applications::start_run),
        )
        .route(
            "/agents/{id}/application-runs/{run_id}/files/{file_id}",
            get(applications::installer),
        )
        .route("/application-runs", get(applications::list_runs))
        .route("/application-runs/{id}", get(applications::get_run))
        .route("/packages/outdated", get(applications::outdated))
        .route(
            "/packages/outdated/devices",
            get(applications::outdated_devices),
        )
}

fn toolbox_routes() -> Router<AppState> {
    Router::new()
        .route("/toolbox", get(toolbox::list))
        .route("/toolbox/scripts", post(toolbox::create_script))
        .route(
            "/toolbox/scripts/{id}",
            get(toolbox::get_script)
                .put(toolbox::update_script)
                .delete(toolbox::delete_script),
        )
        .route("/toolbox/files", post(toolbox::upload_file))
        .route(
            "/toolbox/files/{id}",
            put(toolbox::update_file).delete(toolbox::delete_file),
        )
        .route("/toolbox/files/{id}/content", get(toolbox::download_file))
        .route("/script-runs", get(runs::list_runs))
        .route("/script-runs/{id}", get(runs::get_run))
        .route("/file-deliveries", get(runs::list_deliveries))
        .route("/file-deliveries/{id}", get(runs::get_delivery))
}

fn automation_routes() -> Router<AppState> {
    Router::new()
        .route("/jobs", get(jobs::list).post(jobs::start))
        .route("/jobs/{id}", get(jobs::get))
        .route("/jobs/{id}/devices", get(jobs::devices))
        .route("/jobs/{id}/cancel", post(jobs::cancel))
        .route(
            "/automation-policies",
            get(automation::list).post(automation::create),
        )
        .route(
            "/automation-policies/{id}",
            put(automation::update).delete(automation::delete),
        )
        .route("/automation-policies/{id}/run", post(automation::run))
}

fn monitor_routes() -> Router<AppState> {
    Router::new()
        .route("/agents/{id}/monitors", get(monitors::device_monitors))
        .route("/agents/{id}/monitors/check", post(monitors::check))
        .route("/monitor-states", get(monitors::rule_states))
        .route("/alerts/{id}/remediations", get(monitors::remediations))
}

fn alert_routes() -> Router<AppState> {
    Router::new()
        .route("/alerts", get(alerts::list))
        .route("/alerts/counts", get(alerts::counts))
        .route("/alerts/{id}", get(alerts::get))
        .route("/alerts/{id}/acknowledge", post(alerts::acknowledge))
        .route("/alerts/{id}/resolve", post(alerts::resolve))
        .route(
            "/alert-rules",
            get(alerts::list_rules).post(alerts::create_rule),
        )
        .route(
            "/alert-rules/{id}",
            put(alerts::update_rule).delete(alerts::delete_rule),
        )
        .route(
            "/notification-channels",
            get(notification_channels::list).post(notification_channels::create),
        )
        .route(
            "/notification-channels/recipients",
            get(notification_channels::recipients),
        )
        .route(
            "/notification-channels/{id}",
            put(notification_channels::update).delete(notification_channels::delete),
        )
        .route(
            "/notification-channels/{id}/test",
            post(notification_channels::test),
        )
}

fn remote_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/remote/handoffs",
            website_only_route(post(handoffs::create)),
        )
        .route("/remote/handoffs/redeem", post(remote::redeem))
        .route("/remote/sessions/{id}/signal", get(remote::signal))
        .route("/remote/sessions/{id}/resume", post(remote::resume))
        .route("/remote/sessions/{id}/end", post(remote::end))
        .route(
            "/remote/sessions/{id}/toolbox",
            get(remote::session_toolbox),
        )
        .route(
            "/remote/sessions/{id}/script-runs",
            post(remote::session_run_script),
        )
        .route(
            "/remote/sessions/{id}/script-runs/{run_id}",
            get(remote::session_script_run),
        )
        .route(
            "/remote/sessions/{id}/file-deliveries",
            post(remote::session_deliver_file),
        )
        .route(
            "/remote/sessions/{id}/file-deliveries/{delivery_id}",
            get(remote::session_file_delivery),
        )
}

/// Routes only the website may use, whatever the token's permissions:
/// signing in and the account's own security, anything that makes
/// credentials or changes how people sign in (invitations, email, which
/// carries the sign-in links, and SSO links included), changing users and
/// roles, and the website's sockets and remote session starts. So a leaked token can't make itself
/// more tokens or take over an account, and revoking it is enough.
async fn website_only(request: axum::extract::Request, next: middleware::Next) -> Response {
    if crate::api_tokens::bearer(request.headers()).is_some() {
        return api_token_not_allowed().into_response();
    }
    next.run(request).await
}

fn website_only_route(route: MethodRouter<AppState>) -> MethodRouter<AppState> {
    route.layer(middleware::from_fn(website_only))
}

/// After a successful write to a route that can change access, asks open
/// event sockets, terminals and remote sessions to check their user's
/// access again, unless the response says [`AccessUnchanged`]. The handler
/// has committed by then. Each of those routes succeeds only for a session,
/// a token, a password or a one-time link, so strangers can't make every
/// socket check.
async fn recheck_access(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let change = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let response = next.run(request).await;
    if change
        && response.status().is_success()
        && response.extensions().get::<AccessUnchanged>().is_none()
    {
        state.presence.recheck_access();
    }
    response
}

/// A response extension for a write that succeeded without changing
/// anyone's access, such as signing out a browser that had no session.
#[derive(Debug, Clone, Copy)]
struct AccessUnchanged;

/// The body of a response that signs the browser in.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum SignInResult {
    SignedIn {
        /// The user must set up two-factor authentication before anything
        /// else works.
        two_factor_enrollment_required: bool,
    },
    SecondFactorRequired {
        challenge: String,
        /// `totp`, `passkey` and `recovery_code`, as the user has them.
        methods: Vec<&'static str>,
        /// The prompt for `navigator.credentials.get`, if `methods` has
        /// `passkey`.
        #[serde(skip_serializing_if = "Option::is_none")]
        passkey: Option<Box<webauthn_rs::prelude::RequestChallengeResponse>>,
    },
}

fn signed_in_response(
    status: StatusCode,
    cookie: HeaderValue,
    enrollment_required: bool,
) -> Response {
    (
        status,
        [(header::SET_COOKIE, cookie)],
        Json(SignInResult::SignedIn {
            two_factor_enrollment_required: enrollment_required,
        }),
    )
        .into_response()
}

/// A one-time link to a website page. The token is in the fragment, which
/// browsers never send to a server, so it stays out of logs and referrers.
fn link(state: &AppState, page: &str, token: &str) -> String {
    format!("{}/{page}#token={token}", state.config.public_origin())
}

#[derive(Debug, Clone, Serialize)]
struct RoleRef {
    id: String,
    name: String,
}

impl From<&Role> for RoleRef {
    fn from(role: &Role) -> Self {
        Self {
            id: role.id.clone(),
            name: role.name.clone(),
        }
    }
}

/// Checks the signed-in user's current password before an account change.
/// Wrong guesses are limited per user, so a borrowed session can't be used to
/// guess the password.
async fn confirm_password(state: &AppState, user: &User, password: &str) -> Result<(), ApiError> {
    let Some(hash) = user.password_hash.as_deref() else {
        return Err(ApiError::bad_request(
            "this account has no password; set one with a password reset link first",
        )
        .with_code("no_password"));
    };
    state
        .auth
        .password_confirmations
        .hit(&user.id)
        .map_err(ApiError::rate_limited)?;
    if password::verify(password, Some(hash)).await {
        state.auth.password_confirmations.refund(&user.id);
        Ok(())
    } else {
        Err(ApiError::forbidden("the password is incorrect").with_code("incorrect_password"))
    }
}

/// Call after removing one of a user's second factors, in the same
/// transaction. Refuses to remove the last one when the instance requires
/// two-factor authentication; otherwise the recovery codes go with it.
async fn second_factor_removed(
    executor: &mut impl crate::db::Executor,
    user_id: &str,
) -> Result<(), ApiError> {
    if crate::users::second_factors(executor, user_id).await?.any() {
        return Ok(());
    }
    if crate::settings::load(executor).await?.require_two_factor {
        return Err(ApiError::forbidden(
            "this server requires two-factor authentication, so your last second factor can't be removed",
        )
        .with_code("two_factor_required"));
    }
    crate::users::remove_recovery_codes(executor, user_id).await?;
    Ok(())
}

/// Counts a try at a one-time link token from `ip`. Hand it back with
/// [`token_accepted`] once the token turns out to be valid.
fn limit_token_attempts(state: &AppState, ip: std::net::IpAddr) -> Result<(), ApiError> {
    state
        .auth
        .token_attempts
        .hit(&crate::auth::limits::ip_key(ip))
        .map_err(ApiError::rate_limited)
}

/// A valid one-time link token isn't a guess: only wrong ones count, so
/// people opening their links from one office address don't use the limit
/// up.
fn token_accepted(state: &AppState, ip: std::net::IpAddr) {
    state
        .auth
        .token_attempts
        .refund(&crate::auth::limits::ip_key(ip));
}

/// Users may only grant what they hold: a role, or a set of permissions,
/// beyond the actor's own would let them raise their own access. Grant
/// the Administrator role, which gains every permission future releases add,
/// with [`require_administrator`] as well.
fn ensure_within(
    actor: &Authorized,
    permissions: &Permissions,
    what: &str,
) -> Result<(), ApiError> {
    let missing = permissions
        .difference(&actor.permissions)
        .map(|permission| permission.as_str())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "you can't {what} because it includes permissions you don't have: {}",
            missing.join(", ")
        ))
        .with_code("permission_escalation"))
    }
}

/// For changes as powerful as the Administrator role itself.
fn require_administrator(actor: &Authorized) -> Result<(), ApiError> {
    if actor.is_administrator() {
        Ok(())
    } else {
        Err(ApiError::forbidden("only administrators can do this").with_code("permission_denied"))
    }
}

fn require_any(
    actor: &Authorized,
    permissions: &[crate::rbac::Permission],
) -> Result<(), ApiError> {
    if permissions.iter().any(|permission| actor.has(*permission)) {
        return Ok(());
    }
    actor.require(permissions[0])
}

/// Accepts JSON `null` as `Some(None)` and a missing field (with
/// `#[serde(default)]`) as `None`, for fields where null means "clear".
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}
