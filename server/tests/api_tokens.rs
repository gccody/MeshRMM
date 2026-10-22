//! Personal API tokens: making, scoping, using, revoking and limiting them.
mod common;

use axum::http::{Method, StatusCode};
use common::{App, Response};
use sea_query::{Expr, ExprTrait, Query};
use serde_json::{Value, json};

/// Sends a request the way a script does: the token and no cookie, origin
/// or website header.
async fn call(app: &App, token: &str, method: Method, path: &str, body: Option<Value>) -> Response {
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

async fn get(app: &App, token: &str, path: &str) -> Response {
    call(app, token, Method::GET, path, None).await
}

/// Makes a token as `browser`'s user and returns its ID and secret.
async fn make(browser: &mut common::Browser, body: Value) -> (String, String) {
    let created = browser.post("/v1/api-tokens", body).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    (
        created.body["id"].as_str().unwrap().to_owned(),
        created.body["secret"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn tokens_act_for_their_user_within_their_scope() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let created = admin
            .post(
                "/v1/api-tokens",
                json!({ "name": "Inventory sync", "expires_in_days": 30 }),
            )
            .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            created.body
        );
        let secret = created.body["secret"].as_str().unwrap().to_owned();
        let id = created.body["id"].as_str().unwrap().to_owned();
        assert!(secret.starts_with("mrmm_pat_"), "{name}");
        assert_eq!(secret.len(), 9 + 64);
        assert_eq!(created.body["permissions"], Value::Null);
        assert_eq!(created.body["user"]["email"], common::ADMIN_EMAIL);

        // The list never shows the secret again.
        let listed = admin.get("/v1/api-tokens").await;
        assert_eq!(listed.status, StatusCode::OK, "{name}");
        assert_eq!(listed.body["tokens"][0]["id"], id.as_str());
        assert!(listed.body["tokens"][0].get("secret").is_none());
        assert!(!listed.body.to_string().contains(&secret));
        assert_eq!(
            listed.body["policy"],
            json!({ "enabled": true, "max_lifetime_days": 365 })
        );
        assert_eq!(listed.body["tokens"][0]["last_used_at"], Value::Null);

        // It reads and writes without the website's CSRF headers.
        assert_eq!(
            get(&app, &secret, "/v1/agents").await.status,
            StatusCode::OK
        );
        let tag = call(
            &app,
            &secret,
            Method::POST,
            "/v1/tags",
            Some(json!({ "name": "Servers", "color": "blue" })),
        )
        .await;
        assert_eq!(tag.status, StatusCode::CREATED, "{name}: {:?}", tag.body);
        let used = admin.get("/v1/api-tokens").await;
        assert!(used.body["tokens"][0]["last_used_at"].is_i64(), "{name}");
        assert_eq!(used.body["tokens"][0]["last_used_ip"], "0.0.0.0");

        // The audit log names the user and the token.
        let event = &common::audit_events(&app, "tag.create").await[0];
        assert_eq!(event.actor_label, common::ADMIN_EMAIL, "{name}");
        let token = event.api_token.as_ref().expect("the token is recorded");
        assert_eq!(
            (token.id.as_str(), token.name.as_str()),
            (id.as_str(), "Inventory sync")
        );
        let audit = admin.get(&format!("/v1/audit?api_token_id={id}")).await;
        assert_eq!(audit.body["events"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(
            audit.body["events"][0]["api_token"],
            json!({ "id": id, "name": "Inventory sync" })
        );
        let created_event = &common::audit_events(&app, "api_token.create").await[0];
        assert_eq!(created_event.target_id, id);
        assert!(created_event.api_token.is_none(), "made on the website");

        let current = get(&app, &secret, "/v1/api-tokens/current").await;
        assert_eq!(current.status, StatusCode::OK, "{name}");
        assert_eq!(current.body["token"]["id"], id.as_str());
        assert_eq!(current.body["permissions"].as_array().unwrap().len(), 30);
        assert_eq!(
            admin.get("/v1/api-tokens/current").await.code(),
            "not_an_api_token"
        );

        // A scoped token does only what its scope allows.
        let (_, scoped) = make(
            &mut admin,
            json!({ "name": "Read devices", "expires_in_days": 7, "permissions": ["devices.view"] }),
        )
        .await;
        assert_eq!(
            get(&app, &scoped, "/v1/agents").await.status,
            StatusCode::OK
        );
        let audit = get(&app, &scoped, "/v1/audit").await;
        assert_eq!(audit.status, StatusCode::FORBIDDEN, "{name}");
        assert_eq!(audit.code(), "permission_denied");
        let tag = call(
            &app,
            &scoped,
            Method::POST,
            "/v1/tags",
            Some(json!({ "name": "Desktops", "color": "red" })),
        )
        .await;
        assert_eq!(tag.code(), "permission_denied", "{name}");
        // Nor is a scoped administrator's token the Administrator role.
        assert_eq!(
            get(&app, &scoped, "/v1/settings/smtp").await.code(),
            "permission_denied"
        );
        assert_eq!(
            get(&app, &scoped, "/v1/api-tokens/current").await.body["permissions"],
            json!(["devices.view"])
        );

        // Account security, credentials and the website's sockets are off
        // limits to every token.
        for (method, path, body) in [
            (
                Method::POST,
                "/v1/api-tokens",
                Some(json!({ "name": "More", "expires_in_days": 7 })),
            ),
            (Method::GET, "/v1/account", None),
            (Method::GET, "/v1/account/sessions", None),
            (
                Method::PATCH,
                "/v1/account",
                Some(json!({ "display_name": "Eve" })),
            ),
            (
                Method::POST,
                "/v1/account/password",
                Some(json!({ "current_password": "x", "new_password": "y" })),
            ),
            (Method::GET, "/v1/settings/authentication", None),
            (
                Method::PATCH,
                "/v1/settings/authentication",
                Some(json!({ "api_tokens_enabled": true })),
            ),
            (Method::GET, "/v1/settings/scim", None),
            (Method::GET, "/v1/settings/sso", None),
            (Method::GET, "/v1/events", None),
            (Method::POST, "/v1/remote/handoffs", Some(json!({}))),
        ] {
            let response = call(&app, &secret, method, path, body).await;
            assert_eq!(response.status, StatusCode::FORBIDDEN, "{name}: {path}");
            assert_eq!(response.code(), "api_token_not_allowed", "{name}: {path}");
        }

        // A cookie request still needs the website's headers.
        let mut request = admin.request(Method::POST, "/v1/tags");
        request.headers_mut().unwrap().remove("x-meshrmm-request");
        let forged = admin
            .raw(
                request
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({ "name": "Forged", "color": "red" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(forged.code(), "cross_site_request", "{name}");

        // The secret never reaches the audit log.
        let everything = admin.get("/v1/audit?limit=200").await;
        assert!(
            !everything.body.to_string().contains(&secret[9..]),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn tokens_never_exceed_their_users_current_access() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let role = common::add_role(&mut admin, "Auditor", &["devices.view", "audit.view"]).await;
        let (mut auditor, auditor_id) =
            common::add_user(&app, &mut admin, "auditor@example.com", &[&role]).await;

        let escalation = auditor
            .post(
                "/v1/api-tokens",
                json!({ "name": "Too much", "expires_in_days": 7, "permissions": ["users.manage"] }),
            )
            .await;
        assert_eq!(escalation.code(), "permission_escalation", "{name}");
        assert_eq!(
            auditor.get("/v1/api-tokens").await.body["permissions"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "a user may limit tokens to their own permissions: {name}"
        );

        let (_, secret) = make(
            &mut auditor,
            json!({ "name": "Reports", "expires_in_days": 90 }),
        )
        .await;
        assert_eq!(get(&app, &secret, "/v1/audit").await.status, StatusCode::OK);

        // Taking a permission from the role takes it from the token at once.
        let updated = admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "permissions": ["devices.view"] }),
            )
            .await;
        assert_eq!(updated.status, StatusCode::OK, "{name}: {:?}", updated.body);
        assert_eq!(
            get(&app, &secret, "/v1/audit").await.code(),
            "permission_denied"
        );
        assert_eq!(
            get(&app, &secret, "/v1/agents").await.status,
            StatusCode::OK
        );

        // A disabled user's tokens stop working; so do a deleted user's.
        let disabled = admin
            .patch(
                &format!("/v1/users/{auditor_id}"),
                json!({ "disabled": true }),
            )
            .await;
        assert_eq!(
            disabled.status,
            StatusCode::OK,
            "{name}: {:?}",
            disabled.body
        );
        let refused = get(&app, &secret, "/v1/agents").await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(refused.code(), "user_disabled");
        assert_eq!(
            admin
                .delete(&format!("/v1/users/{auditor_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT,
            "{name}"
        );
        assert_eq!(
            get(&app, &secret, "/v1/agents").await.code(),
            "invalid_token"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn tokens_are_revoked_expire_and_follow_the_policy() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut tech, _) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;

        // Lifetimes within the default 365-day cap only.
        for (body, code) in [
            (
                json!({ "name": "Forever", "expires_in_days": null }),
                "lifetime_too_long",
            ),
            (
                json!({ "name": "Long", "expires_in_days": 400 }),
                "lifetime_too_long",
            ),
            (json!({ "name": "Unset" }), ""),
            (json!({ "name": "", "expires_in_days": 7 }), ""),
            (
                json!({ "name": "None", "expires_in_days": 7, "permissions": [] }),
                "",
            ),
        ] {
            let response = tech.post("/v1/api-tokens", body.clone()).await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST, "{name}: {body}");
            assert_eq!(response.code(), code, "{name}: {body}");
        }

        let (own_id, own) = make(&mut tech, json!({ "name": "Mine", "expires_in_days": 30 })).await;
        let (other_id, other) = make(
            &mut tech,
            json!({ "name": "Shared", "expires_in_days": 365 }),
        )
        .await;
        let (admin_id, admin_token) = make(
            &mut admin,
            json!({ "name": "Admin's", "expires_in_days": 365 }),
        )
        .await;

        // Users see and revoke only their own; api_tokens.manage sees all.
        assert_eq!(
            tech.get("/v1/api-tokens?all=true").await.code(),
            "permission_denied"
        );
        assert_eq!(
            tech.delete(&format!("/v1/api-tokens/{admin_id}"))
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}"
        );
        let all = admin.get("/v1/api-tokens?all=true").await;
        assert_eq!(all.body["tokens"].as_array().unwrap().len(), 3, "{name}");
        // A token revokes itself.
        assert_eq!(
            call(
                &app,
                &own,
                Method::DELETE,
                &format!("/v1/api-tokens/{own_id}"),
                None
            )
            .await
            .status,
            StatusCode::NO_CONTENT,
            "{name}"
        );
        let revoked = get(&app, &own, "/v1/agents").await;
        assert_eq!(revoked.status, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(revoked.code(), "invalid_token");
        assert_eq!(
            admin
                .delete(&format!("/v1/api-tokens/{other_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT,
            "{name}"
        );
        assert_eq!(
            get(&app, &other, "/v1/agents").await.code(),
            "invalid_token"
        );
        let event = &common::audit_events(&app, "api_token.revoke").await[0];
        assert_eq!(event.actor_label, common::ADMIN_EMAIL, "{name}");
        assert_eq!(event.metadata["user_email"], "tech@example.com");
        assert_eq!(event.metadata["name"], "Shared");

        // An expired token is refused but stays listed.
        let (expiring_id, expiring) =
            make(&mut tech, json!({ "name": "Short", "expires_in_days": 1 })).await;
        app.db()
            .execute(
                &Query::update()
                    .table("api_tokens")
                    .value("expires_at", 1)
                    .and_where(Expr::col("id").eq(expiring_id.as_str()))
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(
            get(&app, &expiring, "/v1/agents").await.code(),
            "token_expired"
        );
        assert_eq!(
            tech.get("/v1/api-tokens").await.body["tokens"][0]["id"],
            expiring_id.as_str()
        );

        // Lifting the cap allows tokens that never expire; lowering it
        // shortens existing tokens.
        let lifted = admin
            .patch(
                "/v1/settings/authentication",
                json!({ "api_token_max_lifetime_days": null }),
            )
            .await;
        assert_eq!(
            lifted.body["api_token_max_lifetime_days"],
            Value::Null,
            "{name}"
        );
        let (forever_id, _) = make(
            &mut admin,
            json!({ "name": "Forever", "expires_in_days": null }),
        )
        .await;
        let capped = admin
            .patch(
                "/v1/settings/authentication",
                json!({ "api_token_max_lifetime_days": 30 }),
            )
            .await;
        assert_eq!(capped.body["api_token_max_lifetime_days"], 30, "{name}");
        let tokens = admin.get("/v1/api-tokens").await.body["tokens"].clone();
        for token in tokens.as_array().unwrap() {
            let lifetime =
                token["expires_at"].as_i64().unwrap() - token["created_at"].as_i64().unwrap();
            assert_eq!(lifetime, 30 * 24 * 3600 * 1000, "{name}: {token}");
        }
        assert!(
            tokens
                .as_array()
                .unwrap()
                .iter()
                .any(|token| token["id"] == forever_id.as_str())
        );
        assert_eq!(
            admin
                .patch(
                    "/v1/settings/authentication",
                    json!({ "api_token_max_lifetime_days": 0 }),
                )
                .await
                .status,
            StatusCode::BAD_REQUEST,
            "{name}"
        );

        // Turning tokens off refuses every one, and making new ones.
        let off = admin
            .patch(
                "/v1/settings/authentication",
                json!({ "api_tokens_enabled": false }),
            )
            .await;
        assert_eq!(off.body["api_tokens_enabled"], false, "{name}");
        let refused = get(&app, &admin_token, "/v1/agents").await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(refused.code(), "api_tokens_disabled");
        assert_eq!(
            tech.post(
                "/v1/api-tokens",
                json!({ "name": "New", "expires_in_days": 7 })
            )
            .await
            .code(),
            "api_tokens_disabled"
        );
        assert_eq!(
            tech.get("/v1/api-tokens").await.body["policy"]["enabled"],
            false
        );
        admin
            .patch(
                "/v1/settings/authentication",
                json!({ "api_tokens_enabled": true }),
            )
            .await;
        assert_eq!(
            get(&app, &admin_token, "/v1/agents").await.status,
            StatusCode::OK
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn unknown_tokens_are_rate_limited_like_sign_ins() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (_, secret) = make(&mut admin, json!({ "name": "Valid", "expires_in_days": 7 })).await;
        let guess = format!("mrmm_pat_{}", "0".repeat(64));
        for attempt in 0..30 {
            let response = get(&app, &guess, "/v1/agents").await;
            assert_eq!(
                response.code(),
                "invalid_token",
                "{name}: attempt {attempt}"
            );
        }
        let limited = get(&app, &guess, "/v1/agents").await;
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS, "{name}");
        assert!(!limited.headers["retry-after"].is_empty());
        // Once limited, the address gets nothing more, even with a real token.
        assert_eq!(
            get(&app, &secret, "/v1/agents").await.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{name}"
        );
        // The website is unaffected.
        assert_eq!(
            admin.get("/v1/agents").await.status,
            StatusCode::OK,
            "{name}"
        );
        app.finish().await;
    }
}
