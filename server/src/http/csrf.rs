//! Cross-site request forgery checks for the cookie-authenticated API.
//!
//! The website and API share one origin, so a state-changing request from the
//! website always carries that `Origin` and the `X-MeshRMM-Request` header. A
//! page on another origin cannot send the header without a CORS preflight,
//! which this server never approves.
use axum::{
    extract::{Request, State},
    http::{HeaderMap, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::{ApiError, AppState};
use crate::auth::session;

pub const REQUEST_HEADER: &str = "x-meshrmm-request";

pub async fn check(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if !needs_check(request.method(), request.headers())
        || allowed(request.headers(), &state.config.public_origin())
    {
        return next.run(request).await;
    }
    tracing::debug!(
        method = %request.method(),
        path = request.uri().path(),
        "rejected a request without the website's origin"
    );
    ApiError::forbidden("this request must come from the MeshRMM website")
        .with_code("cross_site_request")
        .into_response()
}

/// Reads never change state. Agents, viewers and SCIM clients authenticate
/// with an `Authorization` header and no session cookie. A request with a
/// session cookie is checked even with the header, which a browser may add on
/// its own (cached Basic credentials for a proxy, say).
fn needs_check(method: &Method, headers: &HeaderMap) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        && (!headers.contains_key(header::AUTHORIZATION) || session::token(headers).is_some())
}

fn allowed(headers: &HeaderMap, public_origin: &str) -> bool {
    headers.contains_key(REQUEST_HEADER)
        && headers
            .get(header::ORIGIN)
            .is_some_and(|origin| origin.as_bytes() == public_origin.as_bytes())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    const ORIGIN: &str = "https://rmm.example.com";

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    header::HeaderName::from_static(name),
                    HeaderValue::from_static(value),
                )
            })
            .collect()
    }

    #[test]
    fn reads_and_bearer_requests_are_not_checked() {
        assert!(!needs_check(&Method::GET, &HeaderMap::new()));
        assert!(!needs_check(
            &Method::POST,
            &headers(&[("authorization", "Bearer token")])
        ));
        assert!(needs_check(&Method::POST, &HeaderMap::new()));
        assert!(needs_check(
            &Method::POST,
            &headers(&[
                ("authorization", "Basic dXNlcjpwYXNz"),
                ("cookie", "__Host-meshrmm-session=abc")
            ])
        ));
        assert!(needs_check(&Method::DELETE, &HeaderMap::new()));
    }

    #[test]
    fn writes_need_the_header_and_the_public_origin() {
        assert!(allowed(
            &headers(&[("origin", ORIGIN), ("x-meshrmm-request", "1")]),
            ORIGIN
        ));
        assert!(!allowed(&headers(&[("origin", ORIGIN)]), ORIGIN));
        assert!(!allowed(&headers(&[("x-meshrmm-request", "1")]), ORIGIN));
        assert!(!allowed(
            &headers(&[
                ("origin", "https://evil.example"),
                ("x-meshrmm-request", "1")
            ]),
            ORIGIN
        ));
        assert!(!allowed(
            &headers(&[("origin", "null"), ("x-meshrmm-request", "1")]),
            ORIGIN
        ));
    }
}
