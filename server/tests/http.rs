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

/// A downloads directory in `dir` holding `files`, listed in its
/// artifacts.json as `targets` (target, file, SHA-256 of the listed contents).
fn write_downloads(dir: &std::path::Path, files: &[(&str, &[u8])], targets: serde_json::Value) {
    let downloads = dir.join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    for (name, contents) in files {
        std::fs::write(downloads.join(name), contents).unwrap();
    }
    let artifacts = serde_json::json!({
        "schema_version": 1,
        "version": "1.2.0",
        "artifacts": targets,
    });
    std::fs::write(
        downloads.join("artifacts.json"),
        serde_json::to_vec(&artifacts).unwrap(),
    )
    .unwrap();
}

fn sha256(contents: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(contents))
}

#[tokio::test]
async fn downloads_serve_the_listed_builds_and_their_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let signature = "ab".repeat(64);
    write_downloads(
        dir.path(),
        &[
            ("meshrmm-agent-windows-x64.exe", b"MZ agent"),
            ("meshrmm-agent-macos.zip", b"PK agent"),
            ("notes.txt", b"not a build"),
        ],
        serde_json::json!({
            "agent-windows-x64": {
                "file": "meshrmm-agent-windows-x64.exe",
                "sha256": sha256(b"MZ agent"),
                "signature": signature,
            },
            "agent-macos": {
                "file": "meshrmm-agent-macos.zip",
                "sha256": sha256(b"PK agent"),
            },
        }),
    );
    let state = meshrmm_server::prepare(common::config(dir.path(), "tls.mode = \"proxy\""))
        .await
        .unwrap();
    assert_eq!(state.downloads.version(), Some("1.2.0"));
    // Neither is signed with this build's release key.
    assert_eq!(
        state.downloads.unsigned(),
        ["agent-macos", "agent-windows-x64"]
    );
    let router = meshrmm_server::http::router(state);

    let (status, headers, body) = get(&router, "/downloads/meshrmm-agent-windows-x64.exe").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"MZ agent");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");

    let (status, headers, body) = get(&router, "/downloads/update-manifest.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let manifest = meshrmm_self_update::UpdateManifest::parse(&body).unwrap();
    assert_eq!(
        manifest.releases["agent-windows-x64"],
        meshrmm_self_update::Release {
            version: "1.2.0".to_owned(),
            url: format!("{}/downloads/meshrmm-agent-windows-x64.exe", common::ORIGIN),
            sha256: sha256(b"MZ agent"),
            signature: Some(signature),
        }
    );
    assert_eq!(manifest.releases["agent-macos"].signature, None);

    // Only the listed builds are served.
    for path in [
        "/downloads/notes.txt",
        "/downloads/artifacts.json",
        "/downloads/missing.exe",
        "/downloads/../data/instance.key",
        "/downloads/nested/meshrmm-agent-macos.zip",
    ] {
        let (status, _, _) = get(&router, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn without_artifacts_json_there_are_no_downloads() {
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
    assert_eq!(state.downloads.version(), None);
    let router = meshrmm_server::http::router(state);
    for path in [
        "/downloads/meshrmm-agent-windows-x64.exe",
        "/downloads/update-manifest.json",
    ] {
        let (status, _, _) = get(&router, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn the_server_refuses_downloads_that_do_not_match_artifacts_json() {
    for (files, file) in [
        (&[("agent.exe", b"tampered".as_slice())][..], "agent.exe"),
        (&[][..], "agent.exe"),
        (&[][..], "../data/instance.key"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        write_downloads(
            dir.path(),
            files,
            serde_json::json!({
                "agent-windows-x64": { "file": file, "sha256": sha256(b"MZ agent") },
            }),
        );
        let error = meshrmm_server::prepare(common::config(dir.path(), "tls.mode = \"proxy\""))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("agent"), "{file}: {error:#}");
    }
}

async fn website_router(website: meshrmm_server::website::Website) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let mut state = meshrmm_server::prepare(common::config(dir.path(), "tls.mode = \"proxy\""))
        .await
        .unwrap();
    state.website = website;
    (dir, meshrmm_server::http::router(state))
}

async fn request(router: &Router, request: Request<Body>) -> axum::response::Response {
    router.clone().oneshot(request).await.unwrap()
}

fn test_website() -> meshrmm_server::website::Website {
    // Large enough to be worth compressing.
    let page = |name: &str| {
        format!("<!doctype html><title>{name}</title>{}", " ".repeat(2048)).into_bytes()
    };
    meshrmm_server::website::Website::from_files([
        ("index.html".to_owned(), page("devices")),
        ("toolbox/index.html".to_owned(), page("toolbox")),
        ("404.html".to_owned(), page("not found")),
        (
            "assets/index-abc123.js".to_owned(),
            b"console.log(1)".to_vec(),
        ),
    ])
}

#[tokio::test]
async fn the_website_serves_prerendered_pages_and_a_404_page() {
    let (_dir, router) = website_router(test_website()).await;

    let (status, headers, body) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8(body)
            .unwrap()
            .contains("<title>devices</title>")
    );
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    assert!(headers.contains_key(header::ETAG));
    assert!(
        headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .starts_with("default-src 'self'")
    );

    for path in ["/toolbox", "/toolbox/"] {
        let (status, _, body) = get(&router, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            String::from_utf8(body)
                .unwrap()
                .contains("<title>toolbox</title>")
        );
    }

    let (status, headers, _) = get(&router, "/assets/index-abc123.js").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "text/javascript; charset=utf-8"
    );

    // Unknown pages get the website's own 404 page; unknown API routes stay JSON.
    for path in ["/nothing-here", "/toolbox/deeper", "/404.html"] {
        let (status, headers, body) = get(&router, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(
            String::from_utf8(body)
                .unwrap()
                .contains("<title>not found</title>")
        );
    }
    let (status, _, body) = get(&router, "/v1/nothing-here").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, br#"{"error":"route not found"}"#);
}

#[tokio::test]
async fn website_pages_revalidate_compress_and_only_answer_reads() {
    let (_dir, router) = website_router(test_website()).await;
    let (_, headers, _) = get(&router, "/").await;
    let etag = headers[header::ETAG].clone();

    let unchanged = request(
        &router,
        Request::get("/")
            .header(header::IF_NONE_MATCH, etag)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(unchanged.status(), StatusCode::NOT_MODIFIED);

    let compressed = request(
        &router,
        Request::get("/")
            .header(header::ACCEPT_ENCODING, "gzip")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(compressed.status(), StatusCode::OK);
    assert_eq!(compressed.headers()[header::CONTENT_ENCODING], "gzip");

    let head = request(
        &router,
        Request::head("/toolbox").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);

    let post = request(
        &router,
        Request::post("/login").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(post.headers()[header::ALLOW], "GET, HEAD");
}

#[tokio::test]
async fn a_server_built_without_its_website_answers_with_404s() {
    let (_dir, router) = website_router(meshrmm_server::website::Website::default()).await;
    let (status, _, body) = get(&router, "/").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, br#"{"error":"route not found"}"#);
}
