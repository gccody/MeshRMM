//! The file browser: listings and changes over the Agent's control
//! connection, who may do what as whom, downloads and uploads streamed
//! through the server with their checks, limits, and the audit log.
mod common;

use std::time::Duration;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use bytes::Bytes;
use common::{
    Agent, App, Browser, Server, Ws, apps_with, audit_events, receive_json, send_json, set_up,
    user_with, ws,
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use meshrmm_server::files::{Files, Timeouts};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

fn quick(state: &mut meshrmm_server::http::AppState) {
    state.files = Files::new(Timeouts {
        request: Duration::from_secs(5),
        agent_connect: Duration::from_millis(1500),
        ..Timeouts::default()
    });
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn enrolled(app: &App, admin: &mut Browser, server: &Server) -> (Agent, Ws) {
    let agent = app.enroll_on(admin, "files-01", "linux").await;
    let control = agent.connect(server).await.unwrap();
    (agent, control)
}

/// The next file request on `control`, answered with `reply`.
async fn answer(control: &mut Ws, reply: Value) -> Value {
    let command = receive_json(control).await;
    assert_eq!(command["type"], "file_request", "{command}");
    let request = command["request"].clone();
    send_json(
        control,
        json!({ "type": "file_reply", "request_id": request["request_id"], "reply": reply }),
    )
    .await;
    request
}

type Call = tokio::task::JoinHandle<common::Response>;

fn get(browser: &Browser, path: String) -> Call {
    let mut browser = browser.clone();
    tokio::spawn(async move { browser.get(&path).await })
}

fn post(browser: &Browser, path: String, body: Value) -> Call {
    let mut browser = browser.clone();
    tokio::spawn(async move { browser.post(&path, body).await })
}

/// Sends `request` to `app` and reads the body as it streams: an aborted
/// body is an error.
async fn fetch(
    app: &App,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Result<Vec<u8>, String>) {
    let router = meshrmm_server::http::router(app.state.clone());
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes().to_vec())
        .map_err(|error| error.to_string());
    (status, headers, body)
}

/// The Agent's socket for transfer `id`.
async fn transfer_socket(server: &Server, agent: &Agent, id: &str) -> Ws {
    ws(
        &server.ws_url(&format!(
            "/v1/agents/{}/file-transfers/{id}",
            agent.device_id
        )),
        &[("authorization", &format!("Bearer {}", agent.token))],
    )
    .await
    .unwrap()
}

/// The next file transfer the Agent is asked for, and its socket.
async fn next_transfer(server: &Server, agent: &Agent, control: &mut Ws) -> (Value, Ws) {
    let command = receive_json(control).await;
    assert_eq!(command["type"], "file_transfer", "{command}");
    let transfer = command["transfer"].clone();
    let socket = transfer_socket(server, agent, transfer["transfer_id"].as_str().unwrap()).await;
    (transfer, socket)
}

async fn send(socket: &mut Ws, message: Message) {
    socket.send(message).await.unwrap();
}

async fn event(socket: &mut Ws, value: Value) {
    send(socket, Message::Text(value.to_string().into())).await;
}

/// The audit events for `action` once there are `count`.
async fn wait_for_audit(
    app: &App,
    action: &str,
    count: usize,
) -> Vec<meshrmm_server::audit::Event> {
    let deadline = tokio::time::Instant::now() + common::SOCKET_WAIT;
    loop {
        let events = audit_events(app, action).await;
        if events.len() >= count {
            return events;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {} {action}",
            events.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn entry(name: &str, kind: &str) -> Value {
    json!({ "name": name, "kind": kind, "modified": 1_700_000_000_000_i64 })
}

#[tokio::test]
async fn listings_and_changes_go_through_the_agent() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server).await;
        let base = format!("/v1/agents/{}/files", agent.device_id);

        // A listing page, in the order asked for, as SYSTEM or root by default.
        let call = get(
            &admin,
            format!("{base}?path=/srv&sort=size&order=desc&limit=2&hidden=true"),
        );
        let listing = json!({
            "reply": "listing", "path": "/srv", "parent": "/", "account": "root",
            "offset": 0, "total": 3, "read_at": 1,
            "entries": [entry("logs", "directory"), { "name": "big.iso", "kind": "file", "size": 9 }],
        });
        let request = answer(&mut control, listing).await;
        assert_eq!(request["run_as"], "system", "{name}");
        assert!(
            request.get("notification").is_none(),
            "{name}: no notice for SYSTEM by default"
        );
        assert_eq!(
            request["operation"],
            json!({ "op": "list", "path": "/srv", "offset": 0, "limit": 2, "sort": "size", "descending": true, "hidden": true, "refresh": false })
        );
        let listed = call.await.unwrap();
        assert_eq!(listed.status, 200, "{name}: {:?}", listed.body);
        assert_eq!(listed.body["total"], 3);
        assert_eq!(listed.body["entries"][1]["size"], 9);

        // As the signed-in user, the first operation in a while tells them.
        for notified in [true, false] {
            let call = get(&admin, format!("{base}/roots?run_as=user"));
            let roots = json!({ "reply": "roots", "account": "ada", "roots": [{ "path": "/", "label": "/", "kind": "root" }], "places": [] });
            let request = answer(&mut control, roots).await;
            assert_eq!(request["run_as"], "user");
            assert_eq!(request["operation"], json!({ "op": "roots" }));
            assert_eq!(
                request.get("notification").is_some(),
                notified,
                "{name}: {request}"
            );
            assert_eq!(call.await.unwrap().body["roots"][0]["kind"], "root");
        }

        // The device's refusal keeps its code.
        let call = get(&admin, format!("{base}?path=/root"));
        answer(
            &mut control,
            json!({ "reply": "failed", "error": { "code": "access_denied", "message": "Access to /root is denied." } }),
        )
        .await;
        let denied = call.await.unwrap();
        assert_eq!(denied.status, 403, "{name}");
        assert_eq!(denied.code(), "access_denied");
        assert_eq!(denied.body["error"], "Access to /root is denied.");

        // Changes are recorded.
        let call = post(
            &admin,
            format!("{base}/folders"),
            json!({ "path": "/srv/new" }),
        );
        let request = answer(
            &mut control,
            json!({ "reply": "entry", "path": "/srv/new", "entry": entry("new", "directory") }),
        )
        .await;
        assert_eq!(
            request["operation"],
            json!({ "op": "create_folder", "path": "/srv/new" })
        );
        assert_eq!(call.await.unwrap().status, 201);
        let call = post(
            &admin,
            format!("{base}/rename"),
            json!({ "path": "/srv/new", "name": "old" }),
        );
        answer(
            &mut control,
            json!({ "reply": "entry", "path": "/srv/old", "entry": entry("old", "directory") }),
        )
        .await;
        assert_eq!(call.await.unwrap().body["path"], "/srv/old");
        let call = post(
            &admin,
            format!("{base}/move"),
            json!({ "paths": ["/srv/a", "/srv/b"], "destination": "/srv/old" }),
        );
        let batch = json!({ "reply": "batch", "done": ["/srv/a"], "failed": [{ "path": "/srv/b", "error": { "code": "already_exists", "message": "/srv/old/b already exists." } }] });
        let request = answer(&mut control, batch).await;
        assert_eq!(request["operation"]["destination"], "/srv/old");
        let moved = call.await.unwrap();
        assert_eq!(
            moved.body["failed"][0]["error"]["code"], "already_exists",
            "{name}"
        );
        let call = post(
            &admin,
            format!("{base}/delete"),
            json!({ "paths": ["/srv/old"], "trash": true, "run_as": "user" }),
        );
        let request = answer(
            &mut control,
            json!({ "reply": "batch", "done": ["/srv/old"], "failed": [] }),
        )
        .await;
        assert_eq!(
            request["operation"],
            json!({ "op": "delete", "paths": ["/srv/old"], "trash": true })
        );
        assert_eq!(call.await.unwrap().status, 200);
        // Nothing deleted, nothing recorded.
        let call = post(
            &admin,
            format!("{base}/delete"),
            json!({ "paths": ["/nope"] }),
        );
        answer(&mut control, json!({ "reply": "batch", "done": [], "failed": [{ "path": "/nope", "error": { "code": "not_found", "message": "gone" } }] })).await;
        assert_eq!(call.await.unwrap().status, 200);

        let created = audit_events(&app, "files.create_folder").await;
        assert_eq!(
            created[0].metadata,
            json!({ "path": "/srv/new", "run_as": "system" }),
            "{name}"
        );
        assert_eq!(created[0].target_id, agent.device_id);
        let renamed = audit_events(&app, "files.rename").await;
        assert_eq!(renamed[0].metadata["new_path"], "/srv/old");
        let moved = audit_events(&app, "files.move").await;
        assert_eq!(moved[0].metadata["paths"], json!(["/srv/a"]));
        assert_eq!(
            moved[0].metadata["failed"],
            json!([{ "path": "/srv/b", "code": "already_exists" }])
        );
        let deleted = audit_events(&app, "files.delete").await;
        assert_eq!(deleted.len(), 1, "{name}");
        assert_eq!(deleted[0].metadata["trash"], true);
        assert_eq!(deleted[0].metadata["run_as"], "user");

        // Bad requests never reach the device.
        assert_eq!(
            admin.get(&format!("{base}?path=/&sort=owner")).await.status,
            400
        );
        assert_eq!(
            admin.get(&format!("{base}?path=/&limit=5000")).await.status,
            400
        );
        assert_eq!(
            admin
                .post(&format!("{base}/delete"), json!({ "paths": [] }))
                .await
                .status,
            400
        );
        common::quiet(&mut control, Duration::from_millis(200)).await;

        // A device that disconnects fails what it was asked; an offline one
        // is refused.
        let call = get(&admin, format!("{base}?path=/"));
        assert_eq!(receive_json(&mut control).await["type"], "file_request");
        drop(control);
        let gone = call.await.unwrap();
        assert_eq!(gone.status, 409, "{name}");
        assert_eq!(gone.code(), "device_disconnected");
        let deadline = tokio::time::Instant::now() + common::SOCKET_WAIT;
        while app.state.agents.is_connected(&agent.device_id) {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            admin.get(&format!("{base}?path=/")).await.code(),
            "device_offline"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn permissions_decide_what_and_as_whom() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server).await;
        let base = format!("/v1/agents/{}/files", agent.device_id);

        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        assert_eq!(
            viewer.get(&format!("{base}?path=/")).await.code(),
            "permission_denied",
            "{name}"
        );
        // Browsing as the signed-in user only.
        let (mut reader, _) =
            user_with(&app, &mut admin, "reader@example.com", &["files.browse"]).await;
        assert_eq!(
            reader.get(&format!("{base}?path=/")).await.status,
            403,
            "{name}: SYSTEM needs files.system"
        );
        assert_eq!(
            reader
                .post(
                    &format!("{base}/folders"),
                    json!({ "path": "/tmp/x", "run_as": "user" })
                )
                .await
                .status,
            403,
            "{name}: changes need files.manage"
        );
        assert_eq!(
            reader
                .post(
                    &format!("{base}/uploads"),
                    json!({ "path": "/tmp/x", "size": 1, "run_as": "user" })
                )
                .await
                .status,
            403
        );
        let call = get(&reader, format!("{base}?path=/home&run_as=user"));
        let listing = json!({ "reply": "listing", "path": "/home", "account": "ada", "offset": 0, "total": 0, "entries": [], "read_at": 1 });
        answer(&mut control, listing.clone()).await;
        assert_eq!(call.await.unwrap().status, 200, "{name}");

        // An API token works within its user's permissions.
        let created = admin
            .post("/v1/api-tokens", json!({ "name": "Collector", "expires_in_days": 30, "permissions": ["files.browse", "files.system"] }))
            .await;
        let secret = created.body["secret"].as_str().unwrap().to_owned();
        let token_get = |path: String| {
            let mut browser = app.browser();
            let secret = secret.clone();
            tokio::spawn(async move {
                let request = Request::builder()
                    .uri(path)
                    .header("authorization", format!("Bearer {secret}"))
                    .body(Body::empty())
                    .unwrap();
                browser.raw(request).await
            })
        };
        let call = token_get(format!("{base}?path=/"));
        answer(&mut control, listing).await;
        assert_eq!(call.await.unwrap().status, 200, "{name}");
        let refused = {
            let mut browser = app.browser();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("{base}/folders"))
                .header("authorization", format!("Bearer {secret}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({ "path": "/x" }).to_string()))
                .unwrap();
            browser.raw(request).await
        };
        assert_eq!(
            refused.code(),
            "permission_denied",
            "{name}: the token can't manage files"
        );

        // The transfer limit is the operator's, and checked up front.
        assert_eq!(
            admin
                .patch("/v1/settings", json!({ "file_transfer_limit_mb": 0 }))
                .await
                .status,
            400
        );
        let changed = admin
            .patch("/v1/settings", json!({ "file_transfer_limit_mb": 1 }))
            .await;
        assert_eq!(changed.status, 200, "{name}: {:?}", changed.body);
        assert_eq!(
            admin.get("/v1/settings").await.body["file_transfer_limit_mb"],
            1
        );
        let large = admin
            .post(
                &format!("{base}/uploads"),
                json!({ "path": "/tmp/big", "size": 2 * 1024 * 1024 }),
            )
            .await;
        assert_eq!(large.status, 413, "{name}");
        assert_eq!(large.code(), "too_large");
        common::quiet(&mut control, Duration::from_millis(200)).await;

        let permissions = admin.get("/v1/permissions").await;
        let names = permissions
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        for permission in ["files.browse", "files.manage", "files.system"] {
            assert!(names.iter().any(|name| name == permission), "{permission}");
        }
        app.finish().await;
    }
}

#[tokio::test]
async fn downloads_stream_verify_and_resume() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server).await;
        let content: Vec<u8> = (0..700_000u32).map(|index| (index % 251) as u8).collect();

        // Setting a download up checks the file on the device.
        let call = post(
            &admin,
            format!("/v1/agents/{}/files/downloads", agent.device_id),
            json!({ "paths": ["/srv/data.bin"] }),
        );
        let info = json!({ "reply": "download", "name": "data.bin", "kind": "file", "size": content.len(), "modified": 123, "files": 1 });
        let request = answer(&mut control, info).await;
        assert_eq!(
            request["operation"],
            json!({ "op": "prepare_download", "paths": ["/srv/data.bin"], "max_bytes": 4096_u64 * 1024 * 1024 }),
            "{name}"
        );
        let created = call.await.unwrap();
        assert_eq!(created.status, 201, "{name}: {:?}", created.body);
        let url = created.body["url"].as_str().unwrap().to_owned();
        assert_eq!(created.body["progress"]["state"], "ready");

        // The whole file, verified as it streams.
        let fetching = {
            let request = admin
                .request(Method::GET, &url)
                .body(Body::empty())
                .unwrap();
            let state = app.state.clone();
            tokio::spawn(async move {
                let router = meshrmm_server::http::router(state);
                let response = router.oneshot(request).await.unwrap();
                let status = response.status();
                let headers = response.headers().clone();
                let body = response
                    .into_body()
                    .collect()
                    .await
                    .map(|c| c.to_bytes().to_vec());
                (status, headers, body.map_err(|error| error.to_string()))
            })
        };
        let (transfer, mut socket) = next_transfer(&server, &agent, &mut control).await;
        assert_eq!(transfer["run_as"], "system");
        assert_eq!(
            transfer["transfer"],
            json!({ "direction": "download", "paths": ["/srv/data.bin"], "kind": "file", "offset": 0, "max_bytes": 4096_u64 * 1024 * 1024 })
        );
        event(
            &mut socket,
            json!({ "type": "begin", "size": content.len(), "modified": 123 }),
        )
        .await;
        for piece in content.chunks(256 * 1024) {
            send(&mut socket, Message::Binary(piece.to_vec().into())).await;
        }
        event(
            &mut socket,
            json!({ "type": "done", "bytes": content.len(), "sha256": hex(&content) }),
        )
        .await;
        let (status, headers, body) = fetching.await.unwrap();
        assert_eq!(status, 200, "{name}");
        assert_eq!(body.unwrap(), content);
        assert_eq!(
            headers["content-length"],
            content.len().to_string().as_str()
        );
        assert_eq!(headers["accept-ranges"], "bytes");
        assert_eq!(
            headers["etag"],
            format!("\"{}-123\"", content.len()).as_str()
        );
        assert!(
            headers["content-disposition"]
                .to_str()
                .unwrap()
                .contains("data.bin")
        );
        let done = wait_for_audit(&app, "files.download", 1).await;
        assert_eq!(done[0].metadata["result"], "completed", "{name}");
        assert_eq!(done[0].metadata["sha256"], hex(&content));
        assert_eq!(done[0].metadata["bytes"], content.len());
        let progress = admin.get(&format!("{url}/progress")).await;
        assert_eq!(progress.body["progress"]["state"], "done");
        assert_eq!(progress.body["progress"]["sha256"], hex(&content));

        // A resumed download goes on from where the browser got to.
        let request = admin
            .request(Method::GET, &url)
            .header("range", "bytes=500000-")
            .header("if-range", format!("\"{}-123\"", content.len()))
            .body(Body::empty())
            .unwrap();
        let state = app.state.clone();
        let fetching = tokio::spawn(async move {
            let router = meshrmm_server::http::router(state);
            let response = router.oneshot(request).await.unwrap();
            let headers = response.headers().clone();
            (
                response.status(),
                headers,
                response.into_body().collect().await.unwrap().to_bytes(),
            )
        });
        let (transfer, mut socket) = next_transfer(&server, &agent, &mut control).await;
        assert_eq!(transfer["transfer"]["offset"], 500_000);
        assert_eq!(
            transfer["transfer"]["expect"],
            json!({ "size": content.len(), "modified": 123 })
        );
        let rest = &content[500_000..];
        event(
            &mut socket,
            json!({ "type": "begin", "size": rest.len(), "modified": 123 }),
        )
        .await;
        send(&mut socket, Message::Binary(rest.to_vec().into())).await;
        event(
            &mut socket,
            json!({ "type": "done", "bytes": rest.len(), "sha256": hex(rest) }),
        )
        .await;
        let (status, headers, body) = fetching.await.unwrap();
        assert_eq!(status, 206, "{name}");
        assert_eq!(
            headers["content-range"],
            format!("bytes 500000-{}/{}", content.len() - 1, content.len()).as_str()
        );
        assert_eq!(body.to_vec(), rest);

        // Bytes that don't match the device's checksum abort the response.
        let request = admin
            .request(Method::GET, &url)
            .body(Body::empty())
            .unwrap();
        let state = app.state.clone();
        let fetching = tokio::spawn(async move {
            let router = meshrmm_server::http::router(state);
            let response = router.oneshot(request).await.unwrap();
            response
                .into_body()
                .collect()
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        });
        let (_, mut socket) = next_transfer(&server, &agent, &mut control).await;
        event(&mut socket, json!({ "type": "begin", "size": 4 })).await;
        send(&mut socket, Message::Binary(Bytes::from_static(b"abcd"))).await;
        event(
            &mut socket,
            json!({ "type": "done", "bytes": 4, "sha256": hex(b"wxyz") }),
        )
        .await;
        assert!(fetching.await.unwrap().is_err(), "{name}");
        let failed = wait_for_audit(&app, "files.download", 3).await;
        assert_eq!(failed[0].metadata["result"], "failed");
        assert!(failed[0].metadata["sha256"].is_null());

        // So does the device going away mid-stream.
        let request = admin
            .request(Method::GET, &url)
            .body(Body::empty())
            .unwrap();
        let state = app.state.clone();
        let fetching = tokio::spawn(async move {
            let router = meshrmm_server::http::router(state);
            let response = router.oneshot(request).await.unwrap();
            response
                .into_body()
                .collect()
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        });
        let (_, mut socket) = next_transfer(&server, &agent, &mut control).await;
        event(&mut socket, json!({ "type": "begin", "size": 10 })).await;
        send(&mut socket, Message::Binary(Bytes::from_static(b"abc"))).await;
        drop(socket);
        assert!(fetching.await.unwrap().is_err(), "{name}");

        // A failure before the first byte is a plain error response.
        let request = admin
            .request(Method::GET, &url)
            .body(Body::empty())
            .unwrap();
        let fetching = {
            let state = app.state.clone();
            tokio::spawn(async move {
                let router = meshrmm_server::http::router(state);
                let response = router.oneshot(request).await.unwrap();
                response.status()
            })
        };
        let (_, mut socket) = next_transfer(&server, &agent, &mut control).await;
        event(
            &mut socket,
            json!({ "type": "failed", "error": { "code": "changed", "message": "It changed." } }),
        )
        .await;
        assert_eq!(fetching.await.unwrap(), StatusCode::CONFLICT, "{name}");

        // Folders arrive as a ZIP archive, of unknown length.
        let call = post(
            &admin,
            format!("/v1/agents/{}/files/downloads", agent.device_id),
            json!({ "paths": ["/srv/logs"] }),
        );
        answer(&mut control, json!({ "reply": "download", "name": "logs", "kind": "archive", "size": 50, "files": 2 })).await;
        let archive_url = call.await.unwrap().body["url"].as_str().unwrap().to_owned();
        let (status, headers, body) = {
            let request = admin
                .request(Method::GET, &archive_url)
                .body(Body::empty())
                .unwrap();
            let fetching = tokio::spawn({
                let state = app.state.clone();
                async move {
                    let router = meshrmm_server::http::router(state);
                    let response = router.oneshot(request).await.unwrap();
                    let headers = response.headers().clone();
                    (
                        response.status(),
                        headers,
                        response.into_body().collect().await.unwrap().to_bytes(),
                    )
                }
            });
            let (transfer, mut socket) = next_transfer(&server, &agent, &mut control).await;
            assert_eq!(transfer["transfer"]["kind"], "archive");
            event(&mut socket, json!({ "type": "begin" })).await;
            send(
                &mut socket,
                Message::Binary(Bytes::from_static(b"PK\x03\x04zip")),
            )
            .await;
            event(
                &mut socket,
                json!({ "type": "done", "bytes": 7, "sha256": hex(b"PK\x03\x04zip") }),
            )
            .await;
            fetching.await.unwrap()
        };
        assert_eq!(status, 200, "{name}");
        assert_eq!(headers["content-type"], "application/zip");
        assert!(
            headers["content-disposition"]
                .to_str()
                .unwrap()
                .contains("logs.zip")
        );
        assert_eq!(&body[..], b"PK\x03\x04zip");

        // Only its user may fetch it.
        let (mut other, _) = user_with(
            &app,
            &mut admin,
            "other@example.com",
            &["files.browse", "files.system"],
        )
        .await;
        assert_eq!(other.get(&url).await.code(), "download_not_found", "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn transfers_are_limited_per_device() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server).await;
        let call = post(
            &admin,
            format!("/v1/agents/{}/files/downloads", agent.device_id),
            json!({ "paths": ["/a"] }),
        );
        answer(
            &mut control,
            json!({ "reply": "download", "name": "a", "kind": "file", "size": 1, "files": 1 }),
        )
        .await;
        let url = call.await.unwrap().body["url"].as_str().unwrap().to_owned();
        // The Agent never connects, so each transfer waits.
        let waiting = (0..meshrmm_server::files::MAX_TRANSFERS_PER_DEVICE)
            .map(|_| {
                let request = admin
                    .request(Method::GET, &url)
                    .body(Body::empty())
                    .unwrap();
                let state = app.state.clone();
                tokio::spawn(async move {
                    let router = meshrmm_server::http::router(state);
                    router.oneshot(request).await.unwrap().status()
                })
            })
            .collect::<Vec<_>>();
        let deadline = tokio::time::Instant::now() + common::SOCKET_WAIT;
        while app.state.files.transfers_on(&agent.device_id)
            < meshrmm_server::files::MAX_TRANSFERS_PER_DEVICE
        {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let refused = fetch(
            &app,
            admin
                .request(Method::GET, &url)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(refused.0, StatusCode::TOO_MANY_REQUESTS, "{name}");
        for call in waiting {
            assert_eq!(call.await.unwrap(), StatusCode::GATEWAY_TIMEOUT, "{name}");
        }
        assert_eq!(app.state.files.transfers_on(&agent.device_id), 0);
        app.finish().await;
    }
}

#[tokio::test]
async fn uploads_resume_and_land_only_when_verified() {
    for app in apps_with(quick).await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let (agent, mut control) = enrolled(&app, &mut admin, &server).await;
        let content: Vec<u8> = (0..600_000u32).map(|index| (index % 253) as u8).collect();

        let call = post(
            &admin,
            format!("/v1/agents/{}/files/uploads", agent.device_id),
            json!({ "path": "/srv/in/up.bin", "size": content.len(), "run_as": "user" }),
        );
        let request = answer(&mut control, json!({ "reply": "upload", "received": 0 })).await;
        let upload_id = request["operation"]["upload_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            request["operation"],
            json!({ "op": "prepare_upload", "upload_id": upload_id, "path": "/srv/in/up.bin", "size": content.len(), "overwrite": false }),
            "{name}"
        );
        let created = call.await.unwrap();
        assert_eq!(created.status, 201, "{name}: {:?}", created.body);
        assert_eq!(created.body["id"], upload_id.as_str());
        let url = format!("/v1/file-uploads/{upload_id}");

        // A first attempt breaks off partway.
        let first = content[..100_000].to_vec();
        let pieces: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from(first.clone())),
            Err(std::io::Error::other("cut")),
        ];
        let request = admin
            .request(Method::PUT, &format!("{url}?offset=0"))
            .body(Body::from_stream(futures_util::stream::iter(pieces)))
            .unwrap();
        let sending = {
            let mut browser = admin.clone();
            tokio::spawn(async move { browser.raw(request).await })
        };
        let (transfer, mut socket) = next_transfer(&server, &agent, &mut control).await;
        assert_eq!(transfer["run_as"], "user");
        assert_eq!(
            transfer["transfer"],
            json!({ "direction": "upload", "upload_id": upload_id, "path": "/srv/in/up.bin", "offset": 0, "size": content.len() })
        );
        event(&mut socket, json!({ "type": "ready", "received": 0 })).await;
        let mut got = Vec::new();
        loop {
            match tokio::time::timeout(common::SOCKET_WAIT, socket.next())
                .await
                .unwrap()
            {
                Some(Ok(Message::Binary(bytes))) => got.extend_from_slice(&bytes),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Text(text))) => panic!("{name}: no end for a broken body: {text}"),
                _ => break,
            }
        }
        assert_eq!(got, first, "{name}");
        let broken = sending.await.unwrap();
        assert_eq!(broken.code(), "upload_interrupted", "{name}");

        // The device says how far it got, and the rest follows from there.
        let call = get(&admin, url.clone());
        let request = answer(
            &mut control,
            json!({ "reply": "upload", "received": 100_000 }),
        )
        .await;
        assert_eq!(
            request["operation"],
            json!({ "op": "upload_status", "upload_id": upload_id, "path": "/srv/in/up.bin" })
        );
        assert_eq!(call.await.unwrap().body["received"], 100_000);
        let request = admin
            .request(Method::PUT, &format!("{url}?offset=100000"))
            .body(Body::from(content[100_000..].to_vec()))
            .unwrap();
        let sending = {
            let mut browser = admin.clone();
            tokio::spawn(async move { browser.raw(request).await })
        };
        let (transfer, mut socket) = next_transfer(&server, &agent, &mut control).await;
        assert_eq!(transfer["transfer"]["offset"], 100_000);
        event(&mut socket, json!({ "type": "ready", "received": 100_000 })).await;
        let mut got = Vec::new();
        loop {
            match tokio::time::timeout(common::SOCKET_WAIT, socket.next())
                .await
                .unwrap()
            {
                Some(Ok(Message::Binary(bytes))) => {
                    assert!(bytes.len() <= 256 * 1024);
                    got.extend_from_slice(&bytes);
                }
                Some(Ok(Message::Text(text))) => {
                    assert_eq!(
                        serde_json::from_str::<Value>(&text).unwrap(),
                        json!({ "type": "end" })
                    );
                    break;
                }
                other => panic!("{name}: {other:?}"),
            }
        }
        assert_eq!(got, content[100_000..]);
        event(
            &mut socket,
            json!({ "type": "done", "bytes": content.len() }),
        )
        .await;
        let sent = sending.await.unwrap();
        assert_eq!(sent.status, 200, "{name}: {:?}", sent.body);
        assert_eq!(sent.body["received"], content.len());

        // A wrong checksum discards it; the right one puts it in place.
        let call = post(
            &admin,
            format!("{url}/finish"),
            json!({ "sha256": hex(&content) }),
        );
        let request = answer(&mut control, json!({ "reply": "entry", "path": "/srv/in/up.bin", "entry": { "name": "up.bin", "kind": "file", "size": content.len() } })).await;
        assert_eq!(
            request["operation"],
            json!({ "op": "finish_upload", "upload_id": upload_id, "path": "/srv/in/up.bin", "size": content.len(), "sha256": hex(&content), "overwrite": false })
        );
        assert_eq!(call.await.unwrap().status, 200, "{name}");
        let uploaded = audit_events(&app, "files.upload").await;
        assert_eq!(
            uploaded[0].metadata,
            json!({ "path": "/srv/in/up.bin", "size": content.len(), "sha256": hex(&content), "overwrite": false, "run_as": "user" }),
            "{name}"
        );
        assert_eq!(
            admin.get(&url).await.code(),
            "upload_not_found",
            "finished uploads are forgotten"
        );

        let call = post(
            &admin,
            format!("/v1/agents/{}/files/uploads", agent.device_id),
            json!({ "path": "/srv/x", "size": 3 }),
        );
        answer(&mut control, json!({ "reply": "upload", "received": 0 })).await;
        let id = call.await.unwrap().body["id"].as_str().unwrap().to_owned();
        let call = post(
            &admin,
            format!("/v1/file-uploads/{id}/finish"),
            json!({ "sha256": hex(b"abc") }),
        );
        answer(&mut control, json!({ "reply": "failed", "error": { "code": "checksum_mismatch", "message": "x arrived different from what was sent." } })).await;
        let mismatch = call.await.unwrap();
        assert_eq!(mismatch.status, 422, "{name}");
        assert_eq!(mismatch.code(), "checksum_mismatch");
        assert_eq!(
            admin.get(&format!("/v1/file-uploads/{id}")).await.code(),
            "upload_not_found"
        );

        // A device that runs out of room stops the upload with its reason.
        let call = post(
            &admin,
            format!("/v1/agents/{}/files/uploads", agent.device_id),
            json!({ "path": "/srv/y", "size": content.len() }),
        );
        answer(&mut control, json!({ "reply": "upload", "received": 0 })).await;
        let id = call.await.unwrap().body["id"].as_str().unwrap().to_owned();
        let request = admin
            .request(Method::PUT, &format!("/v1/file-uploads/{id}"))
            .body(Body::from(content.clone()))
            .unwrap();
        let sending = {
            let mut browser = admin.clone();
            tokio::spawn(async move { browser.raw(request).await })
        };
        let (_, mut socket) = next_transfer(&server, &agent, &mut control).await;
        event(&mut socket, json!({ "type": "ready", "received": 0 })).await;
        event(&mut socket, json!({ "type": "failed", "error": { "code": "no_space", "message": "The disk is full." } })).await;
        let full = sending.await.unwrap();
        assert_eq!(full.status, 507, "{name}");
        assert_eq!(full.code(), "no_space");

        // Cancelling removes the partial file on the device.
        let call = {
            let mut browser = admin.clone();
            let path = format!("/v1/file-uploads/{id}");
            tokio::spawn(async move { browser.delete(&path).await })
        };
        let request = answer(&mut control, json!({ "reply": "upload", "received": 0 })).await;
        assert_eq!(request["operation"]["op"], "cancel_upload");
        assert_eq!(call.await.unwrap().status, 204, "{name}");
        app.finish().await;
    }
}
