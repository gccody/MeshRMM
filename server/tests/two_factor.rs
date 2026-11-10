//! Authenticator apps, recovery codes, and requiring two-factor sign-in.
mod common;

use axum::http::StatusCode;
use common::{ADMIN_EMAIL, ADMIN_PASSWORD, Browser};
use serde_json::{Value, json};

/// Turns on TOTP for the browser's user. Returns the secret, the recovery
/// codes, and the code that confirmed the app.
async fn enroll(browser: &mut Browser, password: &str) -> (String, Vec<String>, String) {
    let started = browser
        .post(
            "/v1/account/two-factor/totp",
            json!({ "password": password }),
        )
        .await;
    assert_eq!(started.status, StatusCode::OK, "{:?}", started.body);
    let secret = started.body["secret"].as_str().unwrap().to_owned();
    assert!(
        started.body["otpauth_uri"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/Acme%20IT:"),
        "{:?}",
        started.body
    );
    let code = common::totp_code(&secret, 0);
    let confirmed = browser
        .post(
            "/v1/account/two-factor/totp/confirm",
            json!({ "code": code }),
        )
        .await;
    assert_eq!(confirmed.status, StatusCode::OK, "{:?}", confirmed.body);
    let codes = confirmed.body["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|code| code.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(codes.len(), 10);
    (secret, codes, code)
}

async fn password_step(browser: &mut Browser, email: &str, password: &str) -> String {
    let response = browser
        .post(
            "/v1/auth/sign-in",
            json!({ "email": email, "password": password }),
        )
        .await;
    assert_eq!(
        response.body["status"], "second_factor_required",
        "{:?}",
        response.body
    );
    assert_eq!(response.body["methods"], json!(["totp", "recovery_code"]));
    assert!(browser.cookie.is_none());
    assert_eq!(common::device_cookie(&response), None);
    response.body["challenge"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn authenticator_codes_and_recovery_codes_sign_in() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let wrong = admin
            .post("/v1/account/two-factor/totp", json!({ "password": "nope" }))
            .await;
        assert_eq!(wrong.code(), "incorrect_password", "{name}");
        let (secret, codes, used) = enroll(&mut admin, ADMIN_PASSWORD).await;
        let account = admin.get("/v1/account").await;
        assert_eq!(account.body["two_factor"]["enabled"], true, "{name}");
        assert_eq!(account.body["two_factor"]["recovery_codes_remaining"], 10);

        let mut browser = app.browser();
        let challenge = password_step(&mut browser, ADMIN_EMAIL, ADMIN_PASSWORD).await;
        // The code that confirmed the app was used up, and earlier ones too.
        for code in [used, common::totp_code(&secret, -2)] {
            let replay = browser
                .post(
                    "/v1/auth/sign-in/second-factor",
                    json!({ "challenge": challenge, "code": code }),
                )
                .await;
            assert_eq!(replay.code(), "invalid_code", "{name}");
        }
        let signed_in = browser
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": challenge, "code": common::totp_code(&secret, 1) }),
            )
            .await;
        assert_eq!(
            signed_in.status,
            StatusCode::OK,
            "{name}: {:?}",
            signed_in.body
        );
        assert_eq!(signed_in.body["status"], "signed_in");
        assert_eq!(browser.get("/v1/account").await.status, StatusCode::OK);
        // A finished challenge can't be used again.
        let reused = app
            .browser()
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": challenge, "code": common::totp_code(&secret, 1) }),
            )
            .await;
        assert_eq!(reused.code(), "challenge_expired");

        // A recovery code works once, in any case or spacing.
        let mut browser = app.browser();
        let challenge = password_step(&mut browser, ADMIN_EMAIL, ADMIN_PASSWORD).await;
        let typed = format!(" {} ", codes[0].to_uppercase());
        let recovered = browser
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": challenge, "recovery_code": typed }),
            )
            .await;
        assert_eq!(
            recovered.status,
            StatusCode::OK,
            "{name}: {:?}",
            recovered.body
        );
        let mut browser = app.browser();
        let challenge = password_step(&mut browser, ADMIN_EMAIL, ADMIN_PASSWORD).await;
        let again = browser
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": challenge, "recovery_code": codes[0] }),
            )
            .await;
        assert_eq!(again.code(), "invalid_code");
        let account = admin.get("/v1/account").await;
        assert_eq!(account.body["two_factor"]["recovery_codes_remaining"], 9);

        // New codes replace the old ones.
        let replaced = admin
            .post(
                "/v1/account/two-factor/recovery-codes",
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        let new_codes = replaced.body["recovery_codes"].as_array().unwrap().clone();
        assert_eq!(new_codes.len(), 10, "{name}");
        let used = browser
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": challenge, "recovery_code": codes[1] }),
            )
            .await;
        assert_eq!(used.code(), "invalid_code");
        let new_code = browser
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": challenge, "recovery_code": new_codes[0] }),
            )
            .await;
        assert_eq!(new_code.status, StatusCode::OK);

        // Turning it off brings back password-only sign-in.
        let off = admin
            .post(
                "/v1/account/two-factor/totp/disable",
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(off.status, StatusCode::NO_CONTENT, "{name}: {:?}", off.body);
        let plain = app
            .browser()
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(plain.body["status"], "signed_in", "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn wrong_codes_are_limited_per_challenge_and_per_user() {
    let app = common::App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    let (secret, _, _) = enroll(&mut admin, ADMIN_PASSWORD).await;
    let mut browser = app.browser();
    let challenge = password_step(&mut browser, ADMIN_EMAIL, ADMIN_PASSWORD).await;
    for _ in 0..5 {
        let wrong = browser
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": challenge, "code": "000000" }),
            )
            .await;
        assert_eq!(wrong.code(), "invalid_code");
    }
    let right = browser
        .post(
            "/v1/auth/sign-in/second-factor",
            json!({ "challenge": challenge, "code": common::totp_code(&secret, 1) }),
        )
        .await;
    assert_eq!(right.status, StatusCode::UNAUTHORIZED);
    assert_eq!(right.code(), "challenge_expired");

    // A fresh challenge doesn't reset the count: wrong codes count against
    // the user, from any address.
    let challenge = password_step(&mut browser, ADMIN_EMAIL, ADMIN_PASSWORD).await;
    for _ in 0..5 {
        let wrong = browser
            .post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": challenge, "code": "000000" }),
            )
            .await;
        assert_eq!(wrong.code(), "invalid_code");
    }
    let mut elsewhere = app.browser();
    let response = elsewhere
        .post_from(
            "203.0.113.20",
            "/v1/auth/sign-in",
            json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
        )
        .await;
    let challenge = response.body["challenge"].as_str().unwrap();
    let limited = elsewhere
        .post(
            "/v1/auth/sign-in/second-factor",
            json!({ "challenge": challenge, "code": common::totp_code(&secret, 1) }),
        )
        .await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    app.finish().await;
}

/// A code is used up in the transaction that starts its session: if that
/// fails, the code still works, and two sign-ins with one code at once
/// can't both get in.
#[tokio::test]
async fn a_code_is_used_up_only_by_the_sign_in_it_completes() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (secret, codes, _) = enroll(&mut admin, ADMIN_PASSWORD).await;
        // Computed once, so a later time step can't stand in for a code the
        // failed attempt used up.
        let totp = common::totp_code(&secret, 1);
        for (field, code) in [("code", &totp), ("recovery_code", &codes[0])] {
            let mut browser = app.browser();
            let challenge = password_step(&mut browser, ADMIN_EMAIL, ADMIN_PASSWORD).await;
            let mut body = json!({ "challenge": challenge });
            body[field] = json!(code);
            common::refuse_writes(&app, "INSERT", "user_sessions", true).await;
            let failed = browser
                .post("/v1/auth/sign-in/second-factor", body.clone())
                .await;
            common::refuse_writes(&app, "INSERT", "user_sessions", false).await;
            assert_eq!(
                failed.status,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{name}: {field}: {:?}",
                failed.body
            );
            assert!(browser.cookie.is_none(), "{name}: {field}");
            let signed_in = browser
                .post("/v1/auth/sign-in/second-factor", body.clone())
                .await;
            assert_eq!(
                signed_in.status,
                StatusCode::OK,
                "{name}: {field}: {:?}",
                signed_in.body
            );
            assert_eq!(browser.get("/v1/account").await.status, StatusCode::OK);
            // Now it is used up.
            let mut again = app.browser();
            let challenge = password_step(&mut again, ADMIN_EMAIL, ADMIN_PASSWORD).await;
            let mut body = json!({ "challenge": challenge });
            body[field] = json!(code);
            let replay = again.post("/v1/auth/sign-in/second-factor", body).await;
            assert_eq!(replay.code(), "invalid_code", "{name}: {field}");
        }
        let remaining = |account: Value| account["two_factor"]["recovery_codes_remaining"].clone();
        assert_eq!(remaining(admin.get("/v1/account").await.body), 9, "{name}");

        // Two sign-ins with the same code at once: one gets in.
        let mut first = app.browser();
        let mut second = app.browser();
        let first_challenge = password_step(&mut first, ADMIN_EMAIL, ADMIN_PASSWORD).await;
        let second_challenge = password_step(&mut second, ADMIN_EMAIL, ADMIN_PASSWORD).await;
        let (a, b) = tokio::join!(
            first.post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": first_challenge, "recovery_code": codes[1] }),
            ),
            second.post(
                "/v1/auth/sign-in/second-factor",
                json!({ "challenge": second_challenge, "recovery_code": codes[1] }),
            ),
        );
        let mut outcomes = [
            (a.status, a.code().to_owned()),
            (b.status, b.code().to_owned()),
        ];
        outcomes.sort_by_key(|(status, _)| status.as_u16());
        assert_eq!(
            outcomes,
            [
                (StatusCode::OK, String::new()),
                (StatusCode::UNAUTHORIZED, "invalid_code".to_owned()),
            ],
            "{name}"
        );
        assert_eq!(remaining(admin.get("/v1/account").await.body), 8, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn a_strangers_wrong_passwords_dont_block_the_second_factor() {
    let app = common::App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    let (secret, _, _) = enroll(&mut admin, ADMIN_PASSWORD).await;
    let mut stranger = app.browser();
    for _ in 0..10 {
        let guess = stranger
            .post_from(
                "198.51.100.7",
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": "a guess" }),
            )
            .await;
        assert_eq!(guess.status, StatusCode::UNAUTHORIZED);
    }
    let mut owner = app.browser();
    let challenge = password_step(&mut owner, ADMIN_EMAIL, ADMIN_PASSWORD).await;
    let signed_in = owner
        .post(
            "/v1/auth/sign-in/second-factor",
            json!({ "challenge": challenge, "code": common::totp_code(&secret, 1) }),
        )
        .await;
    assert_eq!(signed_in.status, StatusCode::OK, "{:?}", signed_in.body);
    // The device cookie comes with the whole sign-in, not the password.
    assert!(common::device_cookie(&signed_in).is_some());
    app.finish().await;
}

#[tokio::test]
async fn required_two_factor_confines_new_users_to_enrollment() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let required = admin
            .patch(
                "/v1/settings/authentication",
                json!({ "require_two_factor": true }),
            )
            .await;
        assert_eq!(
            required.status,
            StatusCode::OK,
            "{name}: {:?}",
            required.body
        );
        // The administrator has no second factor yet either.
        let blocked = admin.get("/v1/users").await;
        assert_eq!(blocked.status, StatusCode::FORBIDDEN, "{name}");
        assert_eq!(blocked.code(), "two_factor_enrollment_required");
        let account = admin.get("/v1/account").await;
        assert_eq!(account.body["two_factor"]["enrollment_required"], true);
        enroll(&mut admin, ADMIN_PASSWORD).await;
        assert_eq!(
            admin.get("/v1/users").await.status,
            StatusCode::OK,
            "{name}"
        );
        let off = admin
            .post(
                "/v1/account/two-factor/totp/disable",
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(off.code(), "two_factor_required");

        let invited = admin
            .post(
                "/v1/invitations",
                json!({ "email": "tech@example.com", "role_ids": ["technician"] }),
            )
            .await;
        let token = common::link_token(invited.body["link"].as_str().unwrap());
        let mut tech = app.browser();
        let accepted = tech
            .post(
                "/v1/auth/invitation/accept",
                json!({ "token": token, "display_name": "Tess", "password": "technician password" }),
            )
            .await;
        assert_eq!(
            accepted.body,
            json!({ "status": "signed_in", "two_factor_enrollment_required": true }),
            "{name}"
        );
        assert_eq!(
            tech.get("/v1/permissions").await.code(),
            "two_factor_enrollment_required"
        );
        enroll(&mut tech, "technician password").await;
        assert_eq!(tech.get("/v1/permissions").await.status, StatusCode::OK);
        let account: Value = tech.get("/v1/account").await.body;
        assert_eq!(
            account["two_factor"]["enrollment_required"], false,
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn an_authenticator_app_shows_on_the_next_request() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let two_factor = async |admin: &mut Browser| -> Value {
            admin.get("/v1/account").await.body["two_factor"].clone()
        };
        let before = two_factor(&mut admin).await;
        assert_eq!(
            (&before["enabled"], &before["totp"]),
            (&json!(false), &json!(false)),
            "{name}"
        );
        // Asked twice, so the account is kept when the app is added.
        two_factor(&mut admin).await;
        enroll(&mut admin, ADMIN_PASSWORD).await;
        let added = two_factor(&mut admin).await;
        assert_eq!(
            (&added["enabled"], &added["totp"]),
            (&json!(true), &json!(true)),
            "{name}"
        );
        let off = admin
            .post(
                "/v1/account/two-factor/totp/disable",
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(off.status, StatusCode::NO_CONTENT, "{name}: {:?}", off.body);
        let removed = two_factor(&mut admin).await;
        assert_eq!(
            (&removed["enabled"], &removed["totp"]),
            (&json!(false), &json!(false)),
            "{name}"
        );
        app.finish().await;
    }
}
