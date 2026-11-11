//! Custom fields: their definitions, devices' and companies' values,
//! inheritance, secrets, the device filter, and toolbox scripts reading and
//! writing them through their Agent.
mod common;

use axum::http::StatusCode;
use common::{App, Browser, audit_events, set_up, user_with};
use meshrmm_protocol_types::AgentCommand;
use meshrmm_server::{db::tables::FieldValues, time::now_ms};
use sea_query::{Expr, ExprTrait, Query};
use serde_json::{Value, json};

/// Sees devices and changes their tags, notes and field values, as a
/// technician does.
const TECHNICIAN: &[&str] = &["devices.view", "devices.edit", "scripts.run"];

async fn create_field(admin: &mut Browser, body: Value) -> Value {
    let created = admin.post("/v1/custom-fields", body.clone()).await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "{body}: {:?}",
        created.body
    );
    created.body
}

async fn create_company(admin: &mut Browser, name: &str) -> String {
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
    assert!(moved.status.is_success(), "{:?}", moved.body);
}

/// The device's or company's entry for the field with `key`.
fn entry<'a>(fields: &'a Value, key: &str) -> &'a Value {
    fields["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["field"]["key"] == key)
        .unwrap_or_else(|| panic!("no {key} in {fields}"))
}

fn keys(fields: &Value) -> Vec<String> {
    fields["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            entry
                .get("field")
                .unwrap_or(entry)
                .get("key")
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

async fn set(browser: &mut Browser, path: &str, value: Value) -> common::Response {
    browser.put(path, json!({ "value": value })).await
}

#[tokio::test]
async fn definitions_are_kept_by_administrators_and_checked() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let (mut technician, _) = user_with(&app, &mut admin, "tech@example.com", TECHNICIAN).await;

        let tier = create_field(
            &mut admin,
            json!({
                "key": "contract_tier",
                "label": " Contract tier ",
                "description": "What the customer pays for",
                "type": "dropdown",
                "options": [" Gold ", "Silver", "Bronze"],
                "applies_to": "both",
                "script_access": "read",
            }),
        )
        .await;
        assert_eq!(tier["label"], "Contract tier", "{name}");
        assert_eq!(tier["options"], json!(["Gold", "Silver", "Bronze"]));
        assert_eq!(tier["applies_to"], "both");
        assert_eq!(tier["script_access"], "read");
        assert_eq!(tier["type"], "dropdown");
        assert_eq!(tier["position"], 0);
        let backup = create_field(
            &mut admin,
            json!({ "key": "backup_vendor", "label": "Backup vendor", "type": "text", "applies_to": "devices" }),
        )
        .await;
        assert_eq!(
            backup["script_access"], "none",
            "scripts get nothing by default"
        );
        assert_eq!(backup["position"], 1, "new fields go last");

        for (invalid, code) in [
            (
                json!({ "key": "Tier", "label": "x", "type": "text", "applies_to": "devices" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "key": "2tier", "label": "x", "type": "text", "applies_to": "devices" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "key": "x", "label": "", "type": "text", "applies_to": "devices" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "key": "x", "label": "x", "type": "colour", "applies_to": "devices" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "key": "x", "label": "x", "type": "text", "applies_to": "nothing" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "key": "x", "label": "x", "type": "dropdown", "applies_to": "devices" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "key": "x", "label": "x", "type": "dropdown", "options": ["A", "a"], "applies_to": "devices" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "key": "x", "label": "x", "type": "number", "options": ["1"], "applies_to": "devices" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "key": "x", "label": "x", "type": "text", "applies_to": "devices", "script_access": "write" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "key": "contract_tier", "label": "Again", "type": "text", "applies_to": "devices" }),
                StatusCode::CONFLICT,
            ),
        ] {
            let refused = admin.post("/v1/custom-fields", invalid.clone()).await;
            assert_eq!(
                refused.status, code,
                "{name}: {invalid}: {:?}",
                refused.body
            );
        }

        // Technicians read the definitions but can't change them.
        let listed = technician.get("/v1/custom-fields").await;
        assert_eq!(listed.status, StatusCode::OK);
        assert_eq!(
            keys(&listed.body),
            ["contract_tier", "backup_vendor"],
            "{name}"
        );
        let tier_id = tier["id"].as_str().unwrap();
        let backup_id = backup["id"].as_str().unwrap();
        for refused in [
            technician
                .post(
                    "/v1/custom-fields",
                    json!({ "key": "y", "label": "y", "type": "text", "applies_to": "devices" }),
                )
                .await,
            technician
                .put(
                    &format!("/v1/custom-fields/{backup_id}"),
                    json!({ "label": "y", "applies_to": "devices", "script_access": "none" }),
                )
                .await,
            technician
                .delete(&format!("/v1/custom-fields/{backup_id}"))
                .await,
            technician
                .put(
                    "/v1/custom-fields/order",
                    json!({ "ids": [backup_id, tier_id] }),
                )
                .await,
        ] {
            assert_eq!(
                refused.status,
                StatusCode::FORBIDDEN,
                "{name}: {:?}",
                refused.body
            );
        }

        // The key and type stay; the rest changes.
        let changed = admin
            .put(
                &format!("/v1/custom-fields/{tier_id}"),
                json!({ "label": "Tier", "options": ["Gold", "Silver", "Platinum"], "applies_to": "both", "script_access": "read_write" }),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{name}: {:?}", changed.body);
        assert_eq!(changed.body["label"], "Tier");
        assert_eq!(changed.body["key"], "contract_tier");
        assert_eq!(changed.body["script_access"], "read_write");
        let retyped = admin
            .put(
                &format!("/v1/custom-fields/{tier_id}"),
                json!({ "label": "Tier", "type": "text", "applies_to": "both", "script_access": "none" }),
            )
            .await;
        assert_eq!(
            retyped.status,
            StatusCode::BAD_REQUEST,
            "{name}: a field's type stays"
        );

        let reordered = admin
            .put(
                "/v1/custom-fields/order",
                json!({ "ids": [backup_id, tier_id] }),
            )
            .await;
        assert_eq!(
            reordered.status,
            StatusCode::OK,
            "{name}: {:?}",
            reordered.body
        );
        assert_eq!(keys(&reordered.body), ["backup_vendor", "contract_tier"]);
        assert_eq!(
            admin
                .put("/v1/custom-fields/order", json!({ "ids": [backup_id] }))
                .await
                .status,
            StatusCode::BAD_REQUEST,
            "{name}: an order names every field"
        );

        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let value = set(
            &mut admin,
            &format!("/v1/agents/{}/custom-fields/backup_vendor", agent.device_id),
            json!("Veeam"),
        )
        .await;
        assert_eq!(value.status, StatusCode::OK, "{name}: {:?}", value.body);
        let deleted = admin
            .delete(&format!("/v1/custom-fields/{backup_id}"))
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name}");
        let left = admin
            .get(&format!("/v1/agents/{}/custom-fields", agent.device_id))
            .await;
        assert_eq!(
            keys(&left.body),
            ["contract_tier"],
            "{name}: its values go with it"
        );
        let audited = audit_events(&app, "custom_field.delete").await;
        assert_eq!(audited.len(), 1, "{name}");
        assert_eq!(audited[0].metadata["values"], 1);
        assert_eq!(
            audit_events(&app, "custom_field.create").await.len(),
            2,
            "{name}"
        );
        assert_eq!(
            audit_events(&app, "custom_field.update").await.len(),
            1,
            "{name}"
        );
        assert_eq!(
            audit_events(&app, "custom_field.reorder").await.len(),
            1,
            "{name}"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn values_are_checked_by_type_and_say_who_changed_them() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let (mut technician, technician_id) =
            user_with(&app, &mut admin, "tech@example.com", TECHNICIAN).await;
        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let device = agent.device_id.clone();
        for (key, field_type, options) in [
            ("text", "text", json!([])),
            ("notes", "multiline", json!([])),
            ("seats", "number", json!([])),
            ("managed", "checkbox", json!([])),
            ("last_dr_test", "date", json!([])),
            ("tier", "dropdown", json!(["Gold", "Silver"])),
            (
                "services",
                "multi_select",
                json!(["Backup", "AV", "Patching"]),
            ),
            ("portal", "url", json!([])),
            ("office", "text", json!([])),
        ] {
            create_field(
                &mut admin,
                json!({ "key": key, "label": key, "type": field_type, "options": options, "applies_to": "devices" }),
            )
            .await;
        }
        create_field(
            &mut admin,
            json!({ "key": "billing_code", "label": "Billing code", "type": "text", "applies_to": "companies" }),
        )
        .await;

        let path = |key: &str| format!("/v1/agents/{device}/custom-fields/{key}");
        for (key, value, stored) in [
            ("text", json!("  Front desk  "), json!("Front desk")),
            ("notes", json!("one\r\ntwo\n"), json!("one\ntwo")),
            ("seats", json!(12.0), json!(12.0)),
            ("seats", json!("7"), json!(7.0)),
            ("managed", json!(true), json!(true)),
            ("last_dr_test", json!("2024-02-29"), json!("2024-02-29")),
            ("tier", json!("gold"), json!("Gold")),
            (
                "services",
                json!(["Patching", "Backup"]),
                json!(["Backup", "Patching"]),
            ),
            (
                "portal",
                json!("https://portal.example.com/x"),
                json!("https://portal.example.com/x"),
            ),
        ] {
            let saved = set(&mut technician, &path(key), value.clone()).await;
            assert_eq!(
                saved.status,
                StatusCode::OK,
                "{name}: {key} {value}: {:?}",
                saved.body
            );
            assert_eq!(saved.body["value"], stored, "{name}: {key}");
            assert_eq!(saved.body["set"], true);
            assert_eq!(saved.body["source"], "device");
            assert_eq!(saved.body["updated_by"]["kind"], "user");
            assert_eq!(saved.body["updated_by"]["user_id"], technician_id.as_str());
            assert_eq!(saved.body["updated_by"]["name"], "tech@example.com");
            assert!(saved.body["updated_at"].as_i64().unwrap() > 0);
        }
        for (key, value) in [
            ("text", json!("two\nlines")),
            ("text", json!(5)),
            ("seats", json!("twelve")),
            ("seats", json!(true)),
            ("managed", json!("maybe")),
            ("last_dr_test", json!("2023-02-29")),
            ("last_dr_test", json!("29/02/2024")),
            ("tier", json!("Bronze")),
            ("services", json!("Backup")),
            ("services", json!(["Firewall"])),
            ("portal", json!("javascript:alert(1)")),
            ("portal", json!("portal.example.com")),
        ] {
            let refused = set(&mut technician, &path(key), value.clone()).await;
            assert_eq!(
                refused.status,
                StatusCode::BAD_REQUEST,
                "{name}: {key} {value}"
            );
            assert_eq!(refused.code(), "invalid_value", "{name}: {key} {value}");
        }
        assert_eq!(
            set(&mut technician, &path("billing_code"), json!("X1"))
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}: a company field has no device values"
        );
        assert_eq!(
            set(&mut viewer, &path("text"), json!("x")).await.status,
            StatusCode::FORBIDDEN,
            "{name}: setting a value needs devices.edit"
        );

        // Clearing: null, or empty text.
        let cleared = set(&mut technician, &path("text"), Value::Null).await;
        assert_eq!(cleared.body["set"], false, "{name}");
        assert_eq!(cleared.body["value"], Value::Null);
        assert_eq!(cleared.body["updated_by"], Value::Null);
        set(&mut technician, &path("office"), json!("Leeds")).await;
        let cleared = set(&mut technician, &path("office"), json!("  ")).await;
        assert_eq!(cleared.body["set"], false, "{name}");

        let listed = viewer
            .get(&format!("/v1/agents/{device}/custom-fields"))
            .await;
        assert_eq!(listed.status, StatusCode::OK, "{name}");
        assert_eq!(
            keys(&listed.body),
            [
                "text",
                "notes",
                "seats",
                "managed",
                "last_dr_test",
                "tier",
                "services",
                "portal",
                "office"
            ],
            "{name}: device fields only, in order"
        );
        assert_eq!(entry(&listed.body, "tier")["value"], "Gold");
        assert_eq!(entry(&listed.body, "text")["set"], false);
        // A field named by its ID works as well as by its key.
        let tier_id = entry(&listed.body, "tier")["field"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            set(&mut technician, &path(&tier_id), json!("Silver"))
                .await
                .body["value"],
            "Silver",
            "{name}"
        );

        let audited = audit_events(&app, "custom_field.set").await;
        let latest = &audited[0];
        assert_eq!(latest.target_id, device, "{name}");
        assert_eq!(latest.metadata["key"], "tier");
        assert_eq!(latest.metadata["value"], "Silver");
        assert_eq!(latest.metadata["source"], "website");
        assert_eq!(
            latest.actor_user_id.as_deref(),
            Some(technician_id.as_str())
        );

        assert_eq!(
            admin
                .get(&format!(
                    "/v1/agents/{}/custom-fields",
                    uuid::Uuid::new_v4()
                ))
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}"
        );
        admin.delete(&format!("/v1/agents/{device}")).await;
        assert_eq!(
            set(&mut technician, &path("tier"), json!("Gold"))
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}: deleted devices have no fields"
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn devices_without_their_own_value_take_their_companys() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let (mut technician, _) = user_with(&app, &mut admin, "tech@example.com", TECHNICIAN).await;
        let acme = create_company(&mut admin, "Acme").await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let device = agent.device_id.clone();
        move_to(&mut admin, &device, Some(&acme)).await;
        create_field(
            &mut admin,
            json!({ "key": "backup_vendor", "label": "Backup vendor", "type": "text", "applies_to": "both" }),
        )
        .await;
        create_field(
            &mut admin,
            json!({ "key": "asset_tag", "label": "Asset tag", "type": "text", "applies_to": "devices" }),
        )
        .await;
        create_field(
            &mut admin,
            json!({ "key": "billing_code", "label": "Billing code", "type": "text", "applies_to": "companies" }),
        )
        .await;

        let company_path = |key: &str| format!("/v1/companies/{acme}/custom-fields/{key}");
        assert_eq!(
            set(
                &mut technician,
                &company_path("backup_vendor"),
                json!("Veeam")
            )
            .await
            .status,
            StatusCode::FORBIDDEN,
            "{name}: company values need companies.manage"
        );
        let saved = set(&mut admin, &company_path("backup_vendor"), json!("Veeam")).await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        assert_eq!(saved.body["source"], "company");
        assert_eq!(
            set(&mut admin, &company_path("asset_tag"), json!("A1"))
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}: a device field has no company values"
        );
        set(&mut admin, &company_path("billing_code"), json!("ACME-01")).await;
        let company = technician
            .get(&format!("/v1/companies/{acme}/custom-fields"))
            .await;
        assert_eq!(company.status, StatusCode::OK, "{name}");
        assert_eq!(keys(&company.body), ["backup_vendor", "billing_code"]);

        let fields_path = format!("/v1/agents/{device}/custom-fields");
        let inherited = technician.get(&fields_path).await.body;
        assert_eq!(keys(&inherited), ["backup_vendor", "asset_tag"], "{name}");
        let vendor = entry(&inherited, "backup_vendor");
        assert_eq!(vendor["value"], "Veeam", "{name}");
        assert_eq!(vendor["source"], "company");
        assert_eq!(
            vendor["inherited_from"],
            json!({ "id": acme, "name": "Acme" })
        );

        let own = set(
            &mut technician,
            &format!("{fields_path}/backup_vendor"),
            json!("Datto"),
        )
        .await;
        assert_eq!(
            own.body["value"], "Datto",
            "{name}: the device's own value wins"
        );
        assert_eq!(own.body["source"], "device");
        assert!(own.body.get("inherited_from").is_none());
        let back = set(
            &mut technician,
            &format!("{fields_path}/backup_vendor"),
            Value::Null,
        )
        .await;
        assert_eq!(
            back.body["value"], "Veeam",
            "{name}: clearing it inherits again"
        );
        assert_eq!(back.body["source"], "company");

        move_to(&mut admin, &device, None).await;
        let alone = technician.get(&fields_path).await.body;
        assert_eq!(entry(&alone, "backup_vendor")["set"], false, "{name}");

        // A field no longer for companies drops their values, so devices
        // stop inheriting them.
        move_to(&mut admin, &device, Some(&acme)).await;
        let vendor_id = entry(&company.body, "backup_vendor")["field"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let narrowed = admin
            .put(
                &format!("/v1/custom-fields/{vendor_id}"),
                json!({ "label": "Backup vendor", "applies_to": "devices", "script_access": "none" }),
            )
            .await;
        assert_eq!(narrowed.status, StatusCode::OK, "{name}");
        let after = technician.get(&fields_path).await.body;
        assert_eq!(entry(&after, "backup_vendor")["set"], false, "{name}");
        assert_eq!(
            audit_events(&app, "custom_field.update").await[0].metadata["values_cleared"],
            1
        );

        let deleted = admin.delete(&format!("/v1/companies/{acme}")).await;
        assert!(deleted.status.is_success(), "{name}");
        assert_eq!(
            technician
                .get(&format!("/v1/companies/{acme}/custom-fields"))
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}"
        );
        app.finish().await;
    }
}

/// The stored value and sealed value of the device's field.
async fn stored(app: &App, device_id: &str) -> Vec<(Option<String>, Option<Vec<u8>>)> {
    app.db()
        .fetch_all(
            &Query::select()
                .columns([FieldValues::Value, FieldValues::ValueEncrypted])
                .from(FieldValues::DeviceTable)
                .and_where(Expr::col(FieldValues::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn secrets_are_sealed_masked_and_revealed_only_with_permission_on_the_record() {
    const SECRET: &str = "hunter2-the-local-admin-password";
    for app in common::apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let (mut technician, _) = user_with(&app, &mut admin, "tech@example.com", TECHNICIAN).await;
        let (mut revealer, revealer_id) = user_with(
            &app,
            &mut admin,
            "lead@example.com",
            &["devices.view", "custom_fields.reveal"],
        )
        .await;
        let acme = create_company(&mut admin, "Acme").await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let device = agent.device_id.clone();
        move_to(&mut admin, &device, Some(&acme)).await;
        create_field(
            &mut admin,
            json!({ "key": "local_admin", "label": "Local admin password", "type": "secret", "applies_to": "both" }),
        )
        .await;

        let path = format!("/v1/agents/{device}/custom-fields/local_admin");
        let saved = set(&mut technician, &path, json!(SECRET)).await;
        assert_eq!(saved.status, StatusCode::OK, "{name}: {:?}", saved.body);
        assert_eq!(saved.body["value"], Value::Null, "{name}: masked");
        assert_eq!(saved.body["set"], true);

        let rows = stored(&app, &device).await;
        assert_eq!(rows.len(), 1, "{name}");
        let (plain, sealed) = &rows[0];
        assert_eq!(plain, &None, "{name}: no plaintext column");
        let sealed = sealed.as_ref().expect("sealed");
        assert!(
            !sealed
                .windows(SECRET.len())
                .any(|window| window == SECRET.as_bytes()),
            "{name}: encrypted at rest"
        );

        // Nothing that lists values carries it.
        for listing in [
            format!("/v1/agents/{device}/custom-fields"),
            format!("/v1/companies/{acme}/custom-fields"),
            "/v1/custom-fields".to_owned(),
            "/v1/agents".to_owned(),
        ] {
            let listed = revealer.get(&listing).await;
            assert_eq!(listed.status, StatusCode::OK, "{name}: {listing}");
            assert!(
                !listed.body.to_string().contains(SECRET),
                "{name}: {listing} shows the secret"
            );
        }
        let audited = audit_events(&app, "custom_field.set").await;
        assert_eq!(audited[0].metadata["value"], "(secret)", "{name}");
        assert!(!format!("{:?}", audited[0].metadata).contains(SECRET));

        let reveal = format!("{path}/reveal");
        let refused = technician.post(&reveal, json!({})).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{name}: revealing needs its permission"
        );
        assert!(audit_events(&app, "custom_field.reveal").await.is_empty());
        let revealed = revealer.post(&reveal, json!({})).await;
        assert_eq!(
            revealed.status,
            StatusCode::OK,
            "{name}: {:?}",
            revealed.body
        );
        assert_eq!(revealed.body["value"], SECRET);
        assert_eq!(revealed.body["source"], "device");
        let reveals = audit_events(&app, "custom_field.reveal").await;
        assert_eq!(reveals.len(), 1, "{name}");
        assert_eq!(
            reveals[0].actor_user_id.as_deref(),
            Some(revealer_id.as_str())
        );
        assert_eq!(reveals[0].target_id, device);
        assert_eq!(reveals[0].metadata["key"], "local_admin");
        assert!(!reveals[0].metadata.to_string().contains(SECRET), "{name}");

        // A company's secret reaches its devices without one of their own.
        set(&mut technician, &path, Value::Null).await;
        let no_value = revealer.post(&reveal, json!({})).await;
        assert_eq!(no_value.status, StatusCode::NOT_FOUND, "{name}");
        let company_path = format!("/v1/companies/{acme}/custom-fields/local_admin");
        set(&mut admin, &company_path, json!("company-wide")).await;
        let inherited = revealer.post(&reveal, json!({})).await;
        assert_eq!(inherited.body["value"], "company-wide", "{name}");
        assert_eq!(inherited.body["source"], "company");
        let from_company = revealer
            .post(&format!("{company_path}/reveal"), json!({}))
            .await;
        assert_eq!(from_company.body["value"], "company-wide", "{name}");
        assert_eq!(audit_events(&app, "custom_field.reveal").await.len(), 3);

        // Other fields aren't revealed: their value is in the list.
        create_field(
            &mut admin,
            json!({ "key": "office", "label": "Office", "type": "text", "applies_to": "devices" }),
        )
        .await;
        set(
            &mut technician,
            &format!("/v1/agents/{device}/custom-fields/office"),
            json!("Leeds"),
        )
        .await;
        let plain = revealer
            .post(
                &format!("/v1/agents/{device}/custom-fields/office/reveal"),
                json!({}),
            )
            .await;
        assert_eq!(plain.code(), "not_secret", "{name}");
        app.finish().await;
    }
}

async fn matching(browser: &mut Browser, field: &str, query: &str) -> Vec<String> {
    let found = browser
        .get(&format!("/v1/custom-fields/{field}/devices?{query}"))
        .await;
    assert_eq!(
        found.status,
        StatusCode::OK,
        "{field}?{query}: {:?}",
        found.body
    );
    assert_eq!(
        found.body["matching"].as_u64().unwrap() as usize,
        found.body["device_ids"].as_array().unwrap().len()
    );
    let mut ids = found.body["device_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

#[tokio::test]
async fn the_device_list_filters_by_field_values() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        let acme = create_company(&mut admin, "Acme").await;
        let mut devices = Vec::new();
        for device_name in ["A", "B", "C"] {
            devices.push(app.enroll(&mut admin, device_name).await.device_id);
        }
        let (a, b, c) = (devices[0].clone(), devices[1].clone(), devices[2].clone());
        move_to(&mut admin, &b, Some(&acme)).await;
        move_to(&mut admin, &c, Some(&acme)).await;
        for body in [
            json!({ "key": "vendor", "label": "Vendor", "type": "text", "applies_to": "both" }),
            json!({ "key": "seats", "label": "Seats", "type": "number", "applies_to": "devices" }),
            json!({ "key": "managed", "label": "Managed", "type": "checkbox", "applies_to": "devices" }),
            json!({ "key": "services", "label": "Services", "type": "multi_select", "options": ["Backup", "AV"], "applies_to": "devices" }),
            json!({ "key": "password", "label": "Password", "type": "secret", "applies_to": "devices" }),
        ] {
            create_field(&mut admin, body).await;
        }
        let put = |device: &str, key: &str| format!("/v1/agents/{device}/custom-fields/{key}");
        set(&mut admin, &put(&a, "vendor"), json!("Veeam Backup")).await;
        set(
            &mut admin,
            &format!("/v1/companies/{acme}/custom-fields/vendor"),
            json!("Datto"),
        )
        .await;
        set(&mut admin, &put(&c, "vendor"), json!("veeam")).await;
        set(&mut admin, &put(&a, "seats"), json!(10)).await;
        set(&mut admin, &put(&b, "seats"), json!(12)).await;
        set(&mut admin, &put(&a, "managed"), json!(true)).await;
        set(&mut admin, &put(&b, "managed"), json!(false)).await;
        set(&mut admin, &put(&a, "services"), json!(["Backup", "AV"])).await;
        set(&mut admin, &put(&c, "services"), json!(["AV"])).await;
        set(&mut admin, &put(&b, "password"), json!("p")).await;

        let sorted = |mut ids: Vec<String>| {
            ids.sort();
            ids
        };
        assert_eq!(
            matching(&mut viewer, "vendor", "op=equals&value=VEEAM").await,
            std::slice::from_ref(&c),
            "{name}"
        );
        assert_eq!(
            matching(&mut viewer, "vendor", "op=contains&value=veeam").await,
            sorted(vec![a.clone(), c.clone()]),
            "{name}"
        );
        assert_eq!(
            matching(&mut viewer, "vendor", "op=equals&value=Datto").await,
            std::slice::from_ref(&b),
            "{name}: an inherited value matches"
        );
        assert!(
            matching(&mut viewer, "vendor", "op=empty").await.is_empty(),
            "{name}"
        );
        assert_eq!(
            matching(&mut viewer, "seats", "op=equals&value=12.0").await,
            std::slice::from_ref(&b)
        );
        assert_eq!(
            matching(&mut viewer, "seats", "op=empty").await,
            std::slice::from_ref(&c)
        );
        assert_eq!(
            matching(&mut viewer, "managed", "op=equals&value=false").await,
            std::slice::from_ref(&b)
        );
        assert_eq!(
            matching(&mut viewer, "managed", "op=not_empty").await,
            sorted(vec![a.clone(), b.clone()])
        );
        assert_eq!(
            matching(&mut viewer, "services", "op=contains&value=backup").await,
            std::slice::from_ref(&a)
        );
        assert_eq!(
            matching(&mut viewer, "password", "op=not_empty").await,
            std::slice::from_ref(&b)
        );
        assert_eq!(
            matching(&mut viewer, "password", "op=empty").await,
            sorted(vec![a.clone(), c.clone()])
        );
        for (field, query) in [
            ("password", "op=equals&value=p"),
            ("seats", "op=contains&value=1"),
            ("services", "op=equals&value=AV"),
            ("vendor", "op=equals"),
            ("vendor", "op=near&value=x"),
        ] {
            let refused = viewer
                .get(&format!("/v1/custom-fields/{field}/devices?{query}"))
                .await;
            assert_eq!(
                refused.status,
                StatusCode::BAD_REQUEST,
                "{name}: {field}?{query}"
            );
        }
        assert_eq!(
            viewer
                .get("/v1/custom-fields/nothing/devices?op=empty")
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        app.finish().await;
    }
}

#[tokio::test]
async fn one_value_goes_on_many_devices_at_once() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let (mut technician, _) = user_with(&app, &mut admin, "tech@example.com", TECHNICIAN).await;
        let (mut viewer, _) =
            user_with(&app, &mut admin, "viewer@example.com", &["devices.view"]).await;
        let a = app.enroll(&mut admin, "A").await.device_id;
        let b = app.enroll(&mut admin, "B").await.device_id;
        create_field(
            &mut admin,
            json!({ "key": "office", "label": "Office", "type": "dropdown", "options": ["Leeds", "York"], "applies_to": "devices" }),
        )
        .await;
        let bulk = |value: Value, devices: Vec<String>| json!({ "field": "office", "device_ids": devices, "value": value });
        let changed = technician
            .post(
                "/v1/device-custom-fields",
                bulk(json!("york"), vec![a.clone(), b.clone()]),
            )
            .await;
        assert_eq!(changed.status, StatusCode::OK, "{name}: {:?}", changed.body);
        assert_eq!(changed.body["changed"], 2);
        for device in [&a, &b] {
            let fields = viewer
                .get(&format!("/v1/agents/{device}/custom-fields"))
                .await
                .body;
            assert_eq!(entry(&fields, "office")["value"], "York", "{name}");
        }
        let audited = audit_events(&app, "custom_field.set").await;
        assert_eq!(audited.len(), 2, "{name}: each device is recorded");
        assert!(
            audited
                .iter()
                .all(|event| event.metadata["source"] == "bulk")
        );

        for (refused, status) in [
            (
                viewer
                    .post(
                        "/v1/device-custom-fields",
                        bulk(json!("York"), vec![a.clone()]),
                    )
                    .await,
                StatusCode::FORBIDDEN,
            ),
            (
                technician
                    .post(
                        "/v1/device-custom-fields",
                        bulk(json!("Hull"), vec![a.clone()]),
                    )
                    .await,
                StatusCode::BAD_REQUEST,
            ),
            (
                technician
                    .post("/v1/device-custom-fields", bulk(json!("York"), vec![]))
                    .await,
                StatusCode::BAD_REQUEST,
            ),
            (
                technician
                    .post(
                        "/v1/device-custom-fields",
                        bulk(
                            json!("York"),
                            vec![a.clone(), uuid::Uuid::new_v4().to_string()],
                        ),
                    )
                    .await,
                StatusCode::NOT_FOUND,
            ),
        ] {
            assert_eq!(refused.status, status, "{name}: {:?}", refused.body);
        }
        let cleared = technician
            .post(
                "/v1/device-custom-fields",
                bulk(Value::Null, vec![a.clone()]),
            )
            .await;
        assert_eq!(cleared.body["changed"], 1, "{name}");
        let fields = viewer
            .get(&format!("/v1/agents/{a}/custom-fields"))
            .await
            .body;
        assert_eq!(entry(&fields, "office")["set"], false, "{name}");
        app.finish().await;
    }
}

/// Creates a Command Prompt script and returns its ID.
async fn create_script(browser: &mut Browser, name: &str) -> String {
    let created = browser
        .post(
            "/v1/toolbox/scripts",
            json!({ "name": name, "language": "cmd", "body": "echo hi", "timeout_seconds": 60 }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

/// The fields the run's script got, by key: value and whether writable.
fn granted(run: &meshrmm_protocol_types::ScriptRunRequest) -> Vec<(String, Option<String>, bool)> {
    run.fields
        .iter()
        .map(|field| (field.key.clone(), field.value.clone(), field.writable))
        .collect()
}

#[tokio::test]
async fn scripts_read_the_fields_they_may_and_write_back_as_their_run() {
    for app in common::apps().await {
        let name = app.name;
        let mut admin = set_up(&app).await;
        let (mut technician, technician_id) =
            user_with(&app, &mut admin, "tech@example.com", TECHNICIAN).await;
        let acme = create_company(&mut admin, "Acme").await;
        let agent = app.enroll(&mut admin, "DESKTOP-1").await;
        let other = app.enroll(&mut admin, "DESKTOP-2").await;
        let device = agent.device_id.clone();
        move_to(&mut admin, &device, Some(&acme)).await;
        for body in [
            json!({ "key": "tier", "label": "Tier", "type": "dropdown", "options": ["Gold", "Silver"], "applies_to": "both", "script_access": "read" }),
            json!({ "key": "last_backup", "label": "Last backup", "type": "date", "applies_to": "devices", "script_access": "read_write" }),
            json!({ "key": "services", "label": "Services", "type": "multi_select", "options": ["Backup", "AV"], "applies_to": "devices", "script_access": "read_write" }),
            json!({ "key": "internal", "label": "Internal", "type": "text", "applies_to": "devices" }),
            json!({ "key": "local_admin", "label": "Local admin", "type": "secret", "applies_to": "devices", "script_access": "read" }),
            json!({ "key": "billing_code", "label": "Billing", "type": "text", "applies_to": "companies", "script_access": "read_write" }),
        ] {
            create_field(&mut admin, body).await;
        }
        set(
            &mut admin,
            &format!("/v1/companies/{acme}/custom-fields/tier"),
            json!("Gold"),
        )
        .await;
        let put = |key: &str| format!("/v1/agents/{device}/custom-fields/{key}");
        set(&mut admin, &put("internal"), json!("not for scripts")).await;
        set(&mut admin, &put("local_admin"), json!("s3cret")).await;
        set(&mut admin, &put("services"), json!(["AV", "Backup"])).await;

        // A run from the website gets its device's readable fields.
        let script = create_script(&mut technician, "Backup check").await;
        let mut connection = app.state.agents.connect(&device);
        let started = technician
            .post(
                &format!("/v1/agents/{device}/script-runs"),
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
        let Some(AgentCommand::RunScript { run }) = common::next_command(&mut connection).await
        else {
            panic!("expected a script run");
        };
        assert_eq!(
            granted(&run),
            [
                ("tier".to_owned(), Some("Gold".to_owned()), false),
                ("last_backup".to_owned(), None, true),
                ("services".to_owned(), Some("Backup\nAV".to_owned()), true),
                ("local_admin".to_owned(), Some("s3cret".to_owned()), false),
            ],
            "{name}: inherited, writable, multi-select one per line, and the secret opened; nothing scripts may not read"
        );

        let write = |key: &str, value: &str| json!({ "key": key, "value": value });
        let path = format!("script-runs/{run_id}/custom-fields");
        let written = agent
            .report(&path, write("last_backup", "2024-05-01"))
            .await;
        assert_eq!(
            written.status,
            StatusCode::NO_CONTENT,
            "{name}: {:?}",
            written.body
        );
        let fields = technician
            .get(&format!("/v1/agents/{device}/custom-fields"))
            .await
            .body;
        let backup = entry(&fields, "last_backup");
        assert_eq!(backup["value"], "2024-05-01", "{name}");
        assert_eq!(
            backup["updated_by"],
            json!({ "kind": "script", "run_id": run_id, "script_name": "Backup check" }),
            "{name}: recorded as the run's"
        );
        let audited = audit_events(&app, "custom_field.set").await;
        assert_eq!(audited[0].actor_label, "script “Backup check”", "{name}");
        assert_eq!(audited[0].actor_user_id, None);
        assert_eq!(audited[0].metadata["run_id"], run_id.as_str());
        assert_eq!(
            audited[0].metadata["requested_by_user_id"],
            technician_id.as_str()
        );
        assert_eq!(audited[0].metadata["source"], "script");
        let cleared = agent.report(&path, write("services", "")).await;
        assert_eq!(cleared.status, StatusCode::NO_CONTENT, "{name}");

        for (key, value, status, code) in [
            (
                "tier",
                "Silver",
                StatusCode::FORBIDDEN,
                "field_not_writable",
            ),
            ("internal", "x", StatusCode::FORBIDDEN, "field_not_writable"),
            (
                "local_admin",
                "x",
                StatusCode::FORBIDDEN,
                "field_not_writable",
            ),
            (
                "billing_code",
                "x",
                StatusCode::NOT_FOUND,
                "field_not_found",
            ),
            ("nothing", "x", StatusCode::NOT_FOUND, "field_not_found"),
            (
                "last_backup",
                "yesterday",
                StatusCode::BAD_REQUEST,
                "invalid_value",
            ),
            (
                "services",
                "Firewall",
                StatusCode::BAD_REQUEST,
                "invalid_value",
            ),
        ] {
            let refused = agent.report(&path, write(key, value)).await;
            assert_eq!(
                refused.status, status,
                "{name}: {key}={value}: {:?}",
                refused.body
            );
            assert_eq!(refused.code(), code, "{name}: {key}");
        }
        assert_eq!(
            other
                .report(&path, write("last_backup", "2024-05-02"))
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{name}: another device can't write as this run"
        );
        let stranger = agent.with_token(&"0".repeat(64));
        assert_eq!(
            stranger
                .report(&path, write("last_backup", "2024-05-02"))
                .await
                .status,
            StatusCode::UNAUTHORIZED,
            "{name}"
        );
        let fields = technician
            .get(&format!("/v1/agents/{device}/custom-fields"))
            .await
            .body;
        assert_eq!(
            entry(&fields, "tier")["value"],
            "Gold",
            "{name}: refused writes change nothing"
        );
        assert_eq!(entry(&fields, "last_backup")["value"], "2024-05-01");
        assert_eq!(entry(&fields, "services")["set"], false);

        // Once the run has reported, its script can't write any more.
        let reported = agent
            .report(
                &format!("script-runs/{run_id}/result"),
                json!({ "status": "completed", "ran_as": "SYSTEM", "exit_code": 0, "stdout": "", "stderr": "" }),
            )
            .await;
        assert_eq!(reported.status, StatusCode::NO_CONTENT, "{name}");
        let late = agent
            .report(&path, write("last_backup", "2024-05-03"))
            .await;
        assert_eq!(late.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(late.code(), "run_finished");

        // A job's runs get the fields too, read as each run starts.
        set(&mut admin, &put("last_backup"), json!("2024-06-01")).await;
        let job = technician
            .post(
                "/v1/jobs",
                json!({ "script_id": script, "run_as": "system", "target": "devices", "device_ids": [device], "offline": "skip" }),
            )
            .await;
        assert_eq!(job.status, StatusCode::CREATED, "{name}: {:?}", job.body);
        meshrmm_server::automation::pass(&app.state, now_ms())
            .await
            .unwrap();
        let Some(AgentCommand::RunScript { run }) = common::next_command(&mut connection).await
        else {
            panic!("expected the job's run");
        };
        assert!(
            granted(&run).contains(&(
                "last_backup".to_owned(),
                Some("2024-06-01".to_owned()),
                true
            )),
            "{name}: {:?}",
            granted(&run)
        );
        let job_write = agent
            .report(
                &format!("script-runs/{}/custom-fields", run.run_id),
                write("last_backup", "2024-06-02"),
            )
            .await;
        assert_eq!(job_write.status, StatusCode::NO_CONTENT, "{name}");

        // Without fields scripts may read, a run gets none.
        let fields = admin.get("/v1/custom-fields").await.body;
        for field in fields["fields"].as_array().unwrap() {
            admin
                .delete(&format!(
                    "/v1/custom-fields/{}",
                    field["id"].as_str().unwrap()
                ))
                .await;
        }
        technician
            .post(
                &format!("/v1/agents/{device}/script-runs"),
                json!({ "script_id": script, "run_as": "system" }),
            )
            .await;
        let Some(AgentCommand::RunScript { run }) = common::next_command(&mut connection).await
        else {
            panic!("expected a script run");
        };
        assert!(run.fields.is_empty(), "{name}");
        drop(connection);
        app.finish().await;
    }
}
