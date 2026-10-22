//! Automation policies, for users with `automation.manage`. Saving or
//! running one also needs what its action needs (`scripts.run`,
//! `updates.view` or `updates.install`): the policy runs with the access of
//! whoever saved it last.
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use sea_query::{Expr, ExprTrait, Query};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    agents::parse_id,
    audit::{self, Target as AuditTarget},
    auth::Authorized,
    automation::{
        jobs::{self, Viewer},
        policies::{self, Action, ActionKind, Policy, PolicyInput, PolicyView, Valid},
        schedule::Trigger,
    },
    db::{Executor, tables::AutomationPolicies},
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    time::now_ms,
    users::new_id,
};

/// A policy with its latest job, as the policies page lists it.
#[derive(Debug, Serialize)]
pub struct Listed {
    #[serde(flatten)]
    policy: PolicyView,
    last_job: Option<Value>,
}

fn require_manage(actor: &Authorized) -> Result<(), ApiError> {
    actor.require(Permission::AutomationManage)
}

/// Saving a policy hands it the saver's access, which must do what it does.
fn require_save(actor: &Authorized, action: ActionKind) -> Result<(), ApiError> {
    require_manage(actor)?;
    actor.require(action.permission())
}

fn not_found() -> ApiError {
    ApiError::not_found("Policy not found")
}

/// `GET /v1/automation-policies`.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Value>, ApiError> {
    require_manage(&actor)?;
    let mut database = &state.database;
    let policies = policies::load(&mut database, None).await?;
    let job_ids = policies
        .iter()
        .filter_map(|policy| policy.last_job_id.clone())
        .collect::<Vec<_>>();
    let jobs = jobs::by_ids(&mut database, &job_ids, now_ms()).await?;
    let viewer = Viewer::new(&actor.user.id, &actor.permissions);
    let listed = policies
        .into_iter()
        .map(|policy| {
            let last_job = policy
                .last_job_id
                .as_ref()
                .and_then(|id| jobs.iter().find(|job| &job.id == id))
                .map(|job| json!(viewer.view(job)));
            Listed {
                policy: policy.view(&actor.user.id),
                last_job,
            }
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "policies": listed })))
}

async fn ensure_name_free(
    executor: &mut impl Executor,
    name: &str,
    except: Option<&str>,
) -> Result<(), ApiError> {
    let mut select = Query::select();
    select
        .column(AutomationPolicies::Id)
        .from(AutomationPolicies::Table)
        .and_where(Expr::col(AutomationPolicies::Name).eq(name));
    if let Some(id) = except {
        select.and_where(Expr::col(AutomationPolicies::Id).ne(id));
    }
    let existing: Option<(String,)> = executor.fetch_optional(&select).await?;
    if existing.is_some() {
        return Err(ApiError::conflict("another policy has this name").with_code("name_taken"));
    }
    Ok(())
}

/// Checks what only the database and the clock can tell.
async fn check(
    executor: &mut impl Executor,
    policy: &Valid,
    except: Option<&str>,
    now: i64,
) -> Result<(), ApiError> {
    if policy.enabled
        && matches!(policy.trigger, Trigger::Once { .. })
        && policies::next_run(policy, now).is_none()
    {
        return Err(ApiError::bad_request(
            "choose a date and time in the future, or turn the policy off",
        )
        .with_code("invalid_policy"));
    }
    ensure_name_free(executor, &policy.name, except).await?;
    policy.check_references(executor).await
}

fn metadata(policy: &Policy) -> Value {
    let mut metadata = json!({
        "name": policy.name,
        "enabled": policy.enabled,
        "action": policy.action.kind().as_str(),
        "target": policy.target.as_str(),
        "companies": policy.company_ids.len(),
        "devices": policy.device_ids.len(),
        "trigger": policy.trigger.kind(),
        "next_run_at": policy.next_run_at,
        "offline": policy.offline.as_str(),
        "catch_up_hours": policy.catch_up_hours,
        "channels": policy.channel_ids.len(),
    });
    match &policy.action {
        Action::Script { script_id, run_as } => {
            metadata["script_id"] = script_id.as_str().into();
            metadata["script_name"] = policy.script_name.clone().into();
            metadata["run_as"] = run_as.as_str().into();
        }
        Action::UpdateScan | Action::Wake => {}
        Action::UpdateInstall {
            categories,
            reboot,
            reboot_delay_minutes,
        } => {
            metadata["categories"] = json!(categories);
            metadata["reboot"] = reboot.as_str().into();
            metadata["reboot_delay_minutes"] = (*reboot_delay_minutes).into();
        }
    }
    metadata
}

/// `POST /v1/automation-policies`.
pub async fn create(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(input): JsonBody<PolicyInput>,
) -> Result<(StatusCode, Json<PolicyView>), ApiError> {
    require_manage(&actor)?;
    let input = input.validate()?;
    require_save(&actor, input.action.kind())?;
    let id = new_id();
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    check(&mut transaction, &input, None, now).await?;
    policies::save(&mut transaction, &id, &input, &actor.user.id, now, true).await?;
    let policy = policies::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "automation_policy.create",
        AuditTarget::automation_policy(&id),
        metadata(&policy),
    )
    .await?;
    transaction.commit().await?;
    state.automation.wake();
    Ok((StatusCode::CREATED, Json(policy.view(&actor.user.id))))
}

/// `PUT /v1/automation-policies/{id}`: replaces the policy, which then runs
/// with the access of the user who saved it.
pub async fn update(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(input): JsonBody<PolicyInput>,
) -> Result<Json<PolicyView>, ApiError> {
    require_manage(&actor)?;
    let id = parse_id(&id, "policy")?;
    let input = input.validate()?;
    require_save(&actor, input.action.kind())?;
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    policies::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(not_found)?;
    check(&mut transaction, &input, Some(&id), now).await?;
    policies::save(&mut transaction, &id, &input, &actor.user.id, now, false).await?;
    let policy = policies::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "automation_policy.update",
        AuditTarget::automation_policy(&id),
        metadata(&policy),
    )
    .await?;
    transaction.commit().await?;
    state.automation.wake();
    Ok(Json(policy.view(&actor.user.id)))
}

/// `DELETE /v1/automation-policies/{id}`. Its jobs stay in the history.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_manage(&actor)?;
    let id = parse_id(&id, "policy")?;
    let mut transaction = state.database.begin().await?;
    let policy = policies::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(not_found)?;
    transaction
        .execute(
            &Query::delete()
                .from_table(AutomationPolicies::Table)
                .and_where(Expr::col(AutomationPolicies::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "automation_policy.delete",
        AuditTarget::automation_policy(&id),
        json!({ "name": policy.name }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/automation-policies/{id}/run`: starts a job of the policy now,
/// as the user, whatever its schedule.
pub async fn run(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_manage(&actor)?;
    let id = parse_id(&id, "policy")?;
    let mut database = &state.database;
    let policy = policies::by_id(&mut database, &id)
        .await?
        .ok_or_else(not_found)?;
    require_save(&actor, policy.action.kind())?;
    let now = now_ms();
    let job_id = policies::run_now(&state, &policy, &actor.user.id, actor.actor(), now).await?;
    audit::record(
        &mut database,
        &actor.actor(),
        "automation_policy.run",
        AuditTarget::automation_policy(&id),
        json!({ "name": policy.name, "job_id": job_id }),
    )
    .await?;
    state.automation.wake();
    let viewer = Viewer::new(&actor.user.id, &actor.permissions);
    let job = jobs::get(&mut database, &job_id, now)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok((StatusCode::CREATED, Json(json!(viewer.view(&job)))))
}
