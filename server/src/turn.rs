//! The built-in STUN/TURN server.
//!
//! The peers of a remote session get two ICE servers: STUN, to learn their
//! public address, and TURN, to relay through this server when they can't
//! reach each other directly. Agents and viewers are webrtc-rs peers, which
//! use TURN over UDP and IPv4 only, so that is all the server offers.
//!
//! A session's TURN username is its ID and its password an HMAC of the ID
//! under a key derived from the instance key, so the credentials stay valid
//! across a restart. They work only while the session is live: its actor
//! registers it when it starts, and when the session ends its relays close
//! and new ones are refused.
use std::{
    any::Any,
    collections::HashSet,
    fmt,
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::Duration,
};

use anyhow::{Context, bail};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use ipnet::IpNet;
use meshrmm_protocol_types::IceServer;
use sha2::Sha256;
use tokio::net::UdpSocket;
use turn::{
    auth::{AuthHandler, generate_auth_key},
    relay::RelayAddressGenerator,
    server::{
        Server,
        config::{ConnConfig, ServerConfig},
    },
};
use webrtc_util::Conn;

use crate::{
    config::Config,
    secrets::{InstanceKey, hex, random_bytes},
};

const CREDENTIAL_KEY_LABEL: &[u8] = b"meshrmm turn credentials v1";

#[derive(Clone)]
pub struct Turn {
    inner: Arc<Inner>,
}

struct Inner {
    credentials: Arc<Credentials>,
    running: OnceLock<Running>,
}

struct Running {
    server: Server,
    stun_url: String,
    turn_url: String,
}

impl fmt::Debug for Turn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let running = self.inner.running.get();
        formatter
            .debug_struct("Turn")
            .field("url", &running.map(|running| &running.turn_url))
            .finish_non_exhaustive()
    }
}

impl Turn {
    pub fn new(instance_key: &InstanceKey) -> Self {
        Self {
            inner: Arc::new(Inner {
                credentials: Arc::new(Credentials {
                    key: instance_key.derive(CREDENTIAL_KEY_LABEL),
                    live: Mutex::default(),
                }),
                running: OnceLock::new(),
            }),
        }
    }

    /// Starts answering STUN and TURN requests, unless `turn.enabled` is
    /// off. Returns the address it listens on. Until it has started, sessions
    /// get no ICE servers and their peers can only connect directly.
    pub async fn start(&self, config: &Config) -> anyhow::Result<Option<SocketAddr>> {
        let turn = &config.turn;
        if !turn.enabled {
            tracing::info!(
                "the TURN server is disabled; remote session peers connect only directly"
            );
            return Ok(None);
        }
        if self.inner.running.get().is_some() {
            bail!("the TURN server is already running");
        }
        let host = config.turn_host();
        let public_ip = match turn.public_ip {
            Some(ip) => ip,
            None => resolve(&host).await?,
        };
        let socket = UdpSocket::bind(turn.listen)
            .await
            .with_context(|| format!("could not listen for TURN on {}", turn.listen))?;
        let address = socket.local_addr()?;
        let IpAddr::V4(bind_ip) = address.ip() else {
            bail!("turn.listen must be an IPv4 address");
        };
        let relays = Relays {
            bind_ip,
            public_ip,
            ports: (turn.relay_port_min, turn.relay_port_max),
            peers: Arc::new(PeerFilter::new(
                SocketAddr::from((public_ip, address.port())),
                &turn.blocked_peers,
            )),
        };
        let server = Server::new(ServerConfig {
            conn_configs: vec![ConnConfig {
                conn: Arc::new(socket),
                relay_addr_generator: Box::new(relays),
            }],
            realm: host.clone(),
            auth_handler: self.inner.credentials.clone(),
            channel_bind_timeout: Duration::ZERO,
            alloc_close_notify: None,
        })
        .await
        .context("could not start the TURN server")?;
        let port = address.port();
        let running = Running {
            server,
            stun_url: format!("stun:{host}:{port}"),
            turn_url: format!("turn:{host}:{port}?transport=udp"),
        };
        if let Err(running) = self.inner.running.set(running) {
            let _ = running.server.close().await;
            bail!("the TURN server is already running");
        }
        if public_ip.is_private() || public_ip.is_loopback() || public_ip.is_link_local() {
            tracing::warn!(
                %public_ip,
                "the TURN relay address is not a public address; peers outside its network can't use the relay. Set turn.public_ip if the server is behind NAT"
            );
        }
        tracing::info!(
            %address,
            %public_ip,
            %host,
            relay_ports = format!("{}-{}", turn.relay_port_min, turn.relay_port_max),
            "serving STUN and TURN"
        );
        Ok(Some(address))
    }

    /// Stops answering STUN and TURN requests.
    pub async fn stop(&self) {
        if let Some(running) = self.inner.running.get()
            && let Err(error) = running.server.close().await
        {
            tracing::warn!(%error, "could not stop the TURN server");
        }
    }

    /// The STUN and TURN servers for a session's peers. None if the TURN
    /// server isn't running.
    pub fn ice_servers(&self, session_id: &str) -> Vec<IceServer> {
        let Some(running) = self.inner.running.get() else {
            return Vec::new();
        };
        vec![
            IceServer {
                urls: vec![running.stun_url.clone()],
                username: None,
                credential: None,
            },
            IceServer {
                urls: vec![running.turn_url.clone()],
                username: Some(session_id.to_owned()),
                credential: Some(self.inner.credentials.password(session_id)),
            },
        ]
    }

    /// Lets the session's credentials open relays until
    /// [`Turn::session_ended`].
    pub fn session_started(&self, session_id: &str) {
        self.inner.credentials.live().insert(session_id.to_owned());
    }

    /// Closes the session's relays and refuses its credentials.
    pub async fn session_ended(&self, session_id: &str) {
        let ended = self.inner.credentials.live().remove(session_id);
        if ended
            && let Some(running) = self.inner.running.get()
            && let Err(error) = running
                .server
                .delete_allocations_by_username(session_id.to_owned())
                .await
        {
            tracing::warn!(session_id, %error, "could not close an ended session's TURN relays");
        }
    }
}

/// The IPv4 address `host` names or resolves to.
pub async fn resolve(host: &str) -> anyhow::Result<Ipv4Addr> {
    if let Ok(ip) = host.parse() {
        return Ok(ip);
    }
    let mut addresses = tokio::net::lookup_host((host, 0))
        .await
        .with_context(|| format!("could not look up the TURN host {host}; set turn.public_ip"))?;
    addresses
        .find_map(|address| match address.ip() {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        })
        .with_context(|| {
            format!(
                "the TURN host {host} has no IPv4 address, which Agents and viewers need; set turn.host or turn.public_ip"
            )
        })
}

struct Credentials {
    key: [u8; 32],
    /// The live sessions' IDs.
    live: Mutex<HashSet<String>>,
}

impl Credentials {
    fn password(&self, session_id: &str) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts keys of any length");
        mac.update(session_id.as_bytes());
        hex(&mac.finalize().into_bytes())
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl AuthHandler for Credentials {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        src_addr: SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        if !self.live().contains(username) {
            tracing::debug!(%src_addr, "refused TURN credentials for a session that isn't live");
            return Err(turn::Error::ErrNoSuchUser);
        }
        Ok(generate_auth_key(username, realm, &self.password(username)))
    }
}

/// Opens relays on the configured ports.
struct Relays {
    bind_ip: Ipv4Addr,
    public_ip: Ipv4Addr,
    /// First and last relay port.
    ports: (u16, u16),
    peers: Arc<PeerFilter>,
}

#[async_trait]
impl RelayAddressGenerator for Relays {
    fn validate(&self) -> Result<(), turn::Error> {
        Ok(())
    }

    async fn allocate_conn(
        &self,
        use_ipv4: bool,
        requested_port: u16,
    ) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), turn::Error> {
        if !use_ipv4 {
            return Err(turn::Error::Other("relays are IPv4 only".into()));
        }
        let (first, last) = self.ports;
        let count = u32::from(last - first) + 1;
        // Start at a random port so relay addresses aren't predictable, and
        // try every port so a busy range still finds a free one.
        let start = u32::from(u16::from_be_bytes(random_bytes())) % count;
        let candidates = (0..count).map(|offset| {
            first + u16::try_from((start + offset) % count).expect("the offset is a port")
        });
        let candidates: Box<dyn Iterator<Item = u16> + Send> = match requested_port {
            0 => Box::new(candidates),
            port if (first..=last).contains(&port) => Box::new(std::iter::once(port)),
            _ => {
                return Err(turn::Error::Other(
                    "the requested port is not a relay port".into(),
                ));
            }
        };
        for port in candidates {
            match UdpSocket::bind((self.bind_ip, port)).await {
                Ok(socket) => {
                    let relay = Relay {
                        socket,
                        peers: self.peers.clone(),
                    };
                    return Ok((Arc::new(relay), SocketAddr::from((self.public_ip, port))));
                }
                Err(error) if error.kind() == ErrorKind::AddrInUse => {}
                Err(error) => {
                    return Err(turn::Error::Other(format!(
                        "could not open a relay: {error}"
                    )));
                }
            }
        }
        tracing::warn!(
            first,
            last,
            "every TURN relay port is in use; widen turn.relay_port_min to turn.relay_port_max"
        );
        Err(turn::Error::ErrMaxRetriesExceeded)
    }
}

/// The peers a relay may exchange packets with.
#[derive(Debug)]
struct PeerFilter {
    /// The TURN server's own listener. Relaying to it would let a relay make
    /// requests that appear to come from the server.
    listener: SocketAddr,
    blocked: Vec<IpNet>,
    /// Loopback peers are allowed only when the relay itself is on loopback,
    /// as in development.
    loopback: bool,
}

impl PeerFilter {
    fn new(listener: SocketAddr, blocked: &[IpNet]) -> Self {
        Self {
            listener,
            blocked: blocked.to_vec(),
            loopback: listener.ip().is_loopback(),
        }
    }

    fn allows(&self, peer: SocketAddr) -> bool {
        let IpAddr::V4(ip) = peer.ip().to_canonical() else {
            return false;
        };
        let reserved = ip.is_unspecified()
            || ip.is_broadcast()
            || ip.is_multicast()
            || ip.is_link_local()
            || ip.octets()[0] == 0
            || (ip.is_loopback() && !self.loopback);
        !reserved
            && SocketAddr::from((ip, peer.port())) != self.listener
            && !self
                .blocked
                .iter()
                .any(|range| range.contains(&IpAddr::V4(ip)))
    }
}

/// A relay's socket, which drops packets to and from peers it may not use.
struct Relay {
    socket: UdpSocket,
    peers: Arc<PeerFilter>,
}

fn unsupported() -> webrtc_util::Error {
    webrtc_util::Error::Other("a relay only sends and receives datagrams".into())
}

#[async_trait]
impl Conn for Relay {
    async fn connect(&self, _addr: SocketAddr) -> webrtc_util::Result<()> {
        Err(unsupported())
    }

    async fn recv(&self, _buf: &mut [u8]) -> webrtc_util::Result<usize> {
        Err(unsupported())
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        loop {
            let (length, from) = self.socket.recv_from(buf).await?;
            if self.peers.allows(from) {
                return Ok((length, from));
            }
        }
    }

    async fn send(&self, _buf: &[u8]) -> webrtc_util::Result<usize> {
        Err(unsupported())
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        if !self.peers.allows(target) {
            return Err(webrtc_util::Error::Other(format!(
                "relaying to {target} is not allowed"
            )));
        }
        Ok(self.socket.send_to(buf, target).await?)
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> InstanceKey {
        let dir = tempfile::tempdir().unwrap();
        InstanceKey::load_or_create(dir.path()).unwrap()
    }

    #[test]
    fn credentials_work_only_while_the_session_is_live() {
        let turn = Turn::new(&key());
        let credentials = &turn.inner.credentials;
        let realm = "rmm.example.com";
        let source = SocketAddr::from(([198, 51, 100, 7], 50000));
        assert!(credentials.auth_handle("s1", realm, source).is_err());

        turn.session_started("s1");
        let expected = generate_auth_key("s1", realm, &credentials.password("s1"));
        assert_eq!(
            credentials.auth_handle("s1", realm, source).unwrap(),
            expected
        );
        assert!(credentials.auth_handle("s2", realm, source).is_err());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(turn.session_ended("s1"));
        assert!(credentials.auth_handle("s1", realm, source).is_err());
        // Ending it again does nothing.
        runtime.block_on(turn.session_ended("s1"));
    }

    #[test]
    fn passwords_depend_on_the_session_and_the_instance_key() {
        let instance = key();
        let turn = Turn::new(&instance);
        let password = turn.inner.credentials.password("s1");
        assert_eq!(password.len(), 64);
        assert_eq!(
            password,
            Turn::new(&instance).inner.credentials.password("s1")
        );
        assert_ne!(password, turn.inner.credentials.password("s2"));
        assert_ne!(password, Turn::new(&key()).inner.credentials.password("s1"));
    }

    #[test]
    fn no_ice_servers_until_the_server_runs() {
        assert!(Turn::new(&key()).ice_servers("s1").is_empty());
    }

    #[test]
    fn relays_refuse_reserved_and_blocked_peers() {
        let listener = SocketAddr::from(([203, 0, 113, 5], 3478));
        let peers = PeerFilter::new(listener, &["10.0.0.0/8".parse().unwrap()]);
        for allowed in [
            "198.51.100.7:50000",
            "192.168.1.20:50000",
            "203.0.113.5:49170",
            "[::ffff:198.51.100.7]:50000",
        ] {
            assert!(peers.allows(allowed.parse().unwrap()), "{allowed}");
        }
        for refused in [
            "127.0.0.1:5432",
            "0.0.0.0:53",
            "0.1.2.3:53",
            "169.254.169.254:80",
            "224.0.0.251:5353",
            "255.255.255.255:9",
            "10.1.2.3:50000",
            "203.0.113.5:3478",
            "[2001:db8::1]:50000",
            "[::1]:50000",
        ] {
            assert!(!peers.allows(refused.parse().unwrap()), "{refused}");
        }

        let development = PeerFilter::new(SocketAddr::from(([127, 0, 0, 1], 3478)), &[]);
        assert!(development.allows("127.0.0.1:50000".parse().unwrap()));
        assert!(!development.allows("127.0.0.1:3478".parse().unwrap()));
    }
}
