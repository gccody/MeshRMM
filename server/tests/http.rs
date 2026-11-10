//! The HTTP surface: health, errors, security headers, compression and
//! downloads.
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
            url: format!(
                "{}/downloads/release/meshrmm-agent-windows-x64.exe",
                common::ORIGIN
            ),
            sha256: sha256(b"MZ agent"),
            signature: Some(signature),
            developer_id: None,
        }
    );
    assert_eq!(manifest.releases["agent-macos"].signature, None);
    // Without macOS signing, every build installs as the release shipped it.
    for path in [
        "/downloads/release/meshrmm-agent-windows-x64.exe",
        "/downloads/meshrmm-agent-macos.zip",
        "/downloads/release/meshrmm-agent-macos.zip",
    ] {
        let (status, _, _) = get(&router, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }

    // Only the listed builds are served.
    for path in [
        "/downloads/notes.txt",
        "/downloads/artifacts.json",
        "/downloads/missing.exe",
        "/downloads/../data/instance.key",
        "/downloads/nested/meshrmm-agent-macos.zip",
        "/downloads/release/artifacts.json",
        "/downloads/release/../artifacts.json",
        "/downloads/developer-id/meshrmm-agent-macos.zip",
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

/// A stand-in for rcodesign that marks what it signs and notarizes, and logs
/// its calls to `calls` beside it. With `fail`, signing fails.
#[cfg(unix)]
fn fake_rcodesign(dir: &std::path::Path, fail: bool) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("rcodesign");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
echo "$1" >> "$(dirname "$0")/calls"
for last; do :; done
case "$1" in
    --version) echo "rcodesign 0.29.0" ;;
    analyze-certificate) printf '# Certificate 0\nTeam ID:                     ABCDE12345\nGuessed Certificate Profile: DeveloperIdApplication\n' ;;
    sign) {sign} ;;
    notary-submit) echo ticket > "$last/Contents/CodeResources" ;;
    *) exit 2 ;;
esac
"#,
            sign = if fail {
                "echo 'Error: the time-stamp server is unreachable' >&2; exit 1"
            } else {
                r#"mkdir -p "$last/Contents/_CodeSignature" && echo signed > "$last/Contents/_CodeSignature/CodeResources""#
            },
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A zipped `MeshRMM Agent.app`, as the release ships it.
#[cfg(unix)]
fn agent_archive() -> Vec<u8> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .add_directory(
            "MeshRMM Agent.app/Contents/MacOS/",
            SimpleFileOptions::default(),
        )
        .unwrap();
    writer
        .start_file(
            "MeshRMM Agent.app/Contents/MacOS/meshrmm-agent",
            SimpleFileOptions::default().unix_permissions(0o755),
        )
        .unwrap();
    writer.write_all(b"agent").unwrap();
    writer
        .start_file(
            "MeshRMM Agent.app/Contents/Info.plist",
            SimpleFileOptions::default(),
        )
        .unwrap();
    writer.write_all(b"plist").unwrap();
    writer.finish().unwrap().into_inner()
}

#[cfg(unix)]
fn signing_config(
    dir: &std::path::Path,
    rcodesign: &std::path::Path,
) -> meshrmm_server::config::Config {
    let secrets = dir.join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    for (name, contents) in [
        ("developer-id.p12", "certificate"),
        ("password", "secret"),
        ("notary.json", "{}"),
    ] {
        if !secrets.join(name).exists() {
            std::fs::write(secrets.join(name), contents).unwrap();
        }
    }
    common::config(
        dir,
        &format!(
            r#"
            tls.mode = "proxy"
            [downloads.macos_signing]
            certificate = "{secrets}/developer-id.p12"
            certificate_password_file = "{secrets}/password"
            notary_api_key = "{secrets}/notary.json"
            rcodesign = "{rcodesign}"
            "#,
            secrets = secrets.display(),
            rcodesign = rcodesign.display(),
        ),
    )
}

#[cfg(unix)]
async fn wait_for_signing(
    downloads: &meshrmm_server::downloads::Downloads,
) -> meshrmm_server::downloads::SigningStatus {
    use meshrmm_server::downloads::SigningStatus;
    for _ in 0..200 {
        match downloads.macos_signing().unwrap() {
            SigningStatus::Signing => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await
            }
            status => return status,
        }
    }
    panic!("signing never finished");
}

#[cfg(unix)]
#[tokio::test]
async fn the_server_signs_macos_builds_with_the_company_developer_id() {
    use meshrmm_server::downloads::SigningStatus;
    let dir = tempfile::tempdir().unwrap();
    let rcodesign = fake_rcodesign(dir.path(), false);
    let archive = agent_archive();
    write_downloads(
        dir.path(),
        &[
            ("meshrmm-agent-windows-x64.exe", b"MZ agent"),
            ("meshrmm-agent-macos.zip", &archive),
        ],
        serde_json::json!({
            "agent-windows-x64": {
                "file": "meshrmm-agent-windows-x64.exe",
                "sha256": sha256(b"MZ agent"),
            },
            "agent-macos": { "file": "meshrmm-agent-macos.zip", "sha256": sha256(&archive) },
        }),
    );
    let state = meshrmm_server::prepare(signing_config(dir.path(), &rcodesign))
        .await
        .unwrap();
    let downloads = state.downloads.clone();
    assert_eq!(downloads.macos_signing(), Some(SigningStatus::Signing));
    let router = meshrmm_server::http::router(state);

    // Until it's signed, the macOS installer waits; updates to the release
    // build, and other platforms, don't.
    let (status, _, body) = get(&router, "/downloads/meshrmm-agent-macos.zip").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(String::from_utf8_lossy(&body).contains("still signing"));
    let (status, _, body) = get(&router, "/downloads/release/meshrmm-agent-macos.zip").await;
    assert_eq!((status, body), (StatusCode::OK, archive.clone()));
    let (status, _, _) = get(&router, "/downloads/meshrmm-agent-windows-x64.exe").await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, body) = get(&router, "/downloads/update-manifest.json").await;
    let manifest = meshrmm_self_update::UpdateManifest::parse(&body).unwrap();
    assert_eq!(manifest.releases["agent-macos"].developer_id, None);

    downloads.start_signing();
    assert_eq!(wait_for_signing(&downloads).await, SigningStatus::Signed);

    let (status, _, signed) = get(&router, "/downloads/meshrmm-agent-macos.zip").await;
    assert_eq!(status, StatusCode::OK);
    let mut unpacked = zip::ZipArchive::new(std::io::Cursor::new(signed.clone())).unwrap();
    for (name, contents) in [
        ("MeshRMM Agent.app/Contents/MacOS/meshrmm-agent", "agent"),
        (
            "MeshRMM Agent.app/Contents/_CodeSignature/CodeResources",
            "signed\n",
        ),
        ("MeshRMM Agent.app/Contents/CodeResources", "ticket\n"),
    ] {
        let mut entry = unpacked.by_name(name).unwrap();
        let mut read = String::new();
        std::io::Read::read_to_string(&mut entry, &mut read).unwrap();
        assert_eq!(read, contents, "{name}");
        if name.ends_with("meshrmm-agent") {
            assert_eq!(entry.unix_mode().unwrap() & 0o777, 0o755);
        }
    }
    let (_, _, body) = get(&router, "/downloads/update-manifest.json").await;
    let manifest = meshrmm_self_update::UpdateManifest::parse(&body).unwrap();
    let macos = &manifest.releases["agent-macos"];
    assert_eq!(macos.sha256, sha256(&archive));
    assert_eq!(
        macos.developer_id,
        Some(meshrmm_self_update::Build {
            url: format!(
                "{}/downloads/developer-id/meshrmm-agent-macos.zip",
                common::ORIGIN
            ),
            sha256: sha256(&signed),
        })
    );
    assert_eq!(manifest.releases["agent-windows-x64"].developer_id, None);
    let (status, _, body) = get(&router, "/downloads/developer-id/meshrmm-agent-macos.zip").await;
    assert_eq!((status, body), (StatusCode::OK, signed.clone()));
    let (status, _, _) = get(&router, "/downloads/developer-id/builds.json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The next start reuses the signed build.
    let state = meshrmm_server::prepare(signing_config(dir.path(), &rcodesign))
        .await
        .unwrap();
    assert_eq!(state.downloads.macos_signing(), Some(SigningStatus::Signed));
    let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
    assert_eq!(calls.matches("sign\n").count(), 1, "{calls}");

    // Another certificate signs again.
    std::fs::write(dir.path().join("secrets/developer-id.p12"), "another").unwrap();
    let state = meshrmm_server::prepare(signing_config(dir.path(), &rcodesign))
        .await
        .unwrap();
    assert_eq!(
        state.downloads.macos_signing(),
        Some(SigningStatus::Signing)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_signing_keeps_the_macos_installer_unavailable() {
    use meshrmm_server::downloads::SigningStatus;
    let dir = tempfile::tempdir().unwrap();
    let rcodesign = fake_rcodesign(dir.path(), true);
    let archive = agent_archive();
    write_downloads(
        dir.path(),
        &[("meshrmm-agent-macos.zip", &archive)],
        serde_json::json!({
            "agent-macos": { "file": "meshrmm-agent-macos.zip", "sha256": sha256(&archive) },
        }),
    );
    let state = meshrmm_server::prepare(signing_config(dir.path(), &rcodesign))
        .await
        .unwrap();
    let downloads = state.downloads.clone();
    let router = meshrmm_server::http::router(state);
    downloads.start_signing();
    let SigningStatus::Failed(error) = wait_for_signing(&downloads).await else {
        panic!("signing succeeded");
    };
    assert!(
        error.contains("time-stamp server is unreachable"),
        "{error}"
    );
    let (status, _, body) = get(&router, "/downloads/meshrmm-agent-macos.zip").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(String::from_utf8_lossy(&body).contains("could not sign"));
    let (status, _, _) = get(&router, "/downloads/release/meshrmm-agent-macos.zip").await;
    assert_eq!(status, StatusCode::OK);
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

#[tokio::test]
async fn a_request_that_finds_no_free_database_connection_is_told_to_retry() {
    for app in common::apps().await {
        let admin = common::set_up(&app).await;
        let timeout = std::time::Duration::from_millis(200);
        let database = meshrmm_server::db::Database::connect_with(
            &app.state.config.database_url(),
            meshrmm_server::db::Limits {
                connections: 1,
                acquire_timeout: timeout,
                write_timeout: timeout,
            },
        )
        .await
        .unwrap();
        let mut state = app.state.clone();
        state.database = database.clone();
        let router = meshrmm_server::http::router(state);
        // Holds SQLite's writer, or PostgreSQL's one connection.
        let transaction = database.begin().await.unwrap();

        let started = std::time::Instant::now();
        let response = request(
            &router,
            admin
                .request(axum::http::Method::POST, "/v1/companies")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Acme"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            app.name
        );
        assert_eq!(response.headers()[header::RETRY_AFTER], "5", "{}", app.name);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "{}",
            app.name
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "server_busy", "{}", app.name);

        transaction.rollback().await.unwrap();
        database.close().await;
        app.finish().await;
    }
}

/// What browsers send.
const BROWSER_ENCODINGS: &str = "gzip, deflate, br, zstd";

/// `GET path` as `browser`, asking for `encoding`, with the body as sent.
async fn fetch(
    browser: &mut common::Browser,
    path: &str,
    encoding: Option<&str>,
) -> common::RawResponse {
    let mut request = browser.request(axum::http::Method::GET, path);
    if let Some(encoding) = encoding {
        request = request.header(header::ACCEPT_ENCODING, encoding);
    }
    browser.bytes(request.body(Body::empty()).unwrap()).await
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(bytes), &mut decoded).unwrap();
    decoded
}

fn unbrotli(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(
        &mut brotli_decompressor::Decompressor::new(bytes, 4096),
        &mut decoded,
    )
    .unwrap();
    decoded
}

type Decoder = fn(&[u8]) -> Vec<u8>;

/// Whether `response` says it would differ for another `Accept-Encoding`.
fn varies_by_encoding(response: &common::RawResponse) -> bool {
    response
        .header("vary")
        .to_ascii_lowercase()
        .contains("accept-encoding")
}

/// Whether `response` was sent exactly as its handler made it.
fn sent_as_is(response: &common::RawResponse) -> bool {
    !response.headers.contains_key(header::CONTENT_ENCODING) && !varies_by_encoding(response)
}

#[tokio::test]
async fn large_api_json_is_compressed_for_clients_that_accept_it() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;

        // Without Accept-Encoding, the body is sent as it is, with its
        // length; caches are told other clients may get it compressed.
        let plain = fetch(&mut admin, "/v1/permissions", None).await;
        assert_eq!(plain.status, StatusCode::OK, "{name}");
        assert!(plain.body.len() > 1024, "{name}: {}", plain.body.len());
        assert!(!plain.headers.contains_key(header::CONTENT_ENCODING));
        assert!(varies_by_encoding(&plain), "{name}: {:?}", plain.headers);
        assert_eq!(plain.header("content-length"), plain.body.len().to_string());
        assert_eq!(plain.header("cache-control"), "no-store");
        let expected: serde_json::Value = serde_json::from_slice(&plain.body).unwrap();

        let decoders: [(&str, Decoder); 2] = [("gzip", gunzip), ("br", unbrotli)];
        for (encoding, decode) in decoders {
            let compressed = fetch(&mut admin, "/v1/permissions", Some(encoding)).await;
            assert_eq!(compressed.status, StatusCode::OK, "{name}");
            assert_eq!(compressed.header("content-encoding"), encoding, "{name}");
            assert!(
                varies_by_encoding(&compressed),
                "{name}: {:?}",
                compressed.headers
            );
            assert_eq!(compressed.header("content-type"), "application/json");
            assert_eq!(compressed.header("cache-control"), "no-store");
            assert_eq!(compressed.header("x-content-type-options"), "nosniff");
            assert!(!compressed.headers.contains_key(header::CONTENT_LENGTH));
            assert!(
                compressed.body.len() < plain.body.len() / 2,
                "{name}: {encoding} made {} bytes {}",
                plain.body.len(),
                compressed.body.len()
            );
            let decoded: serde_json::Value =
                serde_json::from_slice(&decode(&compressed.body)).unwrap();
            assert_eq!(decoded, expected, "{name}: {encoding}");
        }

        // Browsers offer several encodings; brotli is chosen.
        let browser = fetch(&mut admin, "/v1/permissions", Some(BROWSER_ENCODINGS)).await;
        assert_eq!(browser.header("content-encoding"), "br", "{name}");

        // SCIM's JSON is compressed too.
        let created = admin
            .post(
                "/v1/settings/scim/tokens",
                serde_json::json!({ "name": "Okta" }),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
        let secret = created.body["secret"].as_str().unwrap().to_owned();
        let scim = |encoding: Option<&str>| {
            let mut request = Request::get("/scim/v2/Schemas")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"));
            if let Some(encoding) = encoding {
                request = request.header(header::ACCEPT_ENCODING, encoding);
            }
            let mut browser = app.browser();
            async move { browser.bytes(request.body(Body::empty()).unwrap()).await }
        };
        let plain = scim(None).await;
        assert_eq!(plain.status, StatusCode::OK, "{name}");
        assert!(!plain.headers.contains_key(header::CONTENT_ENCODING));
        assert!(plain.body.len() > 1024, "{name}: {}", plain.body.len());
        let compressed = scim(Some("gzip")).await;
        assert_eq!(compressed.header("content-type"), "application/scim+json");
        assert_eq!(compressed.header("content-encoding"), "gzip", "{name}");
        assert_eq!(gunzip(&compressed.body), plain.body, "{name}");

        app.finish().await;
    }
}

#[tokio::test]
async fn small_api_responses_and_errors_are_sent_as_they_are() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;

        let small = fetch(&mut admin, "/v1/instance", Some(BROWSER_ENCODINGS)).await;
        assert_eq!(small.status, StatusCode::OK, "{name}");
        assert!(small.body.len() < 1024, "{name}: {}", small.body.len());
        assert!(sent_as_is(&small), "{name}: {:?}", small.headers);
        assert_eq!(small.header("content-length"), small.body.len().to_string());
        serde_json::from_slice::<serde_json::Value>(&small.body).unwrap();

        // Errors are small JSON, from a handler or from no route at all.
        let mut stranger = app.browser();
        let refused = fetch(&mut stranger, "/v1/agents", Some(BROWSER_ENCODINGS)).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{name}");
        assert!(sent_as_is(&refused), "{name}: {:?}", refused.headers);
        let error: serde_json::Value = serde_json::from_slice(&refused.body).unwrap();
        assert!(error["error"].is_string(), "{name}: {error}");
        let missing = fetch(&mut admin, "/v1/nothing-here", Some(BROWSER_ENCODINGS)).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{name}");
        assert!(sent_as_is(&missing), "{name}");
        assert_eq!(missing.body, br#"{"error":"route not found"}"#);

        app.finish().await;
    }
}

#[tokio::test]
async fn images_and_file_downloads_are_sent_as_they_are() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let thumbnail = format!("/v1/agents/{}/thumbnail", agent.device_id);

        // A device without a thumbnail answers 204.
        let none = fetch(&mut admin, &thumbnail, Some(BROWSER_ENCODINGS)).await;
        assert_eq!(none.status, StatusCode::NO_CONTENT, "{name}");
        assert!(sent_as_is(&none), "{name}: {:?}", none.headers);
        assert!(none.body.is_empty());

        // A JPEG, however compressible, is sent as it is, and revalidates.
        let mut jpeg = vec![0u8; 64 * 1024];
        jpeg[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        let upload = agent
            .request(axum::http::Method::PUT, &thumbnail)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .body(Body::from(jpeg.clone()))
            .unwrap();
        assert_eq!(agent.send(upload).await.status, StatusCode::NO_CONTENT);
        let image = fetch(&mut admin, &thumbnail, Some(BROWSER_ENCODINGS)).await;
        assert_eq!(image.status, StatusCode::OK, "{name}");
        assert_eq!(image.header("content-type"), "image/jpeg");
        assert!(sent_as_is(&image), "{name}: {:?}", image.headers);
        assert_eq!(image.header("content-length"), jpeg.len().to_string());
        assert_eq!(image.body, jpeg);
        let request = admin
            .request(axum::http::Method::GET, &thumbnail)
            .header(header::ACCEPT_ENCODING, BROWSER_ENCODINGS)
            .header(header::IF_NONE_MATCH, image.header("etag"))
            .body(Body::empty())
            .unwrap();
        let unchanged = admin.bytes(request).await;
        assert_eq!(unchanged.status, StatusCode::NOT_MODIFIED, "{name}");
        assert!(sent_as_is(&unchanged), "{name}: {:?}", unchanged.headers);

        // A toolbox file streams with its length.
        let content = vec![b'a'; 300_000];
        let digest = {
            use sha2::Digest;
            meshrmm_server::secrets::hex(&sha2::Sha256::digest(&content))
        };
        let request = admin
            .request(
                axum::http::Method::POST,
                &format!("/v1/toolbox/files?name=big.txt&folder=Installers&sha256={digest}"),
            )
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(content.clone()))
            .unwrap();
        let uploaded = admin.bytes(request).await.json(name);
        assert_eq!(uploaded.status, StatusCode::CREATED, "{:?}", uploaded.body);
        let file_id = uploaded.body["id"].as_str().unwrap();
        let downloaded = fetch(
            &mut admin,
            &format!("/v1/toolbox/files/{file_id}/content"),
            Some(BROWSER_ENCODINGS),
        )
        .await;
        assert_eq!(downloaded.status, StatusCode::OK, "{name}");
        assert_eq!(
            downloaded.header("content-type"),
            "application/octet-stream"
        );
        assert!(sent_as_is(&downloaded), "{name}: {:?}", downloaded.headers);
        assert_eq!(
            downloaded.header("content-length"),
            content.len().to_string()
        );
        assert_eq!(downloaded.body, content);

        app.finish().await;
    }
}

#[tokio::test]
async fn sockets_upgrade_through_the_compressing_router() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    for app in common::apps().await {
        let name = app.name;
        let server = app.serve().await;
        let admin = common::set_up(&app).await;
        let mut request = server.ws_url("/v1/events").into_client_request().unwrap();
        let headers = request.headers_mut();
        headers.insert(header::ORIGIN, common::ORIGIN.parse().unwrap());
        headers.insert(
            header::COOKIE,
            admin.cookie.as_deref().unwrap().parse().unwrap(),
        );
        headers.insert(header::ACCEPT_ENCODING, BROWSER_ENCODINGS.parse().unwrap());
        let (mut events, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS, "{name}");
        assert!(
            !response.headers().contains_key(header::CONTENT_ENCODING),
            "{name}: {:?}",
            response.headers()
        );
        assert!(!response.headers().contains_key(header::VARY), "{name}");
        let snapshot = common::receive_json(&mut events).await;
        assert_eq!(snapshot["type"], "snapshot", "{name}");

        drop(events);
        drop(server);
        app.finish().await;
    }
}
