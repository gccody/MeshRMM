//! Enrolling devices with Agent installers, and how Agents authenticate.
mod common;

use axum::http::{Method, StatusCode};
use common::{audit_events, random_hex, redeem};
use serde_json::json;

#[tokio::test]
async fn installers_enroll_one_computer_that_can_recover_its_credential() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let installer = admin
            .post("/v1/agent-installers", json!({ "platform": "macos" }))
            .await;
        assert_eq!(installer.status, StatusCode::CREATED, "{name}");
        assert_eq!(installer.body["server"], common::ORIGIN);
        let token = installer.body["install_token"].as_str().unwrap().to_owned();
        assert_eq!(token.len(), 64);
        assert!(
            installer.body["expires_at_unix_ms"].as_i64().unwrap() > meshrmm_server::time::now_ms()
        );

        let key = random_hex(32);
        let config = redeem(&app, &token, "  DESKTOP-1 ", &key).await;
        assert_eq!(config.status, StatusCode::OK, "{name}: {:?}", config.body);
        assert_eq!(config.body["server"], common::ORIGIN);
        assert_eq!(
            config.body["update_manifest_url"],
            format!("{}/downloads/update-manifest.json", common::ORIGIN)
        );
        assert_eq!(config.body["frames_per_second"], 60);
        let device_id = config.body["device_id"].as_str().unwrap().to_owned();
        let agent_token = config.body["agent_token"].as_str().unwrap().to_owned();

        // The same computer finishing an interrupted install gets the same
        // credential; any other is refused.
        let again = redeem(&app, &token, "DESKTOP-1", &key).await;
        assert_eq!(again.status, StatusCode::OK, "{name}: {:?}", again.body);
        assert_eq!(again.body["device_id"], device_id.as_str());
        assert_eq!(again.body["agent_token"], agent_token.as_str());
        for (computer, other_key) in [("DESKTOP-1", random_hex(32)), ("DESKTOP-2", key.clone())] {
            let refused = redeem(&app, &token, computer, &other_key).await;
            assert_eq!(
                refused.status,
                StatusCode::UNAUTHORIZED,
                "{name}: {computer}"
            );
            assert_eq!(refused.code(), "installer_rejected");
        }
        assert_eq!(
            redeem(&app, &random_hex(32), "DESKTOP-1", &key)
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );

        let devices = admin.get("/v1/agents").await;
        assert_eq!(devices.status, StatusCode::OK, "{name}");
        assert_eq!(
            devices.body["agents"],
            json!([{
                "id": device_id,
                "name": "DESKTOP-1",
                "connected": false,
                "created_at": devices.body["agents"][0]["created_at"],
            }])
        );

        let redeemed = audit_events(&app, "agent_installer.redeem").await;
        assert_eq!(redeemed.len(), 2, "{name}");
        assert_eq!(redeemed[0].metadata["recovered"], true);
        assert_eq!(redeemed[1].metadata["recovered"], false);
        assert_eq!(redeemed[1].target_id, device_id);
        assert_eq!(redeemed[1].actor_label, "Agent installer");
        assert_eq!(audit_events(&app, "agent_installer.issue").await.len(), 1);

        // Once the device is deleted, the installer can't bring it back.
        assert_eq!(
            admin
                .delete(&format!("/v1/agents/{device_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            redeem(&app, &token, "DESKTOP-1", &key).await.status,
            StatusCode::UNAUTHORIZED,
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn redemptions_are_validated_and_installers_expire() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        for invalid in [
            json!({ "platform": "linux" }),
            json!({ "platform": "macos", "extra": 1 }),
        ] {
            assert_eq!(
                admin
                    .post("/v1/agent-installers", invalid.clone())
                    .await
                    .status,
                StatusCode::BAD_REQUEST,
                "{name}: {invalid}"
            );
        }
        let token = admin
            .post("/v1/agent-installers", json!({ "platform": "windows-x64" }))
            .await
            .body["install_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let key = random_hex(32);
        for (computer, redemption_key) in [
            ("", key.as_str()),
            ("two\nlines", key.as_str()),
            ("DESKTOP-1", "short"),
            ("DESKTOP-1", ""),
        ] {
            assert_eq!(
                redeem(&app, &token, computer, redemption_key).await.status,
                StatusCode::BAD_REQUEST,
                "{name}: {computer:?} {redemption_key:?}"
            );
        }
        let missing_token = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/agent-installers/redeem")
            .header("authorization", "Basic dXNlcjpwYXNz")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({ "name": "DESKTOP-1", "redemption_key": key }).to_string(),
            ))
            .unwrap();
        assert_eq!(
            app.browser().raw(missing_token).await.status,
            StatusCode::UNAUTHORIZED
        );

        // An expired installer enrolls nothing.
        app.db()
            .execute(
                &sea_query::Query::update()
                    .table(sea_query::Alias::new("agent_install_tokens"))
                    .value(
                        sea_query::Alias::new("expires_at"),
                        meshrmm_server::time::now_ms() - 1,
                    )
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(
            redeem(&app, &token, "DESKTOP-1", &key).await.status,
            StatusCode::UNAUTHORIZED,
            "{name}"
        );
        assert!(
            admin.get("/v1/agents").await.body["agents"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn parallel_redemptions_by_one_computer_enroll_one_device() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let token = admin
            .post("/v1/agent-installers", json!({ "platform": "windows-x64" }))
            .await
            .body["install_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let key = random_hex(32);
        let results =
            futures_util::future::join_all((0..8).map(|_| redeem(&app, &token, "DESKTOP-1", &key)))
                .await;
        let mut devices = results
            .iter()
            .map(|result| {
                assert_eq!(result.status, StatusCode::OK, "{name}: {:?}", result.body);
                result.body["device_id"].as_str().unwrap().to_owned()
            })
            .collect::<Vec<_>>();
        devices.dedup();
        assert_eq!(devices.len(), 1, "{name}: {devices:?}");
        assert_eq!(
            admin.get("/v1/agents").await.body["agents"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn enrolling_needs_the_permission() {
    for app in common::apps().await {
        let mut admin = common::set_up(&app).await;
        let (mut viewer, _) =
            common::user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        let refused = viewer
            .post("/v1/agent-installers", json!({ "platform": "macos" }))
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", app.name);
        assert_eq!(refused.code(), "permission_denied");
        let (mut enroller, _) = common::user_with(
            &app,
            &mut admin,
            "enroller@example.com",
            &["devices.enroll"],
        )
        .await;
        app.enroll(&mut enroller, "DESKTOP-1").await;
        assert_eq!(
            viewer.get("/v1/agents").await.body["agents"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            enroller.get("/v1/agents").await.status,
            StatusCode::FORBIDDEN,
            "listing devices needs devices.view"
        );
        app.finish().await;
    }
}
