//! Alerts and the rules that raise them. See docs/alerts.md.
use axum::{
    Json,
    extract::{Path, Query as QueryParameters, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    agents::parse_id,
    alerts::{
        self,
        rules::{self, Rule, RuleInput, Severity},
        store::{self, Alert, Counts, Filter, Resolution},
    },
    audit::{self, Target},
    auth::Authorized,
    db::{Executor, tables::AlertRules},
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    time::now_ms,
    users::new_id,
};

const DEFAULT_LIMIT: u64 = 200;
const MAX_LIMIT: u64 = 1000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParameters {
    /// `active` (open and acknowledged, the default), `open`,
    /// `acknowledged`, `resolved` or `all`.
    state: Option<String>,
    severity: Option<Severity>,
    /// A company ID, or `none` for devices in no company.
    company: Option<String>,
    device: Option<String>,
    limit: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct AlertList {
    alerts: Vec<Alert>,
    counts: Counts,
    /// More alerts match than were listed.
    truncated: bool,
}

/// `GET /v1/alerts`: open alerts first, then acknowledged, then resolved,
/// each newest first.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
    QueryParameters(parameters): QueryParameters<ListParameters>,
) -> Result<Json<AlertList>, ApiError> {
    actor.require(Permission::AlertsView)?;
    use store::State::{Acknowledged, Open, Resolved};
    let states = match parameters.state.as_deref().unwrap_or("active") {
        "active" => vec![Open, Acknowledged],
        "open" => vec![Open],
        "acknowledged" => vec![Acknowledged],
        "resolved" => vec![Resolved],
        "all" => vec![],
        _ => return Err(ApiError::bad_request("unknown alert state")),
    };
    let company_id = match parameters.company.as_deref() {
        None => None,
        Some("none") => Some(None),
        Some(id) => Some(Some(parse_id(id, "company")?)),
    };
    let device_id = parameters
        .device
        .as_deref()
        .map(|id| parse_id(id, "device"))
        .transpose()?;
    let filter = Filter {
        states,
        severity: parameters.severity,
        company_id,
        device_id,
    };
    let limit = parameters
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let mut database = &state.database;
    let mut alerts = store::list(&mut database, &filter, limit + 1).await?;
    let truncated = alerts.len() as u64 > limit;
    alerts.truncate(limit as usize);
    Ok(Json(AlertList {
        alerts,
        counts: store::counts(&mut database).await?,
        truncated,
    }))
}

/// `GET /v1/alerts/counts`: how many alerts are unresolved.
pub async fn counts(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Counts>, ApiError> {
    actor.require(Permission::AlertsView)?;
    Ok(Json(store::counts(&mut &state.database).await?))
}

async fn find(executor: &mut impl Executor, id: &str) -> Result<Alert, ApiError> {
    store::get(executor, &parse_id(id, "alert")?)
        .await?
        .ok_or_else(|| ApiError::not_found("Alert not found"))
}

/// `GET /v1/alerts/{id}`
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Alert>, ApiError> {
    actor.require(Permission::AlertsView)?;
    Ok(Json(find(&mut &state.database, &id).await?))
}

fn audit_metadata(alert: &Alert) -> serde_json::Value {
    json!({
        "rule_id": alert.rule_id,
        "name": alert.rule_name,
        "device_id": alert.device_id,
        "device_name": alert.device_name,
        "subject": alert.subject,
    })
}

/// `POST /v1/alerts/{id}/acknowledge`: someone is looking into it. The alert
/// stays unresolved, and still resolves itself when its condition passes.
pub async fn acknowledge(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Alert>, ApiError> {
    actor.require(Permission::AlertsRespond)?;
    let mut transaction = state.database.begin().await?;
    let alert = find(&mut transaction, &id).await?;
    if !store::acknowledge(&mut transaction, &alert.id, &actor.actor(), now_ms()).await? {
        return Err(ApiError::conflict(format!(
            "the alert is already {}",
            alert.state
        )));
    }
    audit::record(
        &mut transaction,
        &actor.actor(),
        "alert.acknowledge",
        Target::alert(&alert.id),
        audit_metadata(&alert),
    )
    .await?;
    transaction.commit().await?;
    alerts::changed(&state, &alert.id).await?;
    Ok(Json(find(&mut &state.database, &alert.id).await?))
}

/// `POST /v1/alerts/{id}/resolve`
pub async fn resolve(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Alert>, ApiError> {
    actor.require(Permission::AlertsRespond)?;
    let mut database = &state.database;
    let alert = find(&mut database, &id).await?;
    let rule = rules::by_id(&mut database, &alert.rule_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Alert not found"))?;
    let actor_record = actor.actor();
    if !alerts::resolve(
        &state,
        &alert.id,
        &rule,
        Resolution::Manual,
        Some(&actor_record),
        now_ms(),
    )
    .await?
    {
        return Err(ApiError::conflict("the alert is already resolved"));
    }
    audit::record(
        &mut database,
        &actor_record,
        "alert.resolve",
        Target::alert(&alert.id),
        audit_metadata(&alert),
    )
    .await?;
    Ok(Json(find(&mut database, &alert.id).await?))
}

#[derive(Debug, Serialize)]
pub struct RuleView {
    #[serde(flatten)]
    rule: Rule,
    /// The condition in words.
    description: String,
}

impl From<Rule> for RuleView {
    fn from(rule: Rule) -> Self {
        Self {
            description: rule.describe(),
            rule,
        }
    }
}

/// `GET /v1/alert-rules`
pub async fn list_rules(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Vec<RuleView>>, ApiError> {
    actor.require(Permission::AlertsView)?;
    let rules = rules::load(&mut &state.database, None).await?;
    Ok(Json(rules.into_iter().map(RuleView::from).collect()))
}

async fn ensure_name_free(
    executor: &mut impl Executor,
    name: &str,
    except_id: Option<&str>,
) -> Result<(), ApiError> {
    use sea_query::Query;
    let names: Vec<(String, String)> = executor
        .fetch_all(
            &Query::select()
                .columns([AlertRules::Id, AlertRules::Name])
                .from(AlertRules::Table)
                .to_owned(),
        )
        .await?;
    let wanted = name.to_lowercase();
    if names
        .iter()
        .any(|(id, existing)| Some(id.as_str()) != except_id && existing.to_lowercase() == wanted)
    {
        return Err(ApiError::conflict("a rule with this name already exists"));
    }
    Ok(())
}

fn rule_metadata(input: &RuleInput, rule: &Rule) -> serde_json::Value {
    json!({
        "name": input.name,
        "condition": input.condition.as_str(),
        "description": rule.describe(),
        "severity": input.severity.as_str(),
        "enabled": input.enabled,
        "all_companies": input.all_companies,
        "company_ids": input.company_ids,
        "channel_ids": input.channel_ids,
        "notify_on_resolve": input.notify_on_resolve,
    })
}

/// `POST /v1/alert-rules`
pub async fn create_rule(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(input): JsonBody<RuleInput>,
) -> Result<(StatusCode, Json<RuleView>), ApiError> {
    actor.require(Permission::AlertsManage)?;
    let input = input.validate()?;
    let id = new_id();
    let mut transaction = state.database.begin().await?;
    ensure_name_free(&mut transaction, &input.name, None).await?;
    input.check_references(&mut transaction).await?;
    rules::save(
        &mut transaction,
        &id,
        &input,
        &actor.user.id,
        now_ms(),
        true,
    )
    .await?;
    let rule = rules::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "alert_rule.create",
        Target::alert_rule(&id),
        rule_metadata(&input, &rule),
    )
    .await?;
    transaction.commit().await?;
    state.alerts.wake();
    Ok((StatusCode::CREATED, Json(rule.into())))
}

/// `PUT /v1/alert-rules/{id}`: replaces the rule. Its alerts stay; the next
/// evaluation resolves those the changed rule no longer calls for.
pub async fn update_rule(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(input): JsonBody<RuleInput>,
) -> Result<Json<RuleView>, ApiError> {
    actor.require(Permission::AlertsManage)?;
    let id = parse_id(&id, "rule")?;
    let input = input.validate()?;
    let mut transaction = state.database.begin().await?;
    let previous = rules::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("Rule not found"))?;
    if previous.condition != input.condition {
        return Err(ApiError::bad_request(
            "a rule's condition can't change; create another rule instead",
        )
        .with_code("invalid_rule"));
    }
    ensure_name_free(&mut transaction, &input.name, Some(&id)).await?;
    input.check_references(&mut transaction).await?;
    rules::save(
        &mut transaction,
        &id,
        &input,
        &actor.user.id,
        now_ms(),
        false,
    )
    .await?;
    let rule = rules::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    let mut metadata = rule_metadata(&input, &rule);
    metadata["previous_name"] = json!(previous.name);
    audit::record(
        &mut transaction,
        &actor.actor(),
        "alert_rule.update",
        Target::alert_rule(&id),
        metadata,
    )
    .await?;
    transaction.commit().await?;
    state.alerts.wake();
    Ok(Json(rule.into()))
}

/// `DELETE /v1/alert-rules/{id}`: deletes the rule and its alerts.
pub async fn delete_rule(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::AlertsManage)?;
    let id = parse_id(&id, "rule")?;
    let mut transaction = state.database.begin().await?;
    let rule = rules::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("Rule not found"))?;
    use sea_query::{Expr, ExprTrait, Query};
    transaction
        .execute(
            &Query::delete()
                .from_table(AlertRules::Table)
                .and_where(Expr::col(AlertRules::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "alert_rule.delete",
        Target::alert_rule(&id),
        json!({ "name": rule.name, "condition": rule.condition.as_str() }),
    )
    .await?;
    transaction.commit().await?;
    alerts::announce_counts(&state).await;
    Ok(StatusCode::NO_CONTENT)
}
