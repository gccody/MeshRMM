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
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "{name}: {:?}",
            refused.body
        );
        let refused = scim.delete(&format!("/Users/{admin_id}")).await;
        assert_eq!(refused.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(admin.get("/v1/account").await.status, StatusCode::OK);
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
