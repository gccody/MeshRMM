//! Relaying Wake-on-LAN: when the server asks, the Agent broadcasts magic
//! packets that wake offline computers on the networks it is on.
//!
//! Each packet request names the subnet by its directed broadcast address and
//! prefix length. The Agent finds its own address on that subnet as the
//! interfaces are now, binds a UDP socket to it so a computer on several
//! networks sends out of the right one, and sends the packet for each of the
//! target's hardware addresses to ports 9 and 7 at the directed broadcast
//! address and at 255.255.255.255. On macOS the socket is also bound to the
//! interface, as macOS otherwise sends 255.255.255.255 out of the primary
//! one. On Linux the packet also goes out as a raw Ethernet frame of type
//! 0x0842, which some network cards wake on instead. Sending runs on a
//! blocking thread; the results come back through [`Relay::next`] for the
//! coordinator to report.
use std::io;
use std::net::{Ipv4Addr, UdpSocket};

use meshrmm_protocol::{
    MAX_WAKE_MAC_ADDRESSES, MAX_WAKE_PACKETS, WAKE_PORTS, WakePacket, WakePacketResult,
    directed_broadcast, magic_packet, on_subnet, parse_mac, wake_prefix_supported,
};
use tokio::sync::mpsc;

use super::system_info::{LocalAddress, local_ipv4_addresses};

/// Sends the server's packet requests and hands back their results.
pub(crate) struct Relay {
    sender: mpsc::UnboundedSender<Vec<WakePacketResult>>,
    results: mpsc::UnboundedReceiver<Vec<WakePacketResult>>,
}

impl Relay {
    pub(crate) fn new() -> Self {
        let (sender, results) = mpsc::unbounded_channel();
        Self { sender, results }
    }

    /// Sends `packets` on a blocking thread.
    pub(crate) fn send(&self, mut packets: Vec<WakePacket>) {
        packets.truncate(MAX_WAKE_PACKETS);
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            let results = send_all(&packets);
            // The coordinator owns the receiver for as long as it runs.
            let _ = sender.send(results);
        });
    }

    /// Completes with the results of a [`Self::send`]. Cancel safe.
    pub(crate) async fn next(&mut self) -> Vec<WakePacketResult> {
        match self.results.recv().await {
            Some(results) => results,
            // The relay holds a sender, so the channel never closes.
            None => std::future::pending().await,
        }
    }
}

fn send_all(packets: &[WakePacket]) -> Vec<WakePacketResult> {
    let addresses = match local_ipv4_addresses() {
        Ok(addresses) => addresses,
        Err(error) => {
            let message = format!("could not list this computer's addresses: {error:#}");
            tracing::warn!(error = message, "could not send wake packets");
            return packets
                .iter()
                .map(|packet| result(packet, None, Some(message.clone())))
                .collect();
        }
    };
    packets
        .iter()
        .map(|packet| {
            let result = send_one(packet, &addresses);
            match &result.error {
                None => tracing::info!(
                    wake_id = packet.wake_id,
                    broadcast = packet.broadcast,
                    source = result.source_address,
                    mac_addresses = ?packet.mac_addresses,
                    "sent wake packets"
                ),
                Some(error) => tracing::warn!(
                    wake_id = packet.wake_id,
                    broadcast = packet.broadcast,
                    error,
                    "could not send wake packets"
                ),
            }
            result
        })
        .collect()
}

fn result(
    packet: &WakePacket,
    source_address: Option<String>,
    error: Option<String>,
) -> WakePacketResult {
    WakePacketResult {
        wake_id: packet.wake_id.clone(),
        broadcast: packet.broadcast.clone(),
        source_address,
        error,
    }
}

/// The address to send from: one on the packet's subnet.
fn source(
    addresses: &[LocalAddress],
    broadcast: Ipv4Addr,
    prefix_length: u8,
) -> Option<&LocalAddress> {
    addresses
        .iter()
        .find(|local| on_subnet(local.address, broadcast, prefix_length))
}

fn send_one(packet: &WakePacket, addresses: &[LocalAddress]) -> WakePacketResult {
    let fail = |message: String| result(packet, None, Some(message));
    let prefix_length = packet.prefix_length;
    let Some(broadcast) = packet
        .broadcast
        .parse::<Ipv4Addr>()
        .ok()
        .filter(|broadcast| {
            wake_prefix_supported(prefix_length)
                && directed_broadcast(*broadcast, prefix_length) == *broadcast
        })
    else {
        return fail(format!(
            "{}/{prefix_length} is not a subnet's broadcast address",
            packet.broadcast
        ));
    };
    let macs = packet
        .mac_addresses
        .iter()
        .filter_map(|mac| parse_mac(mac))
        .take(MAX_WAKE_MAC_ADDRESSES)
        .collect::<Vec<_>>();
    if macs.is_empty() {
        return fail("the request named no valid hardware address".to_owned());
    }
    let Some(local) = source(addresses, broadcast, prefix_length) else {
        let network =
            Ipv4Addr::from(u32::from(broadcast) & meshrmm_protocol::subnet_mask(prefix_length));
        return fail(format!(
            "this computer is no longer on {network}/{prefix_length}"
        ));
    };
    let source_address = Some(local.address.to_string());
    #[cfg(target_os = "linux")]
    if let Err(error) = ethernet::send(&local.interface, &macs) {
        // The UDP packets below are what matters; few cards need the frame.
        tracing::debug!(interface = local.interface, %error, "could not send a Wake-on-LAN Ethernet frame");
    }
    match send_udp(local, broadcast, &macs) {
        Ok(()) => result(packet, source_address, None),
        Err(error) => result(
            packet,
            source_address,
            Some(format!(
                "could not broadcast from {} on {}: {error}",
                local.address, local.interface
            )),
        ),
    }
}

/// Sends each address's magic packet to [`WAKE_PORTS`] at the subnet's
/// broadcast address and at 255.255.255.255. Succeeds if any datagram went
/// out.
fn send_udp(local: &LocalAddress, broadcast: Ipv4Addr, macs: &[[u8; 6]]) -> io::Result<()> {
    let socket = UdpSocket::bind((local.address, 0))?;
    socket.set_broadcast(true)?;
    #[cfg(target_os = "macos")]
    bind_to_interface(&socket, &local.interface)?;
    let mut sent = false;
    let mut last_error = None;
    for mac in macs {
        let packet = magic_packet(*mac);
        for destination in [broadcast, Ipv4Addr::BROADCAST] {
            for port in WAKE_PORTS {
                match socket.send_to(&packet, (destination, port)) {
                    Ok(_) => sent = true,
                    Err(error) => last_error = Some(error),
                }
            }
        }
    }
    match (sent, last_error) {
        (false, Some(error)) => Err(error),
        _ => Ok(()),
    }
}

/// Sends the socket's datagrams out of `interface` only.
#[cfg(target_os = "macos")]
fn bind_to_interface(socket: &UdpSocket, interface: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let name = std::ffi::CString::new(interface)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface name"))?;
    // SAFETY: the name is NUL-terminated.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        return Err(io::Error::last_os_error());
    }
    let index = index as libc::c_int;
    // SAFETY: the socket is open and the option is an int.
    let status = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_BOUND_IF,
            (&index as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Magic packets as Ethernet frames of their own type, broadcast on one
/// interface. Needs root, which the Linux Agent's service has.
#[cfg(target_os = "linux")]
mod ethernet {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use meshrmm_protocol::magic_packet;

    /// The EtherType of Wake-on-LAN frames.
    const ETHERTYPE_WAKE: u16 = 0x0842;

    pub(super) fn send(interface: &str, macs: &[[u8; 6]]) -> io::Result<()> {
        let name = std::ffi::CString::new(interface)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface name"))?;
        // SAFETY: the name is NUL-terminated.
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if index == 0 {
            return Err(io::Error::last_os_error());
        }
        // A datagram packet socket: the kernel writes the Ethernet header.
        // SAFETY: plain socket creation.
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                i32::from(ETHERTYPE_WAKE.to_be()),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor was just opened and nothing else owns it.
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: an all-zero sockaddr_ll is valid.
        let mut address: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        address.sll_family = libc::AF_PACKET as libc::c_ushort;
        address.sll_protocol = ETHERTYPE_WAKE.to_be();
        address.sll_ifindex = index as libc::c_int;
        address.sll_halen = 6;
        address.sll_addr[..6].copy_from_slice(&[0xFF; 6]);
        for mac in macs {
            let packet = magic_packet(*mac);
            // SAFETY: the buffer and address are valid for their lengths.
            let sent = unsafe {
                libc::sendto(
                    socket.as_raw_fd(),
                    packet.as_ptr().cast(),
                    packet.len(),
                    0,
                    (&address as *const libc::sockaddr_ll).cast(),
                    std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };
            if sent < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(interface: &str, address: [u8; 4], prefix_length: u8) -> LocalAddress {
        LocalAddress {
            interface: interface.to_owned(),
            address: Ipv4Addr::from(address),
            prefix_length,
        }
    }

    fn packet(broadcast: &str, prefix_length: u8, macs: &[&str]) -> WakePacket {
        WakePacket {
            wake_id: "w".into(),
            mac_addresses: macs.iter().map(|mac| (*mac).to_owned()).collect(),
            broadcast: broadcast.into(),
            prefix_length,
        }
    }

    #[test]
    fn packets_go_out_of_the_interface_on_their_subnet() {
        let addresses = [
            local("en0", [10, 0, 0, 5], 24),
            local("en7", [192, 168, 1, 140], 24),
        ];
        let found = source(&addresses, Ipv4Addr::new(192, 168, 1, 255), 24).unwrap();
        assert_eq!(found.interface, "en7");
        assert!(source(&addresses, Ipv4Addr::new(192, 168, 2, 255), 24).is_none());
    }

    #[test]
    fn invalid_requests_and_other_subnets_are_refused() {
        let addresses = [local("en0", [10, 0, 0, 5], 24)];
        let mac = ["00:15:5D:01:02:03"];
        let error = |packet: WakePacket| send_one(&packet, &addresses).error.unwrap();
        assert!(error(packet("10.0.0.7", 24, &mac)).contains("not a subnet's broadcast"));
        assert!(error(packet("10.0.0.255", 31, &mac)).contains("not a subnet's broadcast"));
        assert!(error(packet("nonsense", 24, &mac)).contains("not a subnet's broadcast"));
        assert!(error(packet("10.0.0.255", 24, &["FF:FF:FF:FF:FF:FF"])).contains("no valid"));
        assert_eq!(
            error(packet("192.168.1.255", 24, &mac)),
            "this computer is no longer on 192.168.1.0/24"
        );
    }

    /// Broadcasts real packets from the loopback interface's subnet, which
    /// every computer has but no network hears.
    #[test]
    fn packets_are_sent_from_the_chosen_address() {
        let interface = if cfg!(target_os = "macos") {
            "lo0"
        } else {
            "lo"
        };
        let addresses = [local(interface, [127, 0, 0, 1], 8)];
        let result = send_one(
            &packet("127.255.255.255", 8, &["00:15:5D:01:02:03"]),
            &addresses,
        );
        assert_eq!(result.source_address.as_deref(), Some("127.0.0.1"));
        assert_eq!(result.wake_id, "w");
        assert_eq!(result.broadcast, "127.255.255.255");
    }
}
