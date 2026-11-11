//! Each monitor's latest result on each device.
use meshrmm_protocol_types::KnownMonitorResult;
use sea_query::{Expr, ExprTrait, OnConflict, Order, Query};
use serde::Serialize;

use crate::{
    access::CompanySet,
    db::{self, Executor, tables::MonitorStates},
};

/// A monitor's latest result on a device.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct MonitorState {
    pub rule_id: String,
    pub device_id: String,
    /// When the Agent checked, by its clock but never later than
    /// `received_at`.
    pub checked_at: i64,
    pub received_at: i64,
    pub passing: bool,
    /// Failed checks in a row, counting the latest.
    pub failures: i64,
    pub value: Option<f64>,
    pub detail: String,
    pub output: String,
}

fn select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            MonitorStates::RuleId,
            MonitorStates::DeviceId,
            MonitorStates::CheckedAt,
            MonitorStates::ReceivedAt,
            MonitorStates::Passing,
            MonitorStates::Failures,
            MonitorStates::Value,
            MonitorStates::Detail,
            MonitorStates::Output,
        ])
        .from(MonitorStates::Table)
        .to_owned()
}

/// Keeps `states` as their monitors' latest results, in one statement. Each
/// monitor and device appears at most once.
pub async fn upsert(executor: &mut impl Executor, states: &[MonitorState]) -> db::Result<()> {
    if states.is_empty() {
        return Ok(());
    }
    let mut insert = Query::insert();
    insert.into_table(MonitorStates::Table).columns([
        MonitorStates::RuleId,
        MonitorStates::DeviceId,
        MonitorStates::CheckedAt,
        MonitorStates::ReceivedAt,
        MonitorStates::Passing,
        MonitorStates::Failures,
        MonitorStates::Value,
        MonitorStates::Detail,
        MonitorStates::Output,
    ]);
    for state in states {
        insert.values_panic([
            state.rule_id.as_str().into(),
            state.device_id.as_str().into(),
            state.checked_at.into(),
            state.received_at.into(),
            state.passing.into(),
            state.failures.into(),
            state.value.into(),
            state.detail.as_str().into(),
            state.output.as_str().into(),
        ]);
    }
    executor
        .execute(
            &insert
                .on_conflict(
                    OnConflict::columns([MonitorStates::RuleId, MonitorStates::DeviceId])
                        .update_columns([
                            MonitorStates::CheckedAt,
                            MonitorStates::ReceivedAt,
                            MonitorStates::Passing,
                            MonitorStates::Failures,
                            MonitorStates::Value,
                            MonitorStates::Detail,
                            MonitorStates::Output,
                        ])
                        .to_owned(),
                )
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// What the server holds of each monitor's latest result on a device, for
/// its Agent to compare with its own when it connects.
pub async fn known(
    executor: &mut impl Executor,
    device_id: &str,
) -> db::Result<Vec<KnownMonitorResult>> {
    let rows = executor
        .fetch_all::<(String, i64, bool, i64), _>(
            &Query::select()
                .columns([
                    MonitorStates::RuleId,
                    MonitorStates::CheckedAt,
                    MonitorStates::Passing,
                    MonitorStates::Failures,
                ])
                .from(MonitorStates::Table)
                .and_where(Expr::col(MonitorStates::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(monitor_id, checked_at, passing, failures)| KnownMonitorResult {
                monitor_id,
                checked_at_unix_ms: u64::try_from(checked_at).unwrap_or(0),
                passing,
                failures: u32::try_from(failures).unwrap_or(u32::MAX),
            },
        )
        .collect())
}

/// A device's monitor results.
pub async fn for_device(
    executor: &mut impl Executor,
    device_id: &str,
) -> db::Result<Vec<MonitorState>> {
    executor
        .fetch_all(
            &select()
                .and_where(Expr::col(MonitorStates::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await
}

/// A monitor's results on every device, failing ones first.
/// A rule's latest result on each device in `within`.
pub async fn for_rule(
    executor: &mut impl Executor,
    rule_id: &str,
    within: &CompanySet,
) -> db::Result<Vec<MonitorState>> {
    executor
        .fetch_all(
            &select()
                .and_where(Expr::col(MonitorStates::RuleId).eq(rule_id))
                .and_where_option(
                    within.device_condition((MonitorStates::Table, MonitorStates::DeviceId)),
                )
                .order_by(MonitorStates::Passing, Order::Asc)
                .order_by(MonitorStates::CheckedAt, Order::Desc)
                .to_owned(),
        )
        .await
}

/// Forgets a rule's results, such as when what it checks changed.
pub async fn delete_for_rule(executor: &mut impl Executor, rule_id: &str) -> db::Result<u64> {
    executor
        .execute(
            &Query::delete()
                .from_table(MonitorStates::Table)
                .and_where(Expr::col(MonitorStates::RuleId).eq(rule_id))
                .to_owned(),
        )
        .await
}

pub async fn delete_for_device(executor: &mut impl Executor, device_id: &str) -> db::Result<u64> {
    executor
        .execute(
            &Query::delete()
                .from_table(MonitorStates::Table)
                .and_where(Expr::col(MonitorStates::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await
}
