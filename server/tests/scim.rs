//! SCIM 2.0: tokens, provisioning and deprovisioning users, and groups that
//! grant roles.
mod common;

use axum::http::{Method, StatusCode};
use common::{ADMIN_EMAIL, App, Browser, ORIGIN, Response};
use serde_json::{Value, json};

struct Scim<'a> {
    app: &'a App,
    token: String,
}

impl Scim<'_> {
    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> Response {
        let request = axum::http::Request::builder()
            .method(method)
            .uri(format!("/scim/v2{path}"))
            .header("authorization", format!("Bearer {}", self.token));
        let request = match body {
            Some(body) => request
                .header("content-type", "application/scim+json")
                .body(axum::body::Body::from(body.to_string())),
            None => request.body(axum::body::Body::empty()),
        }
        .unwrap();
        let response = self.app.browser().raw(request).await;
        if response.status != StatusCode::NO_CONTENT {
            assert_eq!(
                response.headers["content-type"], "application/scim+json",
                "{:?}",
                response.body
            );
        }
        response
    }

    async fn get(&self, path: &str) -> Response {
        self.send(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Response {
        self.send(Method::POST, path, Some(body)).await
    }

    async fn put(&self, path: &str, body: Value) -> Response {
        self.send(Method::PUT, path, Some(body)).await
    }

    async fn patch(&self, path: &str, operations: Value) -> Response {
        self.send(
            Method::PATCH,
            path,
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                "Operations": operations,
            })),
        )
        .await
    }

    async fn delete(&self, path: &str) -> Response {
        self.send(Method::DELETE, path, None).await
    }
}

async fn token<'a>(app: &'a App, admin: &mut Browser) -> Scim<'a> {
    let created = admin
        .post("/v1/settings/scim/tokens", json!({ "name": "Okta" }))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    Scim {
        app,
        token: created.body["secret"].as_str().unwrap().to_owned(),
    }
}

fn user(email: &str) -> Value {
    json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
        "userName": email,
        "name": { "givenName": "Grace", "familyName": "Hopper" },
        "emails": [{ "value": email, "type": "work", "primary": true }],
        "externalId": format!("ext-{email}"),
        "active": true,
    })
}

#[tokio::test]
async fn only_administrators_hand_out_scim_tokens() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let (mut manager, _) = common::user_with(
            &app,
            &mut admin,
            "manager@example.com",
            &["authentication.manage", "users.manage", "roles.manage"],
        )
        .await;
        let refused = manager
            .post("/v1/settings/scim/tokens", json!({ "name": "Mine" }))
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{name}");
        assert_eq!(
            manager.get("/v1/settings/scim").await.status,
            StatusCode::FORBIDDEN
        );

        let scim = token(&app, &mut admin).await;
        let settings = admin.get("/v1/settings/scim").await;
        assert_eq!(
            settings.body["base_url"],
            format!("{ORIGIN}/scim/v2"),
            "{name}"
        );
        assert_eq!(settings.body["tokens"][0]["name"], "Okta");
        assert!(settings.body["tokens"][0].get("secret").is_none());
        assert_eq!(scim.get("/Users").await.status, StatusCode::OK, "{name}");
        let used = admin.get("/v1/settings/scim").await;
        assert!(used.body["tokens"][0]["last_used_at"].is_i64(), "{name}");

        let wrong = Scim {
            app: &app,
            token: "wrong".into(),
        };
        let unauthorized = wrong.get("/Users").await;
        assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(
            unauthorized.body["schemas"],
            json!(["urn:ietf:params:scim:api:messages:2.0:Error"])
        );
        // A session cookie is no SCIM credential.
        let request = admin
            .request(Method::GET, "/scim/v2/Users")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(admin.raw(request).await.status, StatusCode::UNAUTHORIZED);

        let id = settings.body["tokens"][0]["id"].as_str().unwrap();
        let revoked = admin
            .delete(&format!("/v1/settings/scim/tokens/{id}"))
            .await;
        assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{name}");
        assert_eq!(scim.get("/Users").await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            common::audit_events(&app, "scim.token_revoke").await.len(),
            1
        );

        let config = scim.get("/ServiceProviderConfig").await;
        assert_eq!(config.body["patch"]["supported"], true, "{name}");
        let types = scim.get("/ResourceTypes").await;
        assert_eq!(types.body["totalResults"], 2, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn scim_provisions_finds_and_updates_users() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let scim = token(&app, &mut admin).await;

        let created = scim.post("/Users", user("Grace@Example.com")).await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            created.body
        );
        let id = created.body["id"].as_str().unwrap().to_owned();
        assert_eq!(
            created.headers["location"],
            format!("{ORIGIN}/scim/v2/Users/{id}").as_str()
        );
        assert_eq!(created.body["userName"], "grace@example.com");
        assert_eq!(created.body["displayName"], "Grace Hopper");
        assert_eq!(created.body["externalId"], "ext-Grace@Example.com");
        assert_eq!(created.body["active"], true);

        let duplicate = scim.post("/Users", user("grace@example.com")).await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(duplicate.body["scimType"], "uniqueness");
        let invalid = scim.post("/Users", json!({ "userName": "grace" })).await;
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(invalid.body["scimType"], "invalidValue");

        let found = scim
            .get("/Users?filter=userName%20eq%20%22GRACE%40example.com%22")
            .await;
        assert_eq!(found.body["totalResults"], 1, "{name}: {:?}", found.body);
        assert_eq!(found.body["Resources"][0]["id"], id.as_str());
        let by_external_id = scim
            .get("/Users?filter=externalId%20eq%20%22ext-Grace%40Example.com%22")
            .await;
        assert_eq!(by_external_id.body["totalResults"], 1, "{name}");
        let none = scim
            .get("/Users?filter=userName%20eq%20%22nobody%40example.com%22")
            .await;
        assert_eq!(none.body["totalResults"], 0, "{name}");
        assert_eq!(none.body["Resources"], json!([]));
        let bad = scim.get("/Users?filter=userName%20gt%20%22a%22").await;
        assert_eq!(bad.body["scimType"], "invalidFilter", "{name}");
        let deep = format!(
            "/Users?filter={}userName%20pr{}",
            "(".repeat(2000),
            ")".repeat(2000)
        );
        assert_eq!(
            scim.get(&deep).await.body["scimType"],
            "invalidFilter",
            "{name}"
        );
        let non_ascii = scim.get("/Users?filter=abc%C3%A9%20pr").await;
        assert_eq!(non_ascii.status, StatusCode::OK, "{name}");
        let paged = scim.get("/Users?startIndex=2&count=1").await;
        assert_eq!(paged.body["totalResults"], 2, "{name}");
        assert_eq!(paged.body["itemsPerPage"], 1);
        assert_eq!(paged.body["startIndex"], 2);

        // Okta replaces the whole user; Entra patches pieces of it.
        let mut replacement = user("grace.hopper@example.com");
        replacement["displayName"] = json!("Admiral Hopper");
        let replaced = scim.put(&format!("/Users/{id}"), replacement).await;
        assert_eq!(
            replaced.status,
            StatusCode::OK,
            "{name}: {:?}",
            replaced.body
        );
        assert_eq!(replaced.body["userName"], "grace.hopper@example.com");
        let patched = scim
            .patch(
                &format!("/Users/{id}"),
                json!([
                    { "op": "Replace", "path": "displayName", "value": "Grace B. Hopper" },
                    { "op": "Add", "path": "externalId", "value": "entra-1" },
                    { "op": "Add", "path": "title", "value": "ignored" },
                ]),
            )
            .await;
        assert_eq!(patched.status, StatusCode::OK, "{name}: {:?}", patched.body);
        assert_eq!(patched.body["displayName"], "Grace B. Hopper");
        assert_eq!(patched.body["externalId"], "entra-1");
        assert!(patched.body.get("title").is_none());

        let view = admin.get(&format!("/v1/users/{id}")).await;
        assert_eq!(view.body["scim_managed"], true, "{name}");
        assert_eq!(view.body["has_password"], false);
        assert_eq!(view.body["email"], "grace.hopper@example.com");
        let events = common::audit_events(&app, "scim.user_update").await;
        assert_eq!(events[0].actor_label, "SCIM (Okta)", "{name}");
        assert_eq!(events[0].actor_user_id, None);
        assert_eq!(events[0].metadata["display_name"], "Grace B. Hopper");

        let deleted = scim.delete(&format!("/Users/{id}")).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        let gone = scim.get(&format!("/Users/{id}")).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND, "{name}");
        assert_eq!(gone.body["status"], "404");
        app.finish().await;
    }
}

#[tokio::test]
async fn deprovisioning_ends_access_but_keeps_an_administrator() {
    for app in common::apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = common::set_up(&app).await;
        let scim = token(&app, &mut admin).await;
        let (mut tech, tech_id) =
            common::add_user(&app, &mut admin, "tech@example.com", &["technician"]).await;
        let mut socket = tech.events(&server).await.unwrap();
        common::receive_json(&mut socket).await;

        // Entra ID sends the flag as a string.
        let disabled = scim
            .patch(
                &format!("/Users/{tech_id}"),
                json!([{ "op": "Replace", "path": "active", "value": "False" }]),
            )
            .await;
        assert_eq!(
            disabled.status,
            StatusCode::OK,
            "{name}: {:?}",
            disabled.body
        );
        assert_eq!(disabled.body["active"], false);
        assert_eq!(common::receive_close(&mut socket).await, 4001, "{name}");
        let account = tech.get("/v1/account").await;
        assert_eq!(account.status, StatusCode::UNAUTHORIZED, "{name}");
        let enabled = scim
            .patch(
                &format!("/Users/{tech_id}"),
                json!([{ "op": "replace", "value": { "active": true } }]),
            )
            .await;
        assert_eq!(enabled.body["active"], true, "{name}");

        // The last enabled administrator stays.
        let found = scim
            .get(&format!(
                "/Users?filter=userName%20eq%20%22{}%22",
                ADMIN_EMAIL.replace('@', "%40")
            ))
            .await;
        let admin_id = found.body["Resources"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let refused = scim
            .patch(
                &format!("/Users/{admin_id}"),
                json!([{ "op": "replace", "path": "active", "value": false }]),
            )
            .await;
        assert_last_administrator(name, &refused);
        let refused = scim.delete(&format!("/Users/{admin_id}")).await;
        assert_last_administrator(name, &refused);
        assert_eq!(admin.get("/v1/account").await.status, StatusCode::OK);
        app.finish().await;
    }
}

/// A SCIM change refused because it would leave no enabled administrator.
#[track_caller]
fn assert_last_administrator(name: &str, refused: &Response) {
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{name}: {:?}",
        refused.body
    );
    assert_eq!(refused.body["scimType"], "mutability", "{name}");
    assert!(
        refused.body["detail"]
            .as_str()
            .unwrap()
            .contains("without an enabled administrator"),
        "{name}: {:?}",
        refused.body
    );
}

fn group(display_name: &str, member_ids: &[&str]) -> Value {
    json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
        "displayName": display_name,
        "members": member_ids
            .iter()
            .map(|id| json!({ "value": id }))
            .collect::<Vec<_>>(),
    })
}

/// Creates a group of `member_ids` whose members hold the Administrator
/// role, and returns its ID.
async fn administrators_group(
    scim: &Scim<'_>,
    admin: &mut Browser,
    display_name: &str,
    member_ids: &[&str],
) -> String {
    let created = scim.post("/Groups", group(display_name, member_ids)).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let id = created.body["id"].as_str().unwrap().to_owned();
    let mapped = admin
        .patch(
            &format!("/v1/settings/scim/groups/{id}"),
            json!({ "role_id": "administrator" }),
        )
        .await;
    assert_eq!(mapped.status, StatusCode::OK, "{:?}", mapped.body);
    id
}

async fn member_count(scim: &Scim<'_>, group_id: &str) -> usize {
    let group = scim.get(&format!("/Groups/{group_id}")).await;
    assert_eq!(group.status, StatusCode::OK, "{:?}", group.body);
    group.body["members"].as_array().unwrap().len()
}

fn members_change(op: &str, user_id: &str) -> Value {
    json!([{ "op": op, "path": "members", "value": [{ "value": user_id }] }])
}

#[tokio::test]
async fn scim_keeps_an_administrator_who_holds_the_role_through_a_group() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let admin_id = admin.get("/v1/account").await.body["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let scim = token(&app, &mut admin).await;
        let (mut second, second_id) =
            common::add_user(&app, &mut admin, "second@example.com", &["technician"]).await;
        let admins = administrators_group(&scim, &mut admin, "Admins", &[&second_id]).await;
        let backup = administrators_group(&scim, &mut admin, "Backup admins", &[]).await;

        // An administrator through a group counts for the direct paths: the
        // first administrator may step down.
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

        // Now the second user is the only administrator, through Admins. A
        // disabled member of another administrators' group doesn't count.
        let (_, dormant_id) = common::add_user(&app, &mut second, "dormant@example.com", &[]).await;
        let added = scim
            .patch(
                &format!("/Groups/{backup}"),
                members_change("Add", &dormant_id),
            )
            .await;
        assert_eq!(added.status, StatusCode::OK, "{name}: {:?}", added.body);
        let disabled = scim
            .patch(
                &format!("/Users/{dormant_id}"),
                json!([{ "op": "replace", "path": "active", "value": false }]),
            )
            .await;
        assert_eq!(
            disabled.status,
            StatusCode::OK,
            "{name}: {:?}",
            disabled.body
        );

        let removed = scim
            .patch(
                &format!("/Groups/{admins}"),
                members_change("Remove", &second_id),
            )
            .await;
        assert_last_administrator(name, &removed);
        let emptied = scim
            .put(&format!("/Groups/{admins}"), group("Admins", &[]))
            .await;
        assert_last_administrator(name, &emptied);
        let deleted = scim.delete(&format!("/Groups/{admins}")).await;
        assert_last_administrator(name, &deleted);
        let deactivated = scim
            .patch(
                &format!("/Users/{second_id}"),
                json!([{ "op": "replace", "path": "active", "value": false }]),
            )
            .await;
        assert_last_administrator(name, &deactivated);
        let deprovisioned = scim.delete(&format!("/Users/{second_id}")).await;
        assert_last_administrator(name, &deprovisioned);
        let unmapped = second
            .patch(
                &format!("/v1/settings/scim/groups/{admins}"),
                json!({ "role_id": null }),
            )
            .await;
        assert_eq!(unmapped.code(), "last_administrator", "{name}");
        // A deleted member of an administrators' group doesn't count either.
        let deleted = scim.delete(&format!("/Users/{dormant_id}")).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        let deleted = scim.delete(&format!("/Groups/{admins}")).await;
        assert_last_administrator(name, &deleted);
        // Nothing changed.
        assert_eq!(member_count(&scim, &admins).await, 1, "{name}");
        let account = second.get("/v1/account").await;
        assert_eq!(account.body["is_administrator"], true, "{name}");
        assert!(
            common::audit_events(&app, "scim.group_update")
                .await
                .iter()
                .all(|event| event.metadata.get("members_removed").is_none()),
            "{name}"
        );
        assert!(
            common::audit_events(&app, "scim.group_delete")
                .await
                .is_empty(),
            "{name}"
        );

        // With an administrator through another group, the same change goes
        // through.
        let added = scim
            .patch(
                &format!("/Groups/{backup}"),
                members_change("Add", &admin_id),
            )
            .await;
        assert_eq!(added.status, StatusCode::OK, "{name}: {:?}", added.body);
        let removed = scim
            .patch(
                &format!("/Groups/{admins}"),
                members_change("Remove", &second_id),
            )
            .await;
        assert_eq!(removed.status, StatusCode::OK, "{name}: {:?}", removed.body);
        assert_eq!(member_count(&scim, &admins).await, 0, "{name}");
        let account = second.get("/v1/account").await;
        assert_eq!(account.body["is_administrator"], false, "{name}");

        // The first administrator now holds the role only through Backup
        // admins; given it directly too, the group can go.
        let deleted = scim.delete(&format!("/Groups/{backup}")).await;
        assert_last_administrator(name, &deleted);
        let restored = admin
            .patch(
                &format!("/v1/users/{admin_id}"),
                json!({ "role_ids": ["administrator"] }),
            )
            .await;
        assert_eq!(
            restored.status,
            StatusCode::OK,
            "{name}: {:?}",
            restored.body
        );
        let deleted = scim.delete(&format!("/Groups/{backup}")).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn concurrent_group_deletions_keep_an_administrator() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let admin_id = admin.get("/v1/account").await.body["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let scim = token(&app, &mut admin).await;
        let (_, second_id) =
            common::add_user(&app, &mut admin, "second@example.com", &["technician"]).await;
        let first = administrators_group(&scim, &mut admin, "First", &[&admin_id]).await;
        let second = administrators_group(&scim, &mut admin, "Second", &[&second_id]).await;
        let stepped_down = admin
            .patch(
                &format!("/v1/users/{admin_id}"),
                json!({ "role_ids": ["technician"] }),
            )
            .await;
        assert_eq!(stepped_down.status, StatusCode::OK, "{name}");

        // Either deletion alone leaves an administrator; together they would
        // leave none, so the second to run sees the first and is refused.
        let (first, second) = (format!("/Groups/{first}"), format!("/Groups/{second}"));
        let (a, b) = tokio::join!(scim.delete(&first), scim.delete(&second));
        let mut statuses = [a.status, b.status];
        statuses.sort();
        assert_eq!(
            statuses,
            [StatusCode::NO_CONTENT, StatusCode::BAD_REQUEST],
            "{name}: {:?} {:?}",
            a.body,
            b.body
        );
        let remaining = scim.get("/Groups").await;
        assert_eq!(remaining.body["totalResults"], 1, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn scim_groups_grant_the_role_an_administrator_maps() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let scim = token(&app, &mut admin).await;
        let grace = scim.post("/Users", user("grace@example.com")).await;
        let grace_id = grace.body["id"].as_str().unwrap().to_owned();
        let alan = scim.post("/Users", user("alan@example.com")).await;
        let alan_id = alan.body["id"].as_str().unwrap().to_owned();

        let group = scim
            .post(
                "/Groups",
                json!({
                    "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
                    "displayName": "Technicians",
                    "externalId": "g-1",
                    "members": [{ "value": grace_id }],
                }),
            )
            .await;
        assert_eq!(
            group.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            group.body
        );
        let group_id = group.body["id"].as_str().unwrap().to_owned();
        assert_eq!(group.body["members"][0]["display"], "grace@example.com");
        let unknown = scim
            .post(
                "/Groups",
                json!({ "displayName": "Ghosts", "members": [{ "value": "no-such-user" }] }),
            )
            .await;
        assert_eq!(unknown.status, StatusCode::BAD_REQUEST, "{name}");
        let duplicate = scim
            .post("/Groups", json!({ "displayName": "technicians" }))
            .await;
        assert_eq!(duplicate.body["scimType"], "uniqueness", "{name}");

        // Until an administrator maps it, the group grants nothing.
        let view = admin.get(&format!("/v1/users/{grace_id}")).await;
        assert_eq!(view.body["group_roles"], json!([]), "{name}");
        let settings = admin.get("/v1/settings/scim").await;
        assert_eq!(settings.body["groups"][0]["member_count"], 1, "{name}");
        assert_eq!(settings.body["groups"][0]["role_id"], Value::Null);
        let mapped = admin
            .patch(
                &format!("/v1/settings/scim/groups/{group_id}"),
                json!({ "role_id": "technician" }),
            )
            .await;
        assert_eq!(mapped.status, StatusCode::OK, "{name}: {:?}", mapped.body);
        let view = admin.get(&format!("/v1/users/{grace_id}")).await;
        assert_eq!(
            view.body["group_roles"],
            json!([{ "id": "technician", "name": "Technician", "source": "scim", "group": "Technicians" }]),
            "{name}"
        );
        assert_eq!(view.body["roles"], json!([]));

        // Entra adds and removes members by patch.
        let patched = scim
            .patch(
                &format!("/Groups/{group_id}"),
                json!([
                    { "op": "Add", "path": "members", "value": [{ "value": alan_id }] },
                    { "op": "Remove", "path": "members", "value": [{ "value": grace_id }] },
                ]),
            )
            .await;
        assert_eq!(patched.status, StatusCode::OK, "{name}: {:?}", patched.body);
        let members = patched.body["members"].as_array().unwrap();
        assert_eq!(members.len(), 1, "{name}");
        assert_eq!(members[0]["value"], alan_id.as_str());
        let view = admin.get(&format!("/v1/users/{grace_id}")).await;
        assert_eq!(view.body["group_roles"], json!([]), "{name}");
        let alan_view = admin.get(&format!("/v1/users/{alan_id}")).await;
        assert_eq!(
            alan_view.body["group_roles"][0]["id"], "technician",
            "{name}"
        );
        let user = scim.get(&format!("/Users/{alan_id}")).await;
        assert_eq!(user.body["groups"][0]["display"], "Technicians", "{name}");

        // Entra checks membership with a filter and without member lists.
        let check = scim
            .get(&format!(
                "/Groups?filter=id%20eq%20%22{group_id}%22%20and%20members%5Bvalue%20eq%20%22{alan_id}%22%5D&excludedAttributes=members"
            ))
            .await;
        assert_eq!(check.body["totalResults"], 1, "{name}: {:?}", check.body);
        assert!(check.body["Resources"][0].get("members").is_none());
        let by_name = scim
            .get("/Groups?filter=displayName%20eq%20%22technicians%22")
            .await;
        assert_eq!(by_name.body["totalResults"], 1, "{name}");
        let renamed = scim
            .patch(
                &format!("/Groups/{group_id}"),
                json!([{ "op": "replace", "value": { "displayName": "IT" } }]),
            )
            .await;
        assert_eq!(renamed.body["displayName"], "IT", "{name}");

        let roles = admin.get("/v1/roles").await;
        let technician = roles
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|role| role["id"] == "technician")
            .unwrap();
        assert_eq!(technician["member_count"], 1, "{name}");

        let deleted = scim.delete(&format!("/Groups/{group_id}")).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        let alan_view = admin.get(&format!("/v1/users/{alan_id}")).await;
        assert_eq!(alan_view.body["group_roles"], json!([]), "{name}");
        assert_eq!(
            common::audit_events(&app, "scim.group_delete").await.len(),
            1
        );
        app.finish().await;
    }
}

/// A member of the group, signed in to the website with an open event
/// socket, who may see devices only through the group's role.
async fn member_watching(
    app: &App,
    admin: &mut Browser,
    scim: &Scim<'_>,
    server: &common::Server,
    group_id: &str,
) -> (common::Ws, std::time::Instant, String) {
    let email = format!("{}@example.com", common::random_hex(4));
    let (tech, tech_id) = common::add_user(app, admin, &email, &[]).await;
    let added = scim
        .patch(
            &format!("/Groups/{group_id}"),
            json!([{ "op": "Add", "path": "members", "value": [{ "value": tech_id }] }]),
        )
        .await;
    assert_eq!(added.status, StatusCode::OK, "{:?}", added.body);
    let mut socket = tech.events(server).await.unwrap();
    let opened = std::time::Instant::now();
    common::receive_json(&mut socket).await;
    (socket, opened, tech_id)
}

#[tokio::test]
async fn losing_a_groups_role_closes_its_members_sockets_at_once() {
    // Well before the socket's own check, every 30 seconds.
    let prompt = std::time::Duration::from_secs(30);
    for app in common::apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = common::set_up(&app).await;
        let scim = token(&app, &mut admin).await;
        let group = scim
            .post(
                "/Groups",
                json!({
                    "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
                    "displayName": "Technicians",
                }),
            )
            .await;
        assert_eq!(
            group.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            group.body
        );
        let group_id = group.body["id"].as_str().unwrap().to_owned();
        let map = async |admin: &mut Browser, role_id: Value| {
            let mapped = admin
                .patch(
                    &format!("/v1/settings/scim/groups/{group_id}"),
                    json!({ "role_id": role_id }),
                )
                .await;
            assert_eq!(mapped.status, StatusCode::OK, "{name}: {:?}", mapped.body);
        };
        map(&mut admin, json!("technician")).await;

        // The provider removes them from the group.
        let (mut socket, opened, tech_id) =
            member_watching(&app, &mut admin, &scim, &server, &group_id).await;
        let removed = scim
            .patch(
                &format!("/Groups/{group_id}"),
                json!([{ "op": "Remove", "path": "members", "value": [{ "value": tech_id }] }]),
            )
            .await;
        assert_eq!(removed.status, StatusCode::OK, "{name}: {:?}", removed.body);
        assert_eq!(common::receive_close(&mut socket).await, 4001, "{name}");
        assert!(opened.elapsed() < prompt, "{name}");

        // An administrator stops the group granting a role.
        let (mut socket, opened, _) =
            member_watching(&app, &mut admin, &scim, &server, &group_id).await;
        map(&mut admin, Value::Null).await;
        assert_eq!(common::receive_close(&mut socket).await, 4001, "{name}");
        assert!(opened.elapsed() < prompt, "{name}");
        map(&mut admin, json!("technician")).await;

        // The provider deletes the group.
        let (mut socket, opened, _) =
            member_watching(&app, &mut admin, &scim, &server, &group_id).await;
        let deleted = scim.delete(&format!("/Groups/{group_id}")).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        assert_eq!(common::receive_close(&mut socket).await, 4001, "{name}");
        assert!(opened.elapsed() < prompt, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn scim_changes_apply_to_the_members_next_request() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let scim = token(&app, &mut admin).await;
        let (mut tech, tech_id) = common::add_user(&app, &mut admin, "tech@example.com", &[]).await;
        let group = scim
            .post(
                "/Groups",
                json!({
                    "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
                    "displayName": "Technicians",
                    "members": [{ "value": tech_id }],
                }),
            )
            .await;
        assert_eq!(
            group.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            group.body
        );
        let group_id = group.body["id"].as_str().unwrap().to_owned();
        let mapped = admin
            .patch(
                &format!("/v1/settings/scim/groups/{group_id}"),
                json!({ "role_id": "technician" }),
            )
            .await;
        assert_eq!(mapped.status, StatusCode::OK, "{name}: {:?}", mapped.body);
        assert_eq!(
            tech.get("/v1/agents").await.status,
            StatusCode::OK,
            "{name}"
        );
        assert_eq!(
            tech.get("/v1/agents").await.status,
            StatusCode::OK,
            "{name}"
        );

        // The provider renames the user.
        let renamed = scim
            .patch(
                &format!("/Users/{tech_id}"),
                json!([{ "op": "Replace", "path": "displayName", "value": "Tess Tech" }]),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{name}: {:?}", renamed.body);
        let account = tech.get("/v1/account").await;
        assert_eq!(account.body["user"]["display_name"], "Tess Tech", "{name}");

        // The provider takes the user out of the group.
        let removed = scim
            .patch(
                &format!("/Groups/{group_id}"),
                json!([{ "op": "Remove", "path": "members", "value": [{ "value": tech_id }] }]),
            )
            .await;
        assert_eq!(removed.status, StatusCode::OK, "{name}: {:?}", removed.body);
        assert_eq!(
            tech.get("/v1/agents").await.status,
            StatusCode::FORBIDDEN,
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn a_patch_path_too_deep_for_any_attribute_is_refused() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let scim = token(&app, &mut admin).await;
        let created = scim.post("/Users", user("grace@example.com")).await;
        assert_eq!(created.status, StatusCode::CREATED, "{name}");
        let id = created.body["id"].as_str().unwrap().to_owned();

        // A million bytes of `a.a.a…`, well inside the request size limit.
        let deep = vec!["a"; 500_000].join(".");
        for operations in [
            json!([{ "op": "replace", "path": deep, "value": "x" }]),
            json!([{ "op": "add", "value": { deep.clone(): "x" } }]),
        ] {
            let refused = scim.patch(&format!("/Users/{id}"), operations).await;
            assert_eq!(
                refused.status,
                StatusCode::BAD_REQUEST,
                "{name}: {:?}",
                refused.body
            );
            assert_eq!(refused.body["scimType"], "invalidPath", "{name}");
        }

        // The server is still up, and the user as it was.
        let user = scim.get(&format!("/Users/{id}")).await;
        assert_eq!(user.status, StatusCode::OK, "{name}");
        assert_eq!(user.body["displayName"], "Grace Hopper", "{name}");
        app.finish().await;
    }
}

/// `text` as a query parameter's value.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            byte => format!("%{byte:02X}"),
        })
        .collect()
}

/// A list's response and its resources, checked for success.
async fn resources(scim: &Scim<'_>, path: &str) -> (Value, Vec<Value>) {
    let response = scim.get(path).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{path}: {:?}",
        response.body
    );
    let resources = response.body["Resources"].as_array().unwrap().clone();
    (response.body, resources)
}

fn ids(resources: &[Value]) -> Vec<String> {
    resources
        .iter()
        .map(|resource| resource["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn create(scim: &Scim<'_>, path: &str, body: Value) -> String {
    let created = scim.post(path, body).await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "{path}: {:?}",
        created.body
    );
    created.body["id"].as_str().unwrap().to_owned()
}

/// What `filter` finds, checked three ways: as the list runs it (in SQL
/// where it can), wrapped in `not (not (...))` so the list evaluates all of
/// it on every resource, and by the evaluator over the unfiltered list.
async fn agree(scim: &Scim<'_>, endpoint: &str, everything: &[Value], filter: &str) -> usize {
    let parsed = meshrmm_server::scim::filter::parse(filter).unwrap();
    let expected = everything
        .iter()
        .filter(|resource| meshrmm_server::scim::filter::matches(resource, &parsed))
        .cloned()
        .collect::<Vec<_>>();
    let (listed, found) = resources(
        scim,
        &format!("{endpoint}?count=1000&filter={}", encode(filter)),
    )
    .await;
    let evaluated = format!("not (not ({filter}))");
    let (evaluated, _) = resources(
        scim,
        &format!("{endpoint}?count=1000&filter={}", encode(&evaluated)),
    )
    .await;
    assert_eq!(ids(&found), ids(&expected), "{filter}");
    assert_eq!(found, expected, "{filter}");
    assert_eq!(listed, evaluated, "{filter}");
    assert_eq!(listed["totalResults"], expected.len(), "{filter}");
    expected.len()
}

fn with_external_id(email: &str, external_id: Option<&str>) -> Value {
    let mut user = user(email);
    match external_id {
        Some(external_id) => user["externalId"] = json!(external_id),
        None => {
            user.as_object_mut().unwrap().remove("externalId");
        }
    }
    user
}

fn scim_group(name: &str, external_id: Option<&str>, members: &[&String]) -> Value {
    let mut group = json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
        "displayName": name,
        "members": members.iter().map(|id| json!({ "value": id })).collect::<Vec<_>>(),
    });
    if let Some(external_id) = external_id {
        group["externalId"] = json!(external_id);
    }
    group
}

#[tokio::test]
async fn lookups_in_the_database_find_what_the_filter_evaluator_finds() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let scim = token(&app, &mut admin).await;
        let ada = with_external_id("ada@example.com", Some("ext-1"));
        let ada = create(&scim, "/Users", ada).await;
        let lovelace = with_external_id("Ada.Lovelace@Example.com", Some("EXT-1"));
        let lovelace = create(&scim, "/Users", lovelace).await;
        let adam = with_external_id("adam@example.com", Some("ext-10"));
        let adam = create(&scim, "/Users", adam).await;
        let bob = create(&scim, "/Users", with_external_id("bob@example.org", None)).await;
        let admins = scim_group("Admins", Some("g-1"), &[&ada, &adam]);
        let admins = create(&scim, "/Groups", admins).await;
        for (display_name, external_id, members) in [
            ("Admins Team", Some("G-1"), vec![&ada]),
            ("Administrators", None, vec![&bob]),
            ("admin", None, vec![]),
            ("Équipe", Some("g-2"), vec![&lovelace]),
            // A Kelvin sign, which lowercases to an ASCII k.
            ("\u{212A}elvin", None, vec![]),
        ] {
            create(
                &scim,
                "/Groups",
                scim_group(display_name, external_id, &members),
            )
            .await;
        }

        let (_, users) = resources(&scim, "/Users?count=1000").await;
        assert_eq!(users.len(), 5, "{name}");
        let (_, groups) = resources(&scim, "/Groups?count=1000").await;
        assert_eq!(groups.len(), 6, "{name}");
        let user_filters = [
            (r#"userName eq "ADA@example.com""#.to_owned(), 1),
            (r#"userName eq "ada@example""#.to_owned(), 0),
            (
                r#"urn:ietf:params:scim:schemas:core:2.0:User:userName eq "adam@example.com""#
                    .to_owned(),
                1,
            ),
            (
                r#"emails.value eq "Ada.Lovelace@example.COM""#.to_owned(),
                1,
            ),
            (r#"emails[value eq "adam@EXAMPLE.com"]"#.to_owned(), 1),
            (
                r#"emails[type eq "Work" and value eq "bob@example.org"]"#.to_owned(),
                1,
            ),
            (r#"externalId eq "ext-1""#.to_owned(), 1),
            (r#"externalId eq "EXT-1""#.to_owned(), 1),
            (r#"externalId eq "ext""#.to_owned(), 0),
            (format!(r#"id eq "{ada}""#), 1),
            (format!(r#"id eq "{}""#, ada.to_uppercase()), 0),
            (
                r#"userName eq "ada@example.com" and externalId eq "ext-1""#.to_owned(),
                1,
            ),
            (
                r#"userName eq "ada@example.com" and externalId eq "EXT-1""#.to_owned(),
                0,
            ),
            // The rest is evaluated on the users SQL selects, or on all.
            (
                r#"externalId eq "ext-1" and userName sw "ada""#.to_owned(),
                1,
            ),
            (r#"userName sw "ada""#.to_owned(), 3),
            (
                r#"userName co "lovelace" or externalId eq "ext-10""#.to_owned(),
                2,
            ),
            ("not (externalId pr)".to_owned(), 2),
            (r#"groups[display eq "admins"]"#.to_owned(), 2),
            (
                r#"userName eq "ada@example.com" and groups[display eq "Admins Team"]"#.to_owned(),
                1,
            ),
            (
                r#"emails[type eq "home" and value eq "ada@example.com"]"#.to_owned(),
                0,
            ),
        ];
        for (filter, found) in user_filters {
            let matched = agree(&scim, "/Users", &users, &filter).await;
            assert_eq!(matched, found, "{name}: {filter}");
        }
        let group_filters = [
            (r#"displayName eq "admins""#.to_owned(), 1),
            (r#"displayName eq "ADMIN""#.to_owned(), 1),
            (r#"displayName eq "Admins Tea""#.to_owned(), 0),
            (r#"displayName eq "ÉQUIPE""#.to_owned(), 1),
            (r#"displayName eq "équipe""#.to_owned(), 1),
            (r#"displayName eq "kelvin""#.to_owned(), 1),
            (r#"externalId eq "g-1""#.to_owned(), 1),
            (r#"externalId eq "G-1""#.to_owned(), 1),
            (format!(r#"id eq "{admins}""#), 1),
            (format!(r#"members[value eq "{ada}"]"#), 2),
            (format!(r#"members.value eq "{bob}""#), 1),
            (format!(r#"members[value eq "{}"]"#, ada.to_uppercase()), 0),
            (
                format!(r#"id eq "{admins}" and members[value eq "{adam}"]"#),
                1,
            ),
            (
                format!(r#"id eq "{admins}" and members[value eq "{bob}"]"#),
                0,
            ),
            (
                format!(r#"displayName eq "Admins" and members[value eq "{ada}"]"#),
                1,
            ),
            // The rest is evaluated on the groups SQL selects, or on all.
            (r#"displayName sw "admin""#.to_owned(), 4),
            ("members pr".to_owned(), 4),
            ("not (members pr)".to_owned(), 2),
            (
                r#"displayName co "team" or externalId eq "g-2""#.to_owned(),
                2,
            ),
            (
                format!(r#"members[value eq "{ada}"] and displayName sw "admins ""#),
                1,
            ),
            (
                r#"members[display eq "ada.lovelace@example.com"]"#.to_owned(),
                1,
            ),
        ];
        for (filter, found) in group_filters {
            let matched = agree(&scim, "/Groups", &groups, &filter).await;
            assert_eq!(matched, found, "{name}: {filter}");
        }
        app.finish().await;
    }
}

/// What one request read: its statements, less the token's `last_used_at`
/// write, and the rows they returned.
async fn cost(app: &App, scim: &Scim<'_>, path: &str) -> ((u64, u64), Value) {
    let (counts, rows) = (app.db().counts(), app.db().rows());
    let response = scim.get(path).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{path}: {:?}",
        response.body
    );
    let counts = app.db().counts() - counts;
    (
        (counts.statements - counts.writes, app.db().rows() - rows),
        response.body,
    )
}

/// `count` more users and groups, made after the others, each group with
/// two of the new users.
async fn add_many(app: &App, count: usize) {
    use meshrmm_server::db::tables::{ScimGroupMembers, ScimGroups, Users};
    use sea_query::Query;
    let later = meshrmm_server::time::now_ms() + 1_000_000_000;
    let mut users = Query::insert();
    users.into_table(Users::Table).columns([
        Users::Id,
        Users::Email,
        Users::DisplayName,
        Users::ScimExternalId,
        Users::CreatedAt,
        Users::UpdatedAt,
    ]);
    let mut groups = Query::insert();
    groups.into_table(ScimGroups::Table).columns([
        ScimGroups::Id,
        ScimGroups::DisplayName,
        ScimGroups::DisplayNameKey,
        ScimGroups::CreatedAt,
        ScimGroups::UpdatedAt,
    ]);
    let mut members = Query::insert();
    members
        .into_table(ScimGroupMembers::Table)
        .columns([ScimGroupMembers::GroupId, ScimGroupMembers::UserId]);
    for n in 0..count {
        let at = later + i64::try_from(n).unwrap();
        users.values_panic([
            format!("many-{n:05}").into(),
            format!("many-{n:05}@example.com").into(),
            format!("Many {n}").into(),
            format!("ext-many-{n:05}").into(),
            at.into(),
            at.into(),
        ]);
        groups.values_panic([
            format!("team-{n:05}").into(),
            format!("Team {n:05}").into(),
            format!("team {n:05}").into(),
            at.into(),
            at.into(),
        ]);
        for member in [n, (n + 1) % count] {
            members.values_panic([
                format!("team-{n:05}").into(),
                format!("many-{member:05}").into(),
            ]);
        }
    }
    for insert in [users, groups, members] {
        app.db().execute(&insert).await.unwrap();
    }
}

#[tokio::test]
async fn a_lookup_reads_as_much_with_1200_users_and_groups_as_with_5() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let scim = token(&app, &mut admin).await;
        let mut users = Vec::new();
        for n in 0..4 {
            users.push(create(&scim, "/Users", user(&format!("user{n}@example.com"))).await);
        }
        let mut groups = Vec::new();
        for n in 0..5 {
            let members = users
                .iter()
                .enumerate()
                .filter(|(m, _)| (n + m) % 2 == 0)
                .map(|(_, id)| json!({ "value": id }))
                .collect::<Vec<_>>();
            let body = json!({ "displayName": format!("Group {n}"), "members": members });
            groups.push(create(&scim, "/Groups", body).await);
        }
        let (user, group) = (&users[1], &groups[1]);
        let membership = format!(r#"id eq "{group}" and members[value eq "{user}"]"#);
        // Each lookup, and the statements it reads: the token, the page
        // and, where returned, the page's groups or members.
        let lookups = [
            // Okta and Entra look each user up by name or external ID.
            (
                format!(
                    "/Users?filter={}",
                    encode(r#"userName eq "USER1@example.com""#)
                ),
                3,
            ),
            (
                format!(
                    "/Users?filter={}&excludedAttributes=groups",
                    encode(r#"externalId eq "ext-user1@example.com""#)
                ),
                2,
            ),
            // Entra looks groups up without their members.
            (
                format!(
                    "/Groups?filter={}&excludedAttributes=members",
                    encode(r#"displayName eq "group 1""#)
                ),
                2,
            ),
            (
                format!("/Groups?filter={}", encode(r#"displayName eq "Group 1""#)),
                3,
            ),
            (
                format!(
                    "/Groups?filter={}&excludedAttributes=members",
                    encode(&format!(r#"members[value eq "{user}"]"#))
                ),
                2,
            ),
            (
                format!(
                    "/Groups?filter={}&excludedAttributes=members",
                    encode(&membership)
                ),
                2,
            ),
            // A full page counts the rest too.
            ("/Users?count=3".to_owned(), 4),
            ("/Groups?count=3&excludedAttributes=members".to_owned(), 3),
            (format!("/Groups/{group}?excludedAttributes=members"), 2),
            (format!("/Groups/{group}"), 3),
            (format!("/Users/{user}?attributes=userName"), 2),
        ];
        let mut few = Vec::new();
        for (path, statements) in &lookups {
            let ((reads, rows), body) = cost(&app, &scim, path).await;
            assert_eq!(reads, *statements, "{name}: {path}");
            if path.contains("excludedAttributes=members") {
                let listed = body["Resources"].as_array().cloned().unwrap_or_default();
                assert!(
                    body.get("members").is_none()
                        && listed.iter().all(|group| group.get("members").is_none()),
                    "{name}: {path}: {body}"
                );
            }
            few.push((rows, body));
        }
        assert_eq!(
            few[0].1["Resources"][0]["groups"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            few[3].1["Resources"][0]["members"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(few[5].1["totalResults"], 1, "{name}");

        add_many(&app, 1195).await;
        for ((path, statements), (rows, body)) in lookups.iter().zip(&few) {
            let ((reads, more_rows), more_body) = cost(&app, &scim, path).await;
            assert_eq!(reads, *statements, "{name}: {path}");
            assert_eq!(more_rows, *rows, "{name}: {path}");
            if path.contains("count=3") {
                assert_eq!(more_body["Resources"], body["Resources"], "{name}: {path}");
                assert_eq!(more_body["totalResults"], 1200, "{name}: {path}");
            } else {
                assert_eq!(&more_body, body, "{name}: {path}");
            }
        }

        // Pages are capped, with or without a count.
        let (all, page) = resources(&scim, "/Users?count=5000&excludedAttributes=groups").await;
        assert_eq!(all["itemsPerPage"], 1000, "{name}");
        assert_eq!(all["totalResults"], 1200, "{name}");
        assert_eq!(page.len(), 1000, "{name}");
        let (rest, page) = resources(
            &scim,
            "/Users?startIndex=1001&count=1000&excludedAttributes=groups",
        )
        .await;
        assert_eq!(rest["itemsPerPage"], 200, "{name}");
        assert_eq!(page.last().unwrap()["userName"], "many-01194@example.com");
        let (default, _) = resources(&scim, "/Groups").await;
        assert_eq!(default["itemsPerPage"], 100, "{name}");
        assert_eq!(default["totalResults"], 1200, "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn pages_walk_the_whole_list_once() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = common::set_up(&app).await;
        let scim = token(&app, &mut admin).await;
        let mut walkers = Vec::new();
        for n in 0..7 {
            walkers.push(create(&scim, "/Users", user(&format!("walker{n}@example.com"))).await);
        }
        for n in 0..7 {
            let members = walkers[..=n].iter().collect::<Vec<_>>();
            let body = scim_group(&format!("Walkers {n}"), None, &members);
            create(&scim, "/Groups", body).await;
        }
        let in_groups = |user: &String| encode(&format!(r#"members[value eq "{user}"]"#));
        let lists = [
            ("/Users?".to_owned(), 8),
            ("/Groups?".to_owned(), 7),
            (format!("/Groups?filter={}&", in_groups(&walkers[6])), 1),
            (format!("/Groups?filter={}&", in_groups(&walkers[3])), 4),
            (
                format!("/Users?filter={}&", encode(r#"userName sw "walker""#)),
                7,
            ),
        ];
        for (list, total) in lists {
            let (_, everything) = resources(&scim, &format!("{list}count=1000")).await;
            assert_eq!(everything.len(), total, "{name}: {list}");
            let mut walked = Vec::new();
            for start in (1..=total + 3).step_by(3) {
                let path = format!("{list}startIndex={start}&count=3");
                let (page, found) = resources(&scim, &path).await;
                assert_eq!(page["totalResults"], total, "{name}: {path}");
                assert_eq!(page["startIndex"], start, "{name}: {path}");
                assert_eq!(page["itemsPerPage"], found.len(), "{name}: {path}");
                walked.extend(found);
            }
            assert_eq!(walked, everything, "{name}: {list}");

            // A count of 0 returns only the total, and a negative count is
            // 0; a start before 1 is 1.
            for query in ["count=0", "count=-5"] {
                let (page, found) = resources(&scim, &format!("{list}{query}")).await;
                assert_eq!(page["totalResults"], total, "{name}: {list}{query}");
                assert_eq!(page["itemsPerPage"], 0, "{name}: {list}{query}");
                assert!(found.is_empty(), "{name}: {list}{query}");
            }
            let (page, found) = resources(&scim, &format!("{list}startIndex=-3&count=2")).await;
            assert_eq!(page["startIndex"], 1, "{name}: {list}");
            assert_eq!(found, everything[..total.min(2)], "{name}: {list}");
            let (page, found) = resources(&scim, &format!("{list}startIndex=50&count=2")).await;
            assert_eq!(page["totalResults"], total, "{name}: {list}");
            assert!(found.is_empty(), "{name}: {list}");
        }
        app.finish().await;
    }
}
