//! Single sign-on with an OpenID Connect provider: its settings, signing in,
//! linking and creating accounts, and roles from the groups claim.
mod common;

use axum::http::{Method, StatusCode};
use common::{
    ADMIN_EMAIL, App, Browser, ORIGIN,
    oidc_provider::{Authorization, CLIENT_ID, CLIENT_SECRET, MockProvider},
};
use serde_json::{Value, json};

fn settings(provider: &MockProvider, extra: Value) -> Value {
    let mut settings = json!({
        "enabled": true,
        "display_name": "Acme SSO",
        "issuer_url": provider.issuer,
        "client_id": CLIENT_ID,
        "client_secret": CLIENT_SECRET,
        "auto_provision": true,
        "default_role_id": "technician",
        "groups_claim": "groups",
        "group_roles": [{ "group": "rmm-admins", "role_id": "administrator" }],
    });
    for (key, value) in extra.as_object().unwrap() {
        settings[key] = value.clone();
    }
    settings
}

/// Starts an SSO sign-in in a new browser. Returns the browser (holding the
/// binding cookie) and what the provider was asked for.
async fn start(app: &App, next: &str) -> (Browser, Authorization) {
    let mut browser = app.browser();
    let request = browser
        .request(Method::GET, &format!("/v1/auth/sso/start?next={next}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = browser.bytes(request).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let cookie = response.header("set-cookie");
    assert!(cookie.starts_with("__Host-meshrmm-sso="), "{cookie}");
    assert!(
        cookie.contains("HttpOnly; Secure; SameSite=Lax"),
        "{cookie}"
    );
    let authorization = Authorization::parse(response.header("location"));
    (browser, authorization)
}

/// Returns from the provider with `query` and answers where the browser
/// was sent.
async fn callback(browser: &mut Browser, query: &str) -> String {
    let request = browser
        .request(Method::GET, &format!("/v1/auth/sso/callback?{query}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = browser.bytes(request).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    response.header("location").to_owned()
}

/// Signs in through the provider as `claims`. Returns the browser and
/// where it ended up.
async fn sign_in(app: &App, provider: &MockProvider, claims: Value) -> (Browser, String) {
    let (mut browser, authorization) = start(app, "/toolbox").await;
    let code = provider.approve(&authorization, claims);
    let location = callback(
        &mut browser,
        &format!("code={code}&state={}", authorization.state),
    )
    .await;
    (browser, location)
}

fn failed(code: &str) -> String {
    format!("{ORIGIN}/login?sso_error={code}")
}

fn role_ids(user: &Value, key: &str) -> Vec<String> {
    user[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|role| role["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn administrators_set_up_the_provider() {
    let provider = MockProvider::start().await;
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let empty = admin.get("/v1/settings/sso").await;
        assert_eq!(
            empty.body,
            json!({ "redirect_uri": format!("{ORIGIN}/v1/auth/sso/callback"), "provider": null }),
            "{name}"
        );
        let instance = admin.get("/v1/instance").await;
        assert_eq!(instance.body["sign_in"]["sso"], Value::Null, "{name}");

        let unreachable = admin
            .put(
                "/v1/settings/sso",
                settings(&provider, json!({ "issuer_url": "http://127.0.0.1:1" })),
            )
            .await;
        assert_eq!(unreachable.code(), "discovery_failed", "{name}");
        for (field, value) in [
            ("issuer_url", json!("http://idp.example.com")),
            ("client_id", json!(" ")),
            ("default_role_id", json!("no-such-role")),
            (
                "group_roles",
                json!([{ "group": "x", "role_id": "no-such-role" }]),
            ),
        ] {
            let invalid = admin
                .put(
                    "/v1/settings/sso",
                    settings(&provider, json!({ field: value })),
                )
                .await;
            assert_eq!(invalid.status, StatusCode::BAD_REQUEST, "{name}: {field}");
        }

        let saved = admin
            .put("/v1/settings/sso", settings(&provider, json!({})))
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        let view = &saved.body["provider"];
        assert_eq!(view["has_client_secret"], true, "{name}");
        assert!(view.get("client_secret").is_none());
        assert_eq!(view["scopes"], "openid email profile");
        assert_eq!(
            view["group_roles"],
            json!([{ "group": "rmm-admins", "role_id": "administrator" }])
        );
        let instance = admin.get("/v1/instance").await;
        assert_eq!(
            instance.body["sign_in"]["sso"]["name"], "Acme SSO",
            "{name}"
        );

        // Leaving the secret out keeps it.
        let mut update = settings(&provider, json!({ "scopes": "email groups" }));
        update.as_object_mut().unwrap().remove("client_secret");
        let kept = admin.put("/v1/settings/sso", update).await;
        assert_eq!(kept.body["provider"]["has_client_secret"], true, "{name}");
        assert_eq!(kept.body["provider"]["scopes"], "openid email groups");
        let events = common::audit_events(&app, "settings.sso_update").await;
        assert_eq!(events[0].metadata["client_secret_changed"], false, "{name}");
        assert!(!events[1].metadata.to_string().contains(CLIENT_SECRET));

        // Only administrators may see or change it.
        let (mut manager, _) = common::user_with(
            &app,
            &mut admin,
            "manager@example.com",
            &["authentication.manage", "users.manage"],
        )
        .await;
        assert_eq!(
            manager.get("/v1/settings/sso").await.status,
            StatusCode::FORBIDDEN
        );
        let refused = manager
            .put("/v1/settings/sso", settings(&provider, json!({})))
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");

        let deleted = admin.delete("/v1/settings/sso").await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        let mut browser = app.browser();
        let request = browser
            .request(Method::GET, "/v1/auth/sso/start")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = browser.bytes(request).await;
        assert_eq!(response.header("location"), failed("unavailable"), "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn sso_creates_accounts_and_maps_groups_to_roles() {
    let provider = MockProvider::start().await;
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        admin
            .put("/v1/settings/sso", settings(&provider, json!({})))
            .await;

        let (mut browser, authorization) = start(&app, "/toolbox").await;
        assert_eq!(authorization.client_id, CLIENT_ID);
        assert_eq!(
            authorization.redirect_uri,
            format!("{ORIGIN}/v1/auth/sso/callback")
        );
        assert_eq!(authorization.scope, "openid email profile");
        let claims = json!({
            "sub": "user-1",
            "email": "Grace@Example.com",
            "email_verified": true,
            "name": "Grace Hopper",
            "groups": ["rmm-admins", "staff"],
        });
        let code = provider.approve(&authorization, claims.clone());
        let location = callback(
            &mut browser,
            &format!("code={code}&state={}", authorization.state),
        )
        .await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");
        let account = browser.get("/v1/account").await;
        assert_eq!(account.status, StatusCode::OK, "{name}: {:?}", account.body);
        assert_eq!(account.body["user"]["email"], "grace@example.com");
        assert_eq!(account.body["user"]["display_name"], "Grace Hopper");
        assert_eq!(account.body["user"]["has_password"], false);
        assert_eq!(account.body["session"]["auth_method"], "oidc");
        assert_eq!(account.body["is_administrator"], true, "{name}");
        let id = account.body["user"]["id"].as_str().unwrap().to_owned();

        let user = admin.get(&format!("/v1/users/{id}")).await;
        assert_eq!(user.body["sso_linked"], true, "{name}");
        assert_eq!(role_ids(&user.body, "roles"), ["technician"]);
        assert_eq!(
            user.body["group_roles"],
            json!([{ "id": "administrator", "name": "Administrator", "source": "sso", "group": "rmm-admins" }]),
            "{name}"
        );
        let roles = admin.get("/v1/roles").await;
        let administrators = roles
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|role| role["id"] == "administrator")
            .unwrap();
        assert_eq!(administrators["member_count"], 2, "{name}");
        let provisioned = common::audit_events(&app, "user.provision").await;
        assert_eq!(provisioned[0].actor_label, "SSO (Acme SSO)", "{name}");

        // Groups are read again at each sign-in.
        let (mut browser, location) = sign_in(
            &app,
            &provider,
            json!({ "sub": "user-1", "email": "other@example.com", "groups": ["staff"] }),
        )
        .await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");
        let account = browser.get("/v1/account").await;
        // The linked identity wins over a changed email claim.
        assert_eq!(account.body["user"]["email"], "grace@example.com", "{name}");
        assert_eq!(account.body["is_administrator"], false, "{name}");
        let user = admin.get(&format!("/v1/users/{id}")).await;
        assert_eq!(user.body["group_roles"], json!([]), "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn sso_links_existing_accounts_only_by_a_verified_email() {
    let provider = MockProvider::start().await;
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        admin
            .put(
                "/v1/settings/sso",
                settings(&provider, json!({ "auto_provision": false })),
            )
            .await;
        let (_, tech_id) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;

        let (_, location) = sign_in(
            &app,
            &provider,
            json!({ "sub": "tech", "email": "tech@example.com", "email_verified": false }),
        )
        .await;
        assert_eq!(location, failed("email_unverified"), "{name}");
        let (_, location) = sign_in(
            &app,
            &provider,
            json!({ "sub": "nobody", "email": "nobody@example.com", "email_verified": true }),
        )
        .await;
        assert_eq!(location, failed("no_account"), "{name}");
        let (_, location) = sign_in(&app, &provider, json!({ "sub": "anonymous" })).await;
        assert_eq!(location, failed("no_email"), "{name}");

        // Userinfo fills in an email the ID token lacks.
        provider.set_userinfo(Some(
            json!({ "email": "tech@example.com", "email_verified": true }),
        ));
        let (mut browser, location) = sign_in(&app, &provider, json!({ "sub": "tech" })).await;
        provider.set_userinfo(None);
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");
        let account = browser.get("/v1/account").await;
        assert_eq!(account.body["user"]["id"], tech_id.as_str(), "{name}");
        assert_eq!(common::audit_events(&app, "user.sso_link").await.len(), 1);

        // Roles from SSO groups hold only until a sign-in the provider
        // didn't see.
        let (mut sso_browser, _) = sign_in(
            &app,
            &provider,
            json!({ "sub": "tech", "groups": ["rmm-admins"] }),
        )
        .await;
        let account = sso_browser.get("/v1/account").await;
        assert_eq!(account.body["is_administrator"], true, "{name}");
        let mut password_browser = app.browser();
        let signed_in = password_browser
            .post(
                "/v1/auth/sign-in",
                json!({ "email": "tech@example.com", "password": "a long enough password" }),
            )
            .await;
        assert_eq!(
            signed_in.status,
            StatusCode::OK,
            "{name}: {:?}",
            signed_in.body
        );
        let account = sso_browser.get("/v1/account").await;
        assert_eq!(account.body["is_administrator"], false, "{name}");

        // Another identity at the provider with the same email can't take
        // the account over.
        let (_, location) = sign_in(
            &app,
            &provider,
            json!({ "sub": "impostor", "email": "tech@example.com", "email_verified": true }),
        )
        .await;
        assert_eq!(location, failed("conflict"), "{name}");
        let unlinked = admin
            .post(&format!("/v1/users/{tech_id}/unlink-sso"), json!({}))
            .await;
        assert_eq!(unlinked.status, StatusCode::NO_CONTENT, "{name}");
        let (_, location) = sign_in(
            &app,
            &provider,
            json!({ "sub": "replacement", "email": "tech@example.com", "email_verified": true }),
        )
        .await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");

        // Without the verified-email requirement, Entra-style tokens with
        // only a sign-in name work.
        let mut update = settings(
            &provider,
            json!({ "auto_provision": true, "require_verified_email": false }),
        );
        update.as_object_mut().unwrap().remove("client_secret");
        admin.put("/v1/settings/sso", update).await;
        let (mut browser, location) = sign_in(
            &app,
            &provider,
            json!({ "sub": "entra", "preferred_username": "Entra.User@Example.com" }),
        )
        .await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");
        let account = browser.get("/v1/account").await;
        assert_eq!(
            account.body["user"]["email"], "entra.user@example.com",
            "{name}"
        );

        // Disabled accounts stay out.
        admin
            .patch(&format!("/v1/users/{tech_id}"), json!({ "disabled": true }))
            .await;
        let (_, location) = sign_in(&app, &provider, json!({ "sub": "replacement" })).await;
        assert_eq!(location, failed("account_disabled"), "{name}");

        // Changing the provider forgets every link.
        let mut update = settings(
            &provider,
            json!({ "issuer_url": format!("{}/", provider.issuer) }),
        );
        update.as_object_mut().unwrap().remove("client_secret");
        update["enabled"] = json!(false);
        admin.put("/v1/settings/sso", update).await;
        let users = admin.get("/v1/users").await;
        assert!(
            users
                .body
                .as_array()
                .unwrap()
                .iter()
                .all(|user| user["sso_linked"] == false),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn sso_callbacks_must_come_back_to_the_browser_that_started() {
    let provider = MockProvider::start().await;
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        admin
            .put("/v1/settings/sso", settings(&provider, json!({})))
            .await;
        let claims = json!({ "sub": "u", "email": ADMIN_EMAIL, "email_verified": true });

        // A callback URL replayed in another browser does nothing.
        let (_, authorization) = start(&app, "/").await;
        let code = provider.approve(&authorization, claims.clone());
        let mut attacker = app.browser();
        let location = callback(
            &mut attacker,
            &format!("code={code}&state={}", authorization.state),
        )
        .await;
        assert_eq!(location, failed("expired"), "{name}");
        assert!(attacker.cookie.is_none(), "{name}");

        // Each state works once.
        let (mut browser, authorization) = start(&app, "/").await;
        let binding = browser.cookie.clone();
        let code = provider.approve(&authorization, claims.clone());
        let query = format!("code={code}&state={}", authorization.state);
        assert_eq!(
            callback(&mut browser, &query).await,
            format!("{ORIGIN}/"),
            "{name}"
        );
        browser.cookie = binding;
        assert_eq!(
            callback(&mut browser, &query).await,
            failed("expired"),
            "{name}"
        );

        // The provider's refusal, a forged ID token, a wrong nonce, and an
        // open redirect all fail safely.
        let (mut browser, authorization) = start(&app, "/").await;
        let location = callback(
            &mut browser,
            &format!("error=access_denied&state={}", authorization.state),
        )
        .await;
        assert_eq!(location, failed("denied"), "{name}");

        let (mut browser, authorization) = start(&app, "/").await;
        let code = provider.approve(&authorization, claims.clone());
        provider.forged_key();
        let location = callback(
            &mut browser,
            &format!("code={code}&state={}", authorization.state),
        )
        .await;
        assert_eq!(location, failed("failed"), "{name}");

        let (mut browser, authorization) = start(&app, "/").await;
        let mut wrong_nonce = claims.clone();
        wrong_nonce["nonce"] = json!("not the nonce");
        let code = provider.approve(&authorization, wrong_nonce);
        let location = callback(
            &mut browser,
            &format!("code={code}&state={}", authorization.state),
        )
        .await;
        assert_eq!(location, failed("failed"), "{name}");

        let (mut browser, authorization) = start(&app, "https://evil.example/").await;
        let code = provider.approve(&authorization, claims.clone());
        let location = callback(
            &mut browser,
            &format!("code={code}&state={}", authorization.state),
        )
        .await;
        assert_eq!(location, format!("{ORIGIN}/"), "{name}");
        app.finish().await;
    }
}
