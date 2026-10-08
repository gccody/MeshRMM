//! Instance settings and the audit log.
mod common;

use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn general_settings_validate_and_apply_partial_updates() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let current = admin.get("/v1/settings").await;
        assert_eq!(current.status, StatusCode::OK, "{name}");
        assert_eq!(current.body["instance_name"], "Acme IT");
        assert_eq!(current.body["dashboard_idle_timeout_minutes"], 240);
        assert_eq!(current.body["idle_disconnect_minutes"], json!(null));

        let updated = admin
            .patch(
                "/v1/settings",
                json!({
                    "instance_name": "Acme Support",
                    "display_border": false,
                    "idle_disconnect_minutes": 30,
                    "connection_approval": true,
                    "connection_approval_timeout_seconds": 60,
                    "blackout_message": "Back soon, {user_name} is working.",
                }),
            )
            .await;
        assert_eq!(updated.status, StatusCode::OK, "{name}: {:?}", updated.body);
        assert_eq!(updated.body["instance_name"], "Acme Support");
        assert_eq!(updated.body["display_border"], false);
        assert_eq!(updated.body["idle_disconnect_minutes"], 30);
        assert_eq!(updated.body["connection_approval_timeout_seconds"], 60);
        // Untouched fields keep their values.
        assert_eq!(updated.body["session_banner"], true);

        // null clears the idle disconnect; leaving it out doesn't.
        let kept = admin
            .patch("/v1/settings", json!({ "session_banner": false }))
            .await;
        assert_eq!(kept.body["idle_disconnect_minutes"], 30, "{name}");
        let cleared = admin
            .patch("/v1/settings", json!({ "idle_disconnect_minutes": null }))
            .await;
        assert_eq!(cleared.body["idle_disconnect_minutes"], json!(null));

        for invalid in [
            json!({ "instance_name": "" }),
            json!({ "dashboard_idle_timeout_minutes": 4 }),
            json!({ "idle_disconnect_minutes": 7 }),
            json!({ "blackout_message": "   " }),
            json!({ "connection_approval_timeout_seconds": 301 }),
            json!({ "connection_approval_lock_idle_seconds": 3601 }),
            json!({ "unknown_setting": true }),
        ] {
            let response = admin.patch("/v1/settings", invalid.clone()).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "{name}: {invalid}"
            );
        }
        let instance = app.browser().get("/v1/instance").await;
        assert_eq!(instance.body["name"], "Acme Support");

        let authentication = admin
            .patch(
                "/v1/settings/authentication",
                json!({ "password_min_length": 16, "session_lifetime_hours": 8 }),
            )
            .await;
        assert_eq!(
            authentication.body,
            json!({ "require_two_factor": false, "password_min_length": 16, "session_lifetime_hours": 8 }),
            "{name}"
        );
        assert_eq!(
            admin
                .patch(
                    "/v1/settings/authentication",
                    json!({ "password_min_length": 7 })
                )
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
        let instance = app.browser().get("/v1/instance").await;
        assert_eq!(instance.body["password_min_length"], 16);
        // New sessions use the new lifetime.
        let mut browser = app.browser();
        let signed_in = browser
            .post(
                "/v1/auth/sign-in",
                json!({ "email": common::ADMIN_EMAIL, "password": common::ADMIN_PASSWORD }),
            )
            .await;
        let cookie = signed_in.headers["set-cookie"].to_str().unwrap();
        assert!(cookie.contains("Max-Age=28800"), "{name}: {cookie}");

        let audit = admin.get("/v1/audit?action=settings.").await;
        let events = audit.body["events"].as_array().unwrap();
        assert_eq!(events.len(), 4, "{name}: {events:?}");
        assert_eq!(events[0]["action"], "settings.authentication_update");
        assert_eq!(events[3]["metadata"]["instance_name"], "Acme Support");
        app.finish().await;
    }
}

#[tokio::test]
async fn settings_need_their_permissions() {
    let app = common::App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    let invited = admin
        .post(
            "/v1/invitations",
            json!({ "email": "tess@example.com", "role_ids": ["technician"] }),
        )
        .await;
    let token = common::link_token(invited.body["link"].as_str().unwrap());
    let mut tech = app.browser();
    tech.post(
        "/v1/auth/invitation/accept",
        json!({ "token": token, "display_name": "Tess", "password": "technician password" }),
    )
    .await;
    for path in [
        "/v1/settings",
        "/v1/settings/authentication",
        "/v1/settings/smtp",
        "/v1/audit",
        "/v1/users",
        "/v1/roles",
        "/v1/invitations",
    ] {
        let response = tech.get(path).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(response.code(), "permission_denied", "{path}");
    }
    let patch = tech
        .patch("/v1/settings", json!({ "instance_name": "Mine" }))
        .await;
    assert_eq!(patch.status, StatusCode::FORBIDDEN);
    // Everyone may see the permission list and their own account.
    assert_eq!(tech.get("/v1/permissions").await.status, StatusCode::OK);
    assert_eq!(tech.get("/v1/account").await.status, StatusCode::OK);
    app.finish().await;
}

#[tokio::test]
async fn the_audit_log_pages_newest_first() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        for minutes in [10, 20, 30, 40, 50] {
            let response = admin
                .patch(
                    "/v1/settings",
                    json!({ "dashboard_idle_timeout_minutes": minutes }),
                )
                .await;
            assert_eq!(response.status, StatusCode::OK);
        }
        let mut seen = Vec::new();
        let mut path = "/v1/audit?action=settings.update&limit=2".to_owned();
        loop {
            let page = admin.get(&path).await;
            assert_eq!(page.status, StatusCode::OK, "{name}: {:?}", page.body);
            for event in page.body["events"].as_array().unwrap() {
                seen.push(
                    event["metadata"]["dashboard_idle_timeout_minutes"]
                        .as_i64()
                        .unwrap(),
                );
                assert_eq!(event["actor_label"], common::ADMIN_EMAIL);
                assert_eq!(event["target_type"], "settings");
            }
            match page.body["next"].as_str() {
                Some(next) => {
                    path = format!("/v1/audit?action=settings.update&limit=2&before={next}");
                }
                None => break,
            }
        }
        assert_eq!(seen, [50, 40, 30, 20, 10], "{name}");

        let admin_id = admin.get("/v1/account").await.body["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let by_actor = admin
            .get(&format!("/v1/audit?actor_user_id={admin_id}&limit=200"))
            .await;
        assert_eq!(
            by_actor.body["events"].as_array().unwrap().len(),
            6,
            "{name}"
        );
        // '_' and '%' in a prefix are literal, not wildcards.
        let wildcard = admin.get("/v1/audit?action=%25.").await;
        assert!(
            wildcard.body["events"].as_array().unwrap().is_empty(),
            "{name}"
        );
        let bad = admin.get("/v1/audit?before=garbage").await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
        app.finish().await;
    }
}
