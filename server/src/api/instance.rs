//! `GET /v1/instance`: what the website needs before anyone signs in.
use axum::{Json, extract::State};
use serde::Serialize;

use crate::{
    auth::{oidc, passkeys},
    http::{ApiError, AppState},
    settings, users,
};

#[derive(Debug, Serialize)]
pub struct Instance {
    name: String,
    /// No account exists yet; the website shows first-run setup.
    setup_required: bool,
    sign_in: SignInMethods,
    password_min_length: i64,
}

#[derive(Debug, Serialize)]
struct SignInMethods {
    password: bool,
    /// Whether "forgot password" can email a reset link.
    password_reset_email: bool,
    /// Passkeys work with this server's public URL.
    passkey: bool,
    /// The SSO provider, if SSO is on.
    sso: Option<SsoMethod>,
}

#[derive(Debug, Serialize)]
struct SsoMethod {
    /// What the "Sign in with ..." button names.
    name: String,
}

pub async fn get(State(state): State<AppState>) -> Result<Json<Instance>, ApiError> {
    let mut database = &state.database;
    let settings = settings::load(&mut database).await?;
    Ok(Json(Instance {
        setup_required: users::count(&mut database).await? == 0,
        sign_in: SignInMethods {
            password: true,
            password_reset_email: settings.smtp_configured(),
            passkey: passkeys::relying_party(&state.config, &settings.instance_name).is_ok(),
            sso: oidc::enabled(&mut database)
                .await?
                .map(|provider| SsoMethod {
                    name: provider.display_name,
                }),
        },
        password_min_length: settings.password_min_length,
        name: settings.instance_name,
    }))
}
