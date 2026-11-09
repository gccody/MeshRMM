//! Each monitor's latest result on each device.
use sea_query::{Expr, ExprTrait, OnConflict, Order, Query};
use serde::Serialize;

use crate::db::{self, Executor, tables::MonitorStates};

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

pub async fn upsert(executor: &mut impl Executor, state: &MonitorState) -> db::Result<()> {
    executor
        .execute(
            &Query::insert()
                .into_table(MonitorStates::Table)
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
                .values_panic([
                    state.rule_id.as_str().into(),
                    state.device_id.as_str().into(),
                    state.checked_at.into(),
                    state.received_at.into(),
                    state.passing.into(),
                    state.failures.into(),
                    state.value.into(),
                    state.detail.as_str().into(),
                    state.output.as_str().into(),
                ])
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
pub async fn for_rule(
    executor: &mut impl Executor,
    rule_id: &str,
) -> db::Result<Vec<MonitorState>> {
    executor
        .fetch_all(
            &select()
                .and_where(Expr::col(MonitorStates::RuleId).eq(rule_id))
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
