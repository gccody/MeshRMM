//! The audit log: who did what, to what, from where.
use std::net::IpAddr;

use sea_query::{Expr, ExprTrait, LikeExpr, Order, Query};
use serde::Serialize;
use serde_json::Value;

use crate::{
    db::{self, Executor, tables::AuditEvents},
    time::now_ms,
};

/// Who an event is attributed to.
#[derive(Debug, Clone)]
pub struct Actor {
    /// `None` for the server itself, the admin CLI, or an anonymous request.
    pub user_id: Option<String>,
    /// The user's email, or a description such as "admin CLI". Kept on the
    /// event so it stays readable after the user is deleted.
    pub label: String,
    pub ip: Option<IpAddr>,
}

impl Actor {
    pub fn user(user_id: &str, email: &str, ip: Option<IpAddr>) -> Self {
        Self {
            user_id: Some(user_id.to_owned()),
            label: email.to_owned(),
            ip,
        }
    }

    pub fn anonymous(ip: IpAddr) -> Self {
        Self {
            user_id: None,
            label: "anonymous".to_owned(),
            ip: Some(ip),
        }
    }

    pub fn cli() -> Self {
        Self {
            user_id: None,
            label: "admin CLI".to_owned(),
            ip: None,
        }
    }
}

/// What an event is about.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub kind: &'a str,
    pub id: &'a str,
}

impl<'a> Target<'a> {
    pub fn user(id: &'a str) -> Self {
        Self { kind: "user", id }
    }

    pub fn role(id: &'a str) -> Self {
        Self { kind: "role", id }
    }

    pub fn invitation(id: &'a str) -> Self {
        Self {
            kind: "invitation",
            id,
        }
    }

    pub fn settings() -> Self {
        Self {
            kind: "settings",
            id: "instance",
        }
    }
}

/// Records an event. Pass the transaction that makes the change, so the
/// change and its record commit together.
pub async fn record(
    executor: &mut impl Executor,
    actor: &Actor,
    action: &str,
    target: Target<'_>,
    metadata: Value,
) -> db::Result<()> {
    executor
        .execute(
            &Query::insert()
                .into_table(AuditEvents::Table)
                .columns([
                    AuditEvents::Id,
                    AuditEvents::ActorUserId,
                    AuditEvents::ActorLabel,
                    AuditEvents::Action,
                    AuditEvents::TargetType,
                    AuditEvents::TargetId,
                    AuditEvents::MetadataJson,
                    AuditEvents::Ip,
                    AuditEvents::CreatedAt,
                ])
                .values_panic([
                    // Time-ordered, and increasing within a millisecond, so
                    // events in the same millisecond keep their order.
                    uuid::Uuid::now_v7().to_string().into(),
                    actor.user_id.clone().into(),
                    actor.label.clone().into(),
                    action.into(),
                    target.kind.into(),
                    target.id.into(),
                    metadata.to_string().into(),
                    actor.ip.map(|ip| ip.to_string()).into(),
                    now_ms().into(),
                ])
                .to_owned(),
        )
        .await?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Event {
    pub id: String,
    pub actor_user_id: Option<String>,
    pub actor_label: String,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub metadata: Value,
    pub ip: Option<String>,
    pub created_at: i64,
}

#[derive(sqlx::FromRow)]
struct EventRow {
    id: String,
    actor_user_id: Option<String>,
    actor_label: String,
    action: String,
    target_type: String,
    target_id: String,
    metadata_json: String,
    ip: Option<String>,
    created_at: i64,
}

#[derive(Debug, Default)]
pub struct Filter {
    pub actor_user_id: Option<String>,
    pub target: Option<(String, String)>,
    /// An exact action, or a prefix ending in `.` such as `user.`.
    pub action: Option<String>,
    /// Only events before this one in the log's newest-first order.
    pub before: Option<(i64, String)>,
}

/// The newest events matching `filter`, up to `limit`.
pub async fn list(
    executor: &mut impl Executor,
    filter: &Filter,
    limit: u64,
) -> db::Result<Vec<Event>> {
    let mut select = Query::select();
    select
        .columns([
            AuditEvents::Id,
            AuditEvents::ActorUserId,
            AuditEvents::ActorLabel,
            AuditEvents::Action,
            AuditEvents::TargetType,
            AuditEvents::TargetId,
            AuditEvents::MetadataJson,
            AuditEvents::Ip,
            AuditEvents::CreatedAt,
        ])
        .from(AuditEvents::Table)
        .order_by(AuditEvents::CreatedAt, Order::Desc)
        .order_by(AuditEvents::Id, Order::Desc)
        .limit(limit);
    if let Some(actor) = &filter.actor_user_id {
        select.and_where(Expr::col(AuditEvents::ActorUserId).eq(actor.as_str()));
    }
    if let Some((kind, id)) = &filter.target {
        select
            .and_where(Expr::col(AuditEvents::TargetType).eq(kind.as_str()))
            .and_where(Expr::col(AuditEvents::TargetId).eq(id.as_str()));
    }
    if let Some(action) = &filter.action {
        if action.ends_with('.') {
            let pattern = action
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            select.and_where(
                Expr::col(AuditEvents::Action)
                    .like(LikeExpr::new(format!("{pattern}%")).escape('\\')),
            );
        } else {
            select.and_where(Expr::col(AuditEvents::Action).eq(action.as_str()));
        }
    }
    if let Some((created_at, id)) = &filter.before {
        select.and_where(
            Expr::col(AuditEvents::CreatedAt)
                .lt(*created_at)
                .or(Expr::col(AuditEvents::CreatedAt)
                    .eq(*created_at)
                    .and(Expr::col(AuditEvents::Id).lt(id.as_str()))),
        );
    }
    let rows: Vec<EventRow> = executor.fetch_all(&select).await?;
    Ok(rows
        .into_iter()
        .map(|row| Event {
            metadata: serde_json::from_str(&row.metadata_json).unwrap_or(Value::Null),
            id: row.id,
            actor_user_id: row.actor_user_id,
            actor_label: row.actor_label,
            action: row.action,
            target_type: row.target_type,
            target_id: row.target_id,
            ip: row.ip,
            created_at: row.created_at,
        })
        .collect())
}
