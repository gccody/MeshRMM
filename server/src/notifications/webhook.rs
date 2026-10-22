//! Webhook channels: a JSON POST to the channel's URL.
//!
//! The body is the notification's data in an envelope:
//! `{"event", "delivery_id", "sent_at", "server", "summary", "text", ...}`.
//! `text` repeats the summary for chat services that show that field. With a
//! signing secret, `X-MeshRMM-Signature` is `sha256=` and the hex
//! HMAC-SHA256 of `<X-MeshRMM-Timestamp>.<body>` keyed with the secret, so
//! the receiver can check the request came from this server and is recent.
use std::{sync::OnceLock, time::Duration};

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use super::{Failure, Notification};
use crate::{secrets::hex, time::now_ms};

pub const EVENT_HEADER: &str = "x-meshrmm-event";
pub const DELIVERY_HEADER: &str = "x-meshrmm-delivery";
pub const TIMESTAMP_HEADER: &str = "x-meshrmm-timestamp";
pub const SIGNATURE_HEADER: &str = "x-meshrmm-signature";
/// How long to wait before each retry of a failed delivery.
pub const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(10),
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
];
const TIMEOUT: Duration = Duration::from_secs(15);
pub const MAX_URL_LENGTH: usize = 2048;
pub const MAX_SECRET_LENGTH: usize = 256;

/// The client for every webhook. It never follows redirects, so a receiver
/// can't send the request, and its signature, somewhere else.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .user_agent(concat!("MeshRMM/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("the HTTP client's settings are valid")
    })
}

/// Checks a webhook URL: absolute `http` or `https` with a host.
pub fn validate_url(raw: &str) -> Result<String, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_URL_LENGTH {
        return Err("the webhook URL must be 1 to 2048 characters");
    }
    let url = url::Url::parse(raw).map_err(|_| "the webhook URL is not a valid URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err("the webhook URL must start with https:// or http:// and name a host");
    }
    Ok(url.to_string())
}

/// The URL's host, which is all the audit log keeps: URLs such as chat
/// services' often carry a secret token.
pub fn url_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The JSON body for `notification`.
pub fn body(notification: &Notification, delivery_id: &str, sent_at: i64, server: &str) -> Vec<u8> {
    let mut body = json!({
        "event": notification.event,
        "delivery_id": delivery_id,
        "sent_at": sent_at,
        "server": server,
        "summary": notification.summary,
        "text": notification.summary,
    });
    if let Value::Object(fields) = &mut body {
        for (key, value) in &notification.data {
            fields.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    serde_json::to_vec(&body).expect("notifications serialize")
}

/// `sha256=<hex HMAC-SHA256 of "<timestamp>.<body>">`.
pub fn signature(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex(&mac.finalize().into_bytes()))
}

/// Posts `notification` to `url` once. A 2xx answer is success; a network
/// error, a 5xx, 408 or 429 may succeed later.
pub async fn send(
    url: &str,
    secret: Option<&[u8]>,
    notification: &Notification,
    server: &str,
) -> Result<(), Failure> {
    let delivery_id = uuid::Uuid::new_v4().to_string();
    let sent_at = now_ms();
    let body = body(notification, &delivery_id, sent_at, server);
    let mut request = client()
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(EVENT_HEADER, notification.event)
        .header(DELIVERY_HEADER, &delivery_id)
        .header(TIMESTAMP_HEADER, sent_at.to_string());
    if let Some(secret) = secret {
        request = request.header(SIGNATURE_HEADER, signature(secret, sent_at, &body));
    }
    let response = request.body(body).send().await.map_err(|error| {
        Failure::transient(format!(
            "could not reach the webhook: {}",
            without_url(&error)
        ))
    })?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let message = format!("the webhook answered {status}");
    if status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        Err(Failure::transient(message))
    } else {
        Err(Failure::permanent(message))
    }
}

/// Why the request failed, without its URL, which may carry a secret token
/// and is shown to users.
fn without_url(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "it took too long to answer".to_owned();
    }
    // The innermost cause says what actually went wrong.
    let mut cause: &dyn std::error::Error = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    let text = cause.to_string();
    match error.url() {
        Some(url) => text.replace(url.as_str(), "the URL"),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    fn notification() -> Notification {
        let mut data = Map::new();
        data.insert("alert".into(), json!({ "id": "a1" }));
        // The envelope wins over data with the same name.
        data.insert("event".into(), json!("spoofed"));
        Notification {
            event: "alert.raised",
            summary: "CPU above 90% on DESK-1".into(),
            body: String::new(),
            data,
        }
    }

    #[test]
    fn the_body_wraps_the_data_in_an_envelope() {
        let body: Value = serde_json::from_slice(&body(
            &notification(),
            "d1",
            1234,
            "https://rmm.example.com",
        ))
        .unwrap();
        assert_eq!(
            body,
            json!({
                "event": "alert.raised",
                "delivery_id": "d1",
                "sent_at": 1234,
                "server": "https://rmm.example.com",
                "summary": "CPU above 90% on DESK-1",
                "text": "CPU above 90% on DESK-1",
                "alert": { "id": "a1" },
            })
        );
    }

    #[test]
    fn signatures_are_hmac_sha256_of_the_timestamp_and_body() {
        // echo -n '1700000000000.{"a":1}' | openssl dgst -sha256 -hmac secret
        // printf '%s' '1700000000000.{"a":1}' | openssl dgst -sha256 -hmac secret
        assert_eq!(
            signature(b"secret", 1_700_000_000_000, br#"{"a":1}"#),
            "sha256=4ef2732b0d632a6897af3a6d02a6de287f60d6c5f15dabdb0ceb045a34e3c5a7"
        );
        assert_ne!(
            signature(b"secret", 1, b"body"),
            signature(b"other", 1, b"body")
        );
        assert_ne!(
            signature(b"secret", 1, b"body"),
            signature(b"secret", 2, b"body")
        );
    }

    #[test]
    fn urls_must_be_http_with_a_host() {
        assert_eq!(
            validate_url(" https://hooks.example.com/x?token=1 ").unwrap(),
            "https://hooks.example.com/x?token=1"
        );
        assert!(validate_url("http://10.0.0.5:8080/hook").is_ok());
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("https://").is_err());
        assert!(validate_url("not a url").is_err());
        assert!(validate_url("").is_err());
        assert_eq!(
            url_host("https://hooks.slack.com/services/T/B/secret"),
            "hooks.slack.com"
        );
    }
}
