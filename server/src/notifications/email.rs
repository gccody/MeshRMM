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

/// More notifications than this waiting together go out as one digest.
pub const DIGEST_THRESHOLD: usize = 20;
/// How many summaries a digest lists before saying how many more there are.
pub const DIGEST_MAX_LINES: usize = 300;

/// Emails `notifications`, in order, to each recipient separately, so nobody
/// sees the others' addresses. The recipients are looked up once and the
/// messages share an SMTP connection. More than [`DIGEST_THRESHOLD`] become
/// one [`digest`] for each recipient.
pub async fn send(
    state: &AppState,
    channel_id: &str,
    notifications: &[&Notification],
) -> Result<(), Failure> {
    let mut database = &state.database;
    let settings = settings::load(&mut database)
        .await
        .map_err(|_| Failure::transient("the server's settings could not be read"))?;
    let smtp = Smtp::from_settings(&settings, &state.instance_key, &mail::hello_name(state))
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
    let messages = if notifications.len() > DIGEST_THRESHOLD {
        vec![digest(
            notifications,
            &settings.instance_name,
            &state.config.public_origin(),
        )]
    } else {
        notifications
            .iter()
            .map(|notification| (notification.summary.clone(), notification.body.clone()))
            .collect()
    };
    let mut failed = BTreeSet::new();
    for (subject, body) in &messages {
        for to in &recipients {
            if let Err(error) = smtp.send(to, subject, body.clone()).await {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not email a notification"
                );
                failed.insert(to.as_str());
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Failure::permanent(format!(
            "the email to {} of {} recipients failed: {}",
            failed.len(),
            recipients.len(),
            failed.into_iter().collect::<Vec<_>>().join(", ")
        )))
    }
}

/// One email's subject and body for many notifications: how many, and each
/// one's summary, up to [`DIGEST_MAX_LINES`] of them.
pub fn digest(
    notifications: &[&Notification],
    instance_name: &str,
    origin: &str,
) -> (String, String) {
    let count = notifications.len();
    let event = notifications
        .first()
        .map(|first| first.event)
        .filter(|event| notifications.iter().all(|other| other.event == *event));
    let what = match event {
        Some("alert.raised") => "alerts raised",
        Some("alert.resolved") => "alerts resolved",
        _ => "notifications",
    };
    let subject = format!("{count} {what} on {instance_name}");
    let mut body = format!(
        "{count} notifications from {instance_name} came at once, so they are listed in this one email:\n\n"
    );
    for notification in notifications.iter().take(DIGEST_MAX_LINES) {
        body.push_str(&notification.summary);
        body.push('\n');
    }
    if count > DIGEST_MAX_LINES {
        body.push_str(&format!("and {} more\n", count - DIGEST_MAX_LINES));
    }
    body.push_str(&format!("\nSee them at {origin}\n"));
    (subject, body)
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    fn notification(event: &'static str, summary: String) -> Notification {
        Notification {
            event,
            summary,
            body: String::new(),
            data: Map::new(),
        }
    }

    #[test]
    fn digests_count_and_list_the_summaries() {
        let raised: Vec<_> = (0..DIGEST_MAX_LINES + 7)
            .map(|n| notification("alert.raised", format!("Critical: Offline on PC-{n}")))
            .collect();
        let all: Vec<_> = raised.iter().collect();
        let (subject, body) = digest(&all, "Acme IT", "https://rmm.example.com");
        assert_eq!(subject, "307 alerts raised on Acme IT");
        assert!(body.starts_with("307 notifications from Acme IT"), "{body}");
        assert!(body.contains("\nCritical: Offline on PC-0\nCritical: Offline on PC-1\n"));
        assert!(
            body.contains("\nCritical: Offline on PC-299\nand 7 more\n"),
            "{body}"
        );
        assert!(!body.contains("PC-300"));
        assert!(body.ends_with("See them at https://rmm.example.com\n"));

        let mixed = notification("job.failed", "A job failed".into());
        let (subject, body) = digest(&[&raised[0], &mixed], "Acme IT", "https://x");
        assert_eq!(subject, "2 notifications on Acme IT");
        assert!(!body.contains("more"));
    }
}
