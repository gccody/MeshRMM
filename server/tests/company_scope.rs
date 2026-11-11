//! Roles limited to some companies: a user reaches a device only through a
//! role that grants the permission and covers the device's company, and
//! everything about devices out of reach answers as if they didn't exist.
mod common;

use axum::{
    body::Body,
    http::{Method, StatusCode},
};
use common::{
    Agent, App, Browser, Response, add_role, add_user, apps, audit_events, receive_json, send_json,
    set_up,
};
use meshrmm_protocol_types::{MetricsReport, SystemMetrics, VolumeUsage};
use meshrmm_server::{
    alerts,
    time::{SECOND_MS, now_ms},
};
use serde_json::{Value, json};

/// Two companies with a device each, and a device in none.
struct Fleet {
    acme: String,
    globex: String,
    /// In Acme, which the limited roles cover.
    inside: Agent,
    /// In Globex, which they don't.
    outside: Agent,
    /// In no company: only roles over every company reach it.
    loose: Agent,
}

async fn company(admin: &mut Browser, name: &str) -> String {
    let created = admin.post("/v1/companies", json!({ "name": name })).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

async fn move_to(admin: &mut Browser, device_id: &str, company_id: Option<&str>) {
    let moved = admin
        .put(
            &format!("/v1/agents/{device_id}/company"),
            json!({ "company_id": company_id }),
        )
        .await;
    assert_eq!(moved.status, StatusCode::NO_CONTENT, "{:?}", moved.body);
}

async fn fleet(app: &App, admin: &mut Browser) -> Fleet {
    let acme = company(admin, "Acme").await;
    let globex = company(admin, "Globex").await;
    let inside = app.enroll(admin, "Inside").await;
    let outside = app.enroll(admin, "Outside").await;
    let loose = app.enroll(admin, "Loose").await;
    move_to(admin, &inside.device_id, Some(&acme)).await;
    move_to(admin, &outside.device_id, Some(&globex)).await;
    Fleet {
        acme,
        globex,
        inside,
        outside,
        loose,
    }
}

/// Creates a role over `companies` alone and returns its ID.
async fn limited_role(
    admin: &mut Browser,
    name: &str,
    permissions: &[&str],
    companies: &[&str],
) -> String {
    let created = admin
        .post(
            "/v1/roles",
            json!({ "name": name, "permissions": permissions, "all_companies": false, "company_ids": companies }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert_eq!(created.body["all_companies"], false);
    created.body["id"].as_str().unwrap().to_owned()
}

/// A user holding `permissions` in `companies` alone.
async fn limited_user(
    app: &App,
    admin: &mut Browser,
    email: &str,
    permissions: &[&str],
    companies: &[&str],
) -> (Browser, String) {
    let role = limited_role(admin, email, permissions, companies).await;
    add_user(app, admin, email, &[&role]).await
}

/// A device ID nothing has.
fn missing() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Sends the same request about the device out of reach and about one that
/// doesn't exist, and checks both answer 404 alike.
async fn as_if_missing(
    browser: &mut Browser,
    method: Method,
    path: impl Fn(&str) -> String,
    body: Option<Value>,
    outside: &str,
    name: &str,
) -> Response {
    let unknown = browser
        .send(method.clone(), &path(&missing()), body.clone())
        .await;
    let hidden = browser.send(method, &path(outside), body).await;
    assert_eq!(
        hidden.status,
        StatusCode::NOT_FOUND,
        "{name}: {} {:?}",
        path(outside),
        hidden.body
    );
    assert_eq!(
        (hidden.status, &hidden.body),
        (unknown.status, &unknown.body),
        "{name}: {} answers as a missing device does",
        path(outside)
    );
    hidden
}

/// Checks the answer about a device in reach isn't a missing device's.
async fn reached(
    browser: &mut Browser,
    method: Method,
    path: impl Fn(&str) -> String,
    body: Option<Value>,
    inside: &str,
    name: &str,
) {
    let unknown = browser
        .send(method.clone(), &path(&missing()), body.clone())
        .await;
    let answer = browser.send(method, &path(inside), body).await;
    assert_ne!(
        (answer.status, &answer.body),
        (unknown.status, &unknown.body),
        "{name}: {} is found",
        path(inside)
    );
}

fn device_ids(listed: &Value) -> Vec<String> {
    let mut ids: Vec<String> = listed["agents"]
        .as_array()
        .unwrap_or_else(|| panic!("no devices in {listed}"))
        .iter()
        .map(|agent| agent["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

fn sorted(ids: &[&str]) -> Vec<String> {
    let mut ids: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    ids.sort();
    ids
}

fn names(list: &Value, field: &str) -> Vec<String> {
    let mut names: Vec<String> = list
        .as_array()
        .unwrap_or_else(|| panic!("not a list: {list}"))
        .iter()
        .map(|item| item[field].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_limited_role_sees_only_its_companies_devices() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let (mut tech, _) = limited_user(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "devices.edit", "alerts.view"],
            &[&fleet.acme],
        )
        .await;

        let listed = tech.get("/v1/agents").await;
        assert_eq!(listed.status, StatusCode::OK, "{name}: {:?}", listed.body);
        assert_eq!(
            device_ids(&listed.body),
            std::slice::from_ref(&fleet.inside.device_id),
            "{name}"
        );
        assert_eq!(
            names(&listed.body["companies"], "id"),
            std::slice::from_ref(&fleet.acme),
            "{name}"
        );
        assert_eq!(
            device_ids(&admin.get("/v1/agents").await.body),
            sorted(&[
                &fleet.inside.device_id,
                &fleet.outside.device_id,
                &fleet.loose.device_id
            ]),
            "{name}"
        );
        let companies = tech.get("/v1/companies").await;
        assert_eq!(names(&companies.body, "name"), ["Acme"], "{name}");
        assert_eq!(companies.body[0]["device_count"], 1);

        let account = tech.get("/v1/account").await.body;
        assert_eq!(
            account["limited_permissions"],
            json!(["devices.view", "devices.edit", "alerts.view"]),
            "{name}: {account}"
        );
        assert_eq!(
            admin.get("/v1/account").await.body["limited_permissions"],
            json!([]),
            "{name}"
        );

        // Each of a device's own pages: allowed in reach, and as if missing
        // out of it or in no company.
        for path in [
            |id: &str| format!("/v1/agents/{id}/notes"),
            |id: &str| format!("/v1/agents/{id}/metrics"),
            |id: &str| format!("/v1/agents/{id}/system-info"),
            |id: &str| format!("/v1/agents/{id}/monitors"),
            |id: &str| format!("/v1/agents/{id}/wakes"),
            |id: &str| format!("/v1/agents/{id}/thumbnail"),
        ] {
            reached(
                &mut tech,
                Method::GET,
                path,
                None,
                &fleet.inside.device_id,
                name,
            )
            .await;
            for hidden in [&fleet.outside.device_id, &fleet.loose.device_id] {
                as_if_missing(&mut tech, Method::GET, path, None, hidden, name).await;
            }
        }

        // Changes too.
        let saved = tech
            .put(
                &format!("/v1/agents/{}/notes", fleet.inside.device_id),
                json!({ "body": "Front desk", "revision": 0 }),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        as_if_missing(
            &mut tech,
            Method::PUT,
            |id| format!("/v1/agents/{id}/notes"),
            Some(json!({ "body": "Snooping", "revision": 0 })),
            &fleet.outside.device_id,
            name,
        )
        .await;
        let tag = admin
            .post("/v1/tags", json!({ "name": "Servers", "color": "blue" }))
            .await;
        assert_eq!(tag.status, StatusCode::CREATED, "{name}: {:?}", tag.body);
        let tag_id = tag.body["id"].as_str().unwrap().to_owned();
        let refused = tech
            .post(
                "/v1/device-tags",
                json!({ "device_ids": [fleet.inside.device_id, fleet.outside.device_id], "add": [tag_id], "remove": [] }),
            )
            .await;
        assert_eq!(refused.status, StatusCode::NOT_FOUND, "{name}");
        let tagged = tech
            .post(
                "/v1/device-tags",
                json!({ "device_ids": [fleet.inside.device_id], "add": [tag_id], "remove": [] }),
            )
            .await;
        assert_eq!(tagged.status, StatusCode::OK, "{name}: {:?}", tagged.body);
        let tagged = admin
            .post(
                "/v1/device-tags",
                json!({ "device_ids": [fleet.outside.device_id, fleet.loose.device_id], "add": [tag_id], "remove": [] }),
            )
            .await;
        assert_eq!(tagged.status, StatusCode::OK, "{name}");
        // A tag's device count counts the devices the user sees.
        let tags = tech.get("/v1/tags").await.body;
        assert_eq!(tags[0]["device_count"], 1, "{name}: {tags}");
        assert_eq!(admin.get("/v1/tags").await.body[0]["device_count"], 3);

        // A device moved into reach appears; one moved out disappears, and
        // the companies the user sees follow.
        move_to(&mut admin, &fleet.outside.device_id, Some(&fleet.acme)).await;
        move_to(&mut admin, &fleet.inside.device_id, Some(&fleet.globex)).await;
        assert_eq!(
            device_ids(&tech.get("/v1/agents").await.body),
            std::slice::from_ref(&fleet.outside.device_id),
            "{name}"
        );
        as_if_missing(
            &mut tech,
            Method::GET,
            |id| format!("/v1/agents/{id}/notes"),
            None,
            &fleet.inside.device_id,
            name,
        )
        .await;
        app.finish().await;
    }
}

#[tokio::test]
async fn permissions_from_two_roles_never_combine_across_companies() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        // Viewing in Acme, and viewing and editing in Globex: editing never
        // reaches Acme's devices.
        let viewer =
            limited_role(&mut admin, "Acme viewer", &["devices.view"], &[&fleet.acme]).await;
        let editor = limited_role(
            &mut admin,
            "Globex editor",
            &["devices.view", "devices.edit", "terminal.connect"],
            &[&fleet.globex],
        )
        .await;
        let (mut tech, _) =
            add_user(&app, &mut admin, "tech@example.com", &[&viewer, &editor]).await;

        assert_eq!(
            device_ids(&tech.get("/v1/agents").await.body),
            sorted(&[&fleet.inside.device_id, &fleet.outside.device_id]),
            "{name}"
        );
        let notes = |id: &str| format!("/v1/agents/{id}/notes");
        let edit = json!({ "body": "Edited", "revision": 0 });
        assert_eq!(
            tech.put(&notes(&fleet.outside.device_id), edit.clone())
                .await
                .status,
            StatusCode::OK,
            "{name}"
        );
        assert_eq!(
            tech.get(&notes(&fleet.inside.device_id)).await.status,
            StatusCode::OK,
            "{name}"
        );
        let refused = tech
            .put(&notes(&fleet.inside.device_id), edit.clone())
            .await;
        assert_eq!(refused.status, StatusCode::NOT_FOUND, "{name}");
        let terminal =
            json!({ "shell": "powershell", "run_as": "user", "columns": 100, "rows": 30 });
        let opened = tech
            .post(
                &format!("/v1/agents/{}/terminals", fleet.inside.device_id),
                terminal.clone(),
            )
            .await;
        assert_eq!(opened.status, StatusCode::NOT_FOUND, "{name}");
        // The device in reach is merely offline.
        let opened = tech
            .post(
                &format!("/v1/agents/{}/terminals", fleet.outside.device_id),
                terminal,
            )
            .await;
        assert_ne!(
            opened.status,
            StatusCode::NOT_FOUND,
            "{name}: {:?}",
            opened.body
        );
        assert_ne!(
            opened.status,
            StatusCode::FORBIDDEN,
            "{name}: {:?}",
            opened.body
        );

        let account = tech.get("/v1/account").await.body;
        assert_eq!(
            account["permissions"],
            json!(["devices.view", "devices.edit", "terminal.connect"]),
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn instance_permissions_come_only_from_roles_over_every_company() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;

        let permissions = admin.get("/v1/permissions").await.body;
        let reach_of = |permission: &str| {
            permissions
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == permission)
                .unwrap_or_else(|| panic!("{permission} is not in {permissions}"))["reach"]
                .clone()
        };
        assert_eq!(reach_of("users.manage"), "instance", "{name}");
        assert_eq!(reach_of("devices.view"), "company", "{name}");
        assert_eq!(reach_of("jobs.run_wide"), "company", "{name}");

        for permission in [
            "users.manage",
            "audit.view",
            "companies.manage",
            "tags.manage",
            "reports.manage",
        ] {
            let refused = admin
                .post(
                    "/v1/roles",
                    json!({ "name": format!("Limited {permission}"), "permissions": ["devices.view", permission], "all_companies": false, "company_ids": [fleet.acme] }),
                )
                .await;
            assert_eq!(
                refused.status,
                StatusCode::BAD_REQUEST,
                "{name}: {permission}"
            );
            assert_eq!(refused.code(), "instance_permission_on_limited_role");
        }
        // Limiting a role that holds one is refused too, and so is naming
        // no company or one that doesn't exist.
        let auditor = add_role(&mut admin, "Auditor", &["devices.view", "audit.view"]).await;
        let refused = admin
            .patch(
                &format!("/v1/roles/{auditor}"),
                json!({ "company_ids": [fleet.acme] }),
            )
            .await;
        assert_eq!(
            refused.code(),
            "instance_permission_on_limited_role",
            "{name}"
        );
        let empty = admin
            .post(
                "/v1/roles",
                json!({ "name": "Nowhere", "permissions": ["devices.view"], "all_companies": false, "company_ids": [] }),
            )
            .await;
        assert_eq!(empty.code(), "invalid_role_companies", "{name}");
        let unknown = admin
            .post(
                "/v1/roles",
                json!({ "name": "Nowhere", "permissions": ["devices.view"], "all_companies": false, "company_ids": [missing()] }),
            )
            .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{name}");
        // The administrator role covers every company, always.
        let administrator = admin
            .patch(
                "/v1/roles/administrator",
                json!({ "all_companies": false, "company_ids": [fleet.acme] }),
            )
            .await;
        assert!(
            administrator.status.is_client_error(),
            "{name}: {:?}",
            administrator.body
        );

        // A limited role that holds one anyway, as edited in the database,
        // grants it nowhere.
        let limited = limited_role(&mut admin, "Acme", &["devices.view"], &[&fleet.acme]).await;
        app.db()
            .execute(
                &sea_query::Query::insert()
                    .into_table("role_permissions")
                    .columns(["role_id", "permission"])
                    .values_panic([limited.as_str().into(), "audit.view".into()])
                    .to_owned(),
            )
            .await
            .unwrap();
        let (mut tech, _) = add_user(&app, &mut admin, "tech@example.com", &[&limited]).await;
        assert_eq!(
            tech.get("/v1/account").await.body["permissions"],
            json!(["devices.view"]),
            "{name}"
        );
        assert_eq!(tech.get("/v1/audit").await.status, StatusCode::FORBIDDEN);
        app.finish().await;
    }
}

#[tokio::test]
async fn role_scope_changes_are_checked_and_audited() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let role = limited_role(&mut admin, "Field", &["devices.view"], &[&fleet.acme]).await;
        let created = &audit_events(&app, "role.create").await[0];
        assert_eq!(created.metadata["all_companies"], false, "{name}");
        assert_eq!(created.metadata["company_ids"], json!([fleet.acme]));

        let listed = admin.get("/v1/roles").await.body;
        let field = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|role| role["name"] == "Field")
            .unwrap();
        assert_eq!(field["all_companies"], false, "{name}");
        assert_eq!(field["company_ids"], json!([fleet.acme]));
        let administrator = &listed[0];
        assert_eq!(administrator["all_companies"], true);

        let widened = admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "company_ids": [fleet.acme, fleet.globex] }),
            )
            .await;
        assert_eq!(widened.status, StatusCode::OK, "{name}: {:?}", widened.body);
        let updated = &audit_events(&app, "role.update").await[0];
        assert_eq!(updated.metadata["all_companies"], false, "{name}");
        let mut chosen = vec![fleet.acme.clone(), fleet.globex.clone()];
        chosen.sort();
        assert_eq!(updated.metadata["company_ids"], json!(chosen));
        assert_eq!(
            updated.metadata["previous_companies"]["company_ids"],
            json!([fleet.acme])
        );

        let everywhere = admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "all_companies": true }),
            )
            .await;
        assert_eq!(everywhere.status, StatusCode::OK, "{name}");
        assert_eq!(everywhere.body["all_companies"], true);
        assert_eq!(everywhere.body["company_ids"], json!([]));

        // A company's deletion takes it out of the roles limited to it.
        let narrowed = admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "all_companies": false, "company_ids": [fleet.globex] }),
            )
            .await;
        assert_eq!(narrowed.status, StatusCode::OK, "{name}");
        let (mut tech, _) = add_user(&app, &mut admin, "tech@example.com", &[&role]).await;
        assert_eq!(
            device_ids(&tech.get("/v1/agents").await.body),
            std::slice::from_ref(&fleet.outside.device_id),
            "{name}"
        );
        assert_eq!(
            admin
                .delete(&format!("/v1/companies/{}", fleet.globex))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let listed = tech.get("/v1/agents").await.body;
        assert_eq!(device_ids(&listed), Vec::<String>::new(), "{name}");
        assert_eq!(listed["companies"], json!([]));
        app.finish().await;
    }
}

#[tokio::test]
async fn nobody_grants_more_companies_than_they_reach() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        // Managing roles is instance-wide, but viewing devices reaches Acme
        // alone.
        let manager = add_role(&mut admin, "Role manager", &["roles.manage"]).await;
        let viewer =
            limited_role(&mut admin, "Acme viewer", &["devices.view"], &[&fleet.acme]).await;
        let (mut lead, _) =
            add_user(&app, &mut admin, "lead@example.com", &[&manager, &viewer]).await;

        let everywhere = lead
            .post(
                "/v1/roles",
                json!({ "name": "Everywhere", "permissions": ["devices.view"] }),
            )
            .await;
        assert_eq!(everywhere.code(), "permission_escalation", "{name}");
        let elsewhere = lead
            .post(
                "/v1/roles",
                json!({ "name": "Globex", "permissions": ["devices.view"], "all_companies": false, "company_ids": [fleet.globex] }),
            )
            .await;
        assert_eq!(elsewhere.code(), "permission_escalation", "{name}");
        let within = lead
            .post(
                "/v1/roles",
                json!({ "name": "Acme", "permissions": ["devices.view"], "all_companies": false, "company_ids": [fleet.acme] }),
            )
            .await;
        assert_eq!(
            within.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            within.body
        );
        let role = within.body["id"].as_str().unwrap().to_owned();
        let widened = lead
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "company_ids": [fleet.acme, fleet.globex] }),
            )
            .await;
        assert_eq!(widened.code(), "permission_escalation", "{name}");
        let widened = lead
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "all_companies": true }),
            )
            .await;
        assert_eq!(widened.code(), "permission_escalation", "{name}");
        // A role over every company is out of the lead's hands entirely.
        let technician = lead
            .patch(
                "/v1/roles/technician",
                json!({ "company_ids": [fleet.acme] }),
            )
            .await;
        assert_eq!(technician.code(), "permission_escalation", "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn api_tokens_reach_what_their_user_reaches() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let (mut tech, _) = limited_user(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "devices.edit"],
            &[&fleet.acme],
        )
        .await;
        for body in [
            json!({ "name": "Everything", "expires_in_days": 30 }),
            json!({ "name": "Read only", "expires_in_days": 30, "permissions": ["devices.view"] }),
        ] {
            let created = tech.post("/v1/api-tokens", body).await;
            assert_eq!(
                created.status,
                StatusCode::CREATED,
                "{name}: {:?}",
                created.body
            );
            let secret = created.body["secret"].as_str().unwrap().to_owned();
            let call = |path: String| {
                axum::http::Request::builder()
                    .method(Method::GET)
                    .uri(path)
                    .header("authorization", format!("Bearer {secret}"))
                    .body(Body::empty())
                    .unwrap()
            };
            let mut token = app.browser();
            let listed = token.raw(call("/v1/agents".into())).await;
            assert_eq!(listed.status, StatusCode::OK, "{name}: {:?}", listed.body);
            assert_eq!(
                device_ids(&listed.body),
                std::slice::from_ref(&fleet.inside.device_id),
                "{name}"
            );
            let hidden = token
                .raw(call(format!(
                    "/v1/agents/{}/notes",
                    fleet.outside.device_id
                )))
                .await;
            assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{name}");
        }
        app.finish().await;
    }
}

#[tokio::test]
async fn the_event_socket_shows_only_devices_in_reach() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let (tech, _) = limited_user(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view"],
            &[&fleet.acme],
        )
        .await;
        let mut events = tech.events(&server).await.unwrap();
        let snapshot = receive_json(&mut events).await;
        assert_eq!(snapshot["type"], "snapshot", "{name}");
        assert_eq!(
            device_ids(&snapshot),
            std::slice::from_ref(&fleet.inside.device_id),
            "{name}: {snapshot}"
        );
        assert_eq!(names(&snapshot["companies"], "name"), ["Acme"]);
        let revision = snapshot["revision"].as_u64().unwrap();

        // A device out of reach coming online moves the revision on, but
        // says nothing about the device.
        let _outside = fleet.outside.connect(&server).await.unwrap();
        let skipped = receive_json(&mut events).await;
        assert_eq!(
            skipped,
            json!({ "type": "skipped", "revision": revision + 1 }),
            "{name}"
        );
        let _inside = fleet.inside.connect(&server).await.unwrap();
        let online = receive_json(&mut events).await;
        assert_eq!(online["type"], "agent_upsert", "{name}: {online}");
        assert_eq!(online["revision"], revision + 2);
        assert_eq!(online["agent"]["id"], fleet.inside.device_id.as_str());
        assert_eq!(online["agent"]["connected"], true);

        // A device moved in appears, and one moved out goes.
        move_to(&mut admin, &fleet.outside.device_id, Some(&fleet.acme)).await;
        let arrived = receive_json(&mut events).await;
        assert_eq!(arrived["type"], "agent_upsert", "{name}: {arrived}");
        assert_eq!(arrived["agent"]["id"], fleet.outside.device_id.as_str());
        assert_eq!(arrived["agent"]["connected"], true);
        move_to(&mut admin, &fleet.inside.device_id, Some(&fleet.globex)).await;
        let left = receive_json(&mut events).await;
        assert_eq!(
            left,
            json!({ "type": "agent_deleted", "revision": revision + 4, "agent_id": fleet.inside.device_id }),
            "{name}"
        );
        // Then it is out of reach like any other.
        move_to(&mut admin, &fleet.inside.device_id, None).await;
        assert_eq!(receive_json(&mut events).await["type"], "skipped", "{name}");
        let renamed = admin
            .patch(
                &format!("/v1/companies/{}", fleet.globex),
                json!({ "name": "Globex Corp" }),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{name}");
        assert_eq!(receive_json(&mut events).await["type"], "skipped", "{name}");
        let renamed = admin
            .patch(
                &format!("/v1/companies/{}", fleet.acme),
                json!({ "name": "Acme Inc" }),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{name}");
        let company = receive_json(&mut events).await;
        assert_eq!(company["type"], "company_upsert", "{name}: {company}");
        assert_eq!(company["company"]["name"], "Acme Inc");
        app.finish().await;
    }
}

#[tokio::test]
async fn remote_access_out_of_reach_answers_as_missing() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let (mut tech, _) = limited_user(
            &app,
            &mut admin,
            "tech@example.com",
            &[
                "devices.view",
                "devices.wake",
                "sessions.connect",
                "terminal.connect",
                "scripts.run",
                "files.deliver",
                "files.browse",
                "updates.view",
                "updates.install",
                "applications.view",
            ],
            &[&fleet.acme],
        )
        .await;
        let script = tech
            .post(
                "/v1/toolbox/scripts",
                json!({ "name": "Hello", "language": "powershell", "body": "echo hi", "timeout_seconds": 60, "shared": false }),
            )
            .await;
        assert_eq!(
            script.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            script.body
        );
        let script_id = script.body["id"].as_str().unwrap().to_owned();

        let outside = fleet.outside.device_id.as_str();
        for (method, path, body) in [
            (
                Method::POST,
                (|id: &str| format!("/v1/agents/{id}/terminals")) as fn(&str) -> String,
                Some(
                    json!({ "shell": "powershell", "run_as": "user", "columns": 100, "rows": 30 }),
                ),
            ),
            (
                Method::GET,
                |id: &str| format!("/v1/agents/{id}/files?path=/&run_as=user"),
                None,
            ),
            (
                Method::GET,
                |id: &str| format!("/v1/agents/{id}/files/roots?run_as=user"),
                None,
            ),
            (
                Method::POST,
                |id: &str| format!("/v1/agents/{id}/wake"),
                Some(json!({})),
            ),
            (
                Method::POST,
                |id: &str| format!("/v1/agents/{id}/script-runs"),
                Some(json!({ "script_id": script_id, "run_as": "user" })),
            ),
            (
                Method::GET,
                |id: &str| format!("/v1/agents/{id}/updates"),
                None,
            ),
            (
                Method::POST,
                |id: &str| format!("/v1/agents/{id}/updates/scan"),
                Some(json!({})),
            ),
            (
                Method::GET,
                |id: &str| format!("/v1/agents/{id}/packages"),
                None,
            ),
        ] {
            reached(
                &mut tech,
                method.clone(),
                path,
                body.clone(),
                &fleet.inside.device_id,
                name,
            )
            .await;
            as_if_missing(&mut tech, method, path, body, outside, name).await;
        }
        let handoff = |device_id: &str| json!({ "device_id": device_id, "reason": "Printer" });
        let hidden = tech.post("/v1/remote/handoffs", handoff(outside)).await;
        let unknown = tech.post("/v1/remote/handoffs", handoff(&missing())).await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{name}");
        assert_eq!(hidden.body, unknown.body, "{name}");
        let issued = tech
            .post("/v1/remote/handoffs", handoff(&fleet.inside.device_id))
            .await;
        assert_eq!(issued.status, StatusCode::OK, "{name}: {:?}", issued.body);

        let woken = tech
            .post(
                "/v1/wakes",
                json!({ "device_ids": [fleet.inside.device_id, outside] }),
            )
            .await;
        assert_eq!(
            woken.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            woken.body
        );
        // The device out of reach is skipped as an unknown one is.
        assert_eq!(
            woken.body["skipped"],
            json!([{ "device_id": outside, "reason": "not_found" }]),
            "{name}"
        );
        let text = woken.body["wakes"].to_string();
        assert!(!text.contains(outside), "{name}: {text}");

        let history = tech.get("/v1/script-runs").await.body;
        assert!(!history.to_string().contains(outside), "{name}: {history}");
        app.finish().await;
    }
}

#[tokio::test]
async fn limited_jobs_run_only_in_reach() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let (mut tech, _) = limited_user(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "scripts.run", "jobs.run_wide"],
            &[&fleet.acme],
        )
        .await;
        let script = tech
            .post(
                "/v1/toolbox/scripts",
                json!({ "name": "Hello", "language": "powershell", "body": "echo hi", "timeout_seconds": 60, "shared": false }),
            )
            .await;
        assert_eq!(
            script.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            script.body
        );
        let script_id = script.body["id"].as_str().unwrap().to_owned();
        let tag = admin
            .post("/v1/tags", json!({ "name": "Kiosks", "color": "blue" }))
            .await;
        let tag_id = tag.body["id"].as_str().unwrap().to_owned();
        let tagged = admin
            .post(
                "/v1/device-tags",
                json!({ "device_ids": [fleet.inside.device_id, fleet.outside.device_id, fleet.loose.device_id], "add": [tag_id], "remove": [] }),
            )
            .await;
        assert_eq!(tagged.status, StatusCode::OK, "{name}");
        let job = |target: Value| {
            let mut body = json!({ "script_id": script_id, "run_as": "user", "offline": "skip" });
            body.as_object_mut()
                .unwrap()
                .extend(target.as_object().unwrap().clone());
            body
        };

        let everywhere = tech.post("/v1/jobs", job(json!({ "target": "all" }))).await;
        assert_eq!(everywhere.status, StatusCode::FORBIDDEN, "{name}");
        assert_eq!(everywhere.code(), "permission_denied");
        let elsewhere = tech
            .post(
                "/v1/jobs",
                job(json!({ "target": "companies", "company_ids": [fleet.globex] })),
            )
            .await;
        assert_eq!(elsewhere.code(), "no_devices", "{name}");
        let out_of_reach = tech
            .post(
                "/v1/jobs",
                job(json!({ "target": "devices", "device_ids": [fleet.outside.device_id] })),
            )
            .await;
        assert_eq!(out_of_reach.code(), "no_devices", "{name}");

        let mut started = Vec::new();
        for target in [
            json!({ "target": "companies", "company_ids": [fleet.acme, fleet.globex] }),
            json!({ "target": "tags", "tag_ids": [tag_id] }),
            json!({ "target": "devices", "device_ids": [fleet.inside.device_id, fleet.outside.device_id] }),
        ] {
            let created = tech.post("/v1/jobs", job(target.clone())).await;
            assert_eq!(
                created.status,
                StatusCode::CREATED,
                "{name}: {target}: {:?}",
                created.body
            );
            let id = created.body["id"].as_str().unwrap().to_owned();
            let job = common::job_with_devices(&mut tech, &id).await;
            assert_eq!(
                names(&job["devices"], "device_id"),
                std::slice::from_ref(&fleet.inside.device_id),
                "{name}: {target}"
            );
            started.push(id);
        }
        assert_eq!(
            tech.get("/v1/jobs").await.body["jobs"]
                .as_array()
                .unwrap()
                .len(),
            3,
            "{name}"
        );

        // Once its device leaves reach, a job is gone like a missing one.
        move_to(&mut admin, &fleet.inside.device_id, Some(&fleet.globex)).await;
        assert_eq!(tech.get("/v1/jobs").await.body["jobs"], json!([]), "{name}");
        for id in &started {
            for path in [format!("/v1/jobs/{id}"), format!("/v1/jobs/{id}/devices")] {
                assert_eq!(
                    tech.get(&path).await.status,
                    StatusCode::NOT_FOUND,
                    "{name}: {path}"
                );
            }
            assert_eq!(
                tech.post(&format!("/v1/jobs/{id}/cancel"), json!({}))
                    .await
                    .status,
                StatusCode::NOT_FOUND,
                "{name}"
            );
        }
        // A job on devices in reach and out shows only those in reach.
        move_to(&mut admin, &fleet.inside.device_id, Some(&fleet.acme)).await;
        let job = common::job_with_devices(&mut tech, &started[0]).await;
        assert_eq!(
            names(&job["devices"], "device_id"),
            std::slice::from_ref(&fleet.inside.device_id),
            "{name}"
        );
        move_to(&mut admin, &fleet.outside.device_id, Some(&fleet.acme)).await;
        let page = tech.get(&format!("/v1/jobs/{}/devices", started[0])).await;
        assert_eq!(page.body["devices"].as_array().unwrap().len(), 1, "{name}");
        app.finish().await;
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

fn reading(cpu: f32) -> SystemMetrics {
    SystemMetrics {
        cpu_percent: cpu,
        memory_used_bytes: 10,
        memory_total_bytes: 100,
        network_received_bytes_per_second: 0,
        network_sent_bytes_per_second: 0,
        uptime_seconds: 60,
        volumes: vec![VolumeUsage {
            name: "C:".into(),
            total_bytes: 100 * GIB,
            free_bytes: 50 * GIB,
        }],
    }
}

#[tokio::test]
async fn alerts_out_of_reach_are_hidden() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let rule = admin
            .post(
                "/v1/alert-rules",
                json!({ "name": "Busy CPU", "severity": "critical", "condition": "cpu", "threshold": 80, "duration_minutes": 1 }),
            )
            .await;
        assert_eq!(rule.status, StatusCode::CREATED, "{name}: {:?}", rule.body);
        let (mut tech, _) = limited_user(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "alerts.view", "alerts.respond"],
            &[&fleet.acme],
        )
        .await;
        let mut events = tech.events(&server).await.unwrap();
        assert_eq!(receive_json(&mut events).await["type"], "snapshot");

        let now = now_ms();
        let mut connections = Vec::new();
        for device in [&fleet.inside, &fleet.outside, &fleet.loose] {
            connections.push(app.state.agents.connect(&device.device_id));
            let mut at = now - 70 * SECOND_MS;
            while at <= now {
                app.state
                    .alerts
                    .observe(&device.device_id, at, &reading(95.0));
                at += 5 * SECOND_MS;
            }
            app.state
                .metrics
                .record(&device.device_id, MetricsReport::new(reading(95.0), true));
        }
        alerts::evaluate(&app.state, now).await.unwrap();
        let all = admin.get("/v1/alerts").await.body["alerts"].clone();
        assert_eq!(all.as_array().unwrap().len(), 3, "{name}: {all}");
        let id_on = |device: &Agent| {
            all.as_array()
                .unwrap()
                .iter()
                .find(|alert| alert["device_id"] == device.device_id.as_str())
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let (inside, outside) = (id_on(&fleet.inside), id_on(&fleet.outside));

        let listed = tech.get("/v1/alerts").await;
        assert_eq!(listed.status, StatusCode::OK, "{name}: {:?}", listed.body);
        assert_eq!(
            names(&listed.body["alerts"], "id"),
            std::slice::from_ref(&inside),
            "{name}"
        );
        let counts = tech.get("/v1/alerts/counts").await.body;
        assert_eq!(counts["open"], 1, "{name}: {counts}");
        assert_eq!(admin.get("/v1/alerts/counts").await.body["open"], 3);

        assert_eq!(
            tech.get(&format!("/v1/alerts/{inside}")).await.status,
            StatusCode::OK,
            "{name}"
        );
        let unknown = missing();
        for path in [
            |id: &str| format!("/v1/alerts/{id}"),
            |id: &str| format!("/v1/alerts/{id}/remediations"),
        ] {
            let hidden = tech.get(&path(&outside)).await;
            assert_eq!(
                hidden.status,
                StatusCode::NOT_FOUND,
                "{name}: {}",
                path(&outside)
            );
            assert_eq!(hidden.body, tech.get(&path(&unknown)).await.body, "{name}");
        }
        let acknowledged = tech
            .post(&format!("/v1/alerts/{outside}/acknowledge"), json!({}))
            .await;
        assert_eq!(acknowledged.status, StatusCode::NOT_FOUND, "{name}");
        let acknowledged = tech
            .post(&format!("/v1/alerts/{inside}/acknowledge"), json!({}))
            .await;
        assert_eq!(acknowledged.status, StatusCode::OK, "{name}");

        // The socket carries the alerts in reach, and their counts.
        let mut upserts = Vec::new();
        let mut counts = None;
        while counts.is_none() || upserts.is_empty() {
            let event = common::receive_any_json(&mut events).await;
            if event["type"] == "alert_upsert" {
                upserts.push(event["alert"]["id"].clone());
                if event["counts"]["acknowledged"] == 1 {
                    counts = Some(event["counts"].clone());
                }
            }
        }
        assert!(
            upserts.iter().all(|id| *id == inside.as_str()),
            "{name}: {upserts:?}"
        );
        assert_eq!(counts.unwrap()["open"], 0, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn enrollment_stays_in_reach() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let (mut tech, _) = limited_user(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "devices.enroll"],
            &[&fleet.acme],
        )
        .await;
        let installer = |company: Option<&str>| {
            let mut body = json!({ "platform": "windows-x64" });
            if let Some(company) = company {
                body["company_id"] = company.into();
            }
            body
        };
        let nowhere = tech.post("/v1/agent-installers", installer(None)).await;
        assert_eq!(nowhere.status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(nowhere.code(), "company_required");
        let hidden = tech
            .post("/v1/agent-installers", installer(Some(&fleet.globex)))
            .await;
        let unknown = tech
            .post("/v1/agent-installers", installer(Some(&missing())))
            .await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{name}");
        assert_eq!(hidden.body, unknown.body, "{name}");
        let issued = tech
            .post("/v1/agent-installers", installer(Some(&fleet.acme)))
            .await;
        assert_eq!(
            issued.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            issued.body
        );

        let token = |company: &str| json!({ "name": "Rollout", "company_id": company, "expires_in_days": 30 });
        let hidden = tech
            .post("/v1/deployment-tokens", token(&fleet.globex))
            .await;
        assert_eq!(
            hidden.status,
            StatusCode::NOT_FOUND,
            "{name}: {:?}",
            hidden.body
        );
        let theirs = admin
            .post("/v1/deployment-tokens", token(&fleet.globex))
            .await;
        assert_eq!(
            theirs.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            theirs.body
        );
        let theirs = theirs.body["token"]["id"].as_str().unwrap().to_owned();
        let ours = tech.post("/v1/deployment-tokens", token(&fleet.acme)).await;
        assert_eq!(ours.status, StatusCode::CREATED, "{name}: {:?}", ours.body);
        let ours = ours.body["token"]["id"].as_str().unwrap().to_owned();
        let listed = tech.get("/v1/deployment-tokens").await.body;
        assert_eq!(names(&listed["tokens"], "id"), [ours], "{name}");
        let revoked = tech
            .delete(&format!("/v1/deployment-tokens/{theirs}"))
            .await;
        assert_eq!(revoked.status, StatusCode::NOT_FOUND, "{name}");
        app.finish().await;
    }
}

fn package(id: &str, name: &str, version: &str, available: &str) -> Value {
    json!({ "source": "winget", "id": id, "name": name, "version": version, "available_version": available })
}

/// Reports the Agent's packages over a connection of its own, which stays
/// open so the server handles it.
async fn report_packages(
    server: &common::Server,
    agent: &Agent,
    packages: Vec<Value>,
) -> common::Ws {
    let mut control = agent.connect(server).await.unwrap();
    send_json(
        &mut control,
        json!({
            "type": "application_scan", "run_ids": [],
            "scan": { "scanned_at_unix_ms": 1_700_000_000_000_i64, "managers": [{ "source": "winget", "version": "1.29.380" }], "packages": packages },
        }),
    )
    .await;
    control
}

#[tokio::test]
async fn fleet_views_count_only_devices_in_reach() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let (mut tech, _) = limited_user(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "updates.view", "applications.view"],
            &[&fleet.acme],
        )
        .await;
        let firefox = package("Mozilla.Firefox", "Mozilla Firefox", "130.0", "131.0");
        let git = package("Git.Git", "Git", "2.55.0", "2.56.0");
        let mut connections = Vec::new();
        for (agent, packages) in [
            (&fleet.inside, vec![firefox.clone()]),
            (&fleet.outside, vec![firefox.clone(), git.clone()]),
            (&fleet.loose, vec![git.clone()]),
        ] {
            connections.push(report_packages(&server, agent, packages).await);
        }
        let mut outdated = Value::Null;
        for _ in 0..200 {
            outdated = admin.get("/v1/packages/outdated").await.body;
            if outdated["packages"].as_array().map(Vec::len) == Some(2)
                && outdated["packages"][0]["devices"] == 2
                && outdated["packages"][1]["devices"] == 2
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            outdated["packages"].as_array().unwrap().len(),
            2,
            "{name}: {outdated}"
        );

        let limited = tech.get("/v1/packages/outdated").await;
        assert_eq!(limited.status, StatusCode::OK, "{name}: {:?}", limited.body);
        let packages = limited.body["packages"].as_array().unwrap();
        assert_eq!(packages.len(), 1, "{name}: {}", limited.body);
        assert_eq!(packages[0]["package_id"], "Mozilla.Firefox");
        assert_eq!(packages[0]["devices"], 1);
        assert_eq!(packages[0]["available_version"], "131.0");
        let on = tech
            .get("/v1/packages/outdated/devices?package=winget:Mozilla.Firefox")
            .await
            .body;
        assert_eq!(
            names(&on["devices"], "device_id"),
            std::slice::from_ref(&fleet.inside.device_id),
            "{name}: {on}"
        );
        let on = tech
            .get("/v1/packages/outdated/devices?package=winget:Git.Git")
            .await
            .body;
        assert_eq!(on["devices"], json!([]), "{name}: {on}");

        let updates = tech.get("/v1/updates/devices").await;
        assert_eq!(updates.status, StatusCode::OK, "{name}: {:?}", updates.body);
        assert_eq!(
            names(&updates.body["devices"], "device_id"),
            std::slice::from_ref(&fleet.inside.device_id),
            "{name}: {}",
            updates.body
        );
        assert_eq!(
            admin.get("/v1/updates/devices").await.body["devices"]
                .as_array()
                .unwrap()
                .len(),
            3,
            "{name}"
        );
        for path in ["/v1/update-runs", "/v1/application-runs"] {
            let runs = tech.get(path).await;
            assert_eq!(
                runs.status,
                StatusCode::OK,
                "{name}: {path} {:?}",
                runs.body
            );
        }
        app.finish().await;
    }
}

/// Makes the queued report `id` as the server's task would, and
/// returns how many devices its hardware inventory counted.
async fn report_devices(app: &App, browser: &mut Browser, id: &str) -> String {
    meshrmm_server::reports::step(&app.state, now_ms())
        .await
        .unwrap();
    let report = browser.get(&format!("/v1/reports/{id}")).await;
    assert_eq!(report.body["status"], "succeeded", "{:?}", report.body);
    let document = meshrmm_server::reports::document(&app.state, id)
        .await
        .unwrap()
        .unwrap();
    document
        .sections
        .iter()
        .find(|section| section.id == "summary")
        .unwrap()
        .figures
        .iter()
        .find(|figure| figure.label == "Devices")
        .unwrap()
        .value
        .clone()
}

#[tokio::test]
async fn reports_cover_only_devices_in_reach() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let role = limited_role(
            &mut admin,
            "Acme reports",
            &["devices.view", "reports.view"],
            &[&fleet.acme],
        )
        .await;
        let (mut tech, _) = add_user(&app, &mut admin, "tech@example.com", &[&role]).await;
        let tag = admin
            .post("/v1/tags", json!({ "name": "Desks", "color": "blue" }))
            .await;
        let tag_id = tag.body["id"].as_str().unwrap().to_owned();
        let tagged = admin
            .post(
                "/v1/device-tags",
                json!({ "device_ids": [fleet.inside.device_id, fleet.outside.device_id, fleet.loose.device_id], "add": [tag_id], "remove": [] }),
            )
            .await;
        assert_eq!(tagged.status, StatusCode::OK, "{name}");
        let request = |target: Value| {
            let mut body = json!({ "template": "hardware_inventory", "period": "last_7_days", "time_zone": "UTC" });
            body.as_object_mut()
                .unwrap()
                .extend(target.as_object().unwrap().clone());
            body
        };

        let everywhere = tech
            .post("/v1/reports", request(json!({ "target": "all" })))
            .await;
        assert_eq!(everywhere.status, StatusCode::FORBIDDEN, "{name}");
        assert_eq!(everywhere.code(), "permission_denied");
        let hidden = tech
            .post(
                "/v1/reports",
                request(json!({ "target": "companies", "company_ids": [fleet.globex] })),
            )
            .await;
        let unknown = tech
            .post(
                "/v1/reports",
                request(json!({ "target": "companies", "company_ids": [missing()] })),
            )
            .await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{name}");
        assert_eq!(hidden.body, unknown.body, "{name}");

        let mut made = Vec::new();
        for target in [
            json!({ "target": "companies", "company_ids": [fleet.acme] }),
            json!({ "target": "tags", "tag_ids": [tag_id] }),
        ] {
            let queued = tech.post("/v1/reports", request(target.clone())).await;
            assert_eq!(
                queued.status,
                StatusCode::ACCEPTED,
                "{name}: {target}: {:?}",
                queued.body
            );
            let id = queued.body["id"].as_str().unwrap().to_owned();
            assert_eq!(
                report_devices(&app, &mut tech, &id).await,
                "1",
                "{name}: {target}"
            );
            made.push(id);
        }
        let queued = admin
            .post("/v1/reports", request(json!({ "target": "all" })))
            .await;
        let theirs = queued.body["id"].as_str().unwrap().to_owned();
        assert_eq!(
            report_devices(&app, &mut admin, &theirs).await,
            "3",
            "{name}"
        );

        // Only their own reports, and nobody's schedules.
        let mut listed = names(&tech.get("/v1/reports").await.body["reports"], "id");
        listed.sort();
        let mut expected = made.clone();
        expected.sort();
        assert_eq!(listed, expected, "{name}");
        assert_eq!(
            admin.get("/v1/reports").await.body["reports"]
                .as_array()
                .unwrap()
                .len(),
            3,
            "{name}"
        );
        for path in [
            format!("/v1/reports/{theirs}"),
            format!("/v1/reports/{theirs}/html"),
            format!("/v1/reports/{theirs}/tables/devices.csv"),
        ] {
            let hidden = tech
                .bytes(
                    tech.request(Method::GET, &path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
            assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{name}: {path}");
        }
        let created = admin
            .post(
                "/v1/report-schedules",
                json!({
                    "name": "Weekly", "template": "executive_summary", "target": "all",
                    "period": "last_7_days", "frequency": "weekly", "weekday": 1, "time": "07:00",
                    "time_zone": "Europe/London",
                }),
            )
            .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            created.body
        );
        assert_eq!(
            tech.get("/v1/report-schedules").await.body["schedules"],
            json!([]),
            "{name}"
        );

        // Reports made of Acme go once the role no longer reaches it.
        let moved = admin
            .patch(
                &format!("/v1/roles/{role}"),
                json!({ "company_ids": [fleet.globex] }),
            )
            .await;
        assert_eq!(moved.status, StatusCode::OK, "{name}");
        assert_eq!(
            tech.get("/v1/reports").await.body["reports"],
            json!([]),
            "{name}"
        );
        assert_eq!(
            tech.get(&format!("/v1/reports/{}", made[0])).await.status,
            StatusCode::NOT_FOUND,
            "{name}"
        );

        // Schedules run on any company's devices, so saving one needs
        // reports.view in every company.
        let manager = add_role(&mut admin, "Schedules", &["reports.manage"]).await;
        let (mut lead, _) =
            add_user(&app, &mut admin, "lead@example.com", &[&manager, &role]).await;
        let refused = lead
            .post(
                "/v1/report-schedules",
                json!({
                    "name": "Lead's", "template": "executive_summary", "target": "companies",
                    "company_ids": [fleet.globex], "period": "last_7_days", "frequency": "weekly",
                    "weekday": 1, "time": "07:00", "time_zone": "Europe/London",
                }),
            )
            .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{name}: {:?}",
            refused.body
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn maintenance_stays_in_reach() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let (mut tech, _) = limited_user(
            &app,
            &mut admin,
            "tech@example.com",
            &["devices.view", "devices.maintenance"],
            &[&fleet.acme],
        )
        .await;
        let tag = admin
            .post("/v1/tags", json!({ "name": "Racks", "color": "blue" }))
            .await;
        let tag_id = tag.body["id"].as_str().unwrap().to_owned();
        let tagged = admin
            .post(
                "/v1/device-tags",
                json!({ "device_ids": [fleet.inside.device_id, fleet.outside.device_id, fleet.loose.device_id], "add": [tag_id], "remove": [] }),
            )
            .await;
        assert_eq!(tagged.status, StatusCode::OK, "{name}");
        let start = |target: Value| {
            let mut body = json!({ "reason": "Rack move", "duration_minutes": 60 });
            body.as_object_mut()
                .unwrap()
                .extend(target.as_object().unwrap().clone());
            body
        };

        let everywhere = tech
            .post("/v1/maintenance-windows", start(json!({ "target": "all" })))
            .await;
        assert_eq!(everywhere.status, StatusCode::FORBIDDEN, "{name}");
        assert_eq!(everywhere.code(), "permission_denied");
        for (hidden, unknown) in [
            (
                json!({ "target": "companies", "company_ids": [fleet.globex] }),
                json!({ "target": "companies", "company_ids": [missing()] }),
            ),
            (
                json!({ "target": "devices", "device_ids": [fleet.outside.device_id] }),
                json!({ "target": "devices", "device_ids": [missing()] }),
            ),
            (
                json!({ "target": "devices", "device_ids": [fleet.inside.device_id, fleet.loose.device_id] }),
                json!({ "target": "devices", "device_ids": [fleet.inside.device_id, missing()] }),
            ),
        ] {
            let hidden = tech.post("/v1/maintenance-windows", start(hidden)).await;
            let unknown = tech.post("/v1/maintenance-windows", start(unknown)).await;
            assert_eq!(
                hidden.status,
                StatusCode::NOT_FOUND,
                "{name}: {:?}",
                hidden.body
            );
            assert_eq!(hidden.body, unknown.body, "{name}");
        }
        for target in [
            json!({ "target": "devices", "device_ids": [fleet.inside.device_id] }),
            json!({ "target": "companies", "company_ids": [fleet.acme] }),
            json!({ "target": "tags", "tag_ids": [tag_id] }),
        ] {
            let started = tech
                .post("/v1/maintenance-windows", start(target.clone()))
                .await;
            assert_eq!(
                started.status,
                StatusCode::CREATED,
                "{name}: {target}: {:?}",
                started.body
            );
            assert_eq!(started.body["created"], 1, "{name}: {target}");
            assert_eq!(
                started.body["windows"][0]["device_id"],
                fleet.inside.device_id.as_str()
            );
        }

        // Windows on devices out of reach are missing to the technician.
        let theirs = admin
            .post(
                "/v1/maintenance-windows",
                start(json!({ "target": "devices", "device_ids": [fleet.outside.device_id] })),
            )
            .await;
        assert_eq!(
            theirs.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            theirs.body
        );
        let theirs = theirs.body["windows"][0]["id"].as_str().unwrap().to_owned();
        let listed = tech.get("/v1/maintenance-windows").await.body;
        let devices: Vec<&str> = listed["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|window| window["device_id"].as_str().unwrap())
            .collect();
        assert_eq!(devices.len(), 3, "{name}: {listed}");
        assert!(
            devices.iter().all(|id| *id == fleet.inside.device_id),
            "{name}: {listed}"
        );
        assert_eq!(
            admin.get("/v1/maintenance-windows").await.body["windows"]
                .as_array()
                .unwrap()
                .len(),
            4,
            "{name}"
        );
        let unknown = missing();
        for (method, path, body) in [
            (
                Method::GET,
                format!("/v1/maintenance-windows/{theirs}"),
                None,
            ),
            (
                Method::POST,
                format!("/v1/maintenance-windows/{theirs}/end"),
                Some(json!({})),
            ),
            (
                Method::PATCH,
                format!("/v1/maintenance-windows/{theirs}"),
                Some(json!({ "reason": "Mine now" })),
            ),
        ] {
            let hidden = tech.send(method.clone(), &path, body.clone()).await;
            let missing_one = tech
                .send(method, &path.replace(&theirs, &unknown), body)
                .await;
            assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{name}: {path}");
            assert_eq!(hidden.body, missing_one.body, "{name}: {path}");
        }
        as_if_missing(
            &mut tech,
            Method::GET,
            |id| format!("/v1/agents/{id}/maintenance-windows"),
            None,
            &fleet.outside.device_id,
            name,
        )
        .await;
        // Ending maintenance on a tag ends only the windows in reach.
        let ended = tech
            .post(
                "/v1/maintenance-windows/end",
                json!({ "target": "tags", "tag_ids": [tag_id] }),
            )
            .await;
        assert_eq!(ended.status, StatusCode::OK, "{name}: {:?}", ended.body);
        assert_eq!(ended.body["ended"], 3, "{name}");
        let still = admin
            .get(&format!("/v1/maintenance-windows/{theirs}"))
            .await
            .body;
        assert_eq!(still["state"], "current", "{name}: {still}");
        app.finish().await;
    }
}

/// A request about a device or company: its method, its path for an ID, and
/// its body.
type Request = (Method, fn(&str) -> String, Option<Value>);

async fn set_field(browser: &mut Browser, path: &str, value: Value) -> Response {
    browser.put(path, json!({ "value": value })).await
}

#[tokio::test]
async fn custom_field_values_stay_in_reach() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        for body in [
            json!({ "key": "asset_tag", "label": "Asset tag", "type": "text", "applies_to": "both", "script_access": "read_write" }),
            json!({ "key": "local_admin", "label": "Local admin", "type": "secret", "applies_to": "both", "script_access": "read" }),
        ] {
            let created = admin.post("/v1/custom-fields", body.clone()).await;
            assert_eq!(
                created.status,
                StatusCode::CREATED,
                "{name}: {body}: {:?}",
                created.body
            );
        }
        for (device, tag) in [
            (&fleet.inside, "IN-1"),
            (&fleet.outside, "OUT-1"),
            (&fleet.loose, "LOOSE-1"),
        ] {
            for (key, value) in [("asset_tag", tag), ("local_admin", "hunter2")] {
                let saved = set_field(
                    &mut admin,
                    &format!("/v1/agents/{}/custom-fields/{key}", device.device_id),
                    json!(value),
                )
                .await;
                assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
            }
        }
        for company in [&fleet.acme, &fleet.globex] {
            let saved = set_field(
                &mut admin,
                &format!("/v1/companies/{company}/custom-fields/local_admin"),
                json!("company-secret"),
            )
            .await;
            assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        }
        let (mut tech, _) = limited_user(
            &app,
            &mut admin,
            "tech@example.com",
            &[
                "devices.view",
                "devices.edit",
                "scripts.run",
                "custom_fields.reveal",
            ],
            &[&fleet.acme],
        )
        .await;

        // Definitions are the same for everyone.
        let fields = tech.get("/v1/custom-fields").await;
        assert_eq!(fields.status, StatusCode::OK, "{name}");
        assert_eq!(fields.body["fields"].as_array().unwrap().len(), 2);

        // A device's values: reading, setting and revealing them.
        let values = |id: &str| format!("/v1/agents/{id}/custom-fields");
        let value = |id: &str| format!("/v1/agents/{id}/custom-fields/asset_tag");
        let reveal = |id: &str| format!("/v1/agents/{id}/custom-fields/local_admin/reveal");
        let read = tech.get(&values(&fleet.inside.device_id)).await;
        assert_eq!(read.status, StatusCode::OK, "{name}: {:?}", read.body);
        assert!(read.body.to_string().contains("IN-1"), "{name}");
        let saved = set_field(&mut tech, &value(&fleet.inside.device_id), json!("IN-2")).await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        let revealed = tech.post(&reveal(&fleet.inside.device_id), json!({})).await;
        assert_eq!(
            revealed.status,
            StatusCode::OK,
            "{name}: {:?}",
            revealed.body
        );
        assert_eq!(revealed.body["value"], "hunter2");
        for hidden in [&fleet.outside.device_id, &fleet.loose.device_id] {
            as_if_missing(&mut tech, Method::GET, values, None, hidden, name).await;
            as_if_missing(
                &mut tech,
                Method::PUT,
                value,
                Some(json!({ "value": "MINE" })),
                hidden,
                name,
            )
            .await;
            as_if_missing(
                &mut tech,
                Method::POST,
                reveal,
                Some(json!({})),
                hidden,
                name,
            )
            .await;
        }
        assert_eq!(
            audit_events(&app, "custom_field.reveal").await.len(),
            1,
            "{name}: only the reveal in reach is recorded"
        );

        // A company's values, and revealing its secret.
        let company_values = |id: &str| format!("/v1/companies/{id}/custom-fields");
        let company_reveal =
            |id: &str| format!("/v1/companies/{id}/custom-fields/local_admin/reveal");
        assert_eq!(
            tech.get(&company_values(&fleet.acme)).await.status,
            StatusCode::OK,
            "{name}"
        );
        let revealed = tech.post(&company_reveal(&fleet.acme), json!({})).await;
        assert_eq!(
            revealed.body["value"], "company-secret",
            "{name}: {:?}",
            revealed.body
        );
        let unknown = missing();
        let company_requests: [Request; 2] = [
            (Method::GET, company_values, None),
            (Method::POST, company_reveal, Some(json!({}))),
        ];
        for (method, path, body) in company_requests {
            let hidden = tech
                .send(method.clone(), &path(&fleet.globex), body.clone())
                .await;
            let missing_one = tech.send(method, &path(&unknown), body).await;
            assert_eq!(
                hidden.status,
                StatusCode::NOT_FOUND,
                "{name}: {}",
                path(&fleet.globex)
            );
            assert_eq!(hidden.body, missing_one.body, "{name}");
        }

        // Setting many devices at once, and the device wall's filter.
        let bulk =
            |ids: Vec<&str>| json!({ "field": "asset_tag", "device_ids": ids, "value": "BULK" });
        let refused = tech
            .post(
                "/v1/device-custom-fields",
                bulk(vec![&fleet.inside.device_id, &fleet.outside.device_id]),
            )
            .await;
        assert_eq!(refused.status, StatusCode::NOT_FOUND, "{name}");
        let changed = tech
            .post(
                "/v1/device-custom-fields",
                bulk(vec![&fleet.inside.device_id]),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{name}: {:?}", changed.body);
        assert_eq!(changed.body["changed"], 1);
        for field in ["asset_tag", "local_admin"] {
            let path = format!("/v1/custom-fields/{field}/devices?op=not_empty");
            let matching = tech.get(&path).await;
            assert_eq!(
                matching.body["device_ids"],
                json!([fleet.inside.device_id]),
                "{name}: {path}: {:?}",
                matching.body
            );
            assert_eq!(admin.get(&path).await.body["matching"], 3, "{name}: {path}");
        }

        // Scripts read their own device's fields, and write back only there.
        let script = tech
            .post(
                "/v1/toolbox/scripts",
                json!({ "name": "Tag", "language": "cmd", "body": "echo hi", "timeout_seconds": 60 }),
            )
            .await;
        assert_eq!(
            script.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            script.body
        );
        let script = script.body["id"].as_str().unwrap().to_owned();
        let mut connection = app.state.agents.connect(&fleet.inside.device_id);
        let started = tech
            .post(
                &format!("/v1/agents/{}/script-runs", fleet.inside.device_id),
                json!({ "script_id": script, "run_as": "system" }),
            )
            .await;
        assert_eq!(
            started.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            started.body
        );
        let run_id = started.body["id"].as_str().unwrap().to_owned();
        let Some(meshrmm_protocol_types::AgentCommand::RunScript { run }) =
            common::next_command(&mut connection).await
        else {
            panic!("{name}: expected a script run");
        };
        let granted: Vec<(String, Option<String>)> = run
            .fields
            .iter()
            .map(|field| (field.key.clone(), field.value.clone()))
            .collect();
        assert_eq!(
            granted,
            [
                ("asset_tag".to_owned(), Some("BULK".to_owned())),
                ("local_admin".to_owned(), Some("hunter2".to_owned())),
            ],
            "{name}"
        );
        let path = format!("script-runs/{run_id}/custom-fields");
        let write = json!({ "key": "asset_tag", "value": "FROM-SCRIPT" });
        let elsewhere = fleet.outside.report(&path, write.clone()).await;
        assert_eq!(
            elsewhere.status,
            StatusCode::NOT_FOUND,
            "{name}: another device's run"
        );
        let written = fleet.inside.report(&path, write).await;
        assert_eq!(
            written.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            written.body
        );
        let outside = admin.get(&values(&fleet.outside.device_id)).await.body;
        assert!(outside.to_string().contains("OUT-1"), "{name}: {outside}");

        // Viewing in one company and revealing in another reveals nowhere.
        let viewer = limited_role(&mut admin, "Acme view", &["devices.view"], &[&fleet.acme]).await;
        let revealer = limited_role(
            &mut admin,
            "Globex reveal",
            &["custom_fields.reveal"],
            &[&fleet.globex],
        )
        .await;
        let (mut split, _) =
            add_user(&app, &mut admin, "split@example.com", &[&viewer, &revealer]).await;
        for device in [&fleet.inside.device_id, &fleet.outside.device_id] {
            assert_eq!(
                split.post(&reveal(device), json!({})).await.status,
                StatusCode::NOT_FOUND,
                "{name}"
            );
        }
        // A role over every company reaches every device's values.
        let everywhere = add_role(
            &mut admin,
            "Everywhere",
            &["devices.view", "custom_fields.reveal"],
        )
        .await;
        let (mut lead, _) = add_user(&app, &mut admin, "lead@example.com", &[&everywhere]).await;
        for device in [&fleet.outside.device_id, &fleet.loose.device_id] {
            let revealed = lead.post(&reveal(device), json!({})).await;
            assert_eq!(
                revealed.status,
                StatusCode::OK,
                "{name}: {:?}",
                revealed.body
            );
        }
        drop(connection);
        app.finish().await;
    }
}

#[tokio::test]
async fn remote_tools_stay_in_reach() {
    const TOOLS: &[&str] = &[
        "devices.view",
        "tools.processes.view",
        "tools.processes.manage",
        "tools.services.view",
        "tools.services.manage",
        "tools.registry.view",
        "tools.registry.manage",
    ];
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let fleet = fleet(&app, &mut admin).await;
        let (mut tech, _) =
            limited_user(&app, &mut admin, "tech@example.com", TOOLS, &[&fleet.acme]).await;
        let everywhere = add_role(&mut admin, "Everywhere", TOOLS).await;
        let (mut lead, _) = add_user(&app, &mut admin, "lead@example.com", &[&everywhere]).await;
        let key = json!({ "root": "HKLM", "path": r"SOFTWARE\Contoso" });
        let requests: Vec<Request> = vec![
            (Method::GET, |id| format!("/v1/agents/{id}/processes"), None),
            (
                Method::POST,
                |id| format!("/v1/agents/{id}/processes/end"),
                Some(json!({ "processes": [{ "pid": 900 }] })),
            ),
            (Method::GET, |id| format!("/v1/agents/{id}/services"), None),
            (
                Method::POST,
                |id| format!("/v1/agents/{id}/services/control"),
                Some(json!({ "name": "Spooler", "action": "stop" })),
            ),
            (
                Method::POST,
                |id| format!("/v1/agents/{id}/services/startup"),
                Some(json!({ "name": "Spooler", "startup": "manual" })),
            ),
            (
                Method::GET,
                |id| format!("/v1/agents/{id}/registry?root=HKLM"),
                None,
            ),
            (
                Method::GET,
                |id| format!("/v1/agents/{id}/registry/value?root=HKLM&path=SOFTWARE&name=a"),
                None,
            ),
            (
                Method::POST,
                |id| format!("/v1/agents/{id}/registry/keys"),
                Some(key.clone()),
            ),
            (
                Method::POST,
                |id| format!("/v1/agents/{id}/registry/keys/delete"),
                Some(key.clone()),
            ),
            (
                Method::POST,
                |id| format!("/v1/agents/{id}/registry/values"),
                Some(
                    json!({ "root": "HKLM", "path": r"SOFTWARE\Contoso", "type": "dword", "data": 1 }),
                ),
            ),
            (
                Method::POST,
                |id| format!("/v1/agents/{id}/registry/values/rename"),
                Some(
                    json!({ "root": "HKLM", "path": r"SOFTWARE\Contoso", "from": "a", "to": "b" }),
                ),
            ),
            (
                Method::POST,
                |id| format!("/v1/agents/{id}/registry/values/delete"),
                Some(json!({ "root": "HKLM", "path": r"SOFTWARE\Contoso", "name": "a" })),
            ),
        ];
        for (method, path, body) in requests {
            // Found in its company and missing elsewhere; a role over every
            // company finds them all. None of the devices is online, so a
            // device found answers that it is offline, never not found.
            reached(
                &mut tech,
                method.clone(),
                path,
                body.clone(),
                &fleet.inside.device_id,
                name,
            )
            .await;
            for hidden in [&fleet.outside.device_id, &fleet.loose.device_id] {
                as_if_missing(&mut tech, method.clone(), path, body.clone(), hidden, name).await;
                reached(&mut lead, method.clone(), path, body.clone(), hidden, name).await;
            }
        }
        app.finish().await;
    }
}
