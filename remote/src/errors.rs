//! Plain-language explanations for the errors that end the viewer. The full
//! error chain still goes to the viewer log.
use std::fmt;

/// An error response from the MeshRMM API.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "MeshRMM API returned HTTP {}: {}",
            self.status, self.message
        )
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// Reads the API's `{"error": "..."}` body, or a short piece of any other body.
    pub async fn from_response(response: reqwest::Response) -> Self {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Self {
            status,
            message: api_message(&body),
        }
    }
}

fn api_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| body.trim().chars().take(200).collect())
}

/// An error whose text is already written for the user.
#[derive(Debug)]
pub struct UserFacing(pub String);

impl fmt::Display for UserFacing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for UserFacing {}

enum Cause<'a> {
    Status(u16),
    Unreachable,
    Identity,
    Ended,
    Explained(&'a str),
    Other,
}

fn cause(error: &anyhow::Error) -> Cause<'_> {
    use tokio_tungstenite::tungstenite::Error as WebSocketError;
    for cause in error.chain() {
        if let Some(explained) = cause.downcast_ref::<UserFacing>() {
            return Cause::Explained(&explained.0);
        }
        if let Some(api) = cause.downcast_ref::<ApiError>() {
            return Cause::Status(api.status);
        }
        if let Some(http) = cause.downcast_ref::<reqwest::Error>() {
            if let Some(status) = http.status() {
                return Cause::Status(status.as_u16());
            }
            if http.is_timeout() || http.is_connect() {
                return Cause::Unreachable;
            }
        }
        match cause.downcast_ref::<WebSocketError>() {
            Some(WebSocketError::Http(response)) => {
                return Cause::Status(response.status().as_u16());
            }
            Some(WebSocketError::Io(_) | WebSocketError::Tls(_)) => return Cause::Unreachable,
            _ => {}
        }
        if cause
            .downcast_ref::<meshrmm_session_transport::identity::IdentityError>()
            .is_some()
        {
            return Cause::Identity;
        }
        if cause
            .downcast_ref::<meshrmm_signaling_client::TerminalClose>()
            .is_some()
        {
            return Cause::Ended;
        }
    }
    Cause::Other
}

/// Explains why the viewer stopped, in terms a technician can act on.
pub fn user_message(error: &anyhow::Error) -> String {
    let message = match cause(error) {
        Cause::Explained(message) => return message.to_owned(),
        Cause::Status(400 | 401) => {
            "This connection link has expired or was already used. Start the connection again from the dashboard."
        }
        Cause::Status(403) => "You do not have permission to connect to this device.",
        Cause::Status(404) => {
            "The device or remote session was not found. The device may have been removed, or the session already ended."
        }
        Cause::Status(409) => {
            "The device is offline or already has an active remote session. If you just closed a session to it, wait a moment and try again."
        }
        Cause::Status(410) | Cause::Ended => "This remote session was ended.",
        Cause::Status(429) => "Too many connection attempts. Wait a minute and try again.",
        Cause::Status(status) if status >= 500 => {
            "The MeshRMM service is temporarily unavailable. Try again in a moment."
        }
        Cause::Unreachable => {
            "Could not reach the MeshRMM service. Check your internet connection and try again."
        }
        Cause::Identity => {
            "The device's identity did not match the identity this viewer trusts, so the connection was stopped. If the device was reinstalled, trust its new identity first."
        }
        Cause::Status(_) | Cause::Other => {
            let detail: String = format!("{error:#}").chars().take(400).collect();
            return format!("The remote session could not continue.\n\nDetails: {detail}");
        }
    };
    message.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    fn api(status: u16) -> anyhow::Error {
        anyhow::Error::new(ApiError {
            status,
            message: "raw server text".into(),
        })
        .context("remote session request failed")
    }

    #[test]
    fn api_statuses_become_plain_messages_without_server_text() {
        for (status, expected) in [
            (401, "expired or was already used"),
            (403, "permission"),
            (404, "not found"),
            (409, "offline or already has an active remote session"),
            (410, "was ended"),
            (429, "Too many"),
            (503, "temporarily unavailable"),
        ] {
            let message = user_message(&api(status));
            assert!(message.contains(expected), "{status}: {message}");
            assert!(!message.contains("raw server text"), "{status}: {message}");
        }
    }

    #[test]
    fn websocket_upgrade_statuses_are_explained() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(410)
            .body(None)
            .unwrap();
        let error =
            anyhow::Error::new(tokio_tungstenite::tungstenite::Error::Http(response.into()));
        assert!(user_message(&error).contains("was ended"));
    }

    #[test]
    fn identity_and_explained_errors() {
        let identity = anyhow::Error::new(meshrmm_session_transport::identity::IdentityError(
            "Peer identity verification failed: mismatch".into(),
        ))
        .context("remote viewer session can no longer be resumed");
        assert!(user_message(&identity).contains("identity did not match"));
        let explained: anyhow::Error = UserFacing("Close it and try again.".into()).into();
        assert_eq!(
            user_message(&explained.context("startup failed")),
            "Close it and try again."
        );
    }

    #[test]
    fn other_errors_keep_their_detail() {
        let error = Err::<(), _>(anyhow::anyhow!(
            "Agent disconnected from the remote session"
        ))
        .context("remote viewer session can no longer be resumed")
        .unwrap_err();
        let message = user_message(&error);
        assert!(message.starts_with("The remote session could not continue."));
        assert!(message.contains("Agent disconnected"));
    }

    #[test]
    fn api_message_prefers_the_json_error_field() {
        assert_eq!(
            api_message(r#"{"error":"Agent not found"}"#),
            "Agent not found"
        );
        assert_eq!(api_message("  plain text  "), "plain text");
        assert_eq!(api_message(&"x".repeat(500)).len(), 200);
    }
}
