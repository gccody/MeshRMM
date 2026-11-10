//! Device cookies (OWASP "device cookies"): after a full sign-in the browser
//! gets a long-lived cookie naming the account, signed with a key derived
//! from the instance key. Password attempts from a browser presenting one are
//! limited on their own, apart from the account-wide limit, so a stranger's
//! failures can't keep the account's owner out. The cookie grants nothing
//! else; the server keeps no state for it.
use axum::http::{HeaderMap, HeaderValue, header};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::secrets::{InstanceKey, hex, random_bytes};

/// `__Host-` makes browsers insist on Secure, Path=/ and no Domain, like the
/// session cookie.
pub const COOKIE_NAME: &str = "__Host-meshrmm-device";
const MAX_AGE_SECONDS: u64 = 365 * 24 * 60 * 60;
const KEY_LABEL: &[u8] = b"meshrmm device cookie v1";
/// 16 random bytes as hex.
const DEVICE_ID_LENGTH: usize = 32;
/// An HMAC-SHA256 as hex.
const SIGNATURE_LENGTH: usize = 64;

/// A `Set-Cookie` value naming a new device for the account with the
/// normalised `email`.
pub fn cookie(key: &InstanceKey, email: &str) -> HeaderValue {
    let device = hex(&random_bytes::<{ DEVICE_ID_LENGTH / 2 }>());
    let signature = sign(key, email, &device);
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={device}.{signature}; Path=/; Max-Age={MAX_AGE_SECONDS}; HttpOnly; Secure; SameSite=Lax"
    ))
    .expect("hex is a valid header value")
}

/// The device the request's cookie names, if it was issued for the account
/// with the normalised `email`.
pub fn device(key: &InstanceKey, headers: &HeaderMap, email: &str) -> Option<String> {
    let value = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)?
        .1;
    let (device, signature) = value.split_once('.')?;
    if device.len() != DEVICE_ID_LENGTH || signature.len() != SIGNATURE_LENGTH {
        return None;
    }
    let expected = sign(key, email, device);
    bool::from(expected.as_bytes().ct_eq(signature.as_bytes())).then(|| device.to_owned())
}

fn sign(key: &InstanceKey, email: &str, device: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&key.derive(KEY_LABEL))
        .expect("HMAC accepts keys of any length");
    // Neither part can contain a newline, so the message is unambiguous.
    mac.update(email.as_bytes());
    mac.update(b"\n");
    mac.update(device.as_bytes());
    hex(&mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_key() -> InstanceKey {
        InstanceKey::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
    }

    fn presented(set_cookie: &HeaderValue) -> HeaderMap {
        let pair = set_cookie.to_str().unwrap().split(';').next().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("other=1; {pair}")).unwrap(),
        );
        headers
    }

    #[test]
    fn a_device_cookie_names_one_account() {
        let key = new_key();
        let set_cookie = cookie(&key, "ada@example.com");
        let attributes = set_cookie.to_str().unwrap();
        assert!(attributes.starts_with("__Host-meshrmm-device="));
        assert!(attributes.ends_with("; HttpOnly; Secure; SameSite=Lax"));
        let headers = presented(&set_cookie);
        let device = device(&key, &headers, "ada@example.com").unwrap();
        assert_eq!(device.len(), DEVICE_ID_LENGTH);
        assert_eq!(super::device(&key, &headers, "bob@example.com"), None);
        assert_eq!(super::device(&new_key(), &headers, "ada@example.com"), None);
        assert_eq!(
            super::device(&key, &HeaderMap::new(), "ada@example.com"),
            None
        );

        let mut forged = HeaderMap::new();
        forged.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{COOKIE_NAME}={device}.{}", "0".repeat(64))).unwrap(),
        );
        assert_eq!(super::device(&key, &forged, "ada@example.com"), None);
    }
}
