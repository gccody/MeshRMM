//! Deployment tokens: reusable enrollment credentials that enroll any number
//! of computers into one company until they expire, are revoked or run out.
mod common;

use axum::http::{Method, StatusCode};
use common::{App, Browser, Response, audit_events, random_hex, redeem};
use meshrmm_server::time::{DAY_MS, now_ms};
use sea_query::{Alias, Expr, ExprTrait, Query};
use serde_json::{Value, json};

async fn company(admin: &mut Browser, name: &str) -> String {
    let created = admin.post("/v1/companies", json!({ "name": name })).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

/// Makes a deployment token and returns its ID and secret.
async fn make(browser: &mut Browser, body: Value) -> (String, String) {
    let created = browser.post("/v1/deployment-tokens", body).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    (
        created.body["token"]["id"].as_str().unwrap().to_owned(),
        created.body["bootstrap"]["install_token"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
}

/// The listed token with `id`.
async fn listed(browser: &mut Browser, id: &str) -> Value {
    let list = browser.get("/v1/deployment-tokens").await;
    assert_eq!(list.status, StatusCode::OK, "{:?}", list.body);
    list.body["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|token| token["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("{id} is not listed: {:?}", list.body))
}

async fn devices(browser: &mut Browser) -> Vec<Value> {
    browser.get("/v1/agents").await.body["agents"]
        .as_array()
        .unwrap()
        .clone()
}

/// What redeeming a token nobody issued answers, which every refused
/// deployment token must answer too.
async fn bad_token_answer(app: &App) -> Response {
    let refused = redeem(app, &random_hex(32), "DESKTOP-X", &random_hex(32)).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{}", app.name);
    refused
}

fn assert_refused_like(refused: &Response, bad: &Response, what: &str) {
    assert_eq!(refused.status, bad.status, "{what}: {:?}", refused.body);
    assert_eq!(refused.body, bad.body, "{what}");
    assert_eq!(refused.code(), "installer_rejected", "{what}");
}

/// Sends a request the way a script does: the API token and no cookie.
async fn with_api_token(
    app: &App,
    token: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Response {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string())),
        None => request.body(axum::body::Body::empty()),
    }
    .unwrap();
    app.browser().raw(request).await
}

#[tokio::test]
async fn a_token_enrolls_many_devices_into_its_company() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let acme = company(&mut admin, "Acme").await;
        let created = admin
            .post(
                "/v1/deployment-tokens",
                json!({ "name": " Acme rollout ", "company_id": acme }),
            )
            .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            created.body
        );
        let token = &created.body["token"];
        let id = token["id"].as_str().unwrap().to_owned();
        assert_eq!(token["name"], "Acme rollout", "{name}");
        assert_eq!(token["company_id"], acme.as_str());
        assert_eq!(token["company_name"], "Acme");
        assert_eq!(token["created_by"]["email"], common::ADMIN_EMAIL);
        assert_eq!(token["max_uses"], Value::Null);
        assert_eq!(token["uses"], 0);
        assert_eq!(token["state"], "active");
        // Thirty days unless the creator chooses otherwise.
        let created_at = token["created_at"].as_i64().unwrap();
        let expires_at = token["expires_at"].as_i64().unwrap();
        assert_eq!(expires_at - created_at, 30 * DAY_MS, "{name}");
        let bootstrap = &created.body["bootstrap"];
        assert_eq!(bootstrap["server"], common::ORIGIN);
        assert_eq!(bootstrap["expires_at_unix_ms"], expires_at);
        let secret = bootstrap["install_token"].as_str().unwrap().to_owned();
        assert_eq!(secret.len(), 64);

        // The list never shows the secret again.
        let list = admin.get("/v1/deployment-tokens").await;
        assert!(!list.body.to_string().contains(&secret), "{name}");

        let mut enrolled = Vec::new();
        for computer in ["DESKTOP-1", "DESKTOP-2", "LAPTOP-3"] {
            let key = random_hex(32);
            let config = redeem(&app, &secret, computer, &key).await;
            assert_eq!(config.status, StatusCode::OK, "{name}: {:?}", config.body);
            assert_eq!(config.body["server"], common::ORIGIN);
            enrolled.push((
                computer,
                key,
                config.body["device_id"].as_str().unwrap().to_owned(),
                config.body["agent_token"].as_str().unwrap().to_owned(),
            ));
        }
        let listed_devices = devices(&mut admin).await;
        assert_eq!(listed_devices.len(), 3, "{name}");
        for device in &listed_devices {
            assert_eq!(device["company_id"], acme.as_str(), "{name}: {device}");
        }
        // An interrupted install finishing gets the same credential without
        // taking another use; the key with another name is refused.
        let (computer, key, device_id, agent_token) = &enrolled[0];
        let again = redeem(&app, &secret, computer, key).await;
        assert_eq!(again.status, StatusCode::OK, "{name}: {:?}", again.body);
        assert_eq!(again.body["device_id"], device_id.as_str());
        assert_eq!(again.body["agent_token"], agent_token.as_str());
        let bad = bad_token_answer(&app).await;
        assert_refused_like(
            &redeem(&app, &secret, "ELSEWHERE", key).await,
            &bad,
            &format!("{name}: a key reused by another computer"),
        );

        let token = listed(&mut admin, &id).await;
        assert_eq!(token["uses"], 3, "{name}");
        assert_eq!(token["state"], "active");
        assert!(token["last_used_at"].is_i64(), "{name}: {token}");

        assert_eq!(audit_events(&app, "deployment_token.create").await.len(), 1);
        let redeemed = audit_events(&app, "deployment_token.redeem").await;
        assert_eq!(redeemed.len(), 4, "{name}");
        for event in &redeemed {
            assert_eq!(event.metadata["deployment_token_id"], id.as_str());
            assert_eq!(event.metadata["deployment_token_name"], "Acme rollout");
            assert_eq!(event.metadata["company_id"], acme.as_str());
            assert_eq!(event.actor_label, "Deployment token");
            assert!(!event.metadata.to_string().contains(&secret), "{name}");
        }
        assert_eq!(redeemed[0].metadata["recovered"], true);
        assert_eq!(redeemed[0].target_id, device_id.as_str());
        assert_eq!(redeemed[1].metadata["recovered"], false);
        assert!(
            audit_events(&app, "agent_installer.redeem")
                .await
                .is_empty(),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn expired_revoked_and_used_up_tokens_are_refused_like_a_bad_token() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let acme = company(&mut admin, "Acme").await;
        let bad = bad_token_answer(&app).await;

        // Used up: one device, then the limit refuses the next.
        let (limited, limited_secret) = make(
            &mut admin,
            json!({ "name": "One", "company_id": acme, "max_uses": 1, "expires_in_days": 7 }),
        )
        .await;
        let first_key = random_hex(32);
        let first = redeem(&app, &limited_secret, "DESKTOP-1", &first_key).await;
        assert_eq!(first.status, StatusCode::OK, "{name}: {:?}", first.body);
        assert_refused_like(
            &redeem(&app, &limited_secret, "DESKTOP-2", &random_hex(32)).await,
            &bad,
            &format!("{name}: used up"),
        );
        assert_eq!(listed(&mut admin, &limited).await["state"], "exhausted");
        // The device it enrolled can still finish its install.
        assert_eq!(
            redeem(&app, &limited_secret, "DESKTOP-1", &first_key)
                .await
                .status,
            StatusCode::OK,
            "{name}"
        );

        // Expired.
        let (expired, expired_secret) =
            make(&mut admin, json!({ "name": "Old", "company_id": acme })).await;
        app.db()
            .execute(
                &Query::update()
                    .table(Alias::new("deployment_tokens"))
                    .value(Alias::new("expires_at"), now_ms() - 1)
                    .and_where(Expr::col(Alias::new("id")).eq(expired.as_str()))
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_refused_like(
            &redeem(&app, &expired_secret, "DESKTOP-3", &random_hex(32)).await,
            &bad,
            &format!("{name}: expired"),
        );
        assert_eq!(listed(&mut admin, &expired).await["state"], "expired");

        // Revoked, which also stops an install it started finishing.
        let (revoked, revoked_secret) =
            make(&mut admin, json!({ "name": "Leaked", "company_id": acme })).await;
        let started_key = random_hex(32);
        let started = redeem(&app, &revoked_secret, "DESKTOP-4", &started_key).await;
        assert_eq!(started.status, StatusCode::OK, "{name}");
        for _ in 0..2 {
            assert_eq!(
                admin
                    .delete(&format!("/v1/deployment-tokens/{revoked}"))
                    .await
                    .status,
                StatusCode::NO_CONTENT,
                "{name}"
            );
        }
        for (computer, key) in [("DESKTOP-5", random_hex(32)), ("DESKTOP-4", started_key)] {
            assert_refused_like(
                &redeem(&app, &revoked_secret, computer, &key).await,
                &bad,
                &format!("{name}: revoked, {computer}"),
            );
        }
        let token = listed(&mut admin, &revoked).await;
        assert_eq!(token["state"], "revoked", "{name}");
        assert!(token["revoked_at"].is_i64());
        let revocations = audit_events(&app, "deployment_token.revoke").await;
        assert_eq!(
            revocations.len(),
            1,
            "{name}: revoking again records nothing"
        );
        assert_eq!(revocations[0].target_id, revoked.as_str());
        assert_eq!(revocations[0].metadata["name"], "Leaked");
        assert_eq!(
            admin
                .delete(&format!("/v1/deployment-tokens/{}", random_hex(16)))
                .await
                .status,
            StatusCode::NOT_FOUND
        );

        // Devices they enrolled stay enrolled, with their credentials.
        let enrolled = devices(&mut admin).await;
        assert_eq!(enrolled.len(), 2, "{name}");
        for device in &enrolled {
            assert_eq!(device["company_id"], acme.as_str(), "{name}");
        }

        // A deleted company takes its tokens with it.
        let gone_company = company(&mut admin, "Gone").await;
        let (_, gone_secret) = make(
            &mut admin,
            json!({ "name": "Doomed", "company_id": gone_company }),
        )
        .await;
        assert_eq!(
            admin
                .delete(&format!("/v1/companies/{gone_company}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_refused_like(
            &redeem(&app, &gone_secret, "DESKTOP-6", &random_hex(32)).await,
            &bad,
            &format!("{name}: company deleted"),
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn concurrent_redemptions_never_pass_the_limit() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let acme = company(&mut admin, "Acme").await;
        let (id, secret) = make(
            &mut admin,
            json!({ "name": "One", "company_id": acme, "max_uses": 1 }),
        )
        .await;
        let computers = (0..8)
            .map(|index| (format!("DESKTOP-{index}"), random_hex(32)))
            .collect::<Vec<_>>();
        let results = futures_util::future::join_all(
            computers
                .iter()
                .map(|(computer, key)| redeem(&app, &secret, computer, key)),
        )
        .await;
        let enrolled = results
            .iter()
            .filter(|result| result.status == StatusCode::OK)
            .count();
        assert_eq!(enrolled, 1, "{name}");
        for result in &results {
            assert!(
                result.status == StatusCode::OK || result.code() == "installer_rejected",
                "{name}: {:?}",
                result.body
            );
        }
        assert_eq!(devices(&mut admin).await.len(), 1, "{name}");
        assert_eq!(listed(&mut admin, &id).await["uses"], 1, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn an_enrollment_that_fails_gives_its_use_back() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let acme = company(&mut admin, "Acme").await;
        let (id, secret) = make(
            &mut admin,
            json!({ "name": "One", "company_id": acme, "max_uses": 1 }),
        )
        .await;
        // The device's row is written after the use is taken.
        common::refuse_writes(&app, "INSERT", "agents", true).await;
        let failed = redeem(&app, &secret, "DESKTOP-1", &random_hex(32)).await;
        assert_eq!(
            failed.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{name}: {:?}",
            failed.body
        );
        common::refuse_writes(&app, "INSERT", "agents", false).await;
        let token = listed(&mut admin, &id).await;
        assert_eq!(token["uses"], 0, "{name}");
        assert_eq!(token["last_used_at"], Value::Null, "{name}");
        let enrolled = redeem(&app, &secret, "DESKTOP-2", &random_hex(32)).await;
        assert_eq!(
            enrolled.status,
            StatusCode::OK,
            "{name}: {:?}",
            enrolled.body
        );
        assert!(
            audit_events(&app, "deployment_token.redeem").await.len() == 1,
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn tokens_are_made_and_revoked_only_on_the_website_with_the_permission() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let acme = company(&mut admin, "Acme").await;
        let (id, _) = make(&mut admin, json!({ "name": "Rollout", "company_id": acme })).await;

        // An administrator's API token may list them, but not make or revoke one.
        let api_token = admin
            .post(
                "/v1/api-tokens",
                json!({ "name": "script", "expires_in_days": 30 }),
            )
            .await;
        assert_eq!(api_token.status, StatusCode::CREATED, "{name}");
        let api_token = api_token.body["secret"].as_str().unwrap().to_owned();
        let read =
            with_api_token(&app, &api_token, Method::GET, "/v1/deployment-tokens", None).await;
        assert_eq!(read.status, StatusCode::OK, "{name}: {:?}", read.body);
        assert_eq!(read.body["tokens"].as_array().unwrap().len(), 1);
        let created = with_api_token(
            &app,
            &api_token,
            Method::POST,
            "/v1/deployment-tokens",
            Some(json!({ "name": "From a script", "company_id": acme })),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::FORBIDDEN,
            "{name}: {:?}",
            created.body
        );
        assert_eq!(created.code(), "api_token_not_allowed");
        let revoked = with_api_token(
            &app,
            &api_token,
            Method::DELETE,
            &format!("/v1/deployment-tokens/{id}"),
            None,
        )
        .await;
        assert_eq!(revoked.status, StatusCode::FORBIDDEN, "{name}");
        assert_eq!(revoked.code(), "api_token_not_allowed");

        // A user without devices.enroll can do none of it.
        let (mut viewer, _) =
            common::user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        for response in [
            viewer.get("/v1/deployment-tokens").await,
            viewer
                .post(
                    "/v1/deployment-tokens",
                    json!({ "name": "Mine", "company_id": acme }),
                )
                .await,
            viewer.delete(&format!("/v1/deployment-tokens/{id}")).await,
        ] {
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "{name}: {:?}",
                response.body
            );
            assert_eq!(response.code(), "permission_denied");
        }
        // One with it can.
        let (mut enroller, _) = common::user_with(
            &app,
            &mut admin,
            "enroller@example.com",
            &["devices.enroll"],
        )
        .await;
        let (own, _) = make(&mut enroller, json!({ "name": "Mine", "company_id": acme })).await;
        assert_eq!(
            listed(&mut enroller, &own).await["created_by"]["email"],
            "enroller@example.com"
        );
        assert_eq!(
            enroller
                .delete(&format!("/v1/deployment-tokens/{id}"))
                .await
                .status,
            StatusCode::NO_CONTENT,
            "{name}"
        );
        assert_eq!(audit_events(&app, "deployment_token.create").await.len(), 2);
        app.finish().await;
    }
}

#[tokio::test]
async fn new_tokens_are_validated() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let acme = company(&mut admin, "Acme").await;
        for invalid in [
            json!({ "name": "", "company_id": acme }),
            json!({ "name": "x".repeat(81), "company_id": acme }),
            json!({ "name": "two\nlines", "company_id": acme }),
            json!({ "name": "Rollout" }),
            json!({ "name": "Rollout", "company_id": acme, "expires_in_days": 0 }),
            json!({ "name": "Rollout", "company_id": acme, "expires_in_days": 366 }),
            json!({ "name": "Rollout", "company_id": acme, "max_uses": 0 }),
            json!({ "name": "Rollout", "company_id": acme, "platform": "macos" }),
        ] {
            let refused = admin.post("/v1/deployment-tokens", invalid.clone()).await;
            assert!(
                refused.status.is_client_error() && refused.status != StatusCode::NOT_FOUND,
                "{name}: {invalid} {:?}",
                refused.body
            );
        }
        assert_eq!(
            admin
                .post(
                    "/v1/deployment-tokens",
                    json!({ "name": "Rollout", "company_id": "00000000-0000-4000-8000-000000000000" }),
                )
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}"
        );
        let year = admin
            .post(
                "/v1/deployment-tokens",
                json!({ "name": "Year", "company_id": acme, "expires_in_days": 365, "max_uses": 500 }),
            )
            .await;
        assert_eq!(year.status, StatusCode::CREATED, "{name}: {:?}", year.body);
        assert_eq!(year.body["token"]["max_uses"], 500);
        assert!(audit_events(&app, "deployment_token.create").await.len() == 1);
        app.finish().await;
    }
}

#[tokio::test]
async fn redeeming_a_token_counts_against_the_attempt_limit() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let acme = company(&mut admin, "Acme").await;
        let (_, secret) = make(&mut admin, json!({ "name": "Rollout", "company_id": acme })).await;
        let mut limited = None;
        for _ in 0..40 {
            let refused = redeem(&app, &random_hex(32), "DESKTOP-1", &random_hex(32)).await;
            if refused.status == StatusCode::TOO_MANY_REQUESTS {
                limited = Some(refused);
                break;
            }
        }
        assert!(limited.is_some(), "{name}: guesses are never limited");
        assert_eq!(
            redeem(&app, &secret, "DESKTOP-1", &random_hex(32))
                .await
                .status,
            StatusCode::TOO_MANY_REQUESTS,
            "{name}: a valid token is limited too once guessing was"
        );
        app.finish().await;
    }
}
