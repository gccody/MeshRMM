//! A mock OpenID Connect provider on a loopback port: discovery, keys, a
//! token endpoint that checks PKCE and the client secret, and userinfo. A
//! test plays the user's part by approving the sign-in with [`approve`].
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Form, Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{hash::MessageDigest, pkey::PKey, rsa::Rsa, sign::Signer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const CLIENT_ID: &str = "meshrmm";
pub const CLIENT_SECRET: &str = "provider secret";
const KEY_ID: &str = "test-key";

#[derive(Clone)]
struct Approved {
    claims: Value,
    code_challenge: String,
    redirect_uri: String,
}

#[derive(Clone)]
struct Shared {
    issuer: String,
    key: PKey<openssl::pkey::Private>,
    codes: Arc<Mutex<HashMap<String, Approved>>>,
    tokens: Arc<Mutex<HashMap<String, Value>>>,
    userinfo: Arc<Mutex<Option<Value>>>,
}

pub struct MockProvider {
    pub issuer: String,
    shared: Shared,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The parts of the authorization URL the provider would show the user.
#[derive(Debug)]
pub struct Authorization {
    pub state: String,
    pub nonce: String,
    pub code_challenge: String,
    pub redirect_uri: String,
    pub scope: String,
    pub client_id: String,
}

impl Authorization {
    pub fn parse(location: &str) -> Self {
        let url = url::Url::parse(location).unwrap();
        let query = url.query_pairs().into_owned().collect::<HashMap<_, _>>();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["code_challenge_method"], "S256");
        Self {
            state: query["state"].clone(),
            nonce: query["nonce"].clone(),
            code_challenge: query["code_challenge"].clone(),
            redirect_uri: query["redirect_uri"].clone(),
            scope: query["scope"].clone(),
            client_id: query["client_id"].clone(),
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

impl MockProvider {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let shared = Shared {
            issuer: issuer.clone(),
            key: PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap(),
            codes: Arc::default(),
            tokens: Arc::default(),
            userinfo: Arc::default(),
        };
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .route("/userinfo", get(userinfo))
            .with_state(shared.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            issuer,
            shared,
            task,
        }
    }

    /// Approves the sign-in as the user with `claims` (merged over a
    /// standard set) and returns the code the provider would redirect with.
    pub fn approve(&self, authorization: &Authorization, claims: Value) -> String {
        let mut all = json!({
            "iss": self.issuer,
            "aud": CLIENT_ID,
            "iat": now(),
            "exp": now() + 300,
            "nonce": authorization.nonce,
        });
        for (key, value) in claims.as_object().unwrap() {
            all[key] = value.clone();
        }
        let code = random();
        self.shared.codes.lock().unwrap().insert(
            code.clone(),
            Approved {
                claims: all,
                code_challenge: authorization.code_challenge.clone(),
                redirect_uri: authorization.redirect_uri.clone(),
            },
        );
        code
    }

    /// What userinfo answers instead of the ID token's claims.
    pub fn set_userinfo(&self, claims: Option<Value>) {
        *self.shared.userinfo.lock().unwrap() = claims;
    }

    /// An ID token signed with a key the provider doesn't publish.
    pub fn forged_key(&self) {
        let mut codes = self.shared.codes.lock().unwrap();
        for approved in codes.values_mut() {
            approved.claims["forged"] = json!(true);
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn random() -> String {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).unwrap();
    b64(&bytes)
}

async fn discovery(State(shared): State<Shared>) -> Json<Value> {
    let issuer = &shared.issuer;
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "userinfo_endpoint": format!("{issuer}/userinfo"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
    }))
}

async fn jwks(State(shared): State<Shared>) -> Json<Value> {
    let rsa = shared.key.rsa().unwrap();
    Json(json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": KEY_ID,
            "n": b64(&rsa.n().to_vec()),
            "e": b64(&rsa.e().to_vec()),
        }]
    }))
}

fn sign(key: &PKey<openssl::pkey::Private>, claims: &Value) -> String {
    let header = json!({ "alg": "RS256", "typ": "JWT", "kid": KEY_ID });
    let input = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(claims.to_string().as_bytes())
    );
    let mut signer = Signer::new(MessageDigest::sha256(), key).unwrap();
    signer.update(input.as_bytes()).unwrap();
    format!("{input}.{}", b64(&signer.sign_to_vec().unwrap()))
}

fn token_error(error: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
}

async fn token(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
        .and_then(|value| String::from_utf8(value).ok());
    // Basic credentials are form-encoded first (RFC 6749 section 2.3.1).
    let expected = format!(
        "{}:{}",
        CLIENT_ID,
        url::form_urlencoded::byte_serialize(CLIENT_SECRET.as_bytes()).collect::<String>()
    );
    let posted = form.get("client_secret").map(String::as_str) == Some(CLIENT_SECRET);
    if basic.as_deref() != Some(expected.as_str()) && !posted {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_client" })),
        )
            .into_response();
    }
    let Some(approved) = form
        .get("code")
        .and_then(|code| shared.codes.lock().unwrap().remove(code))
    else {
        return token_error("invalid_grant");
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    if b64(&Sha256::digest(verifier.as_bytes())) != approved.code_challenge
        || form.get("redirect_uri") != Some(&approved.redirect_uri)
    {
        return token_error("invalid_grant");
    }
    let access_token = random();
    shared
        .tokens
        .lock()
        .unwrap()
        .insert(access_token.clone(), approved.claims.clone());
    let key = if approved.claims.get("forged").is_some() {
        PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap()
    } else {
        shared.key.clone()
    };
    Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": sign(&key, &approved.claims),
    }))
    .into_response()
}

async fn userinfo(State(shared): State<Shared>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let Some(claims) = shared.tokens.lock().unwrap().get(token).cloned() else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let info = shared
        .userinfo
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| json!({}));
    let mut answer = json!({ "sub": claims["sub"] });
    for (key, value) in info.as_object().unwrap() {
        answer[key] = value.clone();
    }
    Json(answer).into_response()
}
