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
