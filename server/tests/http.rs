//! The HTTP surface: health, errors, security headers and downloads.
mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn get(router: &Router, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

#[tokio::test]
async fn health_reports_the_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let state = meshrmm_server::prepare(common::config(dir.path(), "tls.mode = \"proxy\""))
        .await
        .unwrap();
    let router = meshrmm_server::http::router(state.clone());

    let (status, _, body) = get(&router, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "status": "ok", "schema_version": 1 })
    );

    // A database that stops answering makes the server unhealthy.
    state.database.close().await;
    let (status, _, body) = get(&router, "/healthz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "status": "error", "reason": "database_unavailable" })
    );
}

#[tokio::test]
async fn unknown_routes_are_json_404s_with_security_headers() {
    let dir = tempfile::tempdir().unwrap();
    let state = meshrmm_server::prepare(common::config(dir.path(), "tls.mode = \"proxy\""))
        .await
        .unwrap();
    let router = meshrmm_server::http::router(state);

    let (status, headers, body) = get(&router, "/v1/nothing-here").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, br#"{"error":"route not found"}"#);
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    assert!(
        headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    // Behind a reverse proxy the proxy owns HSTS.
    assert!(!headers.contains_key(header::STRICT_TRANSPORT_SECURITY));
}

#[tokio::test]
async fn hsts_is_sent_when_the_server_terminates_tls() {
    let dir = tempfile::tempdir().unwrap();
    let state = meshrmm_server::prepare(common::config(
        dir.path(),
        r#"tls = { mode = "files", cert_path = "/unused/cert.pem", key_path = "/unused/key.pem" }"#,
    ))
    .await
    .unwrap();
    let (_, headers, _) = get(&meshrmm_server::http::router(state), "/healthz").await;
    assert_eq!(
        headers[header::STRICT_TRANSPORT_SECURITY],
        "max-age=31536000"
    );
}

#[tokio::test]
async fn downloads_are_served_from_the_downloads_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("downloads")).unwrap();
    std::fs::write(
        dir.path().join("downloads/meshrmm-agent-windows-x64.exe"),
        b"MZ agent",
    )
    .unwrap();
    let state = meshrmm_server::prepare(common::config(dir.path(), "tls.mode = \"proxy\""))
        .await
        .unwrap();
    let router = meshrmm_server::http::router(state);

    let (status, _, body) = get(&router, "/downloads/meshrmm-agent-windows-x64.exe").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"MZ agent");
    let (status, _, _) = get(&router, "/downloads/missing.exe").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = get(&router, "/downloads/../data/instance.key").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
