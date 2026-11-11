//! Emailing a schedule's reports to its recipients, through the server's
//! optional SMTP settings.
//!
//! Each recipient gets their own message, so nobody sees the others'
//! addresses: a short summary in text and HTML, a link to the report on the
//! server, and the report's HTML page attached while it is at most
//! [`MAX_ATTACHMENT_BYTES`]. Without email the report is still made and
//! kept, and says it wasn't sent.
use sea_query::{Expr, ExprTrait, Order, Query};

use super::{
    document::{Document, Section, format_bytes},
    html, schedules,
};
use crate::{
    db::tables::Reports,
    http::AppState,
    mail::{Attachment, Email, Smtp},
    settings,
};

/// The largest report attached to an email. Base64 makes it a third larger
/// in the message, which stays under the 25 MB most mail servers accept.
pub const MAX_ATTACHMENT_BYTES: usize = 15 * 1024 * 1024;
/// Reports emailed in one step at most.
const BATCH: u64 = 20;

/// Emails the reports made by schedules that wait to be sent. Returns how
/// many it settled.
pub async fn deliver_pending(state: &AppState) -> anyhow::Result<usize> {
    let pending: Vec<(String, Option<String>)> = state
        .database
        .fetch_all(
            &Query::select()
                .columns([Reports::Id, Reports::ScheduleId])
                .from(Reports::Table)
                .and_where(Expr::col(Reports::Delivery).eq("pending"))
                .and_where(Expr::col(Reports::Status).eq("succeeded"))
                .order_by(Reports::CreatedAt, Order::Asc)
                .limit(BATCH)
                .to_owned(),
        )
        .await?;
    let mut delivered = 0;
    for (id, schedule_id) in pending {
        let (delivery, detail) = deliver(state, &id, schedule_id.as_deref()).await;
        state
            .database
            .execute(
                &Query::update()
                    .table(Reports::Table)
                    .value(Reports::Delivery, delivery)
                    .value(Reports::DeliveryDetail, detail)
                    .and_where(Expr::col(Reports::Id).eq(id.as_str()))
                    .to_owned(),
            )
            .await?;
        delivered += 1;
    }
    Ok(delivered)
}

/// Sends the report, and says how it went: the delivery state and its
/// detail.
async fn deliver(state: &AppState, id: &str, schedule_id: Option<&str>) -> (&'static str, String) {
    let mut database = &state.database;
    let settings = match settings::load(&mut database).await {
        Ok(settings) => settings,
        Err(_) => {
            return (
                "failed",
                "The server's settings couldn't be read.".to_owned(),
            );
        }
    };
    let smtp = match Smtp::from_settings(
        &settings,
        &state.instance_key,
        &crate::mail::hello_name(state),
    ) {
        Ok(Some(smtp)) => smtp,
        Ok(None) => {
            return (
                "not_configured",
                "Email isn't set up on this server, so the report was kept here and not sent."
                    .to_owned(),
            );
        }
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "the email settings are invalid"
            );
            return (
                "failed",
                "The email settings are invalid; check them under Authentication.".to_owned(),
            );
        }
    };
    let schedule = match schedule_id {
        Some(schedule_id) => schedules::by_id(&mut database, schedule_id)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let Some(schedule) = schedule else {
        return (
            "failed",
            "Its schedule was deleted before it could be sent.".to_owned(),
        );
    };
    let recipients = match schedules::recipients(&mut database, &schedule).await {
        Ok(recipients) if recipients.is_empty() => {
            return (
                "failed",
                "Nobody receives it: the schedule's users are disabled or removed.".to_owned(),
            );
        }
        Ok(recipients) => recipients,
        Err(_) => return ("failed", "The recipients couldn't be read.".to_owned()),
    };
    let document = match super::document(state, id).await {
        Ok(Some(document)) => document,
        _ => return ("failed", "The report's file couldn't be read.".to_owned()),
    };
    let link = format!("{}/reports?report={id}", state.config.public_origin());
    let email = message(&document, &link);
    let mut failed = Vec::new();
    for to in &recipients {
        if let Err(error) = smtp.send_email(to, &email).await {
            tracing::warn!(error = format!("{error:#}"), "could not email a report");
            failed.push(to.as_str());
        }
    }
    if failed.is_empty() {
        (
            "sent",
            format!(
                "Sent to {}.",
                super::document::plural(recipients.len() as i64, "recipient")
            ),
        )
    } else {
        (
            "failed",
            format!(
                "The email to {} of {} recipients failed: {}.",
                failed.len(),
                recipients.len(),
                failed.join(", ")
            ),
        )
    }
}

/// The email for a report: what it is, its first section's figures, the
/// link, and the page attached unless it's too large.
pub fn message(document: &Document, link: &str) -> Email {
    let page = html::render(document);
    let attach = page.len() <= MAX_ATTACHMENT_BYTES;
    let subject = format!(
        "{}: {}, {}",
        document.title, document.scope_label, document.period_label
    );
    let figures = document
        .sections
        .first()
        .map(|section: &Section| section.figures.as_slice())
        .unwrap_or_default();
    let mut text = format!(
        "{instance} made the {title} report.\n\nDevices: {scope}\nPeriod: {period}\nTime zone: {zone}\n\n",
        instance = document.instance_name,
        title = document.title,
        scope = document.scope_label,
        period = document.period_label,
        zone = document.time_zone,
    );
    for figure in figures {
        text.push_str(&format!("{}: {}\n", figure.label, figure.value));
    }
    let where_ = if attach {
        "The report is attached as a web page: open it in a browser, and print it to save it as a PDF. It is also kept on the server"
    } else {
        "The report is too large to attach, so open it on the server"
    };
    text.push_str(&format!("\n{where_}:\n{link}\n"));
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let mut body = format!(
        "<!doctype html><html><body style=\"font-family:system-ui,-apple-system,'Segoe UI',Roboto,Arial,sans-serif;font-size:15px;line-height:1.5\">\
         <p style=\"color:#5b6573;margin:0\">{instance}</p><h1 style=\"font-size:22px;margin:4px 0 12px\">{title}</h1>\
         <p style=\"margin:0 0 12px\">{scope}<br>{period} ({zone})</p><table style=\"border-collapse:collapse\">",
        instance = escape(&document.instance_name),
        title = escape(&document.title),
        scope = escape(&document.scope_label),
        period = escape(&document.period_label),
        zone = escape(&document.time_zone),
    );
    for figure in figures {
        body.push_str(&format!(
            "<tr><td style=\"padding:2px 16px 2px 0;color:#5b6573\">{}</td><td style=\"padding:2px 0;font-weight:600\">{}</td></tr>",
            escape(&figure.label),
            escape(&figure.value)
        ));
    }
    body.push_str(&format!(
        "</table><p>{} at <a href=\"{link}\">{link}</a>.</p></body></html>",
        escape(where_),
        link = escape(link)
    ));
    if !attach {
        tracing::info!(
            size = format_bytes(page.len() as i64),
            "a report is too large to attach to its email"
        );
    }
    Email {
        subject,
        text,
        html: Some(body),
        attachment: attach.then(|| Attachment {
            filename: format!("{}.html", html::file_stem(document)),
            content_type: "text/html; charset=utf-8".to_owned(),
            body: page.into_bytes(),
        }),
    }
}
