//! Security headers on every response.
use axum::{
    Router,
    http::{HeaderName, HeaderValue, header},
};
use tower_http::set_header::SetResponseHeaderLayer;

/// Same-origin everything: the website, API and WebSockets share one origin,
/// and nothing may frame the website.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; base-uri 'none'; object-src 'none'; \
     frame-ancestors 'none'; form-action 'self'; img-src 'self' data: blob:; connect-src 'self'";
const STRICT_TRANSPORT_SECURITY: &str = "max-age=31536000";

/// Adds the security headers a handler did not set itself. `hsts` is off
/// behind a reverse proxy, which owns the HTTPS policy for its hostname.
pub fn apply(router: Router, hsts: bool) -> Router {
    let mut headers = vec![
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
    ];
    if hsts {
        headers.push((header::STRICT_TRANSPORT_SECURITY, STRICT_TRANSPORT_SECURITY));
    }
    headers.into_iter().fold(router, |router, (name, value)| {
        router.layer(SetResponseHeaderLayer::if_not_present(
            name,
            HeaderValue::from_static(value),
        ))
    })
}
