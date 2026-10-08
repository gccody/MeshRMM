//! The HTTP application: routes, shared state, errors and response headers.
pub mod client_ip;
mod headers;

use std::{borrow::Cow, sync::Arc};

use axum::{
    Json, Router,
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use tower_http::{services::ServeDir, trace::TraceLayer};

use crate::{
    config::{Config, TlsConfig},
    db::Database,
    health,
    secrets::InstanceKey,
};

/// What every request handler can reach.
#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub database: Database,
    pub instance_key: InstanceKey,
}

pub fn router(state: AppState) -> Router {
    let hsts = !matches!(state.config.tls, TlsConfig::Proxy { .. })
        && state.config.public_url.scheme() == "https";
    let router = Router::new()
        .route("/healthz", get(health::healthz))
        .nest_service("/downloads", ServeDir::new(&state.config.downloads.dir))
        .fallback(not_found)
        .with_state(state);
    headers::apply(router, hsts).layer(TraceLayer::new_for_http())
}

async fn not_found(uri: Uri) -> ApiError {
    tracing::debug!(path = uri.path(), "no route");
    ApiError::new(StatusCode::NOT_FOUND, "route not found")
}

/// An error response: the status and `{"error": message}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: Cow<'static, str>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: &self.message,
            }),
        )
            .into_response()
    }
}

/// A database failure is logged and reported to the client without detail.
impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(%error, "database error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}
