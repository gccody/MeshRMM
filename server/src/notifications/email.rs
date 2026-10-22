//! Email channels: a message to each chosen user and each user holding a
//! chosen role, through the server's SMTP settings. Without SMTP the channel
//! can't deliver.
use std::collections::BTreeSet;

use sea_query::{Expr, ExprTrait, Query};

use super::{Failure, Notification};
use crate::{
    db::{
        self, Executor,
        tables::{EffectiveUserRoles, NotificationChannelRoles, NotificationChannelUsers, Users},
    },
    http::AppState,
    mail::{self, Smtp},
    settings,
};

/// The addresses of the channel's enabled recipients, chosen directly or
/// through a role.
pub async fn recipients(executor: &mut impl Executor, channel_id: &str) -> db::Result<Vec<String>> {
    let direct = Query::select()
        .column(NotificationChannelUsers::UserId)
        .from(NotificationChannelUsers::Table)
        .and_where(Expr::col(NotificationChannelUsers::ChannelId).eq(channel_id))
        .to_owned();
    let roles = Query::select()
        .column(NotificationChannelRoles::RoleId)
        .from(NotificationChannelRoles::Table)
        .and_where(Expr::col(NotificationChannelRoles::ChannelId).eq(channel_id))
        .to_owned();
    let through_roles = Query::select()
        .column(EffectiveUserRoles::UserId)
        .from(EffectiveUserRoles::Table)
        .and_where(Expr::col(EffectiveUserRoles::RoleId).in_subquery(roles))
        .to_owned();
    let rows: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .column(Users::Email)
                .from(Users::Table)
                .and_where(Expr::col(Users::Disabled).eq(false))
                .cond_where(
                    sea_query::Cond::any()
                        .add(Expr::col(Users::Id).in_subquery(direct))
                        .add(Expr::col(Users::Id).in_subquery(through_roles)),
                )
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|(email,)| email)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// Emails `notification` to each recipient separately, so nobody sees the
/// others' addresses.
pub async fn send(
    state: &AppState,
    channel_id: &str,
    notification: &Notification,
) -> Result<(), Failure> {
    let mut database = &state.database;
    let settings = settings::load(&mut database)
        .await
        .map_err(|_| Failure::transient("the server's settings could not be read"))?;
    let smtp = Smtp::from_settings(&settings, &state.instance_key)
        .map_err(|error| Failure::permanent(format!("the email settings are invalid: {error:#}")))?
        .ok_or_else(|| Failure::permanent("email isn't set up on this server"))?;
    let recipients = recipients(&mut database, channel_id)
        .await
        .map_err(|_| Failure::transient("the recipients could not be read"))?;
    if recipients.is_empty() {
        return Err(Failure::permanent(
            "nobody receives this channel: its users and roles have no enabled accounts",
        ));
    }
    let hello = mail::hello_name(state);
    let mut failed = Vec::new();
    for to in &recipients {
        if let Err(error) = smtp
            .send(&hello, to, &notification.summary, notification.body.clone())
            .await
        {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not email a notification"
            );
            failed.push(to.as_str());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Failure::permanent(format!(
            "the email to {} of {} recipients failed: {}",
            failed.len(),
            recipients.len(),
            failed.join(", ")
        )))
    }
}
