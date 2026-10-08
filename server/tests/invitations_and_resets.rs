//! Invitations and password resets, as copyable links and by email, and the
//! admin CLI's repairs.
mod common;

use axum::http::StatusCode;
use common::{ADMIN_EMAIL, ADMIN_PASSWORD, App, Browser, FakeSmtp};
use serde_json::json;

async fn configure_smtp(admin: &mut Browser, smtp: &FakeSmtp) {
    let response = admin
        .put(
            "/v1/settings/smtp",
            json!({
                "host": "127.0.0.1",
                "port": smtp.port,
                "security": "none",
                "from": "Acme IT <rmm@example.com>",
            }),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
    assert_eq!(response.body["configured"], true);
}

#[tokio::test]
async fn invitations_without_email_are_links_to_pass_on() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let invited = admin
            .post(
                "/v1/invitations",
                json!({ "email": "Tess@Example.com", "role_ids": ["technician"] }),
            )
            .await;
        assert_eq!(
            invited.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            invited.body
        );
        assert_eq!(invited.body["emailed"], false);
        assert_eq!(invited.body["invitation"]["email"], "tess@example.com");
        let link = invited.body["link"].as_str().unwrap();
        assert!(
            link.starts_with("https://rmm.example.com/invite#token="),
            "{link}"
        );
        let invitation_id = invited.body["invitation"]["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let duplicate = admin
            .post(
                "/v1/invitations",
                json!({ "email": "tess@example.com", "role_ids": [] }),
            )
            .await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT, "{name}");
        let existing = admin
            .post(
                "/v1/invitations",
                json!({ "email": ADMIN_EMAIL, "role_ids": [] }),
            )
            .await;
        assert_eq!(existing.status, StatusCode::CONFLICT);
        let unknown_role = admin
            .post(
                "/v1/invitations",
                json!({ "email": "x@example.com", "role_ids": ["no-such-role"] }),
            )
            .await;
        assert_eq!(unknown_role.status, StatusCode::BAD_REQUEST);
        let pending = admin.get("/v1/invitations").await;
        assert_eq!(pending.body.as_array().unwrap().len(), 1, "{name}");
        assert_eq!(pending.body[0]["roles"][0]["id"], "technician");

        // Renewing replaces the link.
        let renewed = admin
            .post(&format!("/v1/invitations/{invitation_id}/renew"), json!({}))
            .await;
        assert_eq!(renewed.status, StatusCode::OK, "{name}: {:?}", renewed.body);
        let old_token = common::link_token(link);
        let token = common::link_token(renewed.body["link"].as_str().unwrap());
        let mut tess = app.browser();
        let old = tess
            .post("/v1/auth/invitation", json!({ "token": old_token }))
            .await;
        assert_eq!(old.code(), "invalid_token");
        let details = tess
            .post("/v1/auth/invitation", json!({ "token": token }))
            .await;
        assert_eq!(details.body["email"], "tess@example.com", "{name}");
        assert_eq!(details.body["instance_name"], "Acme IT");

        let weak = tess
            .post(
                "/v1/auth/invitation/accept",
                json!({ "token": token, "display_name": "Tess", "password": "short" }),
            )
            .await;
        assert_eq!(weak.code(), "weak_password");
        let accepted = tess
            .post(
                "/v1/auth/invitation/accept",
                json!({ "token": token, "display_name": "Tess", "password": "technician password" }),
            )
            .await;
        assert_eq!(
            accepted.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            accepted.body
        );
        let account = tess.get("/v1/account").await;
        assert_eq!(account.body["roles"][0]["id"], "technician");
        assert!(
            account.body["permissions"]
                .as_array()
                .unwrap()
                .contains(&json!("sessions.connect"))
        );
        let twice = app
            .browser()
            .post(
                "/v1/auth/invitation/accept",
                json!({ "token": token, "display_name": "Eve", "password": "technician password" }),
            )
            .await;
        assert_eq!(twice.code(), "invalid_token", "{name}");
        assert!(
            admin
                .get("/v1/invitations")
                .await
                .body
                .as_array()
                .unwrap()
                .is_empty()
        );

        // An expired invitation stays listed, and renewing it revives it.
        let late = admin
            .post(
                "/v1/invitations",
                json!({ "email": "late@example.com", "role_ids": [] }),
            )
            .await;
        let late_id = late.body["invitation"]["id"].as_str().unwrap().to_owned();
        let late_token = common::link_token(late.body["link"].as_str().unwrap());
        app.db()
            .execute(
                &sea_query::Query::update()
                    .table(meshrmm_server::db::tables::Invitations::Table)
                    .value(meshrmm_server::db::tables::Invitations::ExpiresAt, 1)
                    .to_owned(),
            )
            .await
            .unwrap();
        let expired = app
            .browser()
            .post("/v1/auth/invitation", json!({ "token": late_token }))
            .await;
        assert_eq!(expired.code(), "invalid_token", "{name}");
        let listed = admin.get("/v1/invitations").await;
        assert_eq!(listed.body[0]["expired"], true, "{name}");
        let renewed = admin
            .post(&format!("/v1/invitations/{late_id}/renew"), json!({}))
            .await;
        assert_eq!(renewed.status, StatusCode::OK, "{name}: {:?}", renewed.body);
        assert_eq!(renewed.body["invitation"]["expired"], false);
        let revived = app
            .browser()
            .post(
                "/v1/auth/invitation",
                json!({ "token": common::link_token(renewed.body["link"].as_str().unwrap()) }),
            )
            .await;
        assert_eq!(revived.body["email"], "late@example.com");

        // A revoked invitation's link stops working.
        let revoked = admin
            .post(
                "/v1/invitations",
                json!({ "email": "gone@example.com", "role_ids": [] }),
            )
            .await;
        let revoked_token = common::link_token(revoked.body["link"].as_str().unwrap());
        let revoked_id = revoked.body["invitation"]["id"].as_str().unwrap();
        assert_eq!(
            admin
                .delete(&format!("/v1/invitations/{revoked_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let gone = app
            .browser()
            .post("/v1/auth/invitation", json!({ "token": revoked_token }))
            .await;
        assert_eq!(gone.code(), "invalid_token", "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn administrators_make_password_reset_links() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let invited = admin
            .post(
                "/v1/invitations",
                json!({ "email": "tess@example.com", "role_ids": ["technician"] }),
            )
            .await;
        let token = common::link_token(invited.body["link"].as_str().unwrap());
        let mut tess = app.browser();
        tess.post(
            "/v1/auth/invitation/accept",
            json!({ "token": token, "display_name": "Tess", "password": "the first password" }),
        )
        .await;
        let tess_id = tess.get("/v1/account").await.body["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let reset = admin
            .post(&format!("/v1/users/{tess_id}/password-reset"), json!({}))
            .await;
        assert_eq!(reset.status, StatusCode::OK, "{name}: {:?}", reset.body);
        assert_eq!(reset.body["emailed"], false);
        let link = reset.body["link"].as_str().unwrap();
        assert!(
            link.starts_with("https://rmm.example.com/reset#token="),
            "{link}"
        );
        let token = common::link_token(link);

        let mut browser = app.browser();
        let lookup = browser
            .post("/v1/auth/password-reset/lookup", json!({ "token": token }))
            .await;
        assert_eq!(lookup.body["email"], "tess@example.com", "{name}");
        let done = browser
            .post(
                "/v1/auth/password-reset/complete",
                json!({ "token": token, "password": "the second password" }),
            )
            .await;
        assert_eq!(
            done.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            done.body
        );
        // It signs the user out and doesn't sign the browser in.
        assert!(browser.cookie.is_none());
        assert_eq!(
            tess.get("/v1/account").await.status,
            StatusCode::UNAUTHORIZED
        );
        let reused = browser
            .post(
                "/v1/auth/password-reset/complete",
                json!({ "token": token, "password": "the third password" }),
            )
            .await;
        assert_eq!(reused.code(), "invalid_token");
        for (password, status) in [
            ("the first password", StatusCode::UNAUTHORIZED),
            ("the second password", StatusCode::OK),
        ] {
            let response = app
                .browser()
                .post(
                    "/v1/auth/sign-in",
                    json!({ "email": "tess@example.com", "password": password }),
                )
                .await;
            assert_eq!(response.status, status, "{name}: {password}");
        }
        app.finish().await;
    }
}

#[tokio::test]
async fn with_email_set_up_links_are_emailed() {
    let app = App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    let mut smtp = FakeSmtp::start().await;
    configure_smtp(&mut admin, &smtp).await;
    let instance = app.browser().get("/v1/instance").await;
    assert_eq!(instance.body["sign_in"]["password_reset_email"], true);

    let test = admin.post("/v1/settings/smtp/test", json!({})).await;
    assert_eq!(test.status, StatusCode::NO_CONTENT, "{:?}", test.body);
    let message = smtp.next().await;
    assert!(message.contains("To: admin@example.com"), "{message}");
    assert!(message.contains("Email is working"), "{message}");

    let invited = admin
        .post(
            "/v1/invitations",
            json!({ "email": "tess@example.com", "role_ids": ["technician"] }),
        )
        .await;
    assert_eq!(invited.body["emailed"], true, "{:?}", invited.body);
    assert!(
        invited.body.get("link").is_none(),
        "an emailed link was also returned"
    );
    let message = smtp.next().await;
    assert!(
        message.contains("Subject: You're invited to Acme IT"),
        "{message}"
    );
    let token = common::link_token(&common::link_in(&message));
    let mut tess = app.browser();
    let accepted = tess
        .post(
            "/v1/auth/invitation/accept",
            json!({ "token": token, "display_name": "Tess", "password": "technician password" }),
        )
        .await;
    assert_eq!(accepted.status, StatusCode::CREATED);

    // Forgotten passwords: the answer never says whether the account exists.
    for email in ["tess@example.com", "nobody@example.com", "not an email"] {
        let requested = app
            .browser()
            .post("/v1/auth/password-reset", json!({ "email": email }))
            .await;
        assert_eq!(requested.status, StatusCode::ACCEPTED, "{email}");
    }
    let message = smtp.next().await;
    assert!(message.contains("To: tess@example.com"), "{message}");
    assert!(message.contains("expires in 1 hour"), "{message}");
    let token = common::link_token(&common::link_in(&message));
    let done = app
        .browser()
        .post(
            "/v1/auth/password-reset/complete",
            json!({ "token": token, "password": "a brand new password" }),
        )
        .await;
    assert_eq!(done.status, StatusCode::NO_CONTENT, "{:?}", done.body);
    // Nothing was sent for the unknown address.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), smtp.messages.recv())
            .await
            .is_err()
    );

    // Email that fails falls back to the link, with a warning.
    let broken = admin
        .put(
            "/v1/settings/smtp",
            json!({ "host": "127.0.0.1", "port": 1, "security": "none", "from": "rmm@example.com" }),
        )
        .await;
    assert_eq!(broken.status, StatusCode::OK);
    let fallback = admin
        .post(
            "/v1/invitations",
            json!({ "email": "later@example.com", "role_ids": [] }),
        )
        .await;
    assert_eq!(fallback.body["emailed"], false);
    assert!(fallback.body["link"].as_str().is_some());
    assert!(fallback.body["email_error"].as_str().is_some());
    let failed_test = admin.post("/v1/settings/smtp/test", json!({})).await;
    assert_eq!(failed_test.status, StatusCode::BAD_GATEWAY);
    assert_eq!(failed_test.code(), "email_failed");

    assert_eq!(
        admin.delete("/v1/settings/smtp").await.status,
        StatusCode::NO_CONTENT
    );
    let smtp_settings = admin.get("/v1/settings/smtp").await;
    assert_eq!(smtp_settings.body["configured"], false);
    app.finish().await;
}

#[tokio::test]
async fn the_smtp_password_is_stored_encrypted_and_never_returned() {
    let app = App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    let saved = admin
        .put(
            "/v1/settings/smtp",
            json!({
                "host": "smtp.example.com",
                "security": "starttls",
                "username": "rmm",
                "password": "smtp secret",
                "from": "rmm@example.com",
            }),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
    assert_eq!(saved.body["has_password"], true);
    assert!(!saved.body.to_string().contains("smtp secret"));
    let settings = meshrmm_server::settings::load(&mut app.db()).await.unwrap();
    let sealed = settings.smtp_password_encrypted.clone().unwrap();
    assert!(!String::from_utf8_lossy(&sealed).contains("smtp secret"));
    assert_eq!(
        app.state
            .instance_key
            .decrypt(meshrmm_server::mail::SMTP_PASSWORD_CONTEXT, &sealed)
            .unwrap(),
        b"smtp secret"
    );

    // Leaving the password out keeps it; null removes it.
    let kept = admin
        .put(
            "/v1/settings/smtp",
            json!({ "host": "smtp.example.com", "security": "tls", "username": "rmm", "from": "rmm@example.com" }),
        )
        .await;
    assert_eq!(kept.body["has_password"], true);
    let removed = admin
        .put(
            "/v1/settings/smtp",
            json!({ "host": "smtp.example.com", "security": "tls", "password": null, "from": "rmm@example.com" }),
        )
        .await;
    assert_eq!(removed.body["has_password"], false);
    for invalid in [
        json!({ "host": "", "security": "tls", "from": "rmm@example.com" }),
        json!({ "host": "smtp.example.com", "security": "ssl", "from": "rmm@example.com" }),
        json!({ "host": "smtp.example.com", "security": "tls", "from": "not an address" }),
    ] {
        let response = admin.put("/v1/settings/smtp", invalid.clone()).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{invalid}");
    }
    app.finish().await;
}

#[tokio::test]
async fn the_admin_cli_repairs_locked_out_accounts() {
    for app in common::apps().await {
        let name = app.name;
        // On a fresh install it creates the first administrator.
        let printed = meshrmm_server::admin::create_user(&app.state, ADMIN_EMAIL, "Ada", &[])
            .await
            .unwrap();
        assert!(
            printed.contains("https://rmm.example.com/reset#token="),
            "{printed}"
        );
        assert_eq!(
            meshrmm_server::announce_setup(&app.state).await.unwrap(),
            None
        );
        let token = common::link_token(common::link_in(&printed).as_str());
        let set = app
            .browser()
            .post(
                "/v1/auth/password-reset/complete",
                json!({ "token": token, "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(set.status, StatusCode::NO_CONTENT, "{name}: {:?}", set.body);
        let mut admin = app.browser();
        admin
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        let account = admin.get("/v1/account").await;
        assert_eq!(account.body["is_administrator"], true, "{name}");

        assert!(
            meshrmm_server::admin::create_user(&app.state, ADMIN_EMAIL, "Ada", &[])
                .await
                .is_err()
        );
        let tech = meshrmm_server::admin::create_user(
            &app.state,
            "tech@example.com",
            "Tess",
            &["Technician".to_owned()],
        )
        .await
        .unwrap();
        assert!(tech.contains("tech@example.com"), "{tech}");
        assert!(
            meshrmm_server::admin::create_user(
                &app.state,
                "x@example.com",
                "X",
                &["nope".to_owned()]
            )
            .await
            .is_err()
        );

        // A forgotten password, and a lost authenticator.
        let started = admin
            .post(
                "/v1/account/two-factor/totp",
                json!({ "password": ADMIN_PASSWORD }),
            )
            .await;
        let secret = started.body["secret"].as_str().unwrap();
        admin
            .post(
                "/v1/account/two-factor/totp/confirm",
                json!({ "code": common::totp_code(secret, 0) }),
            )
            .await;
        let printed = meshrmm_server::admin::reset_password(&app.state, ADMIN_EMAIL)
            .await
            .unwrap();
        let token = common::link_token(&common::link_in(&printed));
        app.browser()
            .post(
                "/v1/auth/password-reset/complete",
                json!({ "token": token, "password": "a recovered password" }),
            )
            .await;
        meshrmm_server::admin::reset_two_factor(&app.state, ADMIN_EMAIL)
            .await
            .unwrap();
        let signed_in = app
            .browser()
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": "a recovered password" }),
            )
            .await;
        assert_eq!(
            signed_in.body["status"], "signed_in",
            "{name}: {:?}",
            signed_in.body
        );
        assert!(
            meshrmm_server::admin::reset_password(&app.state, "nobody@example.com")
                .await
                .is_err()
        );
        let audit = admin.get("/v1/audit").await;
        assert_eq!(
            audit.status,
            StatusCode::UNAUTHORIZED,
            "the reset signed everyone out"
        );
        app.finish().await;
    }
}
