//! Reports and their schedules. `reports.view` sees the templates, the
//! generated reports and the schedules, runs reports and downloads them;
//! `reports.manage` creates, edits, runs and deletes schedules, chooses
//! their recipients, and deletes reports. See docs/reports.md.
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use sea_query::{Expr, ExprTrait, Order};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    agents::parse_id,
    audit,
    auth::Authorized,
    automation::schedule,
    db::{
        Executor,
        tables::{ReportSchedules, Reports, Users},
    },
    http::{ApiError, AppState, JsonBody},
    rbac::Permission,
    reports::{
        self, DEFAULT_STALE_DAYS, MAX_STALE_DAYS, NewReport, RETENTION_MS, Template, Trigger, html,
        period::{self, Period, PeriodKind},
        schedules::{self, ScheduleInput},
        scope::{Scope, ScopeInput},
    },
    targets::{TagMatch, TargetKind},
    time::{DAY_MS, now_ms},
    users::new_id,
};

/// Reports one user may have waiting to be made at once.
const MAX_WAITING_PER_USER: i64 = 5;
const DEFAULT_LIST_LIMIT: u64 = 100;
const MAX_LIST_LIMIT: u64 = 500;

fn report_not_found() -> ApiError {
    ApiError::not_found("Report not found")
}

fn schedule_not_found() -> ApiError {
    ApiError::not_found("Schedule not found")
}

/// `GET /v1/report-templates`: what reports can be made, and whether email
/// can deliver scheduled ones.
pub async fn templates(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::ReportsView)?;
    let settings = state.settings.get(&state.database).await?;
    Ok(Json(json!({
        "templates": Template::ALL.map(Template::view),
        "email_configured": settings.smtp_host.is_some() && settings.smtp_from.is_some(),
        "retention_days": RETENTION_MS / DAY_MS,
        "default_stale_days": DEFAULT_STALE_DAYS,
        "max_stale_days": MAX_STALE_DAYS,
        "max_custom_days": period::MAX_CUSTOM_DAYS,
    })))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    limit: Option<u64>,
    /// Only reports created before this, in Unix milliseconds.
    #[serde(default)]
    before: Option<i64>,
    #[serde(default)]
    schedule_id: Option<String>,
}

/// `GET /v1/reports`: the generated reports, newest first.
pub async fn list(
    State(state): State<AppState>,
    actor: Authorized,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::ReportsView)?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let schedule_id = query
        .schedule_id
        .as_deref()
        .map(|id| parse_id(id, "schedule"))
        .transpose()?;
    let reports = reports::list(
        &mut &state.database,
        limit + 1,
        query.before,
        schedule_id.as_deref(),
        now_ms(),
    )
    .await?;
    let more = reports.len() as u64 > limit;
    let shown = &reports[..Ord::min(reports.len(), limit as usize)];
    Ok(Json(json!({
        "reports": shown.iter().map(|report| report.view(&actor.user.id)).collect::<Vec<_>>(),
        "next_before": if more { shown.last().map(|report| report.created_at) } else { None },
    })))
}

/// A report to make, as the run dialog and the API send it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunInput {
    template: Template,
    target: TargetKind,
    #[serde(default)]
    company_ids: Vec<String>,
    #[serde(default)]
    tag_ids: Vec<String>,
    #[serde(default)]
    tag_match: TagMatch,
    period: PeriodKind,
    /// `YYYY-MM-DD`, the first and last day of a custom period.
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    /// The IANA zone the period's days and the report's times are in.
    time_zone: String,
    #[serde(default)]
    stale_days: Option<i64>,
}

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::bad_request(message).with_code("invalid_report")
}

/// `POST /v1/reports`: queues a report, which the server makes in the
/// background. Answers `202` with the queued report; follow its `status`.
pub async fn run(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(input): JsonBody<RunInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    actor.require(Permission::ReportsView)?;
    let zone = schedule::parse_zone(&input.time_zone)
        .ok_or_else(|| invalid("choose a time zone such as Europe/London"))?;
    let stale_days = input.stale_days.unwrap_or(DEFAULT_STALE_DAYS);
    if !(1..=MAX_STALE_DAYS).contains(&stale_days) {
        return Err(invalid(format!(
            "devices count as not seen after 1 to {MAX_STALE_DAYS} days"
        )));
    }
    let custom = match (&input.from, &input.to) {
        (Some(from), Some(to)) => Some((
            period::parse_date(from).ok_or_else(|| invalid("dates are YYYY-MM-DD"))?,
            period::parse_date(to).ok_or_else(|| invalid("dates are YYYY-MM-DD"))?,
        )),
        (None, None) => None,
        _ => return Err(invalid("choose the first and last day of the range")),
    };
    let now = now_ms();
    let period = Period::resolve(input.period, custom, zone, now)
        .map_err(|error| invalid(error.to_string()))?;
    let scope = Scope::from_input(ScopeInput {
        target: input.target,
        company_ids: input.company_ids,
        tag_ids: input.tag_ids,
        tag_match: input.tag_match,
    })?;
    let mut transaction = state.database.begin().await?;
    scope.check_references(&mut transaction).await?;
    let (waiting,): (i64,) = transaction
        .fetch_one(
            &sea_query::Query::select()
                .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                .from(Reports::Table)
                .and_where(Expr::col(Reports::RequestedByUserId).eq(actor.user.id.as_str()))
                .and_where(Expr::col(Reports::Trigger).ne(Trigger::Schedule.as_str()))
                .and_where(Expr::col(Reports::Status).is_in(["queued", "running"]))
                .to_owned(),
        )
        .await?;
    if waiting >= MAX_WAITING_PER_USER {
        return Err(ApiError::conflict(format!(
            "you have {waiting} reports waiting to be made; wait for them to finish"
        ))
        .with_code("reports_busy"));
    }
    let scope_label = scope.label(&mut transaction).await?;
    let id = new_id();
    reports::queue(
        &mut transaction,
        &NewReport {
            id: &id,
            template: input.template,
            trigger: Trigger::Manual,
            schedule: None,
            user_id: Some(&actor.user.id),
            scope: &scope,
            scope_label: &scope_label,
            period,
            stale_days,
            now,
            problem: None,
        },
        &actor.actor(),
    )
    .await?;
    let report = reports::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    transaction.commit().await?;
    state.reports.wake();
    Ok((StatusCode::ACCEPTED, Json(report.view(&actor.user.id))))
}

async fn visible_report(state: &AppState, id: &str) -> Result<reports::Report, ApiError> {
    let id = parse_id(id, "report")?;
    reports::by_id(&mut &state.database, &id)
        .await?
        .filter(|report| report.expires_at > now_ms())
        .ok_or_else(report_not_found)
}

async fn stored_document(
    state: &AppState,
    report: &reports::Report,
) -> Result<reports::document::Document, ApiError> {
    if report.status != "succeeded" {
        return Err(ApiError::conflict(match report.status.as_str() {
            "failed" => "this report failed, so there is nothing to show",
            _ => "this report isn't ready yet",
        })
        .with_code("report_not_ready"));
    }
    reports::document(state, &report.id)
        .await?
        .ok_or_else(report_not_found)
}

/// `GET /v1/reports/{id}`: the report, with its tables once it is made.
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::ReportsView)?;
    let report = visible_report(&state, &id).await?;
    let mut view = report.view(&actor.user.id);
    if report.status == "succeeded"
        && let Some(document) = reports::document(&state, &report.id).await?
    {
        view["tables"] = document
            .sections
            .iter()
            .flat_map(|section| {
                section.tables.iter().map(|table| {
                    json!({
                        "id": table.id,
                        "section": section.title,
                        "title": table.title,
                        "rows": table.total_rows.unwrap_or(table.rows.len()),
                    })
                })
            })
            .collect::<Vec<_>>()
            .into();
    }
    Ok(Json(view))
}

#[derive(Debug, Deserialize)]
pub struct DownloadQuery {
    #[serde(default)]
    download: Option<String>,
}

fn disposition(download: bool, file: &str) -> HeaderValue {
    let kind = if download { "attachment" } else { "inline" };
    HeaderValue::from_str(&format!("{kind}; filename=\"{file}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("inline"))
}

/// `GET /v1/reports/{id}/html`: the report's page, to open in a tab or,
/// with `?download`, to save.
pub async fn page(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, ApiError> {
    actor.require(Permission::ReportsView)?;
    let report = visible_report(&state, &id).await?;
    let document = stored_document(&state, &report).await?;
    let page = html::render(&document);
    let file = format!("{}.html", html::file_stem(&document));
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_str(&html::CONTENT_SECURITY_POLICY)
                    .map_err(|_| ApiError::internal())?,
            ),
            (
                header::CONTENT_DISPOSITION,
                disposition(query.download.is_some(), &file),
            ),
        ],
        page,
    )
        .into_response())
}

/// `GET /v1/reports/{id}/tables/{table}`: one of the report's tables as
/// CSV. `{table}` is its ID, with or without `.csv`.
pub async fn table_csv(
    State(state): State<AppState>,
    actor: Authorized,
    Path((id, table)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    actor.require(Permission::ReportsView)?;
    let report = visible_report(&state, &id).await?;
    let document = stored_document(&state, &report).await?;
    let wanted = table.strip_suffix(".csv").unwrap_or(&table);
    let table = document
        .sections
        .iter()
        .flat_map(|section| &section.tables)
        .find(|candidate| candidate.id == wanted)
        .ok_or_else(|| ApiError::not_found("The report has no such table"))?;
    let csv = table.csv(html::zone_of(&document));
    let file = format!("{}-{}.csv", html::file_stem(&document), table.id);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/csv; charset=utf-8"),
            ),
            (header::CONTENT_DISPOSITION, disposition(true, &file)),
        ],
        csv,
    )
        .into_response())
}

/// `DELETE /v1/reports/{id}`.
pub async fn delete(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::ReportsManage)?;
    let id = parse_id(&id, "report")?;
    if reports::delete(&state, &id, &actor.actor()).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(report_not_found())
    }
}

/// `GET /v1/report-schedules`.
pub async fn list_schedules(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::ReportsView)?;
    let schedules = schedules::load(&mut &state.database, None).await?;
    Ok(Json(json!({
        "schedules": schedules.iter().map(|schedule| schedule.view(&actor.user.id)).collect::<Vec<_>>(),
    })))
}

/// `GET /v1/report-schedules/recipients`: the users a schedule can email.
pub async fn recipients(
    State(state): State<AppState>,
    actor: Authorized,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::ReportsManage)?;
    let users: Vec<(String, String, String)> = state
        .database
        .fetch_all(
            &sea_query::Query::select()
                .columns([Users::Id, Users::DisplayName, Users::Email])
                .from(Users::Table)
                .and_where(Expr::col(Users::Disabled).eq(false))
                .order_by(Users::DisplayName, Order::Asc)
                .to_owned(),
        )
        .await?;
    Ok(Json(json!({
        "users": users
            .into_iter()
            .map(|(id, display_name, email)| json!({ "id": id, "display_name": display_name, "email": email }))
            .collect::<Vec<_>>(),
    })))
}

async fn ensure_name_free(
    executor: &mut impl Executor,
    name: &str,
    except: Option<&str>,
) -> Result<(), ApiError> {
    let mut select = sea_query::Query::select();
    select
        .column(ReportSchedules::Id)
        .from(ReportSchedules::Table)
        .and_where(Expr::col(ReportSchedules::Name).eq(name));
    if let Some(id) = except {
        select.and_where(Expr::col(ReportSchedules::Id).ne(id));
    }
    let existing: Option<(String,)> = executor.fetch_optional(&select).await?;
    if existing.is_some() {
        return Err(ApiError::conflict("another schedule has this name").with_code("name_taken"));
    }
    Ok(())
}

fn schedule_target(id: &str) -> audit::Target<'_> {
    audit::Target {
        kind: "report_schedule",
        id,
    }
}

/// Validates and saves a schedule; with `existing`, over that one.
async fn save_schedule(
    state: &AppState,
    actor: &Authorized,
    input: ScheduleInput,
    existing: Option<&str>,
) -> Result<schedules::Schedule, ApiError> {
    actor.require(Permission::ReportsManage)?;
    actor.require(Permission::ReportsView)?;
    let valid = input.validate()?;
    let id = existing.map_or_else(new_id, str::to_owned);
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    if existing.is_some() {
        schedules::by_id(&mut transaction, &id)
            .await?
            .ok_or_else(schedule_not_found)?;
    }
    ensure_name_free(&mut transaction, &valid.name, existing).await?;
    valid.check_references(&mut transaction).await?;
    schedules::save(
        &mut transaction,
        &id,
        &valid,
        &actor.user.id,
        now,
        existing.is_none(),
    )
    .await?;
    let schedule = schedules::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(ApiError::internal)?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        if existing.is_some() {
            "report_schedule.update"
        } else {
            "report_schedule.create"
        },
        schedule_target(&id),
        schedule.audit_metadata(),
    )
    .await?;
    transaction.commit().await?;
    state.reports.wake();
    Ok(schedule)
}

/// `POST /v1/report-schedules`.
pub async fn create_schedule(
    State(state): State<AppState>,
    actor: Authorized,
    JsonBody(input): JsonBody<ScheduleInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let schedule = save_schedule(&state, &actor, input, None).await?;
    Ok((StatusCode::CREATED, Json(schedule.view(&actor.user.id))))
}

/// `PUT /v1/report-schedules/{id}`: replaces the schedule, which then runs
/// with the access of the user who saved it.
pub async fn update_schedule(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(input): JsonBody<ScheduleInput>,
) -> Result<Json<Value>, ApiError> {
    actor.require(Permission::ReportsManage)?;
    let id = parse_id(&id, "schedule")?;
    let schedule = save_schedule(&state, &actor, input, Some(&id)).await?;
    Ok(Json(schedule.view(&actor.user.id)))
}

/// `DELETE /v1/report-schedules/{id}`. Its reports stay until they expire.
pub async fn delete_schedule(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    actor.require(Permission::ReportsManage)?;
    let id = parse_id(&id, "schedule")?;
    let mut transaction = state.database.begin().await?;
    let schedule = schedules::by_id(&mut transaction, &id)
        .await?
        .ok_or_else(schedule_not_found)?;
    transaction
        .execute(
            &sea_query::Query::delete()
                .from_table(ReportSchedules::Table)
                .and_where(Expr::col(ReportSchedules::Id).eq(id.as_str()))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "report_schedule.delete",
        schedule_target(&id),
        json!({ "name": schedule.valid.name }),
    )
    .await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/report-schedules/{id}/run`: makes the schedule's report now,
/// for the period that ends now, and emails it to its recipients. Its
/// schedule doesn't change.
pub async fn run_schedule(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    actor.require(Permission::ReportsManage)?;
    actor.require(Permission::ReportsView)?;
    let id = parse_id(&id, "schedule")?;
    let schedule = schedules::by_id(&mut &state.database, &id)
        .await?
        .ok_or_else(schedule_not_found)?;
    let report_id = reports::run_now(&state, &schedule, &actor.actor(), now_ms()).await?;
    let report = reports::by_id(&mut &state.database, &report_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok((StatusCode::ACCEPTED, Json(report.view(&actor.user.id))))
}
