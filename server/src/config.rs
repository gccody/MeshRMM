//! The server's configuration: a TOML file (by default
//! `/etc/meshrmm/server.toml`) overridden by `MESHRMM_*` environment variables.
//!
//! An environment variable names a key path with `__` between levels, e.g.
//! `MESHRMM_TLS__MODE=proxy` or `MESHRMM_DATABASE__URL=postgres://...`.
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use ipnet::IpNet;
use serde::Deserialize;
use url::{Host, Url};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/meshrmm/server.toml";
const DEFAULT_DATA_DIR: &str = "/var/lib/meshrmm";
const DEFAULT_DOWNLOADS_DIR: &str = "/usr/share/meshrmm/downloads";
const LETS_ENCRYPT_DIRECTORY: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// The top-level keys environment variables may set. Anything else with the
/// `MESHRMM_` prefix (for example an Agent's `MESHRMM_SERVER` on a developer
/// machine) is ignored rather than rejected as an unknown key.
const TOP_LEVEL_KEYS: &[&str] = &[
    "public_url",
    "data_dir",
    "database",
    "tls",
    "http",
    "downloads",
    "toolbox",
    "remote",
    "turn",
    "log",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The URL users, Agents and viewers reach this server at.
    pub public_url: Url,
    /// Where the server keeps its instance key, ACME account and certificates,
    /// uploaded files, and (by default) its SQLite database.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub database: DatabaseConfig,
    pub tls: TlsConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub downloads: DownloadsConfig,
    #[serde(default)]
    pub toolbox: ToolboxConfig,
    #[serde(default)]
    pub remote: RemoteConfig,
    #[serde(default)]
    pub turn: TurnConfig,
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// `sqlite://<path>` or `postgres://...`. Defaults to `meshrmm.db` in the
    /// data directory.
    pub url: Option<String>,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: None,
            max_connections: default_max_connections(),
        }
    }
}

/// How the server gets the certificate it serves HTTPS with.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TlsConfig {
    /// Obtain and renew a certificate from an ACME CA (Let's Encrypt by
    /// default) using the TLS-ALPN-01 challenge on the HTTPS port.
    Acme {
        /// Defaults to the host of `public_url`.
        #[serde(default)]
        domains: Vec<String>,
        contact_email: Option<String>,
        #[serde(default = "default_acme_directory")]
        directory_url: Url,
    },
    /// Serve a certificate and key from PEM files, reloaded when they change.
    Files {
        cert_path: PathBuf,
        key_path: PathBuf,
    },
    /// Serve plain HTTP behind a reverse proxy that terminates TLS.
    Proxy {
        /// Peers whose `X-Forwarded-For` header is believed.
        #[serde(default = "default_trusted_proxies")]
        trusted_proxies: Vec<IpNet>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// Defaults to `0.0.0.0:443`, or `127.0.0.1:8080` in proxy mode.
    pub listen: Option<SocketAddr>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadsConfig {
    /// The Agent and viewer builds shipped with this server release.
    #[serde(default = "default_downloads_dir")]
    pub dir: PathBuf,
}

impl Default for DownloadsConfig {
    fn default() -> Self {
        Self {
            dir: default_downloads_dir(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolboxConfig {
    /// The largest library file a user may upload, in bytes. A reverse proxy
    /// in front of the server may cap request bodies lower; Cloudflare's
    /// proxy refuses bodies over 100 MB on its free plan.
    #[serde(default = "default_max_toolbox_file_bytes")]
    pub max_file_bytes: u64,
}

impl Default for ToolboxConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: default_max_toolbox_file_bytes(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    /// How long a remote session lasts after its viewer was last heard from.
    #[serde(default = "default_remote_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            idle_timeout_seconds: default_remote_idle_timeout_seconds(),
        }
    }
}

/// The built-in STUN/TURN server remote sessions use to get through NAT.
///
/// Agents and viewers reach it over UDP and IPv4 only, so `host` must have an
/// A record and must not go through an HTTP proxy.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnConfig {
    /// Without it, peers that can't reach each other directly can't connect.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// The host name or IPv4 address in the STUN and TURN URLs given to
    /// peers. Defaults to the host of `public_url`.
    pub host: Option<String>,
    /// The address relayed traffic appears to come from. Defaults to the
    /// IPv4 address `host` resolves to at startup.
    pub public_ip: Option<Ipv4Addr>,
    /// The UDP address STUN and TURN requests arrive at.
    #[serde(default = "default_turn_listen")]
    pub listen: SocketAddr,
    /// The UDP ports relays are opened on, one per peer of a relayed session.
    #[serde(default = "default_relay_port_min")]
    pub relay_port_min: u16,
    #[serde(default = "default_relay_port_max")]
    pub relay_port_max: u16,
    /// Addresses relays never send to or accept from, in addition to
    /// loopback, link-local, multicast and broadcast addresses.
    #[serde(default)]
    pub blocked_peers: Vec<IpNet>,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            host: None,
            public_ip: None,
            listen: default_turn_listen(),
            relay_port_min: default_relay_port_min(),
            relay_port_max: default_relay_port_max(),
            blocked_peers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// A `tracing` filter such as `info` or `meshrmm_server=debug,info`.
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from(DEFAULT_DATA_DIR)
}

fn default_downloads_dir() -> PathBuf {
    PathBuf::from(DEFAULT_DOWNLOADS_DIR)
}

fn default_max_connections() -> u32 {
    10
}

fn default_max_toolbox_file_bytes() -> u64 {
    95 * 1024 * 1024
}

fn default_remote_idle_timeout_seconds() -> u64 {
    15 * 60
}

fn default_enabled() -> bool {
    true
}

fn default_turn_listen() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::UNSPECIFIED, 3478))
}

fn default_relay_port_min() -> u16 {
    49160
}

fn default_relay_port_max() -> u16 {
    49200
}

fn default_log_level() -> String {
    "info".to_owned()
}

fn default_acme_directory() -> Url {
    Url::parse(LETS_ENCRYPT_DIRECTORY).expect("the Let's Encrypt directory URL is valid")
}

fn default_trusted_proxies() -> Vec<IpNet> {
    vec![
        IpNet::from(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        IpNet::from(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    ]
}

impl Config {
    /// Loads the configuration from `path` (if it exists) and the environment,
    /// then validates it. A missing file is an error only when `required`.
    pub fn load(path: &Path, required: bool) -> anyhow::Result<Self> {
        if required && !path.exists() {
            bail!("configuration file {} does not exist", path.display());
        }
        let environment = Env::prefixed("MESHRMM_").split("__").filter(|key| {
            let top = key.as_str().split('.').next().unwrap_or_default();
            TOP_LEVEL_KEYS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(top))
        });
        Self::from_figment(Figment::new().merge(Toml::file(path)).merge(environment))
            .with_context(|| format!("invalid configuration (file {})", path.display()))
    }

    /// Parses and validates a configuration given as TOML text.
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        Self::from_figment(Figment::new().merge(Toml::string(text)))
    }

    fn from_figment(figment: Figment) -> anyhow::Result<Self> {
        let mut config: Self = figment.extract()?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&mut self) -> anyhow::Result<()> {
        validate_public_url(&self.public_url)?;
        if let TlsConfig::Acme { domains, .. } = &mut self.tls
            && domains.is_empty()
        {
            match self.public_url.host() {
                Some(Host::Domain(domain)) => domains.push(domain.to_owned()),
                _ => bail!(
                    "tls.mode = \"acme\" needs public_url to name a DNS host, not an IP address"
                ),
            }
        }
        if let TlsConfig::Proxy { trusted_proxies } = &self.tls
            && trusted_proxies.is_empty()
        {
            bail!("tls.trusted_proxies must list at least one address");
        }
        if self.database.max_connections == 0 {
            bail!("database.max_connections must be at least 1");
        }
        if self.toolbox.max_file_bytes == 0 {
            bail!("toolbox.max_file_bytes must be at least 1");
        }
        if !(60..=3600).contains(&self.remote.idle_timeout_seconds) {
            bail!("remote.idle_timeout_seconds must be between 60 and 3600");
        }
        if !self.data_dir.is_absolute() {
            bail!("data_dir must be an absolute path");
        }
        self.validate_turn()
    }

    fn validate_turn(&self) -> anyhow::Result<()> {
        let turn = &self.turn;
        if !turn.listen.is_ipv4() {
            bail!("turn.listen must be an IPv4 address; Agents and viewers use TURN over IPv4");
        }
        if turn.relay_port_min == 0 || turn.relay_port_min > turn.relay_port_max {
            bail!("turn.relay_port_min must be at least 1 and no more than turn.relay_port_max");
        }
        let relays = turn.relay_port_min..=turn.relay_port_max;
        if turn.listen.port() != 0 && relays.contains(&turn.listen.port()) {
            bail!("turn.listen's port must be outside the relay ports");
        }
        match &turn.host {
            Some(host) => match Host::parse(host) {
                Ok(Host::Domain(_) | Host::Ipv4(_)) if !host.contains([':', '/']) => {}
                _ => bail!("turn.host must be a host name or an IPv4 address, with no port"),
            },
            None if turn.enabled && matches!(self.public_url.host(), Some(Host::Ipv6(_))) => {
                bail!(
                    "public_url's host is an IPv6 address; set turn.host to a name or IPv4 address"
                )
            }
            None => {}
        }
        Ok(())
    }

    /// `https://host[:port]` with no trailing slash.
    pub fn public_origin(&self) -> String {
        self.public_url.origin().ascii_serialization()
    }

    pub fn database_url(&self) -> String {
        match &self.database.url {
            Some(url) => url.clone(),
            None => format!("sqlite://{}", self.data_dir.join("meshrmm.db").display()),
        }
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.http.listen.unwrap_or(match self.tls {
            TlsConfig::Proxy { .. } => SocketAddr::from((Ipv4Addr::LOCALHOST, 8080)),
            _ => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 443)),
        })
    }

    /// The host in the STUN and TURN URLs.
    pub fn turn_host(&self) -> String {
        match &self.turn.host {
            Some(host) => host.clone(),
            None => self.public_url.host_str().unwrap_or_default().to_owned(),
        }
    }

    /// The proxies whose forwarding headers are trusted. Empty unless the
    /// server runs behind a reverse proxy.
    pub fn trusted_proxies(&self) -> &[IpNet] {
        match &self.tls {
            TlsConfig::Proxy { trusted_proxies } => trusted_proxies,
            _ => &[],
        }
    }
}

/// Agents and viewers only speak HTTPS, so the public URL must be HTTPS. Plain
/// HTTP is allowed for a loopback host, for development.
fn validate_public_url(url: &Url) -> anyhow::Result<()> {
    let loopback = match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => bail!("public_url must include a host"),
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => bail!("public_url must be an https:// URL"),
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        bail!("public_url must be an origin, with no path, query or fragment");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("public_url must not contain credentials");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACME: &str = r#"
        public_url = "https://rmm.example.com"
        [tls]
        mode = "acme"
        contact_email = "ops@example.com"
    "#;

    #[test]
    fn acme_defaults_its_domain_to_the_public_host() {
        let config = Config::from_toml(ACME).unwrap();
        let TlsConfig::Acme {
            domains,
            directory_url,
            ..
        } = &config.tls
        else {
            panic!("expected ACME");
        };
        assert_eq!(domains, &["rmm.example.com"]);
        assert_eq!(directory_url.as_str(), LETS_ENCRYPT_DIRECTORY);
        assert_eq!(config.listen_addr(), "0.0.0.0:443".parse().unwrap());
        assert_eq!(config.public_origin(), "https://rmm.example.com");
        assert_eq!(
            config.database_url(),
            "sqlite:///var/lib/meshrmm/meshrmm.db"
        );
        assert!(config.trusted_proxies().is_empty());
    }

    #[test]
    fn acme_rejects_an_ip_address_host() {
        let error = Config::from_toml(
            r#"
            public_url = "https://203.0.113.5"
            tls.mode = "acme"
        "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("DNS host"), "{error}");
    }

    #[test]
    fn proxy_mode_listens_on_loopback_and_trusts_loopback_proxies() {
        let config = Config::from_toml(
            r#"
            public_url = "https://rmm.example.com"
            tls.mode = "proxy"
        "#,
        )
        .unwrap();
        assert_eq!(config.listen_addr(), "127.0.0.1:8080".parse().unwrap());
        assert_eq!(
            config.trusted_proxies(),
            &[
                "127.0.0.1/32".parse::<IpNet>().unwrap(),
                "::1/128".parse().unwrap()
            ]
        );
    }

    #[test]
    fn files_mode_needs_both_paths() {
        assert!(
            Config::from_toml(
                r#"
                public_url = "https://rmm.example.com"
                tls = { mode = "files", cert_path = "/etc/meshrmm/cert.pem" }
            "#
            )
            .is_err()
        );
        let config = Config::from_toml(
            r#"
            public_url = "https://rmm.example.com"
            data_dir = "/srv/meshrmm"
            database.url = "postgres://meshrmm@db/meshrmm"
            http.listen = "[::]:8443"
            tls = { mode = "files", cert_path = "/etc/meshrmm/cert.pem", key_path = "/etc/meshrmm/key.pem" }
        "#,
        )
        .unwrap();
        assert_eq!(config.database_url(), "postgres://meshrmm@db/meshrmm");
        assert_eq!(config.listen_addr(), "[::]:8443".parse().unwrap());
    }

    #[test]
    fn public_url_must_be_an_https_origin() {
        for url in [
            "http://rmm.example.com",
            "https://rmm.example.com/meshrmm",
            "https://rmm.example.com/?a=b",
            "https://user:pass@rmm.example.com",
            "ftp://rmm.example.com",
        ] {
            let text = format!("public_url = \"{url}\"\ntls.mode = \"proxy\"");
            assert!(Config::from_toml(&text).is_err(), "{url} was accepted");
        }
        for url in [
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "https://rmm.example.com/",
        ] {
            let text = format!("public_url = \"{url}\"\ntls.mode = \"proxy\"");
            Config::from_toml(&text).unwrap_or_else(|error| panic!("{url}: {error}"));
        }
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = Config::from_toml(
            r#"
            public_url = "https://rmm.example.com"
            tls.mode = "proxy"
            databse.url = "sqlite:///tmp/x.db"
        "#,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("databse"), "{error:#}");
    }

    #[test]
    #[allow(
        clippy::result_large_err,
        reason = "figment::Jail's closure returns figment::Error"
    )]
    fn environment_overrides_the_file_and_ignores_unrelated_variables() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("server.toml", ACME)?;
            jail.set_env("MESHRMM_TLS__MODE", "proxy");
            jail.set_env("MESHRMM_DATABASE__URL", "postgres://env@db/meshrmm");
            jail.set_env("MESHRMM_SERVER", "https://agent-setting.example.com");
            let config =
                Config::load(Path::new("server.toml"), true).map_err(|error| error.to_string())?;
            assert!(matches!(config.tls, TlsConfig::Proxy { .. }));
            assert_eq!(config.database_url(), "postgres://env@db/meshrmm");
            Ok(())
        });
    }

    #[test]
    fn the_example_configuration_is_valid() {
        let config = Config::from_toml(include_str!("../server.example.toml")).unwrap();
        assert!(matches!(config.tls, TlsConfig::Acme { .. }));
        assert_eq!(config.public_origin(), "https://rmm.example.com");
        assert_eq!(config.remote.idle_timeout_seconds, 900);
        assert!(config.turn.enabled);
        assert_eq!(config.turn.listen, default_turn_listen());
    }

    #[test]
    fn the_remote_idle_timeout_is_bounded() {
        for seconds in [0, 59, 3601] {
            let text = format!(
                "public_url = \"https://rmm.example.com\"\ntls.mode = \"proxy\"\nremote.idle_timeout_seconds = {seconds}"
            );
            assert!(Config::from_toml(&text).is_err(), "{seconds} was accepted");
        }
    }

    #[test]
    fn turn_defaults_to_the_public_host() {
        let config = Config::from_toml(ACME).unwrap();
        assert!(config.turn.enabled);
        assert_eq!(config.turn_host(), "rmm.example.com");
        assert_eq!(config.turn.listen, "0.0.0.0:3478".parse().unwrap());
        assert_eq!(
            (config.turn.relay_port_min, config.turn.relay_port_max),
            (49160, 49200)
        );
        let config = Config::from_toml(
            r#"
            public_url = "https://rmm.example.com"
            tls.mode = "proxy"
            turn = { host = "turn.example.com", public_ip = "203.0.113.5", blocked_peers = ["10.0.0.0/8"] }
        "#,
        )
        .unwrap();
        assert_eq!(config.turn_host(), "turn.example.com");
        assert_eq!(config.turn.public_ip, Some("203.0.113.5".parse().unwrap()));
    }

    #[test]
    fn turn_settings_are_checked() {
        for turn in [
            "turn.listen = \"[::]:3478\"",
            "turn.relay_port_min = 0",
            "turn = { relay_port_min = 50000, relay_port_max = 49999 }",
            "turn.listen = \"0.0.0.0:49170\"",
            "turn.host = \"turn.example.com:3478\"",
            "turn.host = \"[2001:db8::1]\"",
            "turn.host = \"2001:db8::1\"",
            "turn.public_ip = \"2001:db8::1\"",
        ] {
            let text =
                format!("public_url = \"https://rmm.example.com\"\ntls.mode = \"proxy\"\n{turn}");
            assert!(Config::from_toml(&text).is_err(), "{turn} was accepted");
        }
        let text = "public_url = \"https://rmm.example.com\"\ntls.mode = \"proxy\"\nturn.host = \"198.51.100.7\"";
        assert_eq!(Config::from_toml(text).unwrap().turn_host(), "198.51.100.7");
    }

    #[test]
    fn a_required_file_must_exist() {
        let error = Config::load(Path::new("/nonexistent/meshrmm.toml"), true).unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error}");
    }
}
