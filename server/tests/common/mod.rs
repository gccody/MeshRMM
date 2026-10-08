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
    config: Config,
    customize: Customize,
    database: Option<TestDatabase>,
    _dir: TempDir,
}

/// The same instance after a restart.
pub struct Restarted {
    pub name: &'static str,
    pub state: meshrmm_server::http::AppState,
    router: axum::Router,
}

impl Restarted {
    pub fn browser(&self) -> Browser {
        Browser {
            router: self.router.clone(),
            cookie: None,
            name: self.name,
        }
    }

    pub async fn serve(&self) -> Server {
        Server::start(self.router.clone()).await
    }

    pub async fn finish(self) {
        self.state.database.close().await;
    }
}

/// An app listening on a loopback port.
pub struct Server {
    pub addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(router: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self { addr, task }
    }

    pub fn ws_url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Changes a server's state before it serves anything, e.g. to shorten
/// timeouts.
pub type Customize = fn(&mut meshrmm_server::http::AppState);

impl App {
    pub async fn sqlite() -> Self {
        Self::sqlite_with(|_| {}).await
    }

    pub async fn sqlite_with(customize: Customize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), "tls.mode = \"proxy\"");
        Self::new("sqlite", config, None, dir, customize).await
    }

    pub async fn postgres(admin_url: &str) -> Self {
        Self::postgres_with(admin_url, |_| {}).await
    }

    pub async fn postgres_with(admin_url: &str, customize: Customize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let database = postgres_with(admin_url, false).await;
        let config = config(
            dir.path(),
            &format!("tls.mode = \"proxy\"\ndatabase.url = \"{}\"", database.url),
        );
        Self::new("postgres", config, Some(database), dir, customize).await
    }

    async fn new(
        name: &'static str,
        config: Config,
        database: Option<TestDatabase>,
        dir: TempDir,
        customize: Customize,
    ) -> Self {
        let mut state = meshrmm_server::prepare(config.clone()).await.unwrap();
        customize(&mut state);
        Self {
            name,
            router: meshrmm_server::http::router(state.clone()),
            state,
            config,
            customize,
            database,
            _dir: dir,
        }
    }

    /// A second server on the same data directory and database, as after a
    /// restart, with the remote sessions that were live restored. This one
    /// keeps running; its sockets and sessions are its own.
    pub async fn restarted(&self) -> Restarted {
        let mut state = meshrmm_server::prepare(self.config.clone()).await.unwrap();
        (self.customize)(&mut state);
        state.sessions.restore(&state).await.unwrap();
        Restarted {
            name: self.name,
            router: meshrmm_server::http::router(state.clone()),
            state,
        }
    }

    /// Serves the app on a loopback port, for WebSocket clients.
    pub async fn serve(&self) -> Server {
        Server::start(self.router.clone()).await
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
    apps_with(|_| {}).await
}

/// A server on every backend under test, each changed by `customize`.
pub async fn apps_with(customize: Customize) -> Vec<App> {
    let mut apps = vec![App::sqlite_with(customize).await];
    if let Some(admin_url) = postgres_admin_url() {
        apps.push(App::postgres_with(&admin_url, customize).await);
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
        let response = self.bytes(request).await;
        response.json(self.name)
    }

    /// Sends `request` as is, apart from tracking cookies, and returns the
    /// body as bytes.
    pub async fn bytes(&mut self, request: axum::http::Request<axum::body::Body>) -> RawResponse {
        let response = send(&self.router, request).await;
        if let Some(set_cookie) = response.headers.get("set-cookie") {
            let set_cookie = set_cookie.to_str().unwrap();
            let pair = set_cookie.split(';').next().unwrap();
            self.cookie = if set_cookie.contains("Max-Age=0") {
                None
            } else {
                Some(pair.to_owned())
            };
        }
        response
    }

    /// A request the way the website sends it, with the session cookie.
    pub fn request(&self, method: axum::http::Method, path: &str) -> axum::http::request::Builder {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("origin", ORIGIN)
            .header("x-meshrmm-request", "1");
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", cookie);
        }
        request
    }
}

async fn send(
    router: &axum::Router,
    request: axum::http::Request<axum::body::Body>,
) -> RawResponse {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    RawResponse {
        status,
        headers,
        body,
    }
}

/// A response whose body may not be JSON.
pub struct RawResponse {
    pub status: axum::http::StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl RawResponse {
    pub fn json(self, name: &str) -> Response {
        let body = if self.body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&self.body).unwrap_or_else(|_| {
                panic!(
                    "{name}: response is not JSON: {}",
                    String::from_utf8_lossy(&self.body)
                )
            })
        };
        Response {
            status: self.status,
            headers: self.headers,
            body,
        }
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .map(|value| value.to_str().unwrap())
            .unwrap_or_default()
    }
}

/// An Agent: requests carry its credential and no cookie or origin.
#[derive(Clone)]
pub struct Agent {
    router: axum::Router,
    name: &'static str,
    pub device_id: String,
    pub token: String,
}

impl Agent {
    pub fn request(&self, method: axum::http::Method, path: &str) -> axum::http::request::Builder {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {}", self.token))
    }

    pub async fn send(&self, request: axum::http::Request<axum::body::Body>) -> RawResponse {
        send(&self.router, request).await
    }

    /// Posts JSON to `/v1/agents/{device}/{path}`.
    pub async fn report(&self, path: &str, body: serde_json::Value) -> Response {
        let request = self
            .request(
                axum::http::Method::POST,
                &format!("/v1/agents/{}/{path}", self.device_id),
            )
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        self.send(request).await.json(self.name)
    }

    /// The same Agent with another credential.
    pub fn with_token(&self, token: &str) -> Self {
        Self {
            token: token.to_owned(),
            ..self.clone()
        }
    }
}

impl App {
    /// Issues an installer as `browser` and redeems it as a computer called
    /// `name`, returning the enrolled Agent.
    pub async fn enroll(&self, browser: &mut Browser, name: &str) -> Agent {
        let installer = browser
            .post(
                "/v1/agent-installers",
                serde_json::json!({ "platform": "windows-x64" }),
            )
            .await;
        assert_eq!(installer.status, 201, "{}: {:?}", self.name, installer.body);
        let redeemed = redeem(
            self,
            installer.body["install_token"].as_str().unwrap(),
            name,
            &random_hex(32),
        )
        .await;
        assert_eq!(redeemed.status, 200, "{}: {:?}", self.name, redeemed.body);
        Agent {
            router: self.router.clone(),
            name: self.name,
            device_id: redeemed.body["device_id"].as_str().unwrap().to_owned(),
            token: redeemed.body["agent_token"].as_str().unwrap().to_owned(),
        }
    }
}

/// Redeems an installer the way the Agent installer does.
pub async fn redeem(app: &App, install_token: &str, name: &str, redemption_key: &str) -> Response {
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/v1/agent-installers/redeem")
        .header("authorization", format!("Bearer {install_token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({ "name": name, "redemption_key": redemption_key }).to_string(),
        ))
        .unwrap();
    send(&app.router, request).await.json(app.name)
}

/// Invites `email` with `role_ids`, accepts, and returns the new user's
/// browser and ID.
pub async fn add_user(
    app: &App,
    admin: &mut Browser,
    email: &str,
    role_ids: &[&str],
) -> (Browser, String) {
    let invited = admin
        .post(
            "/v1/invitations",
            serde_json::json!({ "email": email, "role_ids": role_ids }),
        )
        .await;
    assert_eq!(invited.status, 201, "{:?}", invited.body);
    let token = link_token(invited.body["link"].as_str().unwrap());
    let mut browser = app.browser();
    let accepted = browser
        .post(
            "/v1/auth/invitation/accept",
            serde_json::json!({ "token": token, "display_name": email, "password": "a long enough password" }),
        )
        .await;
    assert_eq!(accepted.status, 201, "{:?}", accepted.body);
    let id = browser.get("/v1/account").await.body["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    (browser, id)
}

/// Creates a role with `permissions` and returns its ID.
pub async fn add_role(admin: &mut Browser, name: &str, permissions: &[&str]) -> String {
    let created = admin
        .post(
            "/v1/roles",
            serde_json::json!({ "name": name, "permissions": permissions }),
        )
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    created.body["id"].as_str().unwrap().to_owned()
}

/// A user holding exactly `permissions`, through a role of their own.
pub async fn user_with(
    app: &App,
    admin: &mut Browser,
    email: &str,
    permissions: &[&str],
) -> (Browser, String) {
    let role = add_role(admin, email, permissions).await;
    add_user(app, admin, email, &[&role]).await
}

/// The audit events for `action`, newest first.
pub async fn audit_events(app: &App, action: &str) -> Vec<meshrmm_server::audit::Event> {
    meshrmm_server::audit::list(
        &mut app.db(),
        &meshrmm_server::audit::Filter {
            action: Some(action.to_owned()),
            ..Default::default()
        },
        100,
    )
    .await
    .unwrap()
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

/// The next command for a stand-in Agent connection, or `None` once the
/// connection was replaced.
pub async fn next_command(
    connection: &mut meshrmm_server::realtime::AgentConnection,
) -> Option<meshrmm_protocol_types::AgentCommand> {
    match connection.commands.recv().await? {
        meshrmm_server::realtime::ToAgent::Command(command) => Some(command),
        other => panic!("expected a command, got {other:?}"),
    }
}

pub type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// How long a test waits for a socket to say something.
pub const SOCKET_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Opens a WebSocket with `headers`. A refused handshake is the HTTP status.
pub async fn ws(url: &str, headers: &[(&str, &str)]) -> Result<Ws, u16> {
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

    let mut request = url.into_client_request().unwrap();
    for (name, value) in headers {
        request.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(error) => panic!("could not connect to {url}: {error}"),
    }
}

/// What a socket received, ignoring pings and pongs.
#[derive(Debug, PartialEq)]
pub enum Received {
    Json(serde_json::Value),
    Close(u16),
    /// Closed without a close frame.
    Gone,
}

pub async fn receive(socket: &mut Ws) -> Received {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    loop {
        let frame = tokio::time::timeout(SOCKET_WAIT, socket.next())
            .await
            .expect("the socket said nothing");
        match frame {
            Some(Ok(Message::Text(text))) => {
                return Received::Json(serde_json::from_str(&text).unwrap());
            }
            Some(Ok(Message::Close(frame))) => {
                return Received::Close(frame.map_or(1005, |frame| u16::from(frame.code)));
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(other)) => panic!("unexpected frame {other:?}"),
            Some(Err(_)) | None => return Received::Gone,
        }
    }
}

/// The next JSON message.
pub async fn receive_json(socket: &mut Ws) -> serde_json::Value {
    match receive(socket).await {
        Received::Json(value) => value,
        other => panic!("expected a message, got {other:?}"),
    }
}

/// The code the socket closes with next, skipping messages before it.
pub async fn receive_close(socket: &mut Ws) -> u16 {
    loop {
        match receive(socket).await {
            Received::Json(_) => {}
            Received::Close(code) => return code,
            Received::Gone => panic!("the socket ended without a close frame"),
        }
    }
}

/// Asserts the socket says nothing for a while.
pub async fn quiet(socket: &mut Ws, wait: std::time::Duration) {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(other) => panic!("expected nothing, got {other:?}"),
        }
    }
}

pub async fn send_text(socket: &mut Ws, text: &str) {
    use futures_util::SinkExt;

    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
        .await
        .unwrap();
}

pub async fn send_json(socket: &mut Ws, value: serde_json::Value) {
    send_text(socket, &value.to_string()).await;
}

impl Agent {
    /// Opens the Agent's control connection.
    pub async fn connect(&self, server: &Server) -> Result<Ws, u16> {
        ws(
            &server.ws_url(&format!("/v1/agents/{}/connect", self.device_id)),
            &[("authorization", &format!("Bearer {}", self.token))],
        )
        .await
    }
}

impl Browser {
    /// Opens the website's event socket.
    pub async fn events(&self, server: &Server) -> Result<Ws, u16> {
        let mut headers = vec![("origin", ORIGIN)];
        if let Some(cookie) = &self.cookie {
            headers.push(("cookie", cookie));
        }
        ws(&server.ws_url("/v1/events"), &headers).await
    }
}
