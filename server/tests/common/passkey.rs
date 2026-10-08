//! A software passkey: an ES256 authenticator that answers WebAuthn prompts
//! the way a browser would send them to the server.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{
    bn::BigNumContext,
    ec::{EcGroup, EcKey, PointConversionForm},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    sign::Signer,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const USER_PRESENT: u8 = 0x01;
const USER_VERIFIED: u8 = 0x04;
const ATTESTED_CREDENTIAL: u8 = 0x40;

pub struct SoftPasskey {
    key: PKey<Private>,
    pub credential_id: Vec<u8>,
    /// The user handle from registration, returned with each assertion like
    /// a discoverable credential does.
    user_handle: Vec<u8>,
    counter: u32,
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(text: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).unwrap()
}

/// A CBOR item header: major type and length or value.
fn head(major: u8, value: u64) -> Vec<u8> {
    let major = major << 5;
    match value {
        0..=23 => vec![major | value as u8],
        24..=0xff => vec![major | 24, value as u8],
        0x100..=0xffff => [vec![major | 25], (value as u16).to_be_bytes().to_vec()].concat(),
        _ => [vec![major | 26], (value as u32).to_be_bytes().to_vec()].concat(),
    }
}

fn int(value: i64) -> Vec<u8> {
    if value >= 0 {
        head(0, value as u64)
    } else {
        head(1, (-1 - value) as u64)
    }
}

fn bytes(value: &[u8]) -> Vec<u8> {
    [head(2, value.len() as u64), value.to_vec()].concat()
}

fn text(value: &str) -> Vec<u8> {
    [head(3, value.len() as u64), value.as_bytes().to_vec()].concat()
}

fn map(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = head(5, entries.len() as u64);
    for (key, value) in entries {
        out.extend_from_slice(key);
        out.extend_from_slice(value);
    }
    out
}

fn client_data(kind: &str, challenge: &str, origin: &str) -> Vec<u8> {
    json!({ "type": kind, "challenge": challenge, "origin": origin, "crossOrigin": false })
        .to_string()
        .into_bytes()
}

impl SoftPasskey {
    /// Answers `navigator.credentials.create` for `options` (the server's
    /// `{ publicKey }` JSON), as a page on `origin`.
    pub fn register(options: &Value, origin: &str) -> (Self, Value) {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let ec = EcKey::generate(&group).unwrap();
        let mut credential_id = vec![0; 32];
        getrandom::fill(&mut credential_id).unwrap();
        let passkey = Self {
            key: PKey::from_ec_key(ec).unwrap(),
            credential_id,
            user_handle: unb64(options["publicKey"]["user"]["id"].as_str().unwrap()),
            counter: 0,
        };
        let credential = passkey.registration(options, origin);
        (passkey, credential)
    }

    /// Registers this same credential again, as an authenticator that
    /// ignores `excludeCredentials` would.
    pub fn registration(&self, options: &Value, origin: &str) -> Value {
        let public_key = &options["publicKey"];
        let rp_id = public_key["rp"]["id"].as_str().unwrap();
        let ec = self.key.ec_key().unwrap();
        let mut context = BigNumContext::new().unwrap();
        let point = ec
            .public_key()
            .to_bytes(ec.group(), PointConversionForm::UNCOMPRESSED, &mut context)
            .unwrap();
        let (x, y) = point[1..].split_at(32);
        let cose_key = map(&[
            (int(1), int(2)),
            (int(3), int(-7)),
            (int(-1), int(1)),
            (int(-2), bytes(x)),
            (int(-3), bytes(y)),
        ]);
        let mut auth_data = Sha256::digest(rp_id.as_bytes()).to_vec();
        auth_data.push(USER_PRESENT | USER_VERIFIED | ATTESTED_CREDENTIAL);
        auth_data.extend_from_slice(&0u32.to_be_bytes());
        auth_data.extend_from_slice(&[0; 16]);
        auth_data.extend_from_slice(&(self.credential_id.len() as u16).to_be_bytes());
        auth_data.extend_from_slice(&self.credential_id);
        auth_data.extend_from_slice(&cose_key);
        let attestation = map(&[
            (text("fmt"), text("none")),
            (text("attStmt"), map(&[])),
            (text("authData"), bytes(&auth_data)),
        ]);
        let client_data = client_data(
            "webauthn.create",
            public_key["challenge"].as_str().unwrap(),
            origin,
        );
        json!({
            "id": b64(&self.credential_id),
            "rawId": b64(&self.credential_id),
            "type": "public-key",
            "response": {
                "clientDataJSON": b64(&client_data),
                "attestationObject": b64(&attestation),
            },
            "extensions": {},
        })
    }

    /// Answers `navigator.credentials.get` for `options`, as a page on
    /// `origin`.
    pub fn assert(&mut self, options: &Value, origin: &str) -> Value {
        let public_key = &options["publicKey"];
        let rp_id = public_key["rpId"].as_str().unwrap();
        self.counter += 1;
        let mut auth_data = Sha256::digest(rp_id.as_bytes()).to_vec();
        auth_data.push(USER_PRESENT | USER_VERIFIED);
        auth_data.extend_from_slice(&self.counter.to_be_bytes());
        let client_data = client_data(
            "webauthn.get",
            public_key["challenge"].as_str().unwrap(),
            origin,
        );
        let mut signer = Signer::new(MessageDigest::sha256(), &self.key).unwrap();
        signer.update(&auth_data).unwrap();
        signer.update(&Sha256::digest(&client_data)).unwrap();
        let signature = signer.sign_to_vec().unwrap();
        json!({
            "id": b64(&self.credential_id),
            "rawId": b64(&self.credential_id),
            "type": "public-key",
            "response": {
                "authenticatorData": b64(&auth_data),
                "clientDataJSON": b64(&client_data),
                "signature": b64(&signature),
                "userHandle": b64(&self.user_handle),
            },
            "extensions": {},
        })
    }

    /// Sets the signature counter, e.g. back, as a cloned authenticator would.
    pub fn set_counter(&mut self, counter: u32) {
        self.counter = counter;
    }
}
