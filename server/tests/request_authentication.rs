//! What authenticating a request costs, and that every change to a user's
//! access applies on the very next request even though authentication keeps
//! what it knows about recently active users.
mod common;

use axum::http::{Method, StatusCode};
use common::{App, Browser, Response};
use meshrmm_server::db::Counts;
use sea_query::{Expr, ExprTrait, Query};
use serde_json::{Value, json};

/// A request the way a script sends one, with `token` and nothing else.
async fn with_token(app: &App, token: &str, path: &str) -> Response {
    let request = axum::http::Request::builder()
        .method(Method::GET)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap();
    app.browser().raw(request).await
}

/// Makes an API token as `browser`'s user. Returns its ID and secret.
async fn make_token(browser: &mut Browser) -> (String, String) {
    let created = browser
        .post(
            "/v1/api-tokens",
            json!({ "name": "Script", "expires_in_days": 30 }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    (
        created.body["id"].as_str().unwrap().to_owned(),
        created.body["secret"].as_str().unwrap().to_owned(),
    )
}

/// What sending `request` ran, and its response.
async fn measured<F: Future<Output = Response>>(app: &App, request: F) -> (Counts, Response) {
    let before = app.db().counts();
    let response = request.await;
    (app.db().counts() - before, response)
}

/// One statement, the session's or the token's row, and nothing written.
const WARM: Counts = Counts {
    statements: 1,
    writes: 0,
    transactions: 0,
};

#[tokio::test]
async fn a_warm_request_reads_only_its_session_or_token() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (_, secret) = make_token(&mut admin).await;

        // The first request after an access change reads the user.
        let (cold, response) = measured(&app, admin.get("/v1/permissions")).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{name}: {:?}",
            response.body
        );
        assert!(cold.statements > 1, "{name}: {cold:?}");
        let (warm, response) = measured(&app, admin.get("/v1/permissions")).await;
        assert_eq!(response.status, StatusCode::OK, "{name}");
        assert_eq!(warm, WARM, "{name}: a session");

        // The token's first use records it; later ones within a minute don't.
        let response = with_token(&app, &secret, "/v1/permissions").await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{name}: {:?}",
            response.body
        );
        let (warm, response) = measured(&app, with_token(&app, &secret, "/v1/permissions")).await;
        assert_eq!(response.status, StatusCode::OK, "{name}");
        assert_eq!(warm, WARM, "{name}: a token");

        // Any access change makes the next request read the user again.
        app.state.presence.recheck_access();
        let (cold, _) = measured(&app, admin.get("/v1/permissions")).await;
        assert!(cold.statements > 1, "{name}: {cold:?}");
        let (warm, _) = measured(&app, admin.get("/v1/permissions")).await;
        assert_eq!(warm, WARM, "{name}");
        app.finish().await;
    }
}

/// `path` as `browser` and as `token` both answer `status`.
async fn both(app: &App, browser: &mut Browser, token: &str, path: &str, status: StatusCode) {
    let name = app.name;
    let response = browser.get(path).await;
    assert_eq!(
        response.status, status,
        "{name}: session: {:?}",
        response.body
    );
    let response = with_token(app, token, path).await;
    assert_eq!(
        response.status, status,
        "{name}: token: {:?}",
        response.body
    );
}

#[tokio::test]
async fn permission_and_role_changes_apply_to_the_next_request() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let role = common::add_role(&mut admin, "Viewers", &["devices.view"]).await;
        let (mut tech, tech_id) =
            common::add_user(&app, &mut admin, "tech@example.com", &[&role]).await;
        let (_, token) = make_token(&mut tech).await;
        both(&app, &mut tech, &token, "/v1/agents", StatusCode::OK).await;
        both(&app, &mut tech, &token, "/v1/agents", StatusCode::OK).await;

        // The role loses the permission.
        let changed = admin
            .patch(&format!("/v1/roles/{role}"), json!({ "permissions": [] }))
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{name}: {:?}", changed.body);
        both(&app, &mut tech, &token, "/v1/agents", StatusCode::FORBIDDEN).await;
        admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "permissions": ["devices.view"] }),
            )
            .await;
        both(&app, &mut tech, &token, "/v1/agents", StatusCode::OK).await;

        // The user is taken out of the role.
        let changed = admin
            .patch(&format!("/v1/users/{tech_id}"), json!({ "role_ids": [] }))
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{name}: {:?}", changed.body);
        both(&app, &mut tech, &token, "/v1/agents", StatusCode::FORBIDDEN).await;
        admin
            .patch(
                &format!("/v1/users/{tech_id}"),
                json!({ "role_ids": [role] }),
            )
            .await;
        both(&app, &mut tech, &token, "/v1/agents", StatusCode::OK).await;

        // A renamed user is shown renamed.
        admin
            .patch(
                &format!("/v1/users/{tech_id}"),
                json!({ "display_name": "Tess Tech" }),
            )
            .await;
        let account = tech.get("/v1/account").await;
        assert_eq!(account.body["user"]["display_name"], "Tess Tech", "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn ending_access_applies_to_the_next_request() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut tech, tech_id) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        let sign_in = async |browser: &mut Browser| {
            let signed_in = browser
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
        };
        let (token_id, token) = make_token(&mut tech).await;
        both(&app, &mut tech, &token, "/v1/permissions", StatusCode::OK).await;

        // Disabled: the session ends and the token stops working.
        let disabled = admin
            .patch(&format!("/v1/users/{tech_id}"), json!({ "disabled": true }))
            .await;
        assert_eq!(
            disabled.status,
            StatusCode::OK,
            "{name}: {:?}",
            disabled.body
        );
        both(
            &app,
            &mut tech,
            &token,
            "/v1/permissions",
            StatusCode::UNAUTHORIZED,
        )
        .await;
        assert_eq!(
            with_token(&app, &token, "/v1/permissions").await.code(),
            "user_disabled",
            "{name}"
        );
        admin
            .patch(
                &format!("/v1/users/{tech_id}"),
                json!({ "disabled": false }),
            )
            .await;
        sign_in(&mut tech).await;
        both(&app, &mut tech, &token, "/v1/permissions", StatusCode::OK).await;

        // Signed out everywhere by an administrator.
        let ended = admin
            .post(&format!("/v1/users/{tech_id}/sign-out"), json!({}))
            .await;
        assert!(ended.status.is_success(), "{name}: {:?}", ended.body);
        assert_eq!(
            tech.get("/v1/permissions").await.status,
            StatusCode::UNAUTHORIZED,
            "{name}"
        );
        sign_in(&mut tech).await;
        assert_eq!(tech.get("/v1/permissions").await.status, StatusCode::OK);

        // A completed password reset ends every session.
        let link = admin
            .post(&format!("/v1/users/{tech_id}/password-reset"), json!({}))
            .await;
        assert_eq!(link.status, StatusCode::OK, "{name}: {:?}", link.body);
        let reset = app
            .browser()
            .post(
                "/v1/auth/password-reset/complete",
                json!({
                    "token": common::link_token(link.body["link"].as_str().unwrap()),
                    "password": "a long enough password",
                }),
            )
            .await;
        assert!(reset.status.is_success(), "{name}: {:?}", reset.body);
        assert_eq!(
            tech.get("/v1/permissions").await.status,
            StatusCode::UNAUTHORIZED,
            "{name}"
        );
        sign_in(&mut tech).await;

        // The token is revoked.
        let revoked = tech.delete(&format!("/v1/api-tokens/{token_id}")).await;
        assert!(revoked.status.is_success(), "{name}: {:?}", revoked.body);
        assert_eq!(
            with_token(&app, &token, "/v1/permissions").await.status,
            StatusCode::UNAUTHORIZED,
            "{name}"
        );

        // Deleted.
        let (_, token) = make_token(&mut tech).await;
        both(&app, &mut tech, &token, "/v1/permissions", StatusCode::OK).await;
        let deleted = admin.delete(&format!("/v1/users/{tech_id}")).await;
        assert!(deleted.status.is_success(), "{name}: {:?}", deleted.body);
        both(
            &app,
            &mut tech,
            &token,
            "/v1/permissions",
            StatusCode::UNAUTHORIZED,
        )
        .await;

        // Two-factor authentication becomes required (last, since the
        // administrator has no second factor either).
        let (mut ops, _) =
            common::add_user(&app, &mut admin, "ops@example.com", &["technician"]).await;
        let account = ops.get("/v1/account").await;
        assert_eq!(account.body["two_factor"]["enrollment_required"], false);
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
        let account = ops.get("/v1/account").await;
        assert_eq!(
            account.body["two_factor"]["enrollment_required"], true,
            "{name}"
        );
        assert_eq!(
            ops.get("/v1/permissions").await.code(),
            "two_factor_enrollment_required"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_change_made_outside_the_server_waits_for_the_recheck() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut tech, tech_id) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        assert_eq!(tech.get("/v1/permissions").await.status, StatusCode::OK);

        // As `meshrmm-server admin` would, with no signal: the request reads
        // only the session, so the kept account answers until it expires
        // (the cache's own tests cover the expiry) or access changes.
        app.db()
            .execute(
                &Query::update()
                    .table("users")
                    .value("display_name", "Renamed")
                    .and_where(Expr::col("id").eq(tech_id.as_str()))
                    .to_owned(),
            )
            .await
            .unwrap();
        let account = tech.get("/v1/account").await;
        assert_eq!(
            account.body["user"]["display_name"], "tech@example.com",
            "{name}"
        );
        app.state.presence.recheck_access();
        let account: Value = tech.get("/v1/account").await.body;
        assert_eq!(account["user"]["display_name"], "Renamed", "{name}");
        app.finish().await;
    }
}
