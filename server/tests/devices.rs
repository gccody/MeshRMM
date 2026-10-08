//! Devices: listing, deletion, credential rotation, screen thumbnails and
//! remote handoffs.
mod common;

use axum::{
    body::Body,
    http::{Method, StatusCode},
};
use common::{Agent, App, audit_events};
use meshrmm_protocol_types::AgentCommand;
use serde_json::json;

fn jpeg(seed: u8, len: usize) -> Vec<u8> {
    let mut bytes = vec![seed; len.max(4)];
    bytes[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    bytes
}

async fn upload(agent: &Agent, bytes: Vec<u8>) -> common::RawResponse {
    let request = agent
        .request(
            Method::PUT,
            &format!("/v1/agents/{}/thumbnail", agent.device_id),
        )
        .header("content-type", "image/jpeg")
        .body(Body::from(bytes))
        .unwrap();
    agent.send(request).await
}

async fn thumbnail(
    browser: &mut common::Browser,
    device_id: &str,
    if_none_match: Option<&str>,
) -> common::RawResponse {
    let mut request = browser.request(Method::GET, &format!("/v1/agents/{device_id}/thumbnail"));
    if let Some(etag) = if_none_match {
        request = request.header("if-none-match", etag);
    }
    browser.bytes(request.body(Body::empty()).unwrap()).await
}

/// Whether the agent's credential works, judged by a report on a run that
/// doesn't exist: 404 once authenticated, 401 otherwise.
async fn authenticates(agent: &Agent) -> bool {
    let response = agent
        .report(
            &format!("script-runs/{}/result", uuid::Uuid::new_v4()),
            json!({ "status": "completed", "ran_as": "SYSTEM", "stdout": "", "stderr": "" }),
        )
        .await;
    match response.status {
        StatusCode::NOT_FOUND => true,
        StatusCode::UNAUTHORIZED => false,
        status => panic!("unexpected {status}: {:?}", response.body),
    }
}

async fn credential_hashes(app: &App, device_id: &str) -> (String, Option<String>) {
    app.db()
        .fetch_one(
            &sea_query::Query::select()
                .columns([
                    sea_query::Alias::new("auth_token_hash"),
                    sea_query::Alias::new("pending_auth_token_hash"),
                ])
                .from(sea_query::Alias::new("agents"))
                .and_where(sea_query::ExprTrait::eq(
                    sea_query::Expr::col(sea_query::Alias::new("id")),
                    device_id,
                ))
                .to_owned(),
        )
        .await
        .unwrap()
}

fn hash(token: &str) -> String {
    meshrmm_server::secrets::token_hash(token)
}

#[tokio::test]
async fn devices_list_online_first_and_deleting_one_uninstalls_its_agent() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let zed = app.enroll(&mut admin, "zed").await;
        let alpha = app.enroll(&mut admin, "Alpha").await;
        let beta = app.enroll(&mut admin, "beta").await;
        let names = |body: &serde_json::Value| {
            body["agents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|agent| agent["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&admin.get("/v1/agents").await.body),
            ["Alpha", "beta", "zed"]
        );
        let mut connection = app.state.agents.connect(&zed.device_id);
        let listed = admin.get("/v1/agents").await.body;
        assert_eq!(names(&listed), ["zed", "Alpha", "beta"], "{name}");
        assert_eq!(listed["agents"][0]["connected"], true);
        assert_eq!(listed["agents"][1]["connected"], false);

        assert_eq!(
            upload(&zed, jpeg(1, 100)).await.status,
            StatusCode::NO_CONTENT
        );
        let (mut viewer, _) =
            common::user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        let refused = viewer
            .delete(&format!("/v1/agents/{}", zed.device_id))
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");

        let deleted = admin.delete(&format!("/v1/agents/{}", zed.device_id)).await;
        assert_eq!(
            deleted.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            deleted.body
        );
        assert_eq!(
            connection.commands.recv().await,
            Some(AgentCommand::Uninstall)
        );
        assert_eq!(
            names(&admin.get("/v1/agents").await.body),
            ["Alpha", "beta"]
        );
        assert_eq!(
            thumbnail(&mut admin, &zed.device_id, None).await.status,
            StatusCode::NO_CONTENT,
            "deleting the device removes its thumbnail"
        );
        assert_eq!(
            admin
                .delete(&format!("/v1/agents/{}", zed.device_id))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        // The Agent can still authenticate to hear the uninstall request, but
        // can't put its image back.
        assert!(authenticates(&zed).await, "{name}");
        assert_eq!(
            upload(&zed, jpeg(2, 100)).await.status,
            StatusCode::CONFLICT
        );
        let audited = audit_events(&app, "agent.delete").await;
        assert_eq!(audited.len(), 1);
        assert_eq!(audited[0].metadata["name"], "zed");
        assert_eq!(
            admin.delete("/v1/agents/not-a-device").await.status,
            StatusCode::BAD_REQUEST
        );
        drop((alpha, beta));
        app.finish().await;
    }
}

#[tokio::test]
async fn rotation_resends_the_pending_credential_until_the_agent_uses_it() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let path = format!("/v1/agents/{}/rotate-credential", agent.device_id);
        let original = agent.token.clone();

        let offline = admin.post(&path, json!({})).await;
        assert_eq!(offline.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(offline.code(), "device_offline");
        assert_eq!(
            credential_hashes(&app, &agent.device_id).await,
            (hash(&original), None)
        );

        let mut connection = app.state.agents.connect(&agent.device_id);
        let rotated = admin.post(&path, json!({})).await;
        assert_eq!(
            rotated.status,
            StatusCode::ACCEPTED,
            "{name}: {:?}",
            rotated.body
        );
        let Some(AgentCommand::RotateToken { token: first }) = connection.commands.recv().await
        else {
            panic!("expected a rotation");
        };
        assert_eq!(first.len(), 64);
        assert_eq!(
            credential_hashes(&app, &agent.device_id).await,
            (hash(&original), Some(hash(&first)))
        );

        // Not used yet, so rotating again sends the same credential.
        assert_eq!(
            admin.post(&path, json!({})).await.status,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            connection.commands.recv().await,
            Some(AgentCommand::RotateToken {
                token: first.clone()
            })
        );
        let audited = audit_events(&app, "agent.rotate_credential").await;
        assert_eq!(
            audited
                .iter()
                .map(|event| event.metadata["redelivered"].clone())
                .collect::<Vec<_>>(),
            [json!(true), json!(false)],
            "{name}"
        );

        // Both work until the Agent uses the new one, which retires the old.
        assert!(authenticates(&agent).await);
        assert!(authenticates(&agent.with_token(&first)).await);
        assert_eq!(
            credential_hashes(&app, &agent.device_id).await,
            (hash(&first), None)
        );
        assert!(
            !authenticates(&agent).await,
            "{name}: the old credential is retired"
        );

        // The next rotation is a new credential.
        assert_eq!(
            admin.post(&path, json!({})).await.status,
            StatusCode::ACCEPTED
        );
        let Some(AgentCommand::RotateToken { token: second }) = connection.commands.recv().await
        else {
            panic!("expected a rotation");
        };
        assert_ne!(second, first);

        // A rotation that can't be queued stays pending, and the next one
        // sends it; the current credential keeps working meanwhile.
        assert!(authenticates(&agent.with_token(&second)).await);
        while app
            .state
            .agents
            .send(&agent.device_id, AgentCommand::Uninstall)
        {}
        let refused = admin.post(&path, json!({})).await;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "{name}: {:?}",
            refused.body
        );
        let (current, pending) = credential_hashes(&app, &agent.device_id).await;
        assert_eq!(current, hash(&second), "{name}");
        let pending = pending.expect("the unsent credential stays pending");
        while connection.commands.try_recv().is_ok() {}
        assert_eq!(
            admin.post(&path, json!({})).await.status,
            StatusCode::ACCEPTED
        );
        let Some(AgentCommand::RotateToken { token: third }) = connection.commands.recv().await
        else {
            panic!("expected a rotation");
        };
        assert_eq!(hash(&third), pending, "{name}: the same credential is sent");
        assert!(authenticates(&agent.with_token(&second)).await);

        // Deleting the device mid-rotation leaves the Agent able to use the
        // new credential, so it can still hear the uninstall request.
        let (mut deleter, _) = common::user_with(
            &app,
            &mut admin,
            "deleter@example.com",
            &["devices.view", "devices.delete"],
        )
        .await;
        assert_eq!(
            deleter
                .delete(&format!("/v1/agents/{}", agent.device_id))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert!(authenticates(&agent.with_token(&third)).await, "{name}");
        assert_eq!(
            credential_hashes(&app, &agent.device_id).await,
            (hash(&third), None)
        );
        drop(connection);

        let (mut technician, _) = common::user_with(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "devices.delete"],
        )
        .await;
        assert_eq!(
            technician.post(&path, json!({})).await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            admin.post(&path, json!({})).await.status,
            StatusCode::NOT_FOUND,
            "deleted devices aren't rotated"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn thumbnails_are_small_jpegs_served_with_validators() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let other = app.enroll(&mut admin, "DESKTOP-2").await;

        let none = thumbnail(&mut admin, &agent.device_id, None).await;
        assert_eq!(none.status, StatusCode::NO_CONTENT, "{name}");
        assert_eq!(none.header("cache-control"), "private, no-cache");

        let wrong = agent.with_token(&other.token);
        assert_eq!(
            upload(&wrong, jpeg(1, 100)).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            upload(&wrong, jpeg(1, 512 * 1024 + 1)).await.status,
            StatusCode::UNAUTHORIZED,
            "{name}: the credential is checked before the body is read"
        );
        assert_eq!(
            upload(&agent, b"not a jpeg".to_vec()).await.status,
            StatusCode::BAD_REQUEST
        );
        let large = upload(&agent, jpeg(1, 512 * 1024 + 1)).await;
        assert_eq!(large.status, StatusCode::PAYLOAD_TOO_LARGE, "{name}");
        assert!(
            large.json(name).body["error"]
                .as_str()
                .unwrap()
                .contains("512 KiB")
        );
        assert_eq!(
            thumbnail(&mut admin, &agent.device_id, None).await.status,
            StatusCode::NO_CONTENT,
            "refused uploads store nothing"
        );

        assert_eq!(
            upload(&agent, jpeg(1, 512 * 1024)).await.status,
            StatusCode::NO_CONTENT
        );
        let first = thumbnail(&mut admin, &agent.device_id, None).await;
        assert_eq!(first.status, StatusCode::OK, "{name}");
        assert_eq!(first.body, jpeg(1, 512 * 1024));
        assert_eq!(first.header("content-type"), "image/jpeg");
        assert_eq!(first.header("cache-control"), "private, no-cache");
        assert_eq!(first.header("x-content-type-options"), "nosniff");
        assert!(!first.header("last-modified").is_empty());
        let etag = first.header("etag").to_owned();
        assert!(
            etag.starts_with('"') && etag.ends_with('"') && etag.len() > 2,
            "{etag}"
        );

        let unchanged = thumbnail(&mut admin, &agent.device_id, Some(&etag)).await;
        assert_eq!(unchanged.status, StatusCode::NOT_MODIFIED, "{name}");
        assert!(unchanged.body.is_empty());
        assert_eq!(unchanged.header("etag"), etag);
        let weak = format!("W/{etag}");
        assert_eq!(
            thumbnail(&mut admin, &agent.device_id, Some(&weak))
                .await
                .status,
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            thumbnail(&mut admin, &agent.device_id, Some("\"stale\""))
                .await
                .status,
            StatusCode::OK
        );

        assert_eq!(
            upload(&agent, jpeg(2, 1000)).await.status,
            StatusCode::NO_CONTENT
        );
        let replaced = thumbnail(&mut admin, &agent.device_id, Some(&etag)).await;
        assert_eq!(
            replaced.status,
            StatusCode::OK,
            "a new image replaces the old one"
        );
        assert_eq!(replaced.body, jpeg(2, 1000));
        assert_ne!(replaced.header("etag"), etag);
        assert_eq!(
            thumbnail(&mut admin, &other.device_id, None).await.status,
            StatusCode::NO_CONTENT,
            "each device has its own image"
        );

        let (mut outsider, _) =
            common::user_with(&app, &mut admin, "outsider@example.com", &["scripts.run"]).await;
        assert_eq!(
            thumbnail(&mut outsider, &agent.device_id, None)
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        let thumbnails = app.state.config.data_dir.join("thumbnails");
        let mut stored = std::fs::read_dir(thumbnails)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        stored.sort();
        assert_eq!(stored, [format!("{}.jpg", agent.device_id)], "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn handoffs_need_the_connect_permissions_and_a_live_device() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let (mut technician, technician_id) =
            common::user_with(&app, &mut admin, "tech@example.com", &["sessions.connect"]).await;

        let handoff = technician
            .post(
                "/v1/remote/handoffs",
                json!({ "device_id": agent.device_id, "reason": " Fixing the printer \n" }),
            )
            .await;
        assert_eq!(handoff.status, StatusCode::OK, "{name}: {:?}", handoff.body);
        assert_eq!(handoff.body["api_url"], common::ORIGIN);
        assert_eq!(handoff.body["start_in_background"], false);
        let token = handoff.body["handoff_token"].as_str().unwrap();
        let stored: (String, String, bool, String) = app
            .db()
            .fetch_one(
                &sea_query::Query::select()
                    .columns(
                        ["device_id", "user_id", "start_in_background", "reason"]
                            .map(sea_query::Alias::new),
                    )
                    .from(sea_query::Alias::new("remote_handoffs"))
                    .and_where(sea_query::ExprTrait::eq(
                        sea_query::Expr::col(sea_query::Alias::new("token_hash")),
                        hash(token),
                    ))
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(
            stored,
            (
                agent.device_id.clone(),
                technician_id,
                false,
                "Fixing the printer".to_owned()
            ),
            "{name}: only the token's hash is stored, with the trimmed reason"
        );
        assert_eq!(audit_events(&app, "remote.handoff_create").await.len(), 1);

        let background = technician
            .post(
                "/v1/remote/handoffs",
                json!({ "device_id": agent.device_id, "start_in_background": true }),
            )
            .await;
        assert_eq!(background.status, StatusCode::FORBIDDEN, "{name}");
        let in_background = admin
            .post(
                "/v1/remote/handoffs",
                json!({ "device_id": agent.device_id, "start_in_background": true }),
            )
            .await;
        assert_eq!(in_background.status, StatusCode::OK);
        assert_eq!(in_background.body["start_in_background"], true);

        let too_long = "x".repeat(501);
        for invalid in [
            json!({ "device_id": agent.device_id, "reason": too_long }),
            json!({ "device_id": agent.device_id, "reason": "bell\u{7}" }),
            json!({ "device_id": "device-1" }),
        ] {
            assert_eq!(
                technician
                    .post("/v1/remote/handoffs", invalid.clone())
                    .await
                    .status,
                StatusCode::BAD_REQUEST,
                "{name}: {invalid}"
            );
        }
        assert_eq!(
            technician
                .post(
                    "/v1/remote/handoffs",
                    json!({ "device_id": uuid::Uuid::new_v4().to_string() })
                )
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        admin
            .delete(&format!("/v1/agents/{}", agent.device_id))
            .await;
        assert_eq!(
            technician
                .post(
                    "/v1/remote/handoffs",
                    json!({ "device_id": agent.device_id })
                )
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}: deleted devices take no sessions"
        );
        let (mut viewer, _) =
            common::user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        assert_eq!(
            viewer
                .post(
                    "/v1/remote/handoffs",
                    json!({ "device_id": agent.device_id })
                )
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        app.finish().await;
    }
}
