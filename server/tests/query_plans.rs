//! The queries the engines run every pass, every evaluation or on every Agent
//! connection use an index on both backends rather than reading whole tables
//! that grow with the fleet and its history. Each statement is built the way
//! the engine builds it, with the same bound parameters.
mod common;

use meshrmm_protocol_types::DevicePlatform;
use meshrmm_server::{
    applications::tables::{ApplicationRunItems, ApplicationRuns},
    db::tables::{AlertRemediations, Alerts, FileDeliveries, ScriptRuns, UpdateRuns},
    maintenance,
    updates::store::{self, FleetFilter, FleetStatus},
    wake,
};
use sea_query::{
    DeleteStatement, Expr, ExprTrait, PostgresQueryBuilder, Query, QueryBuilder, SelectStatement,
    SqliteQueryBuilder, UpdateStatement,
};
use sea_query_sqlx::{SqlxBinder, SqlxValues};
use sqlx::{AssertSqlSafe, Connection, PgConnection, SqliteConnection};

const NOW: i64 = 1_800_000_000_000;

enum Statement {
    Select(SelectStatement),
    Delete(DeleteStatement),
    Update(UpdateStatement),
}

impl Statement {
    fn render(&self, builder: impl QueryBuilder) -> (String, SqlxValues) {
        match self {
            Self::Select(select) => select.build_sqlx(builder),
            Self::Delete(delete) => delete.build_sqlx(builder),
            Self::Update(update) => update.build_sqlx(builder),
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
    statements.extend(fleet_statements());
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

/// The Updates page's reads of the fleet: each looks up the devices' status
/// rows by their key, which keep their pending counts and latest installs.
fn fleet_statements() -> Vec<(&'static str, Statement, &'static [&'static str])> {
    let narrowed = FleetFilter {
        query: Some("web".into()),
        company_id: Some(None),
        tag_id: Some("tag".into()),
        platform: Some(DevicePlatform::Windows),
        status: Some(FleetStatus::Failed),
    };
    let status_key: &[&str] = &[
        "sqlite_autoindex_device_update_status_1",
        "device_update_status_pkey",
    ];
    vec![
        (
            "the Updates page's totals",
            Statement::Select(store::fleet_counts_query(&FleetFilter::default(), NOW)),
            status_key,
        ),
        (
            "the Updates page's totals and the devices a filter matches",
            Statement::Select(store::fleet_counts_query(&narrowed, NOW)),
            status_key,
        ),
        (
            "a page of the Updates page's devices",
            Statement::Select(store::fleet_page_query(
                &FleetFilter::default(),
                NOW,
                100,
                200,
            )),
            status_key,
        ),
        (
            "a page of the Updates page's devices with failed installs",
            Statement::Select(store::fleet_page_query(&narrowed, NOW, 100, 0)),
            status_key,
        ),
    ]
}

/// The tables that grow with each device's updates and runs, which the
/// fleet's reads leave alone.
const HISTORY: &[&str] = &["device_pending_updates", "update_runs"];

#[tokio::test]
async fn the_updates_page_reads_neither_pending_updates_nor_runs() {
    let mut misses = Vec::new();
    let mut note = |backend: &str, query: &str, plan: &str| {
        if let Some(table) = HISTORY.iter().find(|table| plan.contains(*table)) {
            misses.push(format!("on {backend}, {query} reads {table}:\n{plan}"));
        }
    };
    let sqlite = common::sqlite().await;
    let mut connection = SqliteConnection::connect(&sqlite.url).await.unwrap();
    for (query, statement, _) in fleet_statements() {
        note(
            "SQLite",
            query,
            &sqlite_plan(&mut connection, &statement).await,
        );
    }
    connection.close().await.unwrap();
    sqlite.drop_database().await;

    if let Some(admin_url) = common::postgres_admin_url() {
        let postgres = common::postgres(&admin_url).await;
        let mut connection = PgConnection::connect(&postgres.url).await.unwrap();
        for (query, statement, _) in fleet_statements() {
            note(
                "PostgreSQL",
                query,
                &postgres_plan(&mut connection, &statement).await,
            );
        }
        connection.close().await.unwrap();
        postgres.drop_database().await;
    }
    assert!(misses.is_empty(), "{}", misses.join("\n\n"));
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

/// A statement, what runs it, the indexes it may use, and what its plan
/// must not show.
type Checked = (
    &'static str,
    Statement,
    &'static [&'static str],
    &'static [&'static str],
);

/// The statements that read or write a job's devices: a job's counts and
/// the dispatcher's reads find one job's devices by its ID, never reading
/// the run tables or the whole of `job_devices`, nor sorting them.
fn job_statements() -> Vec<Checked> {
    use meshrmm_server::automation::{
        dispatch,
        jobs::{self, Changed, Kind, RunTable},
    };
    const RUNS: &[&str] = &[
        "script_runs",
        "file_deliveries",
        "update_runs",
        "application_runs",
        "wake_requests",
        "SCAN job_devices",
        "Seq Scan on job_devices",
    ];
    const SORTS: &[&str] = &[
        "SCAN job_devices",
        "Seq Scan on job_devices",
        "TEMP B-TREE",
        "Sort",
    ];
    let at_limit = ["busy-device".to_owned()];
    vec![
        (
            "a job's counts",
            Statement::Select(jobs::counts_query(&["job".to_owned()], NOW)),
            &["idx_job_devices_job"],
            RUNS,
        ),
        (
            "the counts of a page of jobs",
            Statement::Select(jobs::counts_query(
                &(0..200)
                    .map(|index| format!("job-{index}"))
                    .collect::<Vec<_>>(),
                NOW,
            )),
            &["idx_job_devices_job"],
            RUNS,
        ),
        (
            "the dispatcher's jobs with devices that could start",
            Statement::Select(dispatch::ready_jobs(
                &[Kind::Script, Kind::UpdateInstall, Kind::AppInstall],
                &at_limit,
            )),
            &["idx_job_devices_job"],
            &["SCAN job_devices", "Seq Scan on job_devices"],
        ),
        (
            "the dispatcher's next devices of a script job",
            Statement::Select(dispatch::next_parts("job", Kind::Script, &at_limit, 101)),
            &["idx_job_devices_job"],
            SORTS,
        ),
        (
            "the dispatcher's next devices of an install job",
            Statement::Select(dispatch::next_parts(
                "job",
                Kind::UpdateInstall,
                &at_limit,
                101,
            )),
            &["idx_job_devices_job"],
            SORTS,
        ),
        (
            "the dispatcher's next devices of an application job",
            Statement::Select(dispatch::next_parts(
                "job",
                Kind::AppInstall,
                &at_limit,
                101,
            )),
            &["idx_job_devices_job"],
            SORTS,
        ),
        (
            "a run's report written into its job device",
            Statement::Update(jobs::runs_changed_statement(
                RunTable::Script,
                Changed::Id("run"),
                NOW,
            )),
            &[
                "sqlite_autoindex_job_devices_1",
                "job_devices_pkey",
                "idx_job_devices_job",
            ],
            &["SCAN job_devices", "Seq Scan on job_devices"],
        ),
        (
            "a woken device's job devices written when it came online",
            Statement::Update(jobs::runs_changed_statement(
                RunTable::Wake,
                Changed::Device("device"),
                NOW,
            )),
            &[
                "idx_job_devices_state",
                "job_devices_pkey",
                "sqlite_autoindex_job_devices_1",
                "idx_job_devices_job",
            ],
            &["SCAN job_devices", "Seq Scan on job_devices"],
        ),
    ]
}

#[tokio::test]
async fn job_queries_read_only_the_jobs_own_devices() {
    let mut misses = Vec::new();
    let mut check_all =
        |backend: &str, query: &str, indexes: &[&str], forbidden: &[&str], plan: &str| {
            check(&mut misses, backend, query, indexes, plan);
            for shown in forbidden {
                if plan.contains(shown) {
                    misses.push(format!("on {backend}, {query} shows {shown}:\n{plan}"));
                }
            }
        };
    let sqlite = common::sqlite().await;
    let mut connection = SqliteConnection::connect(&sqlite.url).await.unwrap();
    for (query, statement, indexes, forbidden) in job_statements() {
        let plan = sqlite_plan(&mut connection, &statement).await;
        check_all("SQLite", query, indexes, forbidden, &plan);
    }
    connection.close().await.unwrap();
    sqlite.drop_database().await;

    if let Some(admin_url) = common::postgres_admin_url() {
        let postgres = common::postgres(&admin_url).await;
        let mut connection = PgConnection::connect(&postgres.url).await.unwrap();
        for (query, statement, indexes, forbidden) in job_statements() {
            let plan = postgres_plan(&mut connection, &statement).await;
            check_all("PostgreSQL", query, indexes, forbidden, &plan);
        }
        connection.close().await.unwrap();
        postgres.drop_database().await;
    }
    assert!(misses.is_empty(), "{}", misses.join("\n\n"));
}
