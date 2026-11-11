//! Single sign-on with an OpenID Connect provider: its settings, signing in,
//! linking and creating accounts, and roles from the groups claim.
mod common;

use std::{sync::Arc, time::Duration};

use axum::http::{Method, StatusCode};
use common::{
    ADMIN_EMAIL, ADMIN_PASSWORD, App, Browser, ORIGIN,
    oidc_provider::{Authorization, CLIENT_ID, CLIENT_SECRET, Fetches, MockProvider},
};
use meshrmm_server::auth::oidc_discovery::{DiscoveryCache, Timings};
use sea_query::{Expr, ExprTrait, Query};
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
    start_in(app.browser(), next).await
}

async fn start_in(mut browser: Browser, next: &str) -> (Browser, Authorization) {
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

/// Where the browser is sent when starting a sign-in fails.
async fn start_refused(mut browser: Browser) -> String {
    let request = browser
        .request(Method::GET, "/v1/auth/sso/start")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = browser.bytes(request).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    response.header("location").to_owned()
}

/// The requests to `provider` since `before`.
fn fetched_since(provider: &MockProvider, before: Fetches) -> Fetches {
    let now = provider.fetches();
    Fetches {
        discovery: now.discovery - before.discovery,
        keys: now.keys - before.keys,
    }
}

const ONE_EACH: Fetches = Fetches {
    discovery: 1,
    keys: 1,
};
const NONE: Fetches = Fetches {
    discovery: 0,
    keys: 0,
};

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

/// Without auto-provisioning, sign-ins that don't carry an existing
/// account's verified email are refused.
async fn refuse_unmatched_sign_ins(name: &str, app: &App, provider: &MockProvider) {
    let (_, location) = sign_in(
        app,
        provider,
        json!({ "sub": "tech", "email": "tech@example.com", "email_verified": false }),
    )
    .await;
    assert_eq!(location, failed("email_unverified"), "{name}");
    let (_, location) = sign_in(
        app,
        provider,
        json!({ "sub": "nobody", "email": "nobody@example.com", "email_verified": true }),
    )
    .await;
    assert_eq!(location, failed("no_account"), "{name}");
    let (_, location) = sign_in(app, provider, json!({ "sub": "anonymous" })).await;
    assert_eq!(location, failed("no_email"), "{name}");
}

async fn end_group_roles_on_password_sign_in(name: &str, app: &App, provider: &MockProvider) {
    let (mut sso_browser, _) = sign_in(
        app,
        provider,
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
}

#[tokio::test]
async fn sign_ins_keep_the_last_administrators_sso_group() {
    let provider = MockProvider::start().await;
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let admin_id = admin.get("/v1/account").await.body["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        admin
            .put("/v1/settings/sso", settings(&provider, json!({})))
            .await;
        // Signing in through the provider links the account and makes the
        // administrator one through rmm-admins too, so they may give up the
        // role they hold directly.
        let ada = json!({ "sub": "ada", "email": ADMIN_EMAIL, "email_verified": true });
        let mut claims = ada.clone();
        claims["groups"] = json!(["rmm-admins"]);
        let (_, location) = sign_in(&app, &provider, claims).await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");
        let stepped_down = admin
            .patch(
                &format!("/v1/users/{admin_id}"),
                json!({ "role_ids": ["technician"] }),
            )
            .await;
        assert_eq!(
            stepped_down.status,
            StatusCode::OK,
            "{name}: {:?}",
            stepped_down.body
        );

        // A sign-in whose claims leave the group out, or carry no groups,
        // still signs in, but keeps the group that holds the last
        // administrator's role.
        let mut claims = ada.clone();
        claims["groups"] = json!(["staff"]);
        for claims in [claims, ada.clone()] {
            let (mut browser, location) = sign_in(&app, &provider, claims).await;
            assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");
            let account = browser.get("/v1/account").await;
            assert_eq!(account.body["session"]["auth_method"], "oidc", "{name}");
            assert_eq!(account.body["is_administrator"], true, "{name}");
        }
        // So does a sign-in the provider didn't see.
        let mut password = app.browser();
        let signed_in = password
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(signed_in.status, StatusCode::OK, "{name}");
        let user = admin.get(&format!("/v1/users/{admin_id}")).await;
        assert_eq!(
            user.body["group_roles"],
            json!([{ "id": "administrator", "name": "Administrator", "source": "sso", "group": "rmm-admins" }]),
            "{name}"
        );
        let kept = common::audit_events(&app, "user.sso_groups_kept").await;
        assert_eq!(kept.len(), 3, "{name}");
        assert_eq!(kept[0].target_id, admin_id, "{name}");
        assert_eq!(kept[0].actor_label, ADMIN_EMAIL, "{name}");
        assert_eq!(
            kept[0].metadata,
            json!({ "groups": ["rmm-admins"], "reason": "last_administrator" }),
            "{name}"
        );

        // With another administrator, the group goes as the claims say.
        common::add_user(&app, &mut admin, "second@example.com", &["administrator"]).await;
        let mut claims = ada.clone();
        claims["groups"] = json!(["staff"]);
        let (mut browser, _) = sign_in(&app, &provider, claims).await;
        let account = browser.get("/v1/account").await;
        assert_eq!(account.body["is_administrator"], false, "{name}");
        assert_eq!(
            common::audit_events(&app, "user.sso_groups_kept")
                .await
                .len(),
            3,
            "{name}"
        );
        app.finish().await;
    }
}

/// An administrator with no password or passkey counts as one who remains
/// only while SSO is on and linked to their account: otherwise nobody can
/// sign in as them.
#[tokio::test]
async fn administrators_without_a_password_count_only_while_sso_signs_them_in() {
    let provider = MockProvider::start().await;
    let other = MockProvider::start().await;
    for app in common::apps().await {
        let name = app.name;
        let mut ada = common::set_up(&app).await;
        let ada_id = ada.get("/v1/account").await.body["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let on = settings(&provider, json!({}));
        let off = settings(&provider, json!({ "enabled": false }));
        let saved = ada.put("/v1/settings/sso", on.clone()).await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        // SSO creates Grace's account, so it has no password.
        let (mut grace, location) = sign_in(&app, &provider, verified("grace")).await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");
        let grace_id = grace.get("/v1/account").await.body["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let promoted = ada
            .patch(
                &format!("/v1/users/{grace_id}"),
                json!({ "role_ids": ["administrator"] }),
            )
            .await;
        assert_eq!(
            promoted.status,
            StatusCode::OK,
            "{name}: {:?}",
            promoted.body
        );

        // While SSO is on, Grace can sign in, so Ada may step down.
        for role in ["technician", "administrator"] {
            let changed = grace
                .patch(
                    &format!("/v1/users/{ada_id}"),
                    json!({ "role_ids": [role] }),
                )
                .await;
            assert_eq!(changed.status, StatusCode::OK, "{name}: {:?}", changed.body);
        }

        // With SSO off nobody can sign in as Grace, so Ada, the last
        // administrator with a password, stays one.
        let turned_off = ada.put("/v1/settings/sso", off.clone()).await;
        assert_eq!(
            turned_off.status,
            StatusCode::OK,
            "{name}: {:?}",
            turned_off.body
        );
        let demoted = grace
            .patch(
                &format!("/v1/users/{ada_id}"),
                json!({ "role_ids": ["technician"] }),
            )
            .await;
        let disabled = grace
            .patch(&format!("/v1/users/{ada_id}"), json!({ "disabled": true }))
            .await;
        let deleted = grace.delete(&format!("/v1/users/{ada_id}")).await;
        for refused in [demoted, disabled, deleted] {
            assert_no_administrator_could_sign_in(name, &refused);
        }
        let account = ada.get("/v1/account").await;
        assert_eq!(account.body["is_administrator"], true, "{name}");

        // With SSO back on, Ada may go, leaving Grace the only administrator.
        let turned_on = ada.put("/v1/settings/sso", on.clone()).await;
        assert_eq!(
            turned_on.status,
            StatusCode::OK,
            "{name}: {:?}",
            turned_on.body
        );
        let disabled = grace
            .patch(&format!("/v1/users/{ada_id}"), json!({ "disabled": true }))
            .await;
        assert_eq!(
            disabled.status,
            StatusCode::OK,
            "{name}: {:?}",
            disabled.body
        );

        // Then nothing may stop SSO signing Grace in: turning it off,
        // removing it, moving to another provider (which unlinks every
        // account), or unlinking her account.
        let turned_off = grace.put("/v1/settings/sso", off.clone()).await;
        let removed = grace.delete("/v1/settings/sso").await;
        let moved = grace
            .put("/v1/settings/sso", settings(&other, json!({})))
            .await;
        let unlinked = grace
            .post(&format!("/v1/users/{grace_id}/unlink-sso"), json!({}))
            .await;
        for refused in [turned_off, removed, moved, unlinked] {
            assert_no_administrator_could_sign_in(name, &refused);
        }
        let kept = grace.get("/v1/settings/sso").await;
        assert_eq!(kept.body["provider"]["enabled"], true, "{name}");
        assert_eq!(kept.body["provider"]["issuer_url"], provider.issuer);
        let user = grace.get(&format!("/v1/users/{grace_id}")).await;
        assert_eq!(user.body["sso_linked"], true, "{name}");
        let (_, location) = sign_in(&app, &provider, verified("grace")).await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");

        // Once an administrator has a password again, SSO may go.
        let enabled = grace
            .patch(&format!("/v1/users/{ada_id}"), json!({ "disabled": false }))
            .await;
        assert_eq!(enabled.status, StatusCode::OK, "{name}: {:?}", enabled.body);
        let turned_off = grace.put("/v1/settings/sso", off).await;
        assert_eq!(
            turned_off.status,
            StatusCode::OK,
            "{name}: {:?}",
            turned_off.body
        );
        let removed = grace.delete("/v1/settings/sso").await;
        assert_eq!(
            removed.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            removed.body
        );
        app.finish().await;
    }
}

/// A change refused because no administrator could sign in after it.
#[track_caller]
fn assert_no_administrator_could_sign_in(name: &str, refused: &common::Response) {
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "{name}: {:?}",
        refused.body
    );
    assert_eq!(refused.code(), "no_administrator_can_sign_in", "{name}");
    assert!(
        refused.body["error"]
            .as_str()
            .unwrap()
            .contains("give an administrator a password first"),
        "{name}: {:?}",
        refused.body
    );
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

        refuse_unmatched_sign_ins(name, &app, &provider).await;

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
        end_group_roles_on_password_sign_in(name, &app, &provider).await;

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

/// Tries `password` for the administrator from `ip`.
async fn password_sign_in(browser: &mut Browser, ip: &str, password: &str) -> StatusCode {
    browser
        .post_from(
            ip,
            "/v1/auth/sign-in",
            json!({ "email": ADMIN_EMAIL, "password": password }),
        )
        .await
        .status
}

/// Someone who signs in through SSO and one day needs their password gets
/// in from that browser while guesses from many addresses have used up the
/// account's limit, as after a password sign-in.
#[tokio::test]
async fn an_sso_sign_in_gives_the_browser_a_device_cookie() {
    let provider = MockProvider::start().await;
    let app = App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    admin
        .put("/v1/settings/sso", settings(&provider, json!({})))
        .await;
    let (mut browser, authorization) = start(&app, "/").await;
    let code = provider.approve(
        &authorization,
        json!({ "sub": "ada", "email": ADMIN_EMAIL, "email_verified": true }),
    );
    let request = browser
        .request(
            Method::GET,
            &format!(
                "/v1/auth/sso/callback?code={code}&state={}",
                authorization.state
            ),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    let response = browser.bytes(request).await;
    assert_eq!(response.header("location"), format!("{ORIGIN}/"));
    let device = response
        .headers
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap())
        .find(|value| value.starts_with("__Host-meshrmm-device="))
        .expect("an SSO sign-in sets a device cookie");
    assert!(
        device.ends_with("HttpOnly; Secure; SameSite=Lax"),
        "{device}"
    );
    let device = device.split(';').next().unwrap().to_owned();

    for address in 1..=5 {
        for _ in 0..10 {
            let guess = password_sign_in(
                &mut app.browser(),
                &format!("198.51.100.{address}"),
                "a guess",
            )
            .await;
            assert_eq!(guess, StatusCode::UNAUTHORIZED);
        }
    }
    let unknown = password_sign_in(&mut app.browser(), "203.0.113.20", ADMIN_PASSWORD).await;
    assert_eq!(unknown, StatusCode::TOO_MANY_REQUESTS);
    let mut known = app.browser();
    known.cookie = Some(device);
    let signed_in = password_sign_in(&mut known, "203.0.113.20", ADMIN_PASSWORD).await;
    assert_eq!(signed_in, StatusCode::OK);
    app.finish().await;
}

/// An office behind one address that signs in through SSO all day doesn't
/// use up the cap on starts; refused and abandoned sign-ins still count.
#[tokio::test]
async fn only_unfinished_sso_sign_ins_count_against_the_cap() {
    let provider = MockProvider::start().await;
    let app = App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    admin
        .put("/v1/settings/sso", settings(&provider, json!({})))
        .await;
    for attempt in 0..70 {
        let (_, location) = sign_in(&app, &provider, verified("ada")).await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "attempt {attempt}");
    }
    let (mut browser, authorization) = start(&app, "/").await;
    let location = callback(
        &mut browser,
        &format!("error=access_denied&state={}", authorization.state),
    )
    .await;
    assert_eq!(location, failed("denied"));
    for _ in 0..59 {
        start(&app, "/").await;
    }
    assert_eq!(start_refused(app.browser()).await, failed("rate_limited"));
    app.finish().await;
}

/// Signs `sub` in through the provider, as a member of the group that makes
/// them an administrator, and opens their event socket.
async fn watching(
    app: &App,
    provider: &MockProvider,
    server: &common::Server,
    sub: &str,
) -> (common::Ws, std::time::Instant, String) {
    let (mut browser, _) = sign_in(
        app,
        provider,
        json!({ "sub": sub, "email": format!("{sub}@example.com"), "email_verified": true, "groups": ["rmm-admins"] }),
    )
    .await;
    let account = browser.get("/v1/account").await;
    assert_eq!(account.body["is_administrator"], true, "{:?}", account.body);
    let id = account.body["user"]["id"].as_str().unwrap().to_owned();
    let mut socket = browser.events(server).await.unwrap();
    let opened = std::time::Instant::now();
    common::receive_json(&mut socket).await;
    (socket, opened, id)
}

#[tokio::test]
async fn losing_roles_from_sso_groups_closes_sockets_at_once() {
    // Well before the socket's own check, every 30 seconds.
    let prompt = std::time::Duration::from_secs(30);
    let provider = MockProvider::start().await;
    for app in common::apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = common::set_up(&app).await;
        // Only the group grants anything.
        let grouped = settings(&provider, json!({ "default_role_id": null }));
        let saved = admin.put("/v1/settings/sso", grouped.clone()).await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);

        // The group no longer grants a role.
        let (mut socket, opened, _) = watching(&app, &provider, &server, "ada").await;
        let ungrouped = settings(
            &provider,
            json!({ "default_role_id": null, "group_roles": [] }),
        );
        let saved = admin.put("/v1/settings/sso", ungrouped).await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        assert_eq!(common::receive_close(&mut socket).await, 4001, "{name}");
        assert!(opened.elapsed() < prompt, "{name}");
        admin.put("/v1/settings/sso", grouped).await;

        // The user's SSO identity is unlinked.
        let (mut socket, opened, id) = watching(&app, &provider, &server, "bob").await;
        let unlinked = admin
            .post(&format!("/v1/users/{id}/unlink-sso"), json!({}))
            .await;
        assert_eq!(
            unlinked.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            unlinked.body
        );
        assert_eq!(common::receive_close(&mut socket).await, 4001, "{name}");
        assert!(opened.elapsed() < prompt, "{name}");

        // SSO is turned off.
        let (mut socket, opened, _) = watching(&app, &provider, &server, "cy").await;
        let deleted = admin.delete("/v1/settings/sso").await;
        assert_eq!(
            deleted.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            deleted.body
        );
        assert_eq!(common::receive_close(&mut socket).await, 4001, "{name}");
        assert!(opened.elapsed() < prompt, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn losing_roles_from_sso_groups_applies_to_the_next_request() {
    let provider = MockProvider::start().await;
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let grouped = settings(&provider, json!({ "default_role_id": null }));
        let saved = admin.put("/v1/settings/sso", grouped).await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        let (mut browser, _) = sign_in(
            &app,
            &provider,
            json!({ "sub": "ada", "email": "ada@example.com", "email_verified": true, "groups": ["rmm-admins"] }),
        )
        .await;
        for _ in 0..2 {
            let account = browser.get("/v1/account").await;
            assert_eq!(account.body["is_administrator"], true, "{name}");
        }

        // The group no longer grants a role.
        let ungrouped = settings(
            &provider,
            json!({ "default_role_id": null, "group_roles": [] }),
        );
        let saved = admin.put("/v1/settings/sso", ungrouped).await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        let account = browser.get("/v1/account").await;
        assert_eq!(account.body["is_administrator"], false, "{name}");
        assert_eq!(account.body["roles"], json!([]), "{name}");
        assert_eq!(
            browser.get("/v1/users").await.status,
            StatusCode::FORBIDDEN,
            "{name}"
        );
        app.finish().await;
    }
}

fn verified(sub: &str) -> Value {
    json!({ "sub": sub, "email": format!("{sub}@example.com"), "email_verified": true })
}

#[tokio::test]
async fn the_providers_metadata_is_fetched_once_for_many_sign_ins() {
    let provider = MockProvider::start().await;
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        // Saving fetches afresh, and sign-ins use what it fetched.
        let before = provider.fetches();
        let saved = admin
            .put("/v1/settings/sso", settings(&provider, json!({})))
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        assert_eq!(fetched_since(&provider, before), ONE_EACH, "{name}");
        let before = provider.fetches();
        for _ in 0..5 {
            start(&app, "/").await;
        }
        let (_, location) = sign_in(&app, &provider, verified("ada")).await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");
        assert_eq!(fetched_since(&provider, before), NONE, "{name}");

        // Sign-ins on a server that has fetched nothing share one fetch.
        let restarted = app.restarted().await;
        let before = provider.fetches();
        futures_util::future::join_all((0..8).map(|_| start_in(restarted.browser(), "/"))).await;
        assert_eq!(fetched_since(&provider, before), ONE_EACH, "{name}");
        start_in(restarted.browser(), "/").await;
        assert_eq!(fetched_since(&provider, before), ONE_EACH, "{name}");
        restarted.finish().await;

        // Changed settings are fetched for afresh.
        app.db()
            .execute(
                &Query::update()
                    .table("oidc_provider")
                    .value("updated_at", Expr::col("updated_at").add(1))
                    .to_owned(),
            )
            .await
            .unwrap();
        let before = provider.fetches();
        start(&app, "/").await;
        start(&app, "/").await;
        assert_eq!(fetched_since(&provider, before), ONE_EACH, "{name}");
        app.finish().await;
    }
}

/// How long the provider's metadata is used in tests that let it expire.
const LIFETIME: Duration = Duration::from_secs(1);

fn short_lived(state: &mut meshrmm_server::http::AppState) {
    state.sso_discovery = Arc::new(DiscoveryCache::new(Timings {
        lifetime: LIFETIME,
        ..Timings::default()
    }));
}

async fn expire() {
    tokio::time::sleep(LIFETIME + Duration::from_millis(100)).await;
}

#[tokio::test]
async fn the_providers_metadata_is_fetched_again_when_it_expires() {
    let provider = MockProvider::start().await;
    for app in common::apps_with(short_lived).await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        admin
            .put("/v1/settings/sso", settings(&provider, json!({})))
            .await;
        let (mut early, early_authorization) = start(&app, "/").await;

        // The provider rotates its keys before the metadata expires; the
        // next sign-in fetches it again, once.
        provider.rotate_key();
        expire().await;
        let before = provider.fetches();
        let (mut late, late_authorization) = start(&app, "/").await;
        start(&app, "/").await;
        assert_eq!(fetched_since(&provider, before), ONE_EACH, "{name}");

        // Both sign-ins complete with the new key, the one started before
        // the refresh too, without fetching anything more.
        for (browser, authorization, sub) in [
            (&mut early, &early_authorization, "early"),
            (&mut late, &late_authorization, "late"),
        ] {
            let code = provider.approve(authorization, verified(sub));
            let location = callback(
                browser,
                &format!("code={code}&state={}", authorization.state),
            )
            .await;
            assert_eq!(location, format!("{ORIGIN}/"), "{name}: {sub}");
        }
        assert_eq!(fetched_since(&provider, before), ONE_EACH, "{name}");

        // While the discovery document doesn't load, sign-ins go on with
        // what was fetched before, trying again only now and then.
        provider.set_discovery_down(true);
        expire().await;
        let before = provider.fetches();
        for sub in ["during", "outage"] {
            let (_, location) = sign_in(&app, &provider, verified(sub)).await;
            assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}: {sub}");
        }
        assert_eq!(
            fetched_since(&provider, before),
            Fetches {
                discovery: 1,
                keys: 0
            },
            "{name}"
        );
        // With nothing fetched before, they fail.
        let restarted = app.restarted().await;
        assert_eq!(
            start_refused(restarted.browser()).await,
            failed("failed"),
            "{name}"
        );
        restarted.finish().await;
        provider.set_discovery_down(false);
        app.finish().await;
    }
}

#[tokio::test]
async fn rotated_keys_are_fetched_once_and_unknown_keys_refused() {
    let provider = MockProvider::start().await;
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        admin
            .put("/v1/settings/sso", settings(&provider, json!({})))
            .await;
        start(&app, "/").await;

        // A token signed with the provider's new key fetches its keys again.
        provider.rotate_key();
        let before = provider.fetches();
        let (_, location) = sign_in(&app, &provider, verified("ada")).await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");
        assert_eq!(
            fetched_since(&provider, before),
            Fetches {
                discovery: 0,
                keys: 1
            },
            "{name}"
        );

        // Tokens signed with keys the provider never published are refused,
        // and the keys were fetched a moment ago, so aren't fetched again.
        for _ in 0..5 {
            let (mut browser, authorization) = start(&app, "/").await;
            let code = provider.approve(&authorization, verified("ada"));
            provider.unknown_key();
            let location = callback(
                &mut browser,
                &format!("code={code}&state={}", authorization.state),
            )
            .await;
            assert_eq!(location, failed("failed"), "{name}");
        }
        let (_, location) = sign_in(&app, &provider, verified("ada")).await;
        assert_eq!(location, format!("{ORIGIN}/toolbox"), "{name}");
        assert_eq!(
            fetched_since(&provider, before),
            Fetches {
                discovery: 0,
                keys: 1
            },
            "{name}"
        );
        app.finish().await;
    }
}
