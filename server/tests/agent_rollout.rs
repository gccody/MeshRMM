//! Staged rollouts of new Agent releases: which devices the Agent's update
//! check offers the release the server ships, the website's view and
//! controls of the rollout, and starting again for a new release.
mod common;

use std::collections::BTreeSet;

use axum::http::{Method, StatusCode};
use meshrmm_server::agent_rollout::bucket;
use serde_json::json;

const RELEASE: &str = "1.2.0";
const OLDER: &str = "1.1.0";
const DEVICES: usize = 40;
const ROLLOUT: &str = "/v1/settings/agent-rollout";

/// Whether the update manifest offers the Agent release.
fn offered(response: &common::Response) -> bool {
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    let releases = response.body["releases"].as_object().unwrap();
    match releases.get("agent-windows-x64") {
        Some(release) => {
            assert_eq!(release["version"], RELEASE);
            // The public manifest's entries, viewers' included.
            assert_eq!(
                releases.keys().collect::<Vec<_>>(),
                common::RELEASE_TARGETS.iter().collect::<Vec<_>>()
            );
            true
        }
        None => {
            assert!(releases.is_empty(), "{:?}", response.body);
            false
        }
    }
}

/// The devices among `agents` on an older Agent that the server offers the
/// release.
async fn offered_devices(agents: &[common::Agent]) -> BTreeSet<String> {
    let mut devices = BTreeSet::new();
    for agent in agents {
        if offered(&agent.update_check(OLDER).await) {
            devices.insert(agent.device_id.clone());
        }
    }
    devices
}

/// The devices among `agents` whose bucket is below `percent`.
fn below(agents: &[common::Agent], percent: u8) -> BTreeSet<String> {
    agents
        .iter()
        .filter(|agent| bucket(&agent.device_id) < percent)
        .map(|agent| agent.device_id.clone())
        .collect()
}

async fn enroll_many(app: &common::App, admin: &mut common::Browser) -> Vec<common::Agent> {
    let mut agents = Vec::new();
    for index in 0..DEVICES {
        agents.push(app.enroll(admin, &format!("DESKTOP-{index}")).await);
    }
    agents
}

async fn patch(admin: &mut common::Browser, body: serde_json::Value) -> serde_json::Value {
    let response = admin.patch(ROLLOUT, body.clone()).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{body}: {:?}",
        response.body
    );
    response.body
}

#[tokio::test]
async fn everyone_is_offered_the_release_by_default() {
    for app in common::apps_releasing(RELEASE).await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agents = enroll_many(&app, &mut admin).await;
        let rollout = admin.get(ROLLOUT).await;
        assert_eq!(rollout.status, StatusCode::OK, "{name}: {:?}", rollout.body);
        assert_eq!(rollout.body["release"], RELEASE, "{name}");
        assert_eq!(rollout.body["mode"], "everyone");
        assert_eq!(rollout.body["first_percent"], 10);
        assert_eq!(rollout.body["offered_percent"], 100);
        // No device has checked yet, so none is known to have the release.
        assert_eq!(
            rollout.body["devices"],
            json!({ "on_release": 0, "offered": DEVICES, "waiting": 0 })
        );

        let all: BTreeSet<_> = agents.iter().map(|agent| agent.device_id.clone()).collect();
        assert_eq!(offered_devices(&agents).await, all, "{name}");
        // The manifest is the public file's, byte for byte.
        let public = app
            .browser()
            .bytes(
                axum::http::Request::get("/downloads/update-manifest.json")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        let checked = agents[0]
            .send(
                agents[0]
                    .request(
                        Method::POST,
                        &format!("/v1/agents/{}/update-check", agents[0].device_id),
                    )
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({ "version": OLDER }).to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(checked.status, StatusCode::OK, "{name}");
        assert_eq!(checked.body, public.body, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn stages_offer_the_release_to_the_devices_below_them() {
    for app in common::apps_releasing(RELEASE).await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agents = enroll_many(&app, &mut admin).await;

        // Staged mode starts this release at the first stage.
        let rollout = patch(&mut admin, json!({ "mode": "staged" })).await;
        assert_eq!(rollout["mode"], "staged", "{name}");
        assert_eq!(rollout["percent"], 10);
        assert_eq!(rollout["paused"], false);
        assert_eq!(offered_devices(&agents).await, below(&agents, 10), "{name}");

        let mut previous = below(&agents, 10);
        for percent in [25, 50, 90] {
            let rollout = patch(&mut admin, json!({ "percent": percent })).await;
            assert_eq!(rollout["offered_percent"], percent, "{name}");
            let offered = offered_devices(&agents).await;
            assert_eq!(offered, below(&agents, percent), "{name} at {percent} %");
            // Advancing never stops offering it to a device offered it before.
            assert!(offered.is_superset(&previous), "{name} at {percent} %");
            previous = offered;
        }

        // Lowering the stage stops offering it to the devices above it.
        patch(&mut admin, json!({ "percent": 25 })).await;
        assert_eq!(offered_devices(&agents).await, below(&agents, 25), "{name}");

        // Paused, it is offered to nobody new.
        let rollout = patch(&mut admin, json!({ "paused": true })).await;
        assert_eq!(rollout["offered_percent"], 0, "{name}");
        assert_eq!(offered_devices(&agents).await, BTreeSet::new(), "{name}");
        let rollout = patch(&mut admin, json!({ "paused": false })).await;
        assert_eq!(rollout["percent"], 25, "{name}");
        assert_eq!(offered_devices(&agents).await, below(&agents, 25), "{name}");

        // At 100 % every device is offered it.
        patch(&mut admin, json!({ "percent": 100 })).await;
        assert_eq!(
            offered_devices(&agents).await,
            below(&agents, 100),
            "{name}"
        );
        assert_eq!(below(&agents, 100).len(), DEVICES);

        // And everyone at once is today's behaviour again, whatever the stage.
        patch(&mut admin, json!({ "mode": "staged" })).await;
        patch(&mut admin, json!({ "percent": 10, "paused": true })).await;
        patch(&mut admin, json!({ "mode": "everyone" })).await;
        assert_eq!(offered_devices(&agents).await.len(), DEVICES, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn devices_on_the_release_are_answered_as_before_and_counted() {
    for app in common::apps_releasing(RELEASE).await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agents = enroll_many(&app, &mut admin).await;
        patch(&mut admin, json!({ "mode": "staged", "percent": 50 })).await;
        patch(&mut admin, json!({ "paused": true })).await;
        // Above the stage and paused, a device that has the release, or a
        // newer one, still gets the manifest that names it.
        let above = agents
            .iter()
            .filter(|agent| bucket(&agent.device_id) >= 50)
            .collect::<Vec<_>>();
        assert!(above.len() >= 2, "{name}: too few devices above 50 %");
        assert!(offered(&above[0].update_check(RELEASE).await), "{name}");
        assert!(offered(&above[1].update_check("1.3.0").await), "{name}");

        let on_release = [above[0].device_id.clone(), above[1].device_id.clone()];
        let others: Vec<_> = agents
            .iter()
            .filter(|agent| !on_release.contains(&agent.device_id))
            .cloned()
            .collect();

        // The counts agree with what the devices are offered, at every step.
        for change in [
            json!({ "paused": false }),
            json!({ "percent": 10 }),
            json!({ "percent": 75 }),
            json!({ "paused": true }),
        ] {
            let rollout = patch(&mut admin, change.clone()).await;
            let offered = offered_devices(&others).await;
            assert_eq!(
                rollout["devices"],
                json!({
                    "on_release": 2,
                    "offered": offered.len(),
                    "waiting": others.len() - offered.len(),
                }),
                "{name} {change}"
            );
        }
        app.finish().await;
    }
}

#[tokio::test]
async fn update_checks_need_the_device_credential() {
    for app in common::apps_releasing(RELEASE).await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let other = app.enroll(&mut admin, "DESKTOP-2").await;
        assert!(offered(&agent.update_check(OLDER).await), "{name}");

        let refused = agent.with_token(&"0".repeat(64)).update_check(OLDER).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(refused.code(), "agent_unauthenticated");
        // Another device's credential is not this one's.
        let refused = agent.with_token(&other.token).update_check(OLDER).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{name}");
        // Without a bearer token: refused as cross-site without any
        // Authorization header, and as unauthenticated with another kind.
        for (authorization, status) in [
            (None, StatusCode::FORBIDDEN),
            (Some("Basic YWdlbnQ6c2VjcmV0"), StatusCode::UNAUTHORIZED),
        ] {
            let mut request =
                axum::http::Request::post(format!("/v1/agents/{}/update-check", agent.device_id))
                    .header("content-type", "application/json");
            if let Some(authorization) = authorization {
                request = request.header("authorization", authorization);
            }
            let anonymous = app
                .browser()
                .bytes(
                    request
                        .body(axum::body::Body::from(
                            json!({ "version": OLDER }).to_string(),
                        ))
                        .unwrap(),
                )
                .await;
            assert_eq!(anonymous.status, status, "{name} {authorization:?}");
        }

        for version in ["", "latest", "1.2"] {
            let invalid = agent.update_check(version).await;
            assert_eq!(
                invalid.status,
                StatusCode::BAD_REQUEST,
                "{name} {version:?}"
            );
        }

        // A deleted device is uninstalling, so it is offered nothing, but
        // answered, so it doesn't retry sooner than it would anyway.
        let deleted = admin
            .delete(&format!("/v1/agents/{}", agent.device_id))
            .await;
        assert!(deleted.status.is_success(), "{name}: {:?}", deleted.body);
        assert!(!offered(&agent.update_check(OLDER).await), "{name}");
        assert!(offered(&other.update_check(OLDER).await), "{name}");
        // And it isn't counted.
        let rollout = admin.get(ROLLOUT).await;
        assert_eq!(
            rollout.body["devices"],
            json!({ "on_release": 0, "offered": 1, "waiting": 0 }),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn without_downloads_there_is_no_release_to_offer() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        // As the public manifest answers then.
        let checked = agent.update_check(OLDER).await;
        assert_eq!(checked.status, StatusCode::NOT_FOUND, "{name}");
        let rollout = admin.get(ROLLOUT).await;
        assert_eq!(rollout.status, StatusCode::OK, "{name}");
        assert_eq!(rollout.body["release"], json!(null));
        assert_eq!(rollout.body["devices"], json!(null));
        app.finish().await;
    }
}

#[tokio::test]
async fn a_new_release_starts_the_rollout_again() {
    for app in common::apps_releasing(RELEASE).await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        patch(
            &mut admin,
            json!({ "mode": "staged", "first_percent": 20, "percent": 60 }),
        )
        .await;
        patch(&mut admin, json!({ "paused": true })).await;

        // Restarting on the same release keeps the stage.
        let same = app.restarted().await;
        let mut browser = same.browser();
        browser.cookie = admin.cookie.clone();
        let rollout = browser.get(ROLLOUT).await;
        assert_eq!(rollout.body["percent"], 60, "{name}");
        assert_eq!(rollout.body["paused"], true, "{name}");
        same.finish().await;

        // Upgraded, the server starts the new release at the first stage.
        app.ship("1.3.0");
        let upgraded = app.restarted().await;
        let mut browser = upgraded.browser();
        browser.cookie = admin.cookie.clone();
        let rollout = browser.get(ROLLOUT).await;
        assert_eq!(rollout.status, StatusCode::OK, "{name}: {:?}", rollout.body);
        assert_eq!(rollout.body["release"], "1.3.0", "{name}");
        assert_eq!(rollout.body["mode"], "staged");
        assert_eq!(rollout.body["percent"], 20);
        assert_eq!(rollout.body["paused"], false);
        let checked = upgraded.agent(&agent).update_check(RELEASE).await;
        assert_eq!(checked.status, StatusCode::OK, "{name}");
        let releases = checked.body["releases"].as_object().unwrap();
        assert_eq!(
            releases.is_empty(),
            bucket(&agent.device_id) >= 20,
            "{name}"
        );
        upgraded.finish().await;
        app.finish().await;
    }
}

#[tokio::test]
async fn changing_the_rollout_needs_the_settings_permission_and_is_audited() {
    for app in common::apps_releasing(RELEASE).await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut viewer, _) =
            common::user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        assert_eq!(
            viewer.get(ROLLOUT).await.status,
            StatusCode::FORBIDDEN,
            "{name}"
        );
        let refused = viewer.patch(ROLLOUT, json!({ "mode": "staged" })).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");
        assert_eq!(admin.get(ROLLOUT).await.body["mode"], "everyone", "{name}");

        let (mut manager, _) = common::user_with(
            &app,
            &mut admin,
            "manager@example.com",
            &["settings.manage"],
        )
        .await;
        let changed = manager
            .patch(ROLLOUT, json!({ "mode": "staged", "first_percent": 5 }))
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{name}: {:?}", changed.body);
        assert_eq!(changed.body["percent"], 5);
        let events = common::audit_events(&app, "settings.agent_rollout_update").await;
        assert_eq!(events.len(), 1, "{name}");
        assert_eq!(events[0].target_type, "settings");
        assert_eq!(events[0].actor_label, "manager@example.com");
        assert_eq!(
            events[0].metadata,
            json!({ "mode": "staged", "first_percent": 5 })
        );
        patch(&mut admin, json!({ "percent": 30 })).await;
        assert_eq!(
            common::audit_events(&app, "settings.agent_rollout_update").await[0].metadata,
            json!({ "percent": 30 }),
            "{name}"
        );

        for invalid in [
            json!({ "percent": 0 }),
            json!({ "percent": 101 }),
            json!({ "first_percent": 0 }),
            json!({ "mode": "some" }),
            json!({ "stage": 20 }),
            // The stage and pausing belong to staged mode.
            json!({ "mode": "everyone", "percent": 50 }),
            json!({ "mode": "everyone", "paused": true }),
        ] {
            let response = admin.patch(ROLLOUT, invalid.clone()).await;
            assert!(
                response.status == StatusCode::BAD_REQUEST
                    || response.status == StatusCode::UNPROCESSABLE_ENTITY,
                "{name} {invalid}: {}",
                response.status
            );
        }
        assert_eq!(
            common::audit_events(&app, "settings.agent_rollout_update")
                .await
                .len(),
            2,
            "{name}"
        );
        // Nothing changed, nothing recorded.
        patch(&mut admin, json!({})).await;
        assert_eq!(
            common::audit_events(&app, "settings.agent_rollout_update")
                .await
                .len(),
            2,
            "{name}"
        );
        app.finish().await;
    }
}
