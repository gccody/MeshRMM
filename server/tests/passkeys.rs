//! Passkeys: adding them, signing in with one alone or after a password,
//! and the rules for removing them.
mod common;

use axum::http::StatusCode;
use common::{ADMIN_EMAIL, ADMIN_PASSWORD, App, Browser, ORIGIN, passkey::SoftPasskey};
use serde_json::{Value, json};

/// Adds a software passkey to the browser's account. Returns it and the
/// response body.
async fn add_passkey(browser: &mut Browser, password: &str, name: &str) -> (SoftPasskey, Value) {
    let options = browser
        .post(
            "/v1/account/passkeys/options",
            json!({ "password": password }),
        )
        .await;
    assert_eq!(options.status, StatusCode::OK, "{:?}", options.body);
    let selection = &options.body["options"]["publicKey"]["authenticatorSelection"];
    assert_eq!(selection["residentKey"], "preferred");
    assert_eq!(selection["userVerification"], "required");
    let (passkey, credential) = SoftPasskey::register(&options.body["options"], ORIGIN);
    let added = browser
        .post(
            "/v1/account/passkeys",
            json!({
                "ceremony": options.body["ceremony"],
                "name": name,
                "credential": credential,
            }),
        )
        .await;
    assert_eq!(added.status, StatusCode::CREATED, "{:?}", added.body);
    (passkey, added.body)
}

/// Signs in a new browser with `passkey` alone.
async fn passkey_sign_in(
    app: &App,
    passkey: &mut SoftPasskey,
    origin: &str,
) -> (Browser, common::Response) {
    let mut browser = app.browser();
    let options = browser.post("/v1/auth/passkey/options", json!({})).await;
    assert_eq!(options.status, StatusCode::OK, "{:?}", options.body);
    assert!(options.body["options"].get("mediation").is_none());
    assert_eq!(
        options.body["options"]["publicKey"]["allowCredentials"],
        json!([])
    );
    let credential = passkey.assert(&options.body["options"], origin);
    let response = browser
        .post(
            "/v1/auth/passkey",
            json!({ "ceremony": options.body["ceremony"], "credential": credential }),
        )
        .await;
    (browser, response)
}

#[tokio::test]
async fn a_passkey_signs_in_on_its_own() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let instance = admin.get("/v1/instance").await;
        assert_eq!(instance.body["sign_in"]["passkey"], true, "{name}");

        let wrong = admin
            .post(
                "/v1/account/passkeys/options",
                json!({ "password": "nope" }),
            )
            .await;
        assert_eq!(wrong.code(), "incorrect_password", "{name}");
        let (mut passkey, added) = add_passkey(&mut admin, ADMIN_PASSWORD, "Laptop").await;
        // The first second factor comes with recovery codes.
        assert_eq!(
            added["recovery_codes"].as_array().unwrap().len(),
            10,
            "{name}"
        );
        assert_eq!(added["passkey"]["name"], "Laptop");
        let account = admin.get("/v1/account").await;
        assert_eq!(
            account.body["two_factor"],
            json!({
                "enabled": true,
                "totp": false,
                "passkeys": 1,
                "required": false,
                "enrollment_required": false,
                "recovery_codes_remaining": 10,
            }),
            "{name}"
        );

        let (mut browser, signed_in) = passkey_sign_in(&app, &mut passkey, ORIGIN).await;
        assert_eq!(
            signed_in.status,
            StatusCode::OK,
            "{name}: {:?}",
            signed_in.body
        );
        assert_eq!(signed_in.body["status"], "signed_in");
        let account = browser.get("/v1/account").await;
        assert_eq!(account.body["user"]["email"], ADMIN_EMAIL, "{name}");
        assert_eq!(account.body["session"]["auth_method"], "passkey");
        let listed = browser.get("/v1/account/passkeys").await;
        assert!(listed.body[0]["last_used_at"].is_i64(), "{name}");

        // A prompt answers once.
        let options = browser.post("/v1/auth/passkey/options", json!({})).await;
        let credential = passkey.assert(&options.body["options"], ORIGIN);
        let request = json!({ "ceremony": options.body["ceremony"], "credential": credential });
        let first = app
            .browser()
            .post("/v1/auth/passkey", request.clone())
            .await;
        assert_eq!(first.status, StatusCode::OK, "{name}");
        let replay = app.browser().post("/v1/auth/passkey", request).await;
        assert_eq!(replay.code(), "ceremony_expired", "{name}");

        // Another site's page can't use it, nor can a cloned authenticator.
        let (_, phished) = passkey_sign_in(&app, &mut passkey, "https://evil.example").await;
        assert_eq!(phished.code(), "invalid_passkey", "{name}");
        passkey.set_counter(0);
        let (_, cloned) = passkey_sign_in(&app, &mut passkey, ORIGIN).await;
        assert_eq!(cloned.code(), "invalid_passkey", "{name}");
        passkey.set_counter(100);

        let events = common::audit_events(&app, "auth.sign_in").await;
        assert_eq!(events[0].metadata["method"], "passkey", "{name}");
        assert_eq!(
            common::audit_events(&app, "account.passkey_add")
                .await
                .len(),
            1
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_passkey_is_a_second_factor_after_a_password() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut passkey, _) = add_passkey(&mut admin, ADMIN_PASSWORD, "Key").await;

        let mut browser = app.browser();
        let password = browser
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(password.body["status"], "second_factor_required", "{name}");
        assert_eq!(
            password.body["methods"],
            json!(["passkey", "recovery_code"])
        );
        let allowed = &password.body["passkey"]["publicKey"]["allowCredentials"];
        assert_eq!(allowed.as_array().unwrap().len(), 1, "{name}");
        let challenge = password.body["challenge"].clone();

        let phished = passkey.assert(&password.body["passkey"], "https://evil.example");
        let refused = browser
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": challenge, "passkey": phished }),
            )
            .await;
        assert_eq!(refused.code(), "invalid_code", "{name}");
        // A tried prompt is spent, so its answer can't be replayed.
        let credential = passkey.assert(&password.body["passkey"], ORIGIN);
        let spent = browser
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": challenge, "passkey": credential }),
            )
            .await;
        assert_eq!(spent.code(), "challenge_expired", "{name}");

        let password = browser
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        let credential = passkey.assert(&password.body["passkey"], ORIGIN);
        let signed_in = browser
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": password.body["challenge"], "passkey": credential }),
            )
            .await;
        assert_eq!(
            signed_in.status,
            StatusCode::OK,
            "{name}: {:?}",
            signed_in.body
        );
        let account = browser.get("/v1/account").await;
        assert_eq!(account.body["session"]["auth_method"], "password");
        let events = common::audit_events(&app, "auth.sign_in").await;
        assert_eq!(events[0].metadata["second_factor"], "passkey", "{name}");

        // An authenticator app added later keeps the recovery codes.
        let started = browser
            .post(
                "/v1/account/two-factor/totp",
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        let secret = started.body["secret"].as_str().unwrap();
        let confirmed = browser
            .post(
                "/v1/account/two-factor/totp/confirm",
                json!({ "code": common::totp_code(secret, 0) }),
            )
            .await;
        assert_eq!(
            confirmed.status,
            StatusCode::OK,
            "{name}: {:?}",
            confirmed.body
        );
        assert_eq!(confirmed.body["recovery_codes"], Value::Null, "{name}");
        let password = app
            .browser()
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(
            password.body["methods"],
            json!(["totp", "passkey", "recovery_code"]),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn passkeys_are_managed_within_the_two_factor_policy() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (_, added) = add_passkey(&mut admin, ADMIN_PASSWORD, "Phone").await;
        let id = added["passkey"]["id"].as_str().unwrap().to_owned();

        let renamed = admin
            .patch(
                &format!("/v1/account/passkeys/{id}"),
                json!({ "name": " Work phone " }),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::NO_CONTENT, "{name}");
        let listed = admin.get("/v1/account/passkeys").await;
        assert_eq!(listed.body[0]["name"], "Work phone", "{name}");
        let blank = admin
            .patch(
                &format!("/v1/account/passkeys/{id}"),
                json!({ "name": " " }),
            )
            .await;
        assert_eq!(blank.status, StatusCode::BAD_REQUEST, "{name}");

        // With two-factor required, the last second factor stays.
        let required = admin
            .patch(
                "/v1/settings/authentication",
                json!({ "require_two_factor": true }),
            )
            .await;
        assert_eq!(required.status, StatusCode::OK, "{name}");
        let last = admin
            .post(
                &format!("/v1/account/passkeys/{id}/remove"),
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(last.code(), "two_factor_required", "{name}");
        admin
            .patch(
                "/v1/settings/authentication",
                json!({ "require_two_factor": false }),
            )
            .await;
        let wrong = admin
            .post(
                &format!("/v1/account/passkeys/{id}/remove"),
                json!({ "password": "nope" }),
            )
            .await;
        assert_eq!(wrong.code(), "incorrect_password", "{name}");
        let removed = admin
            .post(
                &format!("/v1/account/passkeys/{id}/remove"),
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT, "{name}");
        let account = admin.get("/v1/account").await;
        assert_eq!(account.body["two_factor"]["enabled"], false, "{name}");
        // The recovery codes went with the last second factor.
        assert_eq!(account.body["two_factor"]["recovery_codes_remaining"], 0);
        app.finish().await;
    }
}

#[tokio::test]
async fn a_passkey_meets_the_two_factor_requirement() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut user, user_id) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        add_passkey(&mut admin, ADMIN_PASSWORD, "Admin key").await;
        admin
            .patch(
                "/v1/settings/authentication",
                json!({ "require_two_factor": true }),
            )
            .await;
        let blocked = user.get("/v1/agents").await;
        assert_eq!(blocked.code(), "two_factor_enrollment_required", "{name}");
        let (mut passkey, _) = add_passkey(&mut user, "a long enough password", "Key").await;
        let allowed = user.get("/v1/agents").await;
        assert_eq!(allowed.status, StatusCode::OK, "{name}: {:?}", allowed.body);

        // Disabled users can't sign in with one.
        let disabled = admin
            .patch(&format!("/v1/users/{user_id}"), json!({ "disabled": true }))
            .await;
        assert_eq!(disabled.status, StatusCode::OK, "{name}");
        let (_, refused) = passkey_sign_in(&app, &mut passkey, ORIGIN).await;
        assert_eq!(refused.code(), "account_disabled", "{name}");
        admin
            .patch(
                &format!("/v1/users/{user_id}"),
                json!({ "disabled": false }),
            )
            .await;

        // An administrator's two-factor reset removes passkeys too.
        let view = admin.get(&format!("/v1/users/{user_id}")).await;
        assert_eq!(view.body["passkeys"], 1, "{name}");
        assert_eq!(view.body["two_factor_enabled"], true);
        let reset = admin
            .post(&format!("/v1/users/{user_id}/reset-two-factor"), json!({}))
            .await;
        assert_eq!(reset.status, StatusCode::NO_CONTENT, "{name}");
        let view = admin.get(&format!("/v1/users/{user_id}")).await;
        assert_eq!(view.body["passkeys"], 0, "{name}");
        let (_, gone) = passkey_sign_in(&app, &mut passkey, ORIGIN).await;
        assert_eq!(gone.code(), "invalid_passkey", "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn registration_prompts_belong_to_one_user_and_one_use() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut other, _) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        let options = admin
            .post(
                "/v1/account/passkeys/options",
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        let (_, credential) = SoftPasskey::register(&options.body["options"], ORIGIN);
        let request = json!({
            "ceremony": options.body["ceremony"],
            "name": "Stolen",
            "credential": credential,
        });
        let stolen = other.post("/v1/account/passkeys", request.clone()).await;
        assert_eq!(stolen.code(), "ceremony_expired", "{name}");
        // Taking it spent it.
        let spent = admin.post("/v1/account/passkeys", request).await;
        assert_eq!(spent.code(), "ceremony_expired", "{name}");

        let options = admin
            .post(
                "/v1/account/passkeys/options",
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        let (_, credential) =
            SoftPasskey::register(&options.body["options"], "https://evil.example");
        let phished = admin
            .post(
                "/v1/account/passkeys",
                json!({
                    "ceremony": options.body["ceremony"],
                    "name": "Phished",
                    "credential": credential,
                }),
            )
            .await;
        assert_eq!(phished.code(), "invalid_passkey", "{name}");

        // The same authenticator can't be registered twice.
        let options = admin
            .post(
                "/v1/account/passkeys/options",
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        let (passkey, credential) = SoftPasskey::register(&options.body["options"], ORIGIN);
        let first = admin
            .post(
                "/v1/account/passkeys",
                json!({ "ceremony": options.body["ceremony"], "name": "A", "credential": credential }),
            )
            .await;
        assert_eq!(first.status, StatusCode::CREATED, "{name}");
        let options = admin
            .post(
                "/v1/account/passkeys/options",
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        let excluded = &options.body["options"]["publicKey"]["excludeCredentials"];
        assert_eq!(excluded.as_array().unwrap().len(), 1, "{name}");
        let credential = passkey.registration(&options.body["options"], ORIGIN);
        let again = admin
            .post(
                "/v1/account/passkeys",
                json!({ "ceremony": options.body["ceremony"], "name": "B", "credential": credential }),
            )
            .await;
        assert_eq!(again.code(), "passkey_exists", "{name}: {:?}", again.body);
        app.finish().await;
    }
}
