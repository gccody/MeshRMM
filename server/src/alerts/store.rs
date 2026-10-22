//! Alert rows: raising, acknowledging and resolving them, and reading them
//! for the website.
use sea_query::{Expr, ExprTrait, Func, OnConflict, Order, Query};
use serde::{Deserialize, Serialize};

use super::rules::Severity;
use crate::{
    audit::Actor,
    db::{
        self, Executor,
        tables::{Agents, AlertRules, Alerts, Companies},
    },
    users::new_id,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Open,
    Acknowledged,
    Resolved,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Acknowledged => "acknowledged",
            Self::Resolved => "resolved",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The condition passed.
    Cleared,
    /// A user resolved it.
    Manual,
    /// The rule was turned off or no longer covers the device.
    NotApplicable,
}

impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cleared => "cleared",
            Self::Manual => "manual",
            Self::NotApplicable => "not_applicable",
        }
    }
}

/// An alert as the website and notifications show it, with its rule's and
/// device's current names.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Alert {
    pub id: String,
    pub rule_id: String,
    pub rule_name: String,
    pub severity: String,
    pub condition: String,
    pub device_id: String,
    pub device_name: String,
    pub company_id: Option<String>,
    pub company_name: Option<String>,
    /// The volume of a disk space alert; empty otherwise.
    pub subject: String,
    pub state: String,
    pub value: Option<f64>,
    pub detail: String,
    pub script_run_id: Option<String>,
    pub occurrences: i64,
    pub raised_at: i64,
    pub last_occurred_at: i64,
    pub acknowledged_at: Option<i64>,
    pub acknowledged_by: Option<String>,
    pub resolved_at: Option<i64>,
    /// Who resolved it; `None` when the server did.
    pub resolved_by: Option<String>,
    pub resolution: Option<String>,
}

fn select() -> sea_query::SelectStatement {
    Query::select()
        .column((Alerts::Table, Alerts::Id))
        .column((Alerts::Table, Alerts::RuleId))
        .expr_as(
            Expr::col((AlertRules::Table, AlertRules::Name)),
            "rule_name",
        )
        .column((AlertRules::Table, AlertRules::Severity))
        .column((AlertRules::Table, AlertRules::Condition))
        .column((Alerts::Table, Alerts::DeviceId))
        .expr_as(Expr::col((Agents::Table, Agents::Name)), "device_name")
        .column((Agents::Table, Agents::CompanyId))
        .expr_as(
            Expr::col((Companies::Table, Companies::Name)),
            "company_name",
        )
        .columns([
            (Alerts::Table, Alerts::Subject),
            (Alerts::Table, Alerts::State),
            (Alerts::Table, Alerts::Value),
            (Alerts::Table, Alerts::Detail),
            (Alerts::Table, Alerts::ScriptRunId),
            (Alerts::Table, Alerts::Occurrences),
            (Alerts::Table, Alerts::RaisedAt),
            (Alerts::Table, Alerts::LastOccurredAt),
            (Alerts::Table, Alerts::AcknowledgedAt),
        ])
        .expr_as(
            Expr::col((Alerts::Table, Alerts::AcknowledgedByLabel)),
            "acknowledged_by",
        )
        .column((Alerts::Table, Alerts::ResolvedAt))
        .expr_as(
            Expr::col((Alerts::Table, Alerts::ResolvedByLabel)),
            "resolved_by",
        )
        .column((Alerts::Table, Alerts::Resolution))
        .from(Alerts::Table)
        .inner_join(
            AlertRules::Table,
            Expr::col((AlertRules::Table, AlertRules::Id)).equals((Alerts::Table, Alerts::RuleId)),
        )
        .inner_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id)).equals((Alerts::Table, Alerts::DeviceId)),
        )
        .left_join(
            Companies::Table,
            Expr::col((Companies::Table, Companies::Id)).equals((Agents::Table, Agents::CompanyId)),
        )
        .to_owned()
}

pub async fn get(executor: &mut impl Executor, id: &str) -> db::Result<Option<Alert>> {
    executor
        .fetch_optional(
            &select()
                .and_where(Expr::col((Alerts::Table, Alerts::Id)).eq(id))
                .to_owned(),
        )
        .await
}

/// Which alerts to list.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    /// Any of these states; every state when empty.
    pub states: Vec<State>,
    pub severity: Option<Severity>,
    /// A company, or `Some(None)` for devices in no company.
    pub company_id: Option<Option<String>>,
    pub device_id: Option<String>,
}

/// Alerts matching `filter`: open ones first, then acknowledged, then
/// resolved, each newest first; at most `limit`.
pub async fn list(
    executor: &mut impl Executor,
    filter: &Filter,
    limit: u64,
) -> db::Result<Vec<Alert>> {
    let mut select = select();
    if !filter.states.is_empty() {
        select.and_where(
            Expr::col((Alerts::Table, Alerts::State))
                .is_in(filter.states.iter().map(|state| state.as_str())),
        );
    }
    if let Some(severity) = filter.severity {
        select
            .and_where(Expr::col((AlertRules::Table, AlertRules::Severity)).eq(severity.as_str()));
    }
    match &filter.company_id {
        Some(Some(company_id)) => {
            select.and_where(Expr::col((Agents::Table, Agents::CompanyId)).eq(company_id.as_str()));
        }
        Some(None) => {
            select.and_where(Expr::col((Agents::Table, Agents::CompanyId)).is_null());
        }
        None => {}
    }
    if let Some(device_id) = &filter.device_id {
        select.and_where(Expr::col((Alerts::Table, Alerts::DeviceId)).eq(device_id.as_str()));
    }
    select
        .order_by_expr(
            Expr::cust(
                "CASE alerts.state WHEN 'open' THEN 0 WHEN 'acknowledged' THEN 1 ELSE 2 END",
            ),
            Order::Asc,
        )
        .order_by((Alerts::Table, Alerts::RaisedAt), Order::Desc)
        .order_by((Alerts::Table, Alerts::Id), Order::Desc)
        .limit(limit);
    executor.fetch_all(&select).await
}

/// How many alerts are unresolved, for the website's navigation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub open: i64,
    pub acknowledged: i64,
    /// Open alerts by severity.
    pub open_info: i64,
    pub open_warning: i64,
    pub open_critical: i64,
}

pub async fn counts(executor: &mut impl Executor) -> db::Result<Counts> {
    let rows: Vec<(String, String, i64)> = executor
        .fetch_all(
            &Query::select()
                .column((Alerts::Table, Alerts::State))
                .column((AlertRules::Table, AlertRules::Severity))
                .expr(Func::count(Expr::col((Alerts::Table, Alerts::Id))))
                .from(Alerts::Table)
                .inner_join(
                    AlertRules::Table,
                    Expr::col((AlertRules::Table, AlertRules::Id))
                        .equals((Alerts::Table, Alerts::RuleId)),
                )
                .and_where(Expr::col((Alerts::Table, Alerts::State)).ne(State::Resolved.as_str()))
                .group_by_col((Alerts::Table, Alerts::State))
                .group_by_col((AlertRules::Table, AlertRules::Severity))
                .to_owned(),
        )
        .await?;
    let mut counts = Counts::default();
    for (state, severity, count) in rows {
        if state == State::Acknowledged.as_str() {
            counts.acknowledged += count;
            continue;
        }
        counts.open += count;
        match Severity::parse(&severity) {
            Some(Severity::Info) => counts.open_info += count,
            Some(Severity::Warning) => counts.open_warning += count,
            Some(Severity::Critical) => counts.open_critical += count,
            None => {}
        }
    }
    Ok(counts)
}

/// An unresolved alert's identity, for the engine.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Unresolved {
    pub id: String,
    pub rule_id: String,
    pub device_id: String,
    pub subject: String,
}

pub async fn unresolved(executor: &mut impl Executor) -> db::Result<Vec<Unresolved>> {
    executor
        .fetch_all(
            &Query::select()
                .columns([
                    Alerts::Id,
                    Alerts::RuleId,
                    Alerts::DeviceId,
                    Alerts::Subject,
                ])
                .from(Alerts::Table)
                .and_where(Expr::col(Alerts::State).ne(State::Resolved.as_str()))
                .to_owned(),
        )
        .await
}

/// What raised an alert.
#[derive(Debug, Clone, PartialEq)]
pub struct Raise<'a> {
    pub rule_id: &'a str,
    pub device_id: &'a str,
    pub subject: &'a str,
    pub value: Option<f64>,
    pub detail: &'a str,
    pub script_run_id: Option<&'a str>,
}

/// Opens an alert, unless the rule already has an unresolved one for the
/// device and subject. Returns the new alert's ID.
pub async fn raise(
    executor: &mut impl Executor,
    raise: &Raise<'_>,
    now: i64,
) -> db::Result<Option<String>> {
    let id = new_id();
    let inserted = executor
        .execute(
            &Query::insert()
                .into_table(Alerts::Table)
                .columns([
                    Alerts::Id,
                    Alerts::RuleId,
                    Alerts::DeviceId,
                    Alerts::Subject,
                    Alerts::State,
                    Alerts::Value,
                    Alerts::Detail,
                    Alerts::ScriptRunId,
                    Alerts::RaisedAt,
                    Alerts::LastOccurredAt,
                ])
                .values_panic([
                    id.as_str().into(),
                    raise.rule_id.into(),
                    raise.device_id.into(),
                    raise.subject.into(),
                    State::Open.as_str().into(),
                    raise.value.into(),
                    raise.detail.into(),
                    raise.script_run_id.into(),
                    now.into(),
                    now.into(),
                ])
                .on_conflict(
                    OnConflict::columns([Alerts::RuleId, Alerts::DeviceId, Alerts::Subject])
                        // Matches idx_alerts_unresolved; a bound value wouldn't.
                        .target_and_where(Expr::cust("state <> 'resolved'"))
                        .do_nothing()
                        .to_owned(),
                )
                .to_owned(),
        )
        .await?;
    Ok((inserted == 1).then_some(id))
}

/// Counts another occurrence on the rule's unresolved alert for the device
/// and subject, and makes it describe this one. Returns its ID.
pub async fn recur(
    executor: &mut impl Executor,
    raise: &Raise<'_>,
    now: i64,
) -> db::Result<Option<String>> {
    let unresolved = || {
        Expr::col(Alerts::RuleId)
            .eq(raise.rule_id)
            .and(Expr::col(Alerts::DeviceId).eq(raise.device_id))
            .and(Expr::col(Alerts::Subject).eq(raise.subject))
            .and(Expr::col(Alerts::State).ne(State::Resolved.as_str()))
    };
    executor
        .execute(
            &Query::update()
                .table(Alerts::Table)
                .value(Alerts::Occurrences, Expr::col(Alerts::Occurrences).add(1))
                .value(Alerts::Value, raise.value)
                .value(Alerts::Detail, raise.detail)
                .value(Alerts::ScriptRunId, raise.script_run_id)
                .value(Alerts::LastOccurredAt, now)
                .and_where(unresolved())
                .to_owned(),
        )
        .await?;
    let row: Option<(String,)> = executor
        .fetch_optional(
            &Query::select()
                .column(Alerts::Id)
                .from(Alerts::Table)
                .and_where(unresolved())
                .to_owned(),
        )
        .await?;
    Ok(row.map(|(id,)| id))
}

/// Marks an unacknowledged open alert acknowledged. Returns whether it was.
pub async fn acknowledge(
    executor: &mut impl Executor,
    id: &str,
    actor: &Actor,
    now: i64,
) -> db::Result<bool> {
    let updated = executor
        .execute(
            &Query::update()
                .table(Alerts::Table)
                .value(Alerts::State, State::Acknowledged.as_str())
                .value(Alerts::AcknowledgedAt, now)
                .value(Alerts::AcknowledgedByUserId, actor.user_id.clone())
                .value(Alerts::AcknowledgedByLabel, actor.label.as_str())
                .and_where(Expr::col(Alerts::Id).eq(id))
                .and_where(Expr::col(Alerts::State).eq(State::Open.as_str()))
                .to_owned(),
        )
        .await?;
    Ok(updated == 1)
}

/// Resolves an unresolved alert, by `actor` or (with `None`) the server.
/// Returns whether it was unresolved.
pub async fn resolve(
    executor: &mut impl Executor,
    id: &str,
    resolution: Resolution,
    actor: Option<&Actor>,
    now: i64,
) -> db::Result<bool> {
    let updated = executor
        .execute(
            &Query::update()
                .table(Alerts::Table)
                .value(Alerts::State, State::Resolved.as_str())
                .value(Alerts::ResolvedAt, now)
                .value(
                    Alerts::ResolvedByUserId,
                    actor.and_then(|actor| actor.user_id.clone()),
                )
                .value(
                    Alerts::ResolvedByLabel,
                    actor.map(|actor| actor.label.clone()),
                )
                .value(Alerts::Resolution, resolution.as_str())
                .and_where(Expr::col(Alerts::Id).eq(id))
                .and_where(Expr::col(Alerts::State).ne(State::Resolved.as_str()))
                .to_owned(),
        )
        .await?;
    Ok(updated == 1)
}

/// Deletes every alert about a deleted device.
pub async fn delete_for_device(executor: &mut impl Executor, device_id: &str) -> db::Result<u64> {
    executor
        .execute(
            &Query::delete()
                .from_table(Alerts::Table)
                .and_where(Expr::col(Alerts::DeviceId).eq(device_id))
                .to_owned(),
        )
        .await
}
