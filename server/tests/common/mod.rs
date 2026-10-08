//! Shared setup for the integration tests.
//!
//! Database tests run against SQLite always, and against PostgreSQL when
//! `MESHRMM_TEST_POSTGRES_URL` names a server the tests may create databases
//! on (e.g. `postgres://postgres@127.0.0.1:5432/postgres`). CI sets it; there,
//! a missing URL fails the tests instead of skipping PostgreSQL silently.
#![allow(dead_code)]

use std::path::Path;

use meshrmm_server::{config::Config, db::Database};
use sqlx::{Connection, PgConnection};
use tempfile::TempDir;

pub const POSTGRES_URL_VARIABLE: &str = "MESHRMM_TEST_POSTGRES_URL";

/// A migrated, empty database that is removed when the test calls
/// [`TestDatabase::drop_database`] (PostgreSQL) or the value is dropped
/// (SQLite).
pub struct TestDatabase {
    pub name: &'static str,
    pub url: String,
    pub database: Database,
    postgres: Option<(String, String)>,
    _dir: Option<TempDir>,
}

impl TestDatabase {
    pub async fn drop_database(self) {
        self.database.close().await;
        if let Some((admin_url, name)) = self.postgres {
            let mut admin = PgConnection::connect(&admin_url).await.unwrap();
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE \"{name}\" WITH (FORCE)"
            )))
            .execute(&mut admin)
            .await
            .unwrap();
        }
    }
}

/// The PostgreSQL admin URL, if PostgreSQL tests should run.
pub fn postgres_admin_url() -> Option<String> {
    match std::env::var(POSTGRES_URL_VARIABLE) {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("{POSTGRES_URL_VARIABLE} must be set in CI so PostgreSQL is tested")
        }
        _ => {
            eprintln!("{POSTGRES_URL_VARIABLE} is not set; skipping PostgreSQL");
            None
        }
    }
}

pub async fn sqlite() -> TestDatabase {
    sqlite_with(true).await
}

pub async fn sqlite_with(migrate: bool) -> TestDatabase {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("test.db").display());
    let database = Database::connect(&url, 4).await.unwrap();
    if migrate {
        database.migrate().await.unwrap();
    }
    TestDatabase {
        name: "sqlite",
        url,
        database,
        postgres: None,
        _dir: Some(dir),
    }
}

pub async fn postgres(admin_url: &str) -> TestDatabase {
    postgres_with(admin_url, true).await
}

pub async fn postgres_with(admin_url: &str, migrate: bool) -> TestDatabase {
    let name = format!("meshrmm_test_{}", random_hex(8));
    let mut admin = PgConnection::connect(admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(&mut admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(admin_url).unwrap();
    url.set_path(&name);
    let url = url.to_string();
    let database = Database::connect(&url, 4).await.unwrap();
    if migrate {
        database.migrate().await.unwrap();
    }
    TestDatabase {
        name: "postgres",
        url,
        database,
        postgres: Some((admin_url.to_owned(), name)),
        _dir: None,
    }
}

/// Every backend under test, each freshly migrated.
pub async fn databases() -> Vec<TestDatabase> {
    let mut databases = vec![sqlite().await];
    if let Some(admin_url) = postgres_admin_url() {
        databases.push(postgres(&admin_url).await);
    }
    databases
}

/// A proxy-mode configuration with its data, database and downloads in `dir`.
pub fn config(dir: &Path, extra: &str) -> Config {
    Config::from_toml(&format!(
        r#"
        public_url = "https://rmm.example.com"
        data_dir = "{data}"
        downloads.dir = "{downloads}"
        {extra}
        "#,
        data = dir.join("data").display(),
        downloads = dir.join("downloads").display(),
    ))
    .unwrap()
}

pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0; bytes];
    getrandom::fill(&mut buffer).unwrap();
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub const ORIGIN: &str = "https://rmm.example.com";

/// A server on one backend, with its own data directory.
pub struct App {
    pub name: &'static str,
    pub state: meshrmm_server::http::AppState,
    router: axum::Router,
    database: Option<TestDatabase>,
    _dir: TempDir,
}

impl App {
    pub async fn sqlite() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = meshrmm_server::prepare(config(dir.path(), "tls.mode = \"proxy\""))
            .await
            .unwrap();
        Self::new("sqlite", state, None, dir)
    }

    pub async fn postgres(admin_url: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let database = postgres_with(admin_url, false).await;
        let state = meshrmm_server::prepare(config(
            dir.path(),
            &format!("tls.mode = \"proxy\"\ndatabase.url = \"{}\"", database.url),
        ))
        .await
        .unwrap();
        Self::new("postgres", state, Some(database), dir)
    }

    fn new(
        name: &'static str,
        state: meshrmm_server::http::AppState,
        database: Option<TestDatabase>,
        dir: TempDir,
    ) -> Self {
        Self {
            name,
            router: meshrmm_server::http::router(state.clone()),
            state,
            database,
            _dir: dir,
        }
    }

    /// A browser with no session.
    pub fn browser(&self) -> Browser {
        Browser {
            router: self.router.clone(),
            cookie: None,
            name: self.name,
        }
    }

    pub fn db(&self) -> &Database {
        &self.state.database
    }

    pub async fn finish(self) {
        self.state.database.close().await;
        if let Some(database) = self.database {
            database.drop_database().await;
        }
    }
}

/// A server on every backend under test.
pub async fn apps() -> Vec<App> {
    let mut apps = vec![App::sqlite().await];
    if let Some(admin_url) = postgres_admin_url() {
        apps.push(App::postgres(&admin_url).await);
    }
    apps
}

pub struct Response {
    pub status: axum::http::StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: serde_json::Value,
}

impl Response {
    /// The `code` of an error response.
    pub fn code(&self) -> &str {
        self.body["code"].as_str().unwrap_or_default()
    }
}

/// Sends requests the way the website does, keeping the session cookie.
#[derive(Clone)]
pub struct Browser {
    router: axum::Router,
    pub cookie: Option<String>,
    name: &'static str,
}

impl Browser {
    pub async fn get(&mut self, path: &str) -> Response {
        self.send(axum::http::Method::GET, path, None).await
    }

    pub async fn post(&mut self, path: &str, body: serde_json::Value) -> Response {
        self.send(axum::http::Method::POST, path, Some(body)).await
    }

    pub async fn patch(&mut self, path: &str, body: serde_json::Value) -> Response {
        self.send(axum::http::Method::PATCH, path, Some(body)).await
    }

    pub async fn put(&mut self, path: &str, body: serde_json::Value) -> Response {
        self.send(axum::http::Method::PUT, path, Some(body)).await
    }

    pub async fn delete(&mut self, path: &str) -> Response {
        self.send(axum::http::Method::DELETE, path, None).await
    }

    pub async fn send(
        &mut self,
        method: axum::http::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Response {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("origin", ORIGIN)
            .header("x-meshrmm-request", "1");
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", cookie);
        }
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string())),
            None => request.body(axum::body::Body::empty()),
        }
        .unwrap();
        self.raw(request).await
    }

    /// Sends `request` as is, apart from tracking cookies.
    pub async fn raw(&mut self, request: axum::http::Request<axum::body::Body>) -> Response {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        if let Some(set_cookie) = headers.get("set-cookie") {
            let set_cookie = set_cookie.to_str().unwrap();
            let pair = set_cookie.split(';').next().unwrap();
            self.cookie = if set_cookie.contains("Max-Age=0") {
                None
            } else {
                Some(pair.to_owned())
            };
        }
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                panic!(
                    "{}: response is not JSON: {}",
                    self.name,
                    String::from_utf8_lossy(&bytes)
                )
            })
        };
        Response {
            status,
            headers,
            body,
        }
    }
}

pub const ADMIN_EMAIL: &str = "admin@example.com";
pub const ADMIN_PASSWORD: &str = "correct horse battery staple";

/// Runs first-run setup and returns the administrator's browser.
pub async fn set_up(app: &App) -> Browser {
    let token = meshrmm_server::announce_setup(&app.state)
        .await
        .unwrap()
        .expect("no users yet")
        .split_once("#token=")
        .unwrap()
        .1
        .to_owned();
    let mut browser = app.browser();
    let response = browser
        .post(
            "/v1/setup",
            serde_json::json!({
                "token": token,
                "instance_name": "Acme IT",
                "email": ADMIN_EMAIL,
                "display_name": "Ada Admin",
                "password": ADMIN_PASSWORD,
            }),
        )
        .await;
    assert_eq!(response.status, 201, "{}: {:?}", app.name, response.body);
    browser
}

/// The token in a one-time link such as `https://host/invite#token=...`.
pub fn link_token(link: &str) -> String {
    link.split_once("#token=").unwrap().1.to_owned()
}

/// A code the authenticator app with this base32 secret shows `offset_steps`
/// 30-second steps from now.
pub fn totp_code(secret_base32: &str, offset_steps: i64) -> String {
    let secret = totp_rs::Secret::Encoded(secret_base32.to_owned())
        .to_bytes()
        .unwrap();
    let totp = totp_rs::TOTP::new_unchecked(
        totp_rs::Algorithm::SHA1,
        6,
        0,
        30,
        secret,
        None,
        String::new(),
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    totp.generate((now + offset_steps * 30) as u64)
}

/// A minimal SMTP server that accepts every message and hands it over.
pub struct FakeSmtp {
    pub port: u16,
    pub messages: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl FakeSmtp {
    pub async fn start() -> Self {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, messages) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let sender = sender.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    write.write_all(b"220 fake ESMTP\r\n").await.unwrap();
                    let mut data: Option<String> = None;
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Some(message) = &mut data {
                            if line == "." {
                                sender.send(std::mem::take(message)).ok();
                                data = None;
                                write.write_all(b"250 queued\r\n").await.unwrap();
                            } else {
                                message.push_str(&line);
                                message.push('\n');
                            }
                            continue;
                        }
                        let verb = line.split(' ').next().unwrap_or_default().to_uppercase();
                        let reply: &[u8] = match verb.as_str() {
                            "EHLO" | "HELO" => b"250-fake\r\n250 8BITMIME\r\n",
                            "DATA" => {
                                data = Some(String::new());
                                b"354 go ahead\r\n"
                            }
                            "QUIT" => {
                                write.write_all(b"221 bye\r\n").await.ok();
                                return;
                            }
                            _ => b"250 OK\r\n",
                        };
                        write.write_all(reply).await.unwrap();
                    }
                });
            }
        });
        Self { port, messages }
    }

    /// The next message, unfolded (quoted-printable soft breaks removed).
    pub async fn next(&mut self) -> String {
        let message =
            tokio::time::timeout(std::time::Duration::from_secs(10), self.messages.recv())
                .await
                .expect("no email arrived")
                .unwrap();
        message.replace("=\n", "").replace("=3D", "=")
    }
}

/// The first `https://...#token=...` link in an email.
pub fn link_in(message: &str) -> String {
    message
        .split_whitespace()
        .find(|word| word.starts_with("https://") && word.contains("#token="))
        .unwrap_or_else(|| panic!("no link in {message}"))
        .to_owned()
}

/// Runs setup on a fresh server, or signs the administrator in again.
pub async fn set_up_or_sign_in(app: &App) -> Browser {
    if meshrmm_server::users::count(&mut app.db()).await.unwrap() == 0 {
        return set_up(app).await;
    }
    let mut browser = app.browser();
    let response = browser
        .post(
            "/v1/auth/sign-in",
            serde_json::json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    browser
}
