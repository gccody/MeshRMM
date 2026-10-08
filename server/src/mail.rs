//! Email through the operator's SMTP server. Email is optional: without it,
//! invitation and password reset links are shown to the administrator to pass
//! on instead.
use std::time::Duration;

use anyhow::Context;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::{
        authentication::Credentials,
        client::{Tls, TlsParameters},
        extension::ClientId,
    },
};

use crate::{http::AppState, secrets::InstanceKey, settings::Settings};

const SEND_TIMEOUT: Duration = Duration::from_secs(20);

/// The encryption context of the stored SMTP password.
pub const SMTP_PASSWORD_CONTEXT: &str = "smtp:password";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// Plain connection upgraded with STARTTLS, which must succeed.
    StartTls,
    /// TLS from the first byte (usually port 465).
    Tls,
    /// No encryption, for a relay on the same host or network.
    None,
}

impl Security {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "starttls" => Some(Self::StartTls),
            "tls" => Some(Self::Tls),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::StartTls => "starttls",
            Self::Tls => "tls",
            Self::None => "none",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Self::StartTls => 587,
            Self::Tls => 465,
            Self::None => 25,
        }
    }
}

/// A ready-to-use SMTP configuration.
#[derive(Debug, Clone)]
pub struct Smtp {
    host: String,
    port: u16,
    security: Security,
    credentials: Option<(String, String)>,
    from: Mailbox,
}

impl Smtp {
    /// The configured server, or `None` when email is off.
    pub fn from_settings(settings: &Settings, key: &InstanceKey) -> anyhow::Result<Option<Self>> {
        let (Some(host), Some(from)) = (&settings.smtp_host, &settings.smtp_from) else {
            return Ok(None);
        };
        let security = Security::parse(&settings.smtp_security)
            .with_context(|| format!("unknown SMTP security {:?}", settings.smtp_security))?;
        let port = match settings.smtp_port {
            Some(port) => u16::try_from(port).context("the SMTP port is out of range")?,
            None => security.default_port(),
        };
        let credentials = match (&settings.smtp_username, &settings.smtp_password_encrypted) {
            (Some(username), Some(sealed)) => {
                let password = key
                    .decrypt(SMTP_PASSWORD_CONTEXT, sealed)
                    .context("could not decrypt the SMTP password")?;
                Some((
                    username.clone(),
                    String::from_utf8(password).context("the SMTP password is not UTF-8")?,
                ))
            }
            (Some(username), None) => Some((username.clone(), String::new())),
            _ => None,
        };
        Ok(Some(Self {
            host: host.clone(),
            port,
            security,
            credentials,
            from: from.parse().context("the SMTP sender address is invalid")?,
        }))
    }

    /// Sends a plain-text message.
    pub async fn send(
        &self,
        hello_name: &str,
        to: &str,
        subject: &str,
        body: String,
    ) -> anyhow::Result<()> {
        let message = Message::builder()
            .from(self.from.clone())
            .to(to
                .parse()
                .with_context(|| format!("{to} is not a valid recipient"))?)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body)
            .context("could not build the message")?;
        let tls = match self.security {
            Security::StartTls => Tls::Required(TlsParameters::new(self.host.clone())?),
            Security::Tls => Tls::Wrapper(TlsParameters::new(self.host.clone())?),
            Security::None => Tls::None,
        };
        let mut transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host)
            .port(self.port)
            .tls(tls)
            .timeout(Some(SEND_TIMEOUT))
            .hello_name(ClientId::Domain(hello_name.to_owned()));
        if let Some((username, password)) = &self.credentials {
            transport = transport.credentials(Credentials::new(username.clone(), password.clone()));
        }
        transport.build().send(message).await.with_context(|| {
            format!(
                "the SMTP server {}:{} refused or failed",
                self.host, self.port
            )
        })?;
        Ok(())
    }
}

/// How a one-time link reached its recipient.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Delivery {
    /// Whether the link was emailed.
    pub emailed: bool,
    /// The link, when it wasn't emailed and the administrator must pass it on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Why email failed, when it was configured but didn't work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_error: Option<String>,
}

/// Emails `link` if SMTP is configured; otherwise (or if sending fails)
/// returns it for the administrator to pass on.
pub async fn deliver_link(
    state: &AppState,
    settings: &Settings,
    to: &str,
    subject: &str,
    body: String,
    link: String,
) -> Delivery {
    let failure = match Smtp::from_settings(settings, &state.instance_key) {
        Ok(None) => None,
        Ok(Some(smtp)) => match smtp.send(&hello_name(state), to, subject, body).await {
            Ok(()) => {
                return Delivery {
                    emailed: true,
                    link: None,
                    email_error: None,
                };
            }
            Err(error) => Some(error),
        },
        Err(error) => Some(error),
    };
    if let Some(error) = &failure {
        tracing::warn!(
            error = format!("{error:#}"),
            "could not send email; showing the link instead"
        );
    }
    Delivery {
        emailed: false,
        link: Some(link),
        email_error: failure
            .map(|_| "the email could not be sent; check the email settings".to_owned()),
    }
}

/// The name the server introduces itself with to the SMTP server.
pub fn hello_name(state: &AppState) -> String {
    state
        .config
        .public_url
        .host_str()
        .unwrap_or("localhost")
        .to_owned()
}

pub fn invitation_message(instance_name: &str, inviter: &str, link: &str) -> (String, String) {
    (
        format!("You're invited to {instance_name}"),
        format!(
            "{inviter} invited you to {instance_name}, the MeshRMM server for your organization.\n\n\
             Accept the invitation and choose a password here:\n{link}\n\n\
             The link works once and expires in 7 days. If you weren't expecting it, ignore this email.\n"
        ),
    )
}

pub fn password_reset_message(instance_name: &str, link: &str, hours: i64) -> (String, String) {
    (
        format!("Reset your {instance_name} password"),
        format!(
            "Someone asked to reset the password for your {instance_name} account.\n\n\
             Choose a new password here:\n{link}\n\n\
             The link works once and expires in {hours} hour{plural}. If you didn't ask, ignore this email; \
             your password hasn't changed.\n",
            plural = if hours == 1 { "" } else { "s" }
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_names_round_trip_with_default_ports() {
        for security in [Security::StartTls, Security::Tls, Security::None] {
            assert_eq!(Security::parse(security.as_str()), Some(security));
        }
        assert_eq!(Security::parse("ssl"), None);
        assert_eq!(Security::StartTls.default_port(), 587);
        assert_eq!(Security::Tls.default_port(), 465);
    }

    #[test]
    fn messages_include_the_link() {
        let (subject, body) =
            invitation_message("Acme IT", "ada@example.com", "https://x/invite#t");
        assert_eq!(subject, "You're invited to Acme IT");
        assert!(body.contains("https://x/invite#t"));
        let (_, body) = password_reset_message("Acme IT", "https://x/reset#t", 1);
        assert!(body.contains("expires in 1 hour."), "{body}");
    }
}
