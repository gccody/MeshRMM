//! The HTTP application: routes, shared state, errors and response headers.
pub mod client_ip;
pub mod csrf;
mod headers;

use std::{borrow::Cow, sync::Arc};

use axum::{
    Json, Router,
    extract::{FromRequest, Request, State, rejection::JsonRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Serialize, de::DeserializeOwned};
use tower_http::{
    compression::{
        CompressionLayer,
        predicate::{DefaultPredicate, NotForContentType, Predicate},
    },
    services::ServeDir,
    trace::TraceLayer,
};

use crate::{
    api,
    auth::AuthState,
    config::{Config, TlsConfig},
    db::Database,
    health,
    realtime::{AgentHub, Presence, Sessions},
    scim,
    secrets::InstanceKey,
    storage::Storage,
    turn::Turn,
    website::Website,
};

/// What every request handler can reach.
#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub database: Database,
    pub instance_key: InstanceKey,
    pub auth: Arc<AuthState>,
    pub storage: Storage,
    pub agents: AgentHub,
    pub presence: Presence,
    pub sessions: Sessions,
    pub turn: Turn,
    pub website: Website,
}

pub fn router(state: AppState) -> Router {
    let hsts = !matches!(state.config.tls, TlsConfig::Proxy { .. })
        && state.config.public_url.scheme() == "https";
    // Fonts are compressed already.
    let compress = DefaultPredicate::new().and(NotForContentType::const_new("font/"));
    let website = Router::new()
        .fallback(website)
        .layer(CompressionLayer::new().compress_when(compress))
        .with_state(state.clone());
    let router = Router::new()
        .route("/healthz", get(health::healthz))
        .nest("/v1", api::router(state.clone()))
        .nest("/scim/v2", scim::router())
        .nest_service("/downloads", ServeDir::new(&state.config.downloads.dir))
        .fallback_service(website)
        .with_state(state);
    headers::apply(router, hsts).layer(TraceLayer::new_for_http())
}

/// Every path the routes above don't take: a website page, or a JSON 404
/// for an unknown API route.
async fn website(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path();
    let api = path == "/v1" || path.starts_with("/v1/");
    if !api
        && let Some(response) = state
            .website
            .respond(request.method(), path, request.headers())
    {
        return response;
    }
    tracing::debug!(path, "no route");
    ApiError::new(StatusCode::NOT_FOUND, "route not found").into_response()
}

/// An error response: the status and `{"error": message}`, plus a `code`
/// the website can act on when the message alone is not enough.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: Cow<'static, str>,
    code: Option<&'static str>,
    retry_after_seconds: Option<u64>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
            retry_after_seconds: None,
        }
    }

    pub fn bad_request(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn forbidden(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub fn not_found(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn conflict(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }

    /// Too many attempts; the client may try again after `seconds`.
    pub fn rate_limited(seconds: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts; wait a few minutes and try again",
        )
        .with_code("rate_limited")
        .with_retry_after(seconds)
    }

    pub fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds.max(1));
        self
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn code(&self) -> Option<&'static str> {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorBody {
                error: &self.message,
                code: self.code,
            }),
        )
            .into_response();
        if let Some(seconds) = self.retry_after_seconds {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        tracing::error!(error = format!("{error:#}"), "request failed");
        Self::internal()
    }
}

/// A JSON request body whose rejection is an [`ApiError`] like every other
/// error, instead of axum's plain-text one.
pub struct JsonBody<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(JsonRejection::JsonDataError(error)) => Err(ApiError::bad_request(format!(
                "invalid request: {}",
                error.body_text()
            ))),
            Err(error) => Err(ApiError::new(error.status(), error.body_text())),
        }
    }
}

/// A database failure is logged and reported to the client without detail.
impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(%error, "database error");
        Self::internal()
    }
}
