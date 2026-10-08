//! First-run setup, signing in and out, sessions and request checks.
mod common;

use axum::http::{Method, Request, StatusCode};
use common::{ADMIN_EMAIL, ADMIN_PASSWORD, ORIGIN};
use meshrmm_server::db::tables::UserSessions;
use sea_query::{Expr, Func, Query};
use serde_json::json;

#[tokio::test]
async fn setup_creates_the_first_administrator_once() {
    for app in common::apps().await {
        let name = app.name;
        let mut browser = app.browser();
        let instance = browser.get("/v1/instance").await;
        assert_eq!(instance.status, StatusCode::OK);
        assert_eq!(instance.body["setup_required"], true, "{name}");
        assert_eq!(instance.body["sign_in"]["password_reset_email"], false);

        let link = meshrmm_server::announce_setup(&app.state)
            .await
            .unwrap()
            .unwrap();
        assert!(
            link.starts_with("https://rmm.example.com/setup#token="),
            "{link}"
        );
        let token = common::link_token(&link);
        let setup = |token: &str, password: &str| {
            json!({
                "token": token,
                "instance_name": " Acme IT ",
                "email": "Admin@Example.com",
                "display_name": "Ada Admin",
                "password": password,
            })
        };

        // Validation happens before the token is spent.
        let weak = browser.post("/v1/setup", setup(&token, "short")).await;
        assert_eq!(weak.status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(weak.code(), "weak_password");
        let wrong = browser
            .post("/v1/setup", setup("wrong", ADMIN_PASSWORD))
            .await;
        assert_eq!(wrong.status, StatusCode::FORBIDDEN);
        assert_eq!(wrong.code(), "invalid_token");

        let done = browser
            .post("/v1/setup", setup(&token, ADMIN_PASSWORD))
            .await;
        assert_eq!(done.status, StatusCode::CREATED, "{name}: {:?}", done.body);
        assert_eq!(done.body["status"], "signed_in");
        assert!(browser.cookie.is_some());
        let set_cookie = done.headers["set-cookie"].to_str().unwrap();
        assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("Secure"));

        let account = browser.get("/v1/account").await;
        assert_eq!(account.status, StatusCode::OK, "{name}");
        assert_eq!(account.body["user"]["email"], ADMIN_EMAIL);
        assert_eq!(account.body["is_administrator"], true);
        assert_eq!(account.body["roles"][0]["name"], "Administrator");
        assert_eq!(
            account.body["permissions"].as_array().unwrap().len(),
            meshrmm_server::rbac::Permission::ALL.len()
        );
        let instance = browser.get("/v1/instance").await;
        assert_eq!(instance.body["setup_required"], false);
        assert_eq!(instance.body["name"], "Acme IT");

        // Neither the same token nor a new one works once a user exists.
        let again = app
            .browser()
            .post("/v1/setup", setup(&token, ADMIN_PASSWORD))
            .await;
        assert_eq!(again.status, StatusCode::CONFLICT);
        assert_eq!(again.code(), "setup_complete");
        assert_eq!(
            meshrmm_server::announce_setup(&app.state).await.unwrap(),
            None
        );

        let audit = browser.get("/v1/audit?action=setup.complete").await;
        assert_eq!(
            audit.body["events"][0]["actor_label"], ADMIN_EMAIL,
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn signing_in_and_out() {
    for app in common::apps().await {
        let name = app.name;
        common::set_up(&app).await;
        let mut browser = app.browser();
        assert_eq!(browser.get("/v1/account").await.code(), "unauthenticated");

        let wrong = browser
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": "not the password" }),
            )
            .await;
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(wrong.code(), "invalid_credentials");
        let unknown = browser
            .post(
                "/v1/auth/sign-in",
                json!({ "email": "nobody@example.com", "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(unknown.code(), "invalid_credentials");

        let signed_in = browser
            .post(
                "/v1/auth/sign-in",
                json!({ "email": " ADMIN@example.com ", "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(
            signed_in.status,
            StatusCode::OK,
            "{name}: {:?}",
            signed_in.body
        );
        assert_eq!(
            signed_in.body,
            json!({ "status": "signed_in", "two_factor_enrollment_required": false })
        );
        assert_eq!(browser.get("/v1/account").await.status, StatusCode::OK);
        let session_cookie = browser.cookie.clone();

        let out = browser.post("/v1/auth/sign-out", json!({})).await;
        assert_eq!(out.status, StatusCode::NO_CONTENT);
        assert!(browser.cookie.is_none());
        // The old cookie no longer works anywhere.
        browser.cookie = session_cookie;
        assert_eq!(
            browser.get("/v1/account").await.status,
            StatusCode::UNAUTHORIZED
        );

        let mut admin = app.browser();
        admin
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        let audit = admin.get("/v1/audit?action=auth.").await;
        let actions = audit.body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["action"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            [
                "auth.sign_in",
                "auth.sign_out",
                "auth.sign_in",
                "auth.sign_in_failed"
            ],
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn repeated_failures_are_rate_limited() {
    let app = common::App::sqlite().await;
    common::set_up(&app).await;
    let mut browser = app.browser();
    let attempt = json!({ "email": ADMIN_EMAIL, "password": "wrong password" });
    for _ in 0..10 {
        assert_eq!(
            browser
                .post("/v1/auth/sign-in", attempt.clone())
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
    }
    let limited = browser.post("/v1/auth/sign-in", attempt).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.code(), "rate_limited");
    assert!(limited.headers.contains_key("retry-after"));
    // Even the right password waits.
    let right = browser
        .post(
            "/v1/auth/sign-in",
            json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
        )
        .await;
    assert_eq!(right.status, StatusCode::TOO_MANY_REQUESTS);
    app.finish().await;
}

#[tokio::test]
async fn parallel_guesses_share_the_limit() {
    let app = common::App::sqlite().await;
    common::set_up(&app).await;
    let attempts = (0..30).map(|_| {
        let mut browser = app.browser();
        tokio::spawn(async move {
            browser
                .post(
                    "/v1/auth/sign-in",
                    json!({ "email": ADMIN_EMAIL, "password": "parallel guess" }),
                )
                .await
                .status
        })
    });
    let statuses = futures_util::future::join_all(attempts).await;
    let guessed = statuses
        .iter()
        .filter(|status| *status.as_ref().unwrap() == StatusCode::UNAUTHORIZED)
        .count();
    assert_eq!(guessed, 10, "{statuses:?}");
    app.finish().await;
}

#[tokio::test]
async fn writes_need_the_website_origin_and_header() {
    let app = common::App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    let cookie = admin.cookie.clone().unwrap();
    let request = |origin: Option<&str>, header: bool| {
        let mut request = Request::builder()
            .method(Method::PATCH)
            .uri("/v1/account")
            .header("cookie", &cookie)
            .header("content-type", "application/json");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        if header {
            request = request.header("x-meshrmm-request", "1");
        }
        request
            .body(axum::body::Body::from(r#"{"display_name":"Mallory"}"#))
            .unwrap()
    };
    for (origin, header) in [
        (None, true),
        (Some(ORIGIN), false),
        (Some("https://evil.example"), true),
    ] {
        let response = admin.raw(request(origin, header)).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{origin:?} {header}"
        );
        assert_eq!(response.code(), "cross_site_request");
    }
    let allowed = admin.raw(request(Some(ORIGIN), true)).await;
    assert_eq!(allowed.status, StatusCode::NO_CONTENT);
    // Reads don't need either.
    let read = admin
        .raw(
            Request::get("/v1/account")
                .header("cookie", &cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(read.body["user"]["display_name"], "Mallory");
    assert_eq!(read.headers["cache-control"], "no-store");
    app.finish().await;
}

#[tokio::test]
async fn idle_and_expired_sessions_end() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        // Idle longer than the 240-minute default.
        app.db()
            .execute(
                &Query::update()
                    .table(UserSessions::Table)
                    .value(UserSessions::LastSeenAt, 0)
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(
            admin.get("/v1/account").await.status,
            StatusCode::UNAUTHORIZED,
            "{name}"
        );
        let (remaining,): (i64,) = app
            .db()
            .fetch_one(
                &Query::select()
                    .expr(Func::count(Expr::col(UserSessions::Id)))
                    .from(UserSessions::Table)
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(remaining, 0, "{name}: the idle session was not deleted");

        admin
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(admin.get("/v1/account").await.status, StatusCode::OK);
        app.db()
            .execute(
                &Query::update()
                    .table(UserSessions::Table)
                    .value(UserSessions::ExpiresAt, 1)
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(
            admin.get("/v1/account").await.status,
            StatusCode::UNAUTHORIZED
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn users_see_and_end_their_sessions() {
    for app in common::apps().await {
        let name = app.name;
        let mut first = common::set_up(&app).await;
        let mut second = app.browser();
        second
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        let sessions = first.get("/v1/account/sessions").await;
        let sessions = sessions.body.as_array().unwrap().clone();
        assert_eq!(sessions.len(), 2, "{name}");
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session["current"] == true)
                .count(),
            1
        );
        let other = sessions
            .iter()
            .find(|session| session["current"] == false)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            first
                .delete(&format!("/v1/account/sessions/{other}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            second.get("/v1/account").await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            first
                .delete(&format!("/v1/account/sessions/{other}"))
                .await
                .status,
            StatusCode::NOT_FOUND
        );

        // Changing the password signs out everywhere else.
        second
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        let wrong = first
            .post(
                "/v1/account/password",
                json!({ "current_password": "nope", "new_password": "a whole new password" }),
            )
            .await;
        assert_eq!(wrong.code(), "incorrect_password", "{name}");
        let changed = first
            .post(
                "/v1/account/password",
                json!({ "current_password": ADMIN_PASSWORD, "new_password": "a whole new password" }),
            )
            .await;
        assert_eq!(
            changed.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            changed.body
        );
        assert_eq!(first.get("/v1/account").await.status, StatusCode::OK);
        assert_eq!(
            second.get("/v1/account").await.status,
            StatusCode::UNAUTHORIZED
        );
        let old = app
            .browser()
            .post(
                "/v1/auth/sign-in",
                json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            )
            .await;
        assert_eq!(old.status, StatusCode::UNAUTHORIZED);
        app.finish().await;
    }
}
