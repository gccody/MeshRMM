//! The queries the engines run every pass, every evaluation or on every Agent
//! connection use an index on both backends rather than reading whole tables
//! that grow with the fleet and its history. Each statement is built the way
//! the engine builds it, with the same bound parameters.
mod common;

use meshrmm_server::{
    applications::tables::{ApplicationRunItems, ApplicationRuns},
    db::tables::{AlertRemediations, Alerts, FileDeliveries, ScriptRuns, UpdateRuns},
    maintenance, wake,
};
use sea_query::{
    DeleteStatement, Expr, ExprTrait, PostgresQueryBuilder, Query, QueryBuilder, SelectStatement,
    SqliteQueryBuilder,
};
use sea_query_sqlx::{SqlxBinder, SqlxValues};
use sqlx::{AssertSqlSafe, Connection, PgConnection, SqliteConnection};

const NOW: i64 = 1_800_000_000_000;

enum Statement {
    Select(SelectStatement),
    Delete(DeleteStatement),
}

impl Statement {
    fn render(&self, builder: impl QueryBuilder) -> (String, SqlxValues) {
        match self {
            Self::Select(select) => select.build_sqlx(builder),
            Self::Delete(delete) => delete.build_sqlx(builder),
        }
    }
}

/// Each statement, what runs it, and the indexes it may use.
fn statements() -> Vec<(&'static str, Statement, &'static [&'static str])> {
    let applications_purged = || {
        Query::select()
            .column(ApplicationRuns::Id)
            .from(ApplicationRuns::Table)
            .and_where(Expr::col(ApplicationRuns::CompletedAt).lt(NOW))
            .to_owned()
    };
    let mut statements: Vec<(&str, Statement, &[&str])> = vec![
        (
            "the dispatcher's count of each device's pending runs",
            Statement::Select(
                Query::select()
                    .column(ScriptRuns::DeviceId)
                    .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                    .from(ScriptRuns::Table)
                    .and_where(
                        Expr::col(ScriptRuns::JobId).is_not_null().and(
                            Expr::col(ScriptRuns::Status)
                                .eq("pending")
                                .and(Expr::cust(format!(
                                    "created_at + timeout_seconds * 1000 + 120000 >= {NOW}"
                                ))),
                        ),
                    )
                    .group_by_col(ScriptRuns::DeviceId)
                    .to_owned(),
            ),
            &["idx_script_runs_status"],
        ),
        (
            "the alert engine's sweep for lost runs",
            Statement::Select(
                Query::select()
                    .columns([
                        ScriptRuns::Id,
                        ScriptRuns::CreatedAt,
                        ScriptRuns::TimeoutSeconds,
                    ])
                    .from(ScriptRuns::Table)
                    .and_where(Expr::col(ScriptRuns::Status).eq("pending"))
                    .to_owned(),
            ),
            &["idx_script_runs_status"],
        ),
        (
            "the dispatcher's count of each device's pending deliveries",
            Statement::Select(
                Query::select()
                    .column(FileDeliveries::DeviceId)
                    .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                    .from(FileDeliveries::Table)
                    .and_where(
                        Expr::col(FileDeliveries::JobId).is_not_null().and(
                            Expr::col(FileDeliveries::Status)
                                .eq("pending")
                                .and(Expr::col(FileDeliveries::CreatedAt).gte(NOW - 1_800_000)),
                        ),
                    )
                    .group_by_col(FileDeliveries::DeviceId)
                    .to_owned(),
            ),
            &["idx_file_deliveries_status"],
        ),
        (
            "the dispatcher's count of each device's pending update runs",
            Statement::Select(
                Query::select()
                    .column(UpdateRuns::DeviceId)
                    .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                    .from(UpdateRuns::Table)
                    .and_where(
                        Expr::col(UpdateRuns::JobId).is_not_null().and(
                            Expr::col(UpdateRuns::Status)
                                .eq("pending")
                                .and(Expr::cust(format!(
                                    "created_at + CASE kind WHEN 'scan' THEN 1 ELSE 2 END >= {NOW}"
                                ))),
                        ),
                    )
                    .group_by_col(UpdateRuns::DeviceId)
                    .to_owned(),
            ),
            &["idx_update_runs_status"],
        ),
        (
            "a connected or reporting device's pending update runs",
            Statement::Select(
                Query::select()
                    .columns([UpdateRuns::DeviceId, UpdateRuns::Kind])
                    .from(UpdateRuns::Table)
                    .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                    .and_where(Expr::col(UpdateRuns::DeviceId).eq("device"))
                    .to_owned(),
            ),
            &["idx_update_runs_status"],
        ),
        (
            "the update engine's sweep for lost runs",
            Statement::Select(
                Query::select()
                    .columns([
                        UpdateRuns::Id,
                        UpdateRuns::DeviceId,
                        UpdateRuns::Kind,
                        UpdateRuns::CreatedAt,
                    ])
                    .from(UpdateRuns::Table)
                    .and_where(Expr::col(UpdateRuns::Status).eq("pending"))
                    .to_owned(),
            ),
            &["idx_update_runs_status"],
        ),
        (
            "the dispatcher's count of each device's pending application runs",
            Statement::Select(
                Query::select()
                    .column(ApplicationRuns::DeviceId)
                    .expr(Expr::cust("CAST(COUNT(*) AS BIGINT)"))
                    .from(ApplicationRuns::Table)
                    .and_where(Expr::col(ApplicationRuns::JobId).is_not_null())
                    .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                    .and_where(Expr::cust(format!(
                        "created_at + CASE action WHEN 'scan' THEN 1 ELSE 2 END >= {NOW}"
                    )))
                    .group_by_col(ApplicationRuns::DeviceId)
                    .to_owned(),
            ),
            // PostgreSQL, seeing the bound status, may read the partial index of
            // pending runs instead; either reads only pending runs.
            &[
                "idx_application_runs_status",
                "idx_application_runs_one_pending",
            ],
        ),
        (
            "the application engine's sweep for lost runs",
            Statement::Select(
                Query::select()
                    .columns([
                        ApplicationRuns::Id,
                        ApplicationRuns::DeviceId,
                        ApplicationRuns::Action,
                        ApplicationRuns::CreatedAt,
                    ])
                    .from(ApplicationRuns::Table)
                    .and_where(Expr::col(ApplicationRuns::Status).eq("pending"))
                    .to_owned(),
            ),
            // PostgreSQL, seeing the bound status, may read the partial index of
            // pending runs instead; either reads only pending runs.
            &[
                "idx_application_runs_status",
                "idx_application_runs_one_pending",
            ],
        ),
        (
            "the application engine's purge of old runs' items",
            Statement::Delete(
                Query::delete()
                    .from_table(ApplicationRunItems::Table)
                    .and_where(
                        Expr::col(ApplicationRunItems::RunId).in_subquery(applications_purged()),
                    )
                    .to_owned(),
            ),
            &["idx_application_runs_completed"],
        ),
        (
            "the application engine's purge of old runs",
            Statement::Delete(
                Query::delete()
                    .from_table(ApplicationRuns::Table)
                    .and_where(Expr::col(ApplicationRuns::CompletedAt).lt(NOW))
                    .to_owned(),
            ),
            &["idx_application_runs_completed"],
        ),
        (
            "self-healing's running script attempts",
            Statement::Select(
                Query::select()
                    .column(AlertRemediations::ScriptRunId)
                    .from(AlertRemediations::Table)
                    .and_where(Expr::col(AlertRemediations::Status).eq("running"))
                    .and_where(Expr::col(AlertRemediations::ScriptRunId).is_not_null())
                    .to_owned(),
            ),
            &["idx_alert_remediations_status"],
        ),
        (
            "self-healing's unanswered built-in actions",
            Statement::Select(
                Query::select()
                    .column(AlertRemediations::Id)
                    .from(AlertRemediations::Table)
                    .and_where(Expr::col(AlertRemediations::Status).eq("running"))
                    .and_where(Expr::col(AlertRemediations::ScriptRunId).is_null())
                    .and_where(Expr::col(AlertRemediations::StartedAt).lt(NOW))
                    .to_owned(),
            ),
            &["idx_alert_remediations_status"],
        ),
        (
            "self-healing's alerts whose next attempt is due",
            Statement::Select(
                Query::select()
                    .column(Alerts::Id)
                    .from(Alerts::Table)
                    .and_where(Expr::col(Alerts::State).ne("resolved"))
                    .and_where(Expr::col(Alerts::RemediationState).eq("waiting"))
                    .and_where(Expr::col(Alerts::NextRemediationAt).lte(NOW))
                    .to_owned(),
            ),
            &["idx_alerts_remediation"],
        ),
        (
            "the alert engine's unresolved alerts",
            Statement::Select(
                Query::select()
                    .columns([
                        Alerts::Id,
                        Alerts::RuleId,
                        Alerts::DeviceId,
                        Alerts::Subject,
                    ])
                    .from(Alerts::Table)
                    // Both planners match the partial index's predicate
                    // against the bound value.
                    .and_where(Expr::col(Alerts::State).ne("resolved"))
                    .to_owned(),
            ),
            &["idx_alerts_unresolved"],
        ),
        (
            "a wake's lookup of the devices on its subnets",
            // 10.0.0.0 and 192.168.1.0.
            Statement::Select(wake::neighbours_query([0x0A00_0000, 0xC0A8_0100], None)),
            &["idx_device_ip_addresses_network"],
        ),
        (
            "a later page of a wake's lookup of the devices on its subnets",
            Statement::Select(wake::neighbours_query(
                [0x0A00_0000, 0xC0A8_0100],
                Some((
                    0x0A00_0000,
                    24,
                    "device".into(),
                    "Ethernet".into(),
                    "10.0.0.5".into(),
                )),
            )),
            &["idx_device_ip_addresses_network"],
        ),
    ];
    statements.extend(
        maintenance::batch_statements(NOW)
            .into_iter()
            .map(|(table, delete)| {
                let (_, query, indexes) = PURGE_INDEXES
                    .iter()
                    .find(|(purged, ..)| *purged == table)
                    .unwrap_or_else(|| panic!("no index is listed for purging {table}"));
                (*query, Statement::Delete(delete), *indexes)
            }),
    );
    statements
}

/// Each table the maintenance task purges, and the index its batches find
/// their rows by.
const PURGE_INDEXES: &[(&str, &str, &[&str])] = &[
    (
        "user_sessions",
        "the purge of expired sessions",
        &["idx_user_sessions_expiry"],
    ),
    (
        "api_tokens",
        "the purge of expired API tokens",
        &["idx_api_tokens_expiry"],
    ),
    (
        "invitations",
        "the purge of expired invitations",
        &["idx_invitations_expiry"],
    ),
    (
        "password_resets",
        "the purge of expired password resets",
        &["idx_password_resets_expiry"],
    ),
    (
        "agent_install_tokens",
        "the purge of expired installers",
        &["idx_agent_install_tokens_expiry"],
    ),
    (
        "remote_handoffs",
        "the purge of expired remote handoffs",
        &["idx_remote_handoffs_expiry"],
    ),
    (
        "remote_sessions",
        "the purge of expired remote sessions",
        &["idx_remote_sessions_expiry"],
    ),
    (
        "script_runs",
        "the purge of old script runs",
        &["idx_script_runs_created"],
    ),
    (
        "file_deliveries",
        "the purge of old file deliveries",
        &["idx_file_deliveries_created"],
    ),
    (
        "device_metrics",
        "the purge of old resource usage",
        &["idx_device_metrics_minute"],
    ),
    (
        "alerts",
        "the purge of old resolved alerts",
        &["idx_alerts_resolved"],
    ),
    (
        "device_application_changes",
        "the purge of old application changes",
        &["idx_device_application_changes_observed"],
    ),
    (
        "job_devices",
        "the purge of old jobs' devices",
        &["idx_jobs_created"],
    ),
    ("jobs", "the purge of old jobs", &["idx_jobs_created"]),
    (
        "update_runs",
        "the purge of old update runs",
        &["idx_update_runs_created"],
    ),
    (
        "wake_requests",
        "the purge of old wake requests",
        &["idx_wake_requests_created"],
    ),
];

async fn sqlite_plan(connection: &mut SqliteConnection, statement: &Statement) -> String {
    let (sql, values) = statement.render(SqliteQueryBuilder);
    let rows: Vec<(i64, i64, i64, String)> =
        sqlx::query_as_with(AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")), values)
            .fetch_all(connection)
            .await
            .unwrap();
    rows.into_iter()
        .map(|(_, _, _, detail)| detail)
        .collect::<Vec<_>>()
        .join("\n")
}

/// PostgreSQL's plan with sequential scans priced out, since the tables are
/// empty and it would rather read an empty table than an index.
async fn postgres_plan(connection: &mut PgConnection, statement: &Statement) -> String {
    let (sql, values) = statement.render(PostgresQueryBuilder);
    let mut transaction = connection.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *transaction)
        .await
        .unwrap();
    // A sequential scan priced out costs enough to be compiled, which needs
    // an LLVM that not every installation has.
    sqlx::query("SET LOCAL jit = off")
        .execute(&mut *transaction)
        .await
        .unwrap();
    let rows: Vec<(String,)> = sqlx::query_as_with(AssertSqlSafe(format!("EXPLAIN {sql}")), values)
        .fetch_all(&mut *transaction)
        .await
        .unwrap();
    transaction.rollback().await.unwrap();
    rows.into_iter()
        .map(|(line,)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Notes `plan` (shown with `--nocapture`) and whether it uses one of
/// `indexes`.
fn check(misses: &mut Vec<String>, backend: &str, query: &str, indexes: &[&str], plan: &str) {
    eprintln!("{backend}: {query}\n{plan}\n");
    if !indexes.iter().any(|index| plan.contains(index)) {
        let indexes = indexes.join(" or ");
        misses.push(format!(
            "on {backend}, {query} does not use {indexes}:\n{plan}"
        ));
    }
}

#[tokio::test]
async fn engine_queries_use_their_indexes() {
    let mut misses = Vec::new();
    let sqlite = common::sqlite().await;
    let mut connection = SqliteConnection::connect(&sqlite.url).await.unwrap();
    for (query, statement, indexes) in statements() {
        let plan = sqlite_plan(&mut connection, &statement).await;
        check(&mut misses, "SQLite", query, indexes, &plan);
    }
    connection.close().await.unwrap();
    sqlite.drop_database().await;

    if let Some(admin_url) = common::postgres_admin_url() {
        let postgres = common::postgres(&admin_url).await;
        let mut connection = PgConnection::connect(&postgres.url).await.unwrap();
        for (query, statement, indexes) in statements() {
            let plan = postgres_plan(&mut connection, &statement).await;
            check(&mut misses, "PostgreSQL", query, indexes, &plan);
        }
        connection.close().await.unwrap();
        postgres.drop_database().await;
    }
    assert!(misses.is_empty(), "{}", misses.join("\n\n"));
}
