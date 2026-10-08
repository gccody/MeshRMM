//! The `/v1` API used by the website, and by Agents and viewers.
mod account;
mod agent;
mod audit_log;
mod devices;
mod enrollment;
mod events;
mod handoffs;
mod instance;
mod invitations;
mod password_resets;
mod remote;
mod roles;
mod runs;
mod settings;
mod setup;
mod sign_in;
mod toolbox;
mod users;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderValue, Method, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};
use serde::Serialize;
use tower_http::set_header::SetResponseHeaderLayer;

pub use self::password_resets::create_reset;
use crate::{
    auth::{Authorized, SignedIn, password},
    http::{ApiError, AppState, csrf},
    rbac::{Permissions, Role},
    users::User,
};

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/instance", get(instance::get))
        .route("/setup", post(setup::complete))
        .route("/auth/sign-in", post(sign_in::password))
        .route("/auth/sign-in/second-factor", post(sign_in::second_factor))
        .route("/auth/sign-out", post(sign_in::sign_out))
        .route("/auth/invitation", post(invitations::lookup))
        .route("/auth/invitation/accept", post(invitations::accept))
        .route("/auth/password-reset", post(password_resets::request))
        .route("/auth/password-reset/lookup", post(password_resets::lookup))
        .route(
            "/auth/password-reset/complete",
            post(password_resets::complete),
        )
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
        .route("/account/sessions", get(account::sessions))
        .route("/account/sessions/{id}", delete(account::end_session))
        .route("/users", get(users::list))
        .route(
            "/users/{id}",
            get(users::get).patch(users::update).delete(users::delete),
        )
        .route(
            "/users/{id}/reset-two-factor",
            post(users::reset_two_factor),
        )
        .route("/users/{id}/password-reset", post(users::password_reset))
        .route("/users/{id}/sign-out", post(users::sign_out))
        .route(
            "/invitations",
            get(invitations::list).post(invitations::create),
        )
        .route("/invitations/{id}", delete(invitations::revoke))
        .route("/invitations/{id}/renew", post(invitations::renew))
        .route("/roles", get(roles::list).post(roles::create))
        .route("/roles/{id}", patch(roles::update).delete(roles::delete))
        .route("/permissions", get(roles::permissions))
        .route("/settings", get(settings::get).patch(settings::update))
        .route(
            "/settings/authentication",
            get(settings::get_authentication).patch(settings::update_authentication),
        )
        .route(
            "/settings/smtp",
            get(settings::get_smtp)
                .put(settings::put_smtp)
                .delete(settings::delete_smtp),
        )
        .route("/settings/smtp/test", post(settings::test_smtp))
        .route("/audit", get(audit_log::list))
        .route("/agent-installers", post(enrollment::create))
        .route("/agent-installers/redeem", post(enrollment::redeem))
        .route("/events", get(events::subscribe))
        .route("/agents", get(devices::list))
        .route("/agents/{id}", delete(devices::delete))
        .route("/agents/{id}/connect", get(agent::connect))
        .route("/agents/{id}/close-session", post(remote::close))
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
        .route("/remote/handoffs", post(handoffs::create))
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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            recheck_access,
        ))
        .layer(middleware::from_fn_with_state(state, csrf::check))
        // Responses carry account data; browsers and proxies must not keep them.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
}

/// What a signed-in user changes may end sessions or change permissions, so
/// open event sockets check their own access again after it.
async fn recheck_access(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let change = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && crate::auth::session::token(request.headers()).is_some();
    let response = next.run(request).await;
    if change && response.status().is_success() {
        state.presence.recheck_access();
    }
    response
}

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
        methods: &'static [&'static str],
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

/// Counts a try at a one-time link token from `ip`.
fn limit_token_attempts(state: &AppState, ip: std::net::IpAddr) -> Result<(), ApiError> {
    state
        .auth
        .token_attempts
        .hit(&crate::auth::limits::ip_key(ip))
        .map_err(ApiError::rate_limited)
}

/// Users may only grant what they hold: a role, or a set of permissions,
/// beyond the actor's own would let them raise their own access. Grant
/// the Administrator role, which gains every permission future releases add,
/// with [`require_administrator`] as well.
fn ensure_within(actor: &SignedIn, permissions: &Permissions, what: &str) -> Result<(), ApiError> {
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
fn require_administrator(actor: &SignedIn) -> Result<(), ApiError> {
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
