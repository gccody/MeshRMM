//! Companies: creating, renaming and deleting them, moving devices between
//! them, enrolling into one, and their presence events.
mod common;

use axum::http::StatusCode;
use common::{apps, audit_events, receive_json, set_up, user_with};
use serde_json::{Value, json};

fn company_of<'a>(listed: &'a Value, device_id: &str) -> &'a Value {
    &listed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == device_id)
        .unwrap_or_else(|| panic!("{device_id} is not in {listed}"))["company_id"]
}

#[tokio::test]
async fn companies_are_created_renamed_and_deleted() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;

        let acme = admin
            .post("/v1/companies", json!({ "name": " Acme " }))
            .await;
        assert_eq!(acme.status, StatusCode::CREATED, "{name}: {:?}", acme.body);
        assert_eq!(acme.body["name"], "Acme");
        assert_eq!(acme.body["device_count"], 0);
        let acme_id = acme.body["id"].as_str().unwrap().to_owned();
        let globex = admin
            .post("/v1/companies", json!({ "name": "globex" }))
            .await;
        assert_eq!(globex.status, StatusCode::CREATED, "{name}");
        let globex_id = globex.body["id"].as_str().unwrap().to_owned();

        for invalid in ["", "   ", "line\nbreak", &"x".repeat(121)] {
            let refused = admin
                .post("/v1/companies", json!({ "name": invalid }))
                .await;
            assert_eq!(
                refused.status,
                StatusCode::BAD_REQUEST,
                "{name}: {invalid:?}"
            );
        }
        let duplicate = admin.post("/v1/companies", json!({ "name": "ACME" })).await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT, "{name}");
        let taken = admin
            .patch(
                &format!("/v1/companies/{globex_id}"),
                json!({ "name": "acme" }),
            )
            .await;
        assert_eq!(taken.status, StatusCode::CONFLICT, "{name}");

        // Renaming to a different case of its own name is fine.
        let renamed = admin
            .patch(
                &format!("/v1/companies/{globex_id}"),
                json!({ "name": "Globex Corp" }),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{name}: {:?}", renamed.body);
        assert_eq!(renamed.body["name"], "Globex Corp");

        let listed = admin.get("/v1/companies").await;
        let names = listed
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|company| company["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Acme", "Globex Corp"], "{name}");

        let missing = admin
            .patch(
                &format!("/v1/companies/{}", uuid::Uuid::new_v4()),
                json!({ "name": "Nobody" }),
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{name}");

        let deleted = admin.delete(&format!("/v1/companies/{acme_id}")).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        assert_eq!(
            admin
                .get("/v1/companies")
                .await
                .body
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            admin
                .delete(&format!("/v1/companies/{acme_id}"))
                .await
                .status,
            StatusCode::NOT_FOUND
        );

        assert_eq!(
            audit_events(&app, "company.create").await.len(),
            2,
            "{name}"
        );
        let rename = audit_events(&app, "company.rename").await;
        assert_eq!(rename[0].metadata["previous_name"], "globex");
        assert_eq!(rename[0].metadata["name"], "Globex Corp");
        let deletion = audit_events(&app, "company.delete").await;
        assert_eq!(deletion[0].target_id, acme_id);
        assert_eq!(deletion[0].metadata["name"], "Acme");
        app.finish().await;
    }
}

#[tokio::test]
async fn devices_move_between_companies_and_leave_a_deleted_one() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let laptop = app.enroll(&mut admin, "Laptop").await;
        let acme = admin.post("/v1/companies", json!({ "name": "Acme" })).await;
        let acme_id = acme.body["id"].as_str().unwrap().to_owned();
        let globex = admin
            .post("/v1/companies", json!({ "name": "Globex" }))
            .await;
        let globex_id = globex.body["id"].as_str().unwrap().to_owned();

        let listed = admin.get("/v1/agents").await.body;
        assert_eq!(company_of(&listed, &desk.device_id), &Value::Null, "{name}");
        let companies = listed["companies"].as_array().unwrap();
        assert_eq!(companies.len(), 2, "{name}: {listed}");
        assert_eq!(companies[0], json!({ "id": acme_id, "name": "Acme" }));

        for device in [&desk, &laptop] {
            let moved = admin
                .put(
                    &format!("/v1/agents/{}/company", device.device_id),
                    json!({ "company_id": acme_id }),
                )
                .await;
            assert_eq!(
                moved.status,
                StatusCode::NO_CONTENT,
                "{name}: {:?}",
                moved.body
            );
        }
        let listed = admin.get("/v1/agents").await.body;
        assert_eq!(
            company_of(&listed, &desk.device_id),
            &json!(acme_id),
            "{name}"
        );
        let counts = admin.get("/v1/companies").await.body;
        assert_eq!(counts[0]["device_count"], 2, "{name}: {counts}");

        let moved = admin
            .put(
                &format!("/v1/agents/{}/company", laptop.device_id),
                json!({ "company_id": globex_id }),
            )
            .await;
        assert_eq!(moved.status, StatusCode::NO_CONTENT, "{name}");
        let event = &audit_events(&app, "agent.move").await[0];
        assert_eq!(event.target_id, laptop.device_id);
        assert_eq!(event.metadata["from_company_id"], acme_id.as_str());
        assert_eq!(event.metadata["company_name"], "Globex");

        // The field is required, and null takes the device out of its company.
        let missing = admin
            .put(
                &format!("/v1/agents/{}/company", laptop.device_id),
                json!({}),
            )
            .await;
        assert_eq!(missing.status, StatusCode::BAD_REQUEST, "{name}");
        let removed = admin
            .put(
                &format!("/v1/agents/{}/company", laptop.device_id),
                json!({ "company_id": null }),
            )
            .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT, "{name}");
        let listed = admin.get("/v1/agents").await.body;
        assert_eq!(company_of(&listed, &laptop.device_id), &Value::Null);

        let unknown = admin
            .put(
                &format!("/v1/agents/{}/company", laptop.device_id),
                json!({ "company_id": uuid::Uuid::new_v4().to_string() }),
            )
            .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{name}");
        let no_device = admin
            .put(
                &format!("/v1/agents/{}/company", uuid::Uuid::new_v4()),
                json!({ "company_id": acme_id }),
            )
            .await;
        assert_eq!(no_device.status, StatusCode::NOT_FOUND, "{name}");

        // Deleting a company keeps its devices, in no company.
        assert_eq!(
            admin
                .delete(&format!("/v1/companies/{acme_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let listed = admin.get("/v1/agents").await.body;
        assert_eq!(listed["agents"].as_array().unwrap().len(), 2, "{name}");
        assert_eq!(company_of(&listed, &desk.device_id), &Value::Null, "{name}");
        assert_eq!(
            audit_events(&app, "company.delete").await[0].metadata["device_count"],
            1
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn managing_companies_needs_its_permission() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let acme = admin.post("/v1/companies", json!({ "name": "Acme" })).await;
        let acme_id = acme.body["id"].as_str().unwrap().to_owned();

        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        assert_eq!(
            viewer.get("/v1/companies").await.status,
            StatusCode::OK,
            "{name}"
        );
        let refusals = [
            viewer
                .post("/v1/companies", json!({ "name": "Initech" }))
                .await,
            viewer
                .patch(
                    &format!("/v1/companies/{acme_id}"),
                    json!({ "name": "Initech" }),
                )
                .await,
            viewer.delete(&format!("/v1/companies/{acme_id}")).await,
            viewer
                .put(
                    &format!("/v1/agents/{}/company", desk.device_id),
                    json!({ "company_id": acme_id }),
                )
                .await,
        ];
        for refused in refusals {
            assert_eq!(
                refused.status,
                StatusCode::FORBIDDEN,
                "{name}: {:?}",
                refused.body
            );
        }

        let (mut nobody, _) = user_with(&app, &mut admin, "nobody@example.com", &[]).await;
        assert_eq!(
            nobody.get("/v1/companies").await.status,
            StatusCode::FORBIDDEN
        );

        let (mut manager, _) = user_with(
            &app,
            &mut admin,
            "manager@example.com",
            &["devices.view", "companies.manage"],
        )
        .await;
        let moved = manager
            .put(
                &format!("/v1/agents/{}/company", desk.device_id),
                json!({ "company_id": acme_id }),
            )
            .await;
        assert_eq!(
            moved.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            moved.body
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn an_installer_can_enroll_its_device_into_a_company() {
    for app in apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let acme = admin.post("/v1/companies", json!({ "name": "Acme" })).await;
        let acme_id = acme.body["id"].as_str().unwrap().to_owned();

        let unknown = admin
            .post(
                "/v1/agent-installers",
                json!({ "platform": "macos", "company_id": uuid::Uuid::new_v4().to_string() }),
            )
            .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{name}");

        let installer = admin
            .post(
                "/v1/agent-installers",
                json!({ "platform": "windows-x64", "company_id": acme_id }),
            )
            .await;
        assert_eq!(
            installer.status,
            StatusCode::CREATED,
            "{name}: {:?}",
            installer.body
        );
        let redeemed = common::redeem(
            &app,
            installer.body["install_token"].as_str().unwrap(),
            "Front desk",
            &common::random_hex(32),
        )
        .await;
        assert_eq!(
            redeemed.status,
            StatusCode::OK,
            "{name}: {:?}",
            redeemed.body
        );
        let device_id = redeemed.body["device_id"].as_str().unwrap();
        let listed = admin.get("/v1/agents").await.body;
        assert_eq!(company_of(&listed, device_id), &json!(acme_id), "{name}");
        app.finish().await;
    }
}

#[tokio::test]
async fn company_changes_reach_the_event_socket() {
    for app in apps().await {
        let name = app.name;
        let server = app.serve().await;
        let mut admin = set_up(&app).await;
        let desk = app.enroll(&mut admin, "Desk").await;
        let mut events = admin.events(&server).await.unwrap();
        let snapshot = receive_json(&mut events).await;
        assert_eq!(snapshot["companies"], json!([]), "{name}");
        let revision = snapshot["revision"].as_u64().unwrap();

        let acme = admin.post("/v1/companies", json!({ "name": "Acme" })).await;
        let acme_id = acme.body["id"].as_str().unwrap().to_owned();
        let created = receive_json(&mut events).await;
        assert_eq!(
            created,
            json!({ "type": "company_upsert", "revision": revision + 1, "company": { "id": acme_id, "name": "Acme" } }),
            "{name}"
        );

        admin
            .put(
                &format!("/v1/agents/{}/company", desk.device_id),
                json!({ "company_id": acme_id }),
            )
            .await;
        let moved = receive_json(&mut events).await;
        assert_eq!(moved["type"], "agent_upsert", "{name}");
        assert_eq!(moved["agent"]["company_id"], acme_id.as_str());

        admin
            .patch(
                &format!("/v1/companies/{acme_id}"),
                json!({ "name": "Acme Inc" }),
            )
            .await;
        let renamed = receive_json(&mut events).await;
        assert_eq!(renamed["type"], "company_upsert", "{name}");
        assert_eq!(renamed["company"]["name"], "Acme Inc");

        // Its devices leave the company before it goes.
        admin.delete(&format!("/v1/companies/{acme_id}")).await;
        let left = receive_json(&mut events).await;
        assert_eq!(left["type"], "agent_upsert", "{name}");
        assert_eq!(left["agent"]["company_id"], Value::Null);
        let deleted = receive_json(&mut events).await;
        assert_eq!(
            deleted,
            json!({ "type": "company_deleted", "revision": revision + 5, "company_id": acme_id }),
            "{name}"
        );
        app.finish().await;
    }
}
