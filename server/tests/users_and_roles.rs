//! Managing users and roles, and the checks that stop anyone from raising
//! their own access.
mod common;

use axum::http::StatusCode;
use common::{ADMIN_EMAIL, App};
use serde_json::{Value, json};

use common::add_user;

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn custom_roles_grant_their_permissions() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let roles = admin.get("/v1/roles").await;
        assert_eq!(ids(&roles.body), ["administrator", "technician"], "{name}");
        assert_eq!(roles.body[0]["member_count"], 1);
        assert_eq!(
            roles.body[1]["permissions"],
            json!([
                "devices.view",
                "devices.enroll",
                "sessions.connect",
                "sessions.connect_background",
                "scripts.run",
                "files.deliver"
            ])
        );
        let permissions = admin.get("/v1/permissions").await;
        assert_eq!(permissions.body.as_array().unwrap().len(), 16);

        let invalid = admin
            .post(
                "/v1/roles",
                json!({ "name": "Auditor", "permissions": ["audit.everything"] }),
            )
            .await;
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST, "{name}");
        let created = admin
            .post(
                "/v1/roles",
                json!({ "name": "Auditor", "description": "Reads the log", "permissions": ["audit.view"] }),
            )
            .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            created.body
        );
        let auditor_role = created.body["id"].as_str().unwrap().to_owned();
        let duplicate = admin
            .post("/v1/roles", json!({ "name": "auditor", "permissions": [] }))
            .await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT);

        let (mut auditor, _) =
            add_user(&app, &mut admin, "auditor@example.com", &[&auditor_role]).await;
        assert_eq!(
            auditor.get("/v1/audit").await.status,
            StatusCode::OK,
            "{name}"
        );
        let denied = auditor.get("/v1/users").await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);
        assert_eq!(denied.code(), "permission_denied");

        // Changing the role changes what its members can do at once.
        let updated = admin
            .patch(
                &format!("/v1/roles/{auditor_role}"),
                json!({ "permissions": ["users.manage"] }),
            )
            .await;
        assert_eq!(updated.status, StatusCode::OK, "{name}: {:?}", updated.body);
        assert_eq!(updated.body["member_count"], 1);
        assert_eq!(auditor.get("/v1/audit").await.status, StatusCode::FORBIDDEN);
        assert_eq!(auditor.get("/v1/users").await.status, StatusCode::OK);

        // Deleting it takes it away from its members.
        assert_eq!(
            admin
                .delete(&format!("/v1/roles/{auditor_role}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(auditor.get("/v1/users").await.status, StatusCode::FORBIDDEN);
        assert_eq!(
            admin.delete("/v1/roles/technician").await.status,
            StatusCode::FORBIDDEN
        );
        let admin_role = admin
            .patch("/v1/roles/administrator", json!({ "permissions": [] }))
            .await;
        assert_eq!(admin_role.status, StatusCode::FORBIDDEN);
        app.finish().await;
    }
}

#[tokio::test]
async fn nobody_can_grant_more_than_they_hold() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let helpdesk = admin
            .post(
                "/v1/roles",
                json!({ "name": "Helpdesk", "permissions": ["users.manage", "roles.manage", "devices.view"] }),
            )
            .await;
        let helpdesk_role = helpdesk.body["id"].as_str().unwrap().to_owned();
        let (mut manager, manager_id) =
            add_user(&app, &mut admin, "manager@example.com", &[&helpdesk_role]).await;
        let (_, tech_id) = add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        let admin_id = admin.get("/v1/account").await.body["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned();

        // Not the Administrator role, for themselves or an invitee.
        let promote = manager
            .patch(
                &format!("/v1/users/{manager_id}"),
                json!({ "role_ids": [helpdesk_role, "administrator"] }),
            )
            .await;
        assert_eq!(promote.status, StatusCode::FORBIDDEN, "{name}");
        assert_eq!(promote.code(), "permission_denied");
        let invite = manager
            .post(
                "/v1/invitations",
                json!({ "email": "friend@example.com", "role_ids": ["administrator"] }),
            )
            .await;
        assert_eq!(invite.code(), "permission_denied");
        // Not the Technician role either: it has permissions they lack.
        let technician = manager
            .patch(
                &format!("/v1/users/{manager_id}"),
                json!({ "role_ids": [helpdesk_role, "technician"] }),
            )
            .await;
        assert_eq!(technician.code(), "permission_escalation");
        // Nor a role, or a change to one, with more than they hold.
        let role = manager
            .post(
                "/v1/roles",
                json!({ "name": "Escalated", "permissions": ["settings.manage"] }),
            )
            .await;
        assert_eq!(role.code(), "permission_escalation");
        let widen = manager
            .patch(
                &format!("/v1/roles/{helpdesk_role}"),
                json!({ "permissions": ["users.manage", "roles.manage", "devices.view", "audit.view"] }),
            )
            .await;
        assert_eq!(widen.code(), "permission_escalation");
        // And no managing users more powerful than they are.
        for (path, code) in [
            (
                format!("/v1/users/{admin_id}/reset-two-factor"),
                "permission_denied",
            ),
            (
                format!("/v1/users/{admin_id}/password-reset"),
                "permission_denied",
            ),
            (
                format!("/v1/users/{tech_id}/sign-out"),
                "permission_escalation",
            ),
        ] {
            let response = manager.post(&path, json!({})).await;
            assert_eq!(response.code(), code, "{name}: {path}");
        }
        let disable = manager
            .patch(
                &format!("/v1/users/{admin_id}"),
                json!({ "disabled": true }),
            )
            .await;
        assert_eq!(disable.code(), "permission_denied");

        // Within their own permissions, they can act.
        let narrower = manager
            .post(
                "/v1/roles",
                json!({ "name": "Viewer", "permissions": ["devices.view"] }),
            )
            .await;
        assert_eq!(narrower.status, StatusCode::CREATED, "{name}");
        let viewer_role = narrower.body["id"].as_str().unwrap().to_owned();
        let (_, viewer_id) =
            add_user(&app, &mut manager, "viewer@example.com", &[&viewer_role]).await;
        let renamed = manager
            .patch(
                &format!("/v1/users/{viewer_id}"),
                json!({ "display_name": "Vera Viewer", "disabled": true }),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{name}: {:?}", renamed.body);
        assert_eq!(renamed.body["display_name"], "Vera Viewer");
        assert_eq!(renamed.body["disabled"], true);
        app.finish().await;
    }
}

#[tokio::test]
async fn an_enabled_administrator_always_remains() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let admin_id = admin.get("/v1/account").await.body["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let demote = admin
            .patch(
                &format!("/v1/users/{admin_id}"),
                json!({ "role_ids": ["technician"] }),
            )
            .await;
        assert_eq!(demote.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(demote.code(), "last_administrator");
        let disable = admin
            .patch(
                &format!("/v1/users/{admin_id}"),
                json!({ "disabled": true }),
            )
            .await;
        assert_eq!(disable.status, StatusCode::BAD_REQUEST);
        let delete = admin.delete(&format!("/v1/users/{admin_id}")).await;
        assert_eq!(delete.status, StatusCode::BAD_REQUEST);
        // The rejected demotion left the role in place.
        let roles = admin.get(&format!("/v1/users/{admin_id}")).await.body["roles"].clone();
        assert_eq!(roles[0]["id"], "administrator", "{name}");

        // With a second administrator, the first can step down.
        let (mut second, second_id) =
            add_user(&app, &mut admin, "second@example.com", &["administrator"]).await;
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
        let last = second
            .patch(&format!("/v1/users/{second_id}"), json!({ "role_ids": [] }))
            .await;
        assert_eq!(last.code(), "last_administrator");
        app.finish().await;
    }
}

#[tokio::test]
async fn disabled_and_deleted_users_lose_access() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut tech, tech_id) =
            add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        let listed = admin.get("/v1/users").await;
        assert_eq!(
            listed
                .body
                .as_array()
                .unwrap()
                .iter()
                .map(|user| user["email"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [ADMIN_EMAIL, "tech@example.com"],
            "{name}"
        );
        assert_eq!(listed.body[1]["roles"][0]["name"], "Technician");
        assert_eq!(listed.body[1]["has_password"], true);

        let disabled = admin
            .patch(&format!("/v1/users/{tech_id}"), json!({ "disabled": true }))
            .await;
        assert_eq!(disabled.status, StatusCode::OK, "{name}");
        assert_eq!(
            tech.get("/v1/account").await.status,
            StatusCode::UNAUTHORIZED
        );
        let sign_in = app
            .browser()
            .post(
                "/v1/auth/sign-in",
                json!({ "email": "tech@example.com", "password": "a long enough password" }),
            )
            .await;
        assert_eq!(sign_in.code(), "account_disabled", "{name}");
        // A wrong password doesn't reveal that the account is disabled.
        let guess = app
            .browser()
            .post(
                "/v1/auth/sign-in",
                json!({ "email": "tech@example.com", "password": "wrong guess at it" }),
            )
            .await;
        assert_eq!(guess.code(), "invalid_credentials");

        admin
            .patch(
                &format!("/v1/users/{tech_id}"),
                json!({ "disabled": false }),
            )
            .await;
        tech.post(
            "/v1/auth/sign-in",
            json!({ "email": "tech@example.com", "password": "a long enough password" }),
        )
        .await;
        assert_eq!(tech.get("/v1/account").await.status, StatusCode::OK);
        assert_eq!(
            admin
                .post(&format!("/v1/users/{tech_id}/sign-out"), json!({}))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            tech.get("/v1/account").await.status,
            StatusCode::UNAUTHORIZED
        );

        assert_eq!(
            admin.delete(&format!("/v1/users/{tech_id}")).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            admin.get(&format!("/v1/users/{tech_id}")).await.status,
            StatusCode::NOT_FOUND
        );
        let audit = admin
            .get(&format!(
                "/v1/audit?target_type=user&target_id={tech_id}&action=user."
            ))
            .await;
        let actions = audit.body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["action"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            ["user.delete", "user.sign_out", "user.update", "user.update"],
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn an_administrator_resets_a_lost_second_factor() {
    let app = App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    let (mut tech, tech_id) = add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
    let started = tech
        .post(
            "/v1/account/two-factor/totp",
            json!({ "password": "a long enough password" }),
        )
        .await;
    let secret = started.body["secret"].as_str().unwrap();
    tech.post(
        "/v1/account/two-factor/totp/confirm",
        json!({ "code": common::totp_code(secret, 0) }),
    )
    .await;
    let users = admin.get("/v1/users").await;
    assert_eq!(users.body[1]["two_factor_enabled"], true);

    let reset = admin
        .post(&format!("/v1/users/{tech_id}/reset-two-factor"), json!({}))
        .await;
    assert_eq!(reset.status, StatusCode::NO_CONTENT);
    assert_eq!(
        tech.get("/v1/account").await.status,
        StatusCode::UNAUTHORIZED
    );
    let sign_in = tech
        .post(
            "/v1/auth/sign-in",
            json!({ "email": "tech@example.com", "password": "a long enough password" }),
        )
        .await;
    assert_eq!(sign_in.body["status"], "signed_in");
    // The administrator stays signed in.
    assert_eq!(admin.get("/v1/account").await.status, StatusCode::OK);
    app.finish().await;
}

#[tokio::test]
async fn only_administrators_grant_or_manage_administrators() {
    let app = App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    // A role with every current permission is still not the Administrator
    // role, which also gains every permission later releases add.
    let everything = meshrmm_server::rbac::Permission::ALL
        .iter()
        .map(|permission| permission.as_str())
        .collect::<Vec<_>>();
    let all = admin
        .post(
            "/v1/roles",
            json!({ "name": "Everything", "permissions": everything }),
        )
        .await;
    let all_role = all.body["id"].as_str().unwrap().to_owned();
    let (mut almighty, almighty_id) =
        add_user(&app, &mut admin, "almighty@example.com", &[&all_role]).await;
    let admin_id = admin.get("/v1/account").await.body["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let grant = almighty
        .patch(
            &format!("/v1/users/{almighty_id}"),
            json!({ "role_ids": [all_role, "administrator"] }),
        )
        .await;
    assert_eq!(grant.code(), "permission_denied");
    let disable = almighty
        .patch(
            &format!("/v1/users/{admin_id}"),
            json!({ "disabled": true }),
        )
        .await;
    assert_eq!(disable.code(), "permission_denied");
    // Email decides where password reset links go, so it's theirs to set.
    let smtp = almighty
        .put(
            "/v1/settings/smtp",
            json!({ "host": "attacker.example", "security": "none", "from": "x@example.com" }),
        )
        .await;
    assert_eq!(smtp.code(), "permission_denied");
    assert_eq!(
        almighty.get("/v1/settings/smtp").await.code(),
        "permission_denied"
    );
    // An administrator may grant it.
    let granted = admin
        .patch(
            &format!("/v1/users/{almighty_id}"),
            json!({ "role_ids": ["administrator"] }),
        )
        .await;
    assert_eq!(granted.status, StatusCode::OK, "{:?}", granted.body);
    app.finish().await;
}

#[tokio::test]
async fn users_cant_reset_their_own_sign_in_from_user_management() {
    let app = App::sqlite().await;
    let mut admin = common::set_up(&app).await;
    let admin_id = admin.get("/v1/account").await.body["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Without the current password, a borrowed session could otherwise
    // remove two-factor and set a new password.
    for path in ["reset-two-factor", "password-reset"] {
        let response = admin
            .post(&format!("/v1/users/{admin_id}/{path}"), json!({}))
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}");
    }
    app.finish().await;
}

#[tokio::test]
async fn two_administrators_demoting_each_other_at_once_leave_one() {
    for app in common::apps().await {
        let name = app.name;
        for _ in 0..5 {
            let mut first = common::set_up_or_sign_in(&app).await;
            let first_id = first.get("/v1/account").await.body["user"]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let email = format!("second-{}@example.com", common::random_hex(4));
            let (mut second, second_id) =
                add_user(&app, &mut first, &email, &["administrator"]).await;
            let (demote_second, demote_first) = (
                format!("/v1/users/{second_id}"),
                format!("/v1/users/{first_id}"),
            );
            let (a, b) = tokio::join!(
                first.patch(&demote_second, json!({ "role_ids": [] })),
                second.patch(&demote_first, json!({ "role_ids": [] })),
            );
            let succeeded = [&a, &b]
                .iter()
                .filter(|response| response.status == StatusCode::OK)
                .count();
            assert_eq!(succeeded, 1, "{name}: {:?} {:?}", a.body, b.body);
            // Leave the first user as the only administrator for the next
            // round.
            if b.status == StatusCode::OK {
                let restored = second
                    .patch(&demote_first, json!({ "role_ids": ["administrator"] }))
                    .await;
                assert_eq!(restored.status, StatusCode::OK, "{name}");
                let demoted = first.patch(&demote_second, json!({ "role_ids": [] })).await;
                assert_eq!(demoted.status, StatusCode::OK, "{name}");
            }
        }
        app.finish().await;
    }
}
