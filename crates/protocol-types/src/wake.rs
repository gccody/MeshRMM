//! Wake-on-LAN: an offline computer is woken by an online one on the same
//! network, which broadcasts a magic packet for it.
//!
//! The server picks the relays and asks each over its control connection
//! ([`crate::AgentCommand::SendWakePackets`]), naming the subnet it shares
//! with the offline computer by that subnet's directed broadcast address and
//! prefix length. The relay sends the packet from its own address on that
//! subnet and reports what happened ([`crate::AgentStatusMessage::WakePacketsSent`]).
//!
//! A magic packet is six `0xFF` bytes followed by the target's hardware
//! address sixteen times. The relay sends it over UDP to [`WAKE_PORTS`], both
//! to the subnet's directed broadcast address and to 255.255.255.255.
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

/// The UDP ports a magic packet is sent to: 9 (discard), which network
/// cards and tools use by default, and 7 (echo), which some older ones do.
pub const WAKE_PORTS: [u16; 2] = [9, 7];
/// The length of a magic packet: 6 + 16 × 6 bytes.
pub const MAGIC_PACKET_BYTES: usize = 102;
/// The most hardware addresses one packet request names.
pub const MAX_WAKE_MAC_ADDRESSES: usize = 8;
/// The most packet requests in one command.
pub const MAX_WAKE_PACKETS: usize = 256;
/// The narrowest and widest subnets a relay broadcasts on. A /31 or /32 has
/// no broadcast address, and a subnet wider than a /8 is not one network.
pub const MIN_WAKE_PREFIX_LENGTH: u8 = 8;
pub const MAX_WAKE_PREFIX_LENGTH: u8 = 30;

/// One computer to wake, as a relay sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakePacket {
    /// The server's record of the wake, which the result names.
    pub wake_id: String,
    /// The computer's hardware addresses, such as `00:15:5D:01:02:03`.
    pub mac_addresses: Vec<String>,
    /// The subnet's directed broadcast address, such as `192.168.1.255`.
    pub broadcast: String,
    /// The subnet's prefix length, such as 24, which with `broadcast` tells
    /// the relay which of its addresses to send from.
    pub prefix_length: u8,
}

/// What a relay did with one [`WakePacket`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakePacketResult {
    pub wake_id: String,
    /// The packet's `broadcast`, which with `wake_id` names it.
    pub broadcast: String,
    /// The relay's address it was sent from, once it found one on the subnet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_address: Option<String>,
    /// Why nothing was sent; `None` once at least one packet went out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A hardware address's six bytes, from `00:15:5D:01:02:03`,
/// `00-15-5d-01-02-03` or `0015.5d01.0203`. The all-zero, broadcast and
/// multicast addresses are no computer's.
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let digits: Vec<u8> = text
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.'))
        .map(|c| c.to_digit(16).map(|digit| digit as u8))
        .collect::<Option<_>>()?;
    if digits.len() != 12 {
        return None;
    }
    let mut mac = [0; 6];
    for (byte, pair) in mac.iter_mut().zip(digits.chunks(2)) {
        *byte = pair[0] << 4 | pair[1];
    }
    // The low bit of the first byte marks group addresses, broadcast too.
    (mac != [0; 6] && mac[0] & 1 == 0).then_some(mac)
}

/// The magic packet that wakes the computer with hardware address `mac`.
pub fn magic_packet(mac: [u8; 6]) -> [u8; MAGIC_PACKET_BYTES] {
    let mut packet = [0xFF; MAGIC_PACKET_BYTES];
    for copy in packet[6..].chunks_exact_mut(6) {
        copy.copy_from_slice(&mac);
    }
    packet
}

/// The mask of a subnet with `prefix_length` leading one bits.
pub fn subnet_mask(prefix_length: u8) -> u32 {
    match prefix_length {
        0 => 0,
        length => u32::MAX << (32 - u32::from(length.min(32))),
    }
}

/// The directed broadcast address of the subnet `address` is on.
pub fn directed_broadcast(address: Ipv4Addr, prefix_length: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(address) | !subnet_mask(prefix_length))
}

/// Whether `address` is on the subnet whose directed broadcast address is
/// `broadcast`.
pub fn on_subnet(address: Ipv4Addr, broadcast: Ipv4Addr, prefix_length: u8) -> bool {
    directed_broadcast(address, prefix_length) == broadcast
}

/// Whether a relay may broadcast on a subnet of `prefix_length`.
pub fn wake_prefix_supported(prefix_length: u8) -> bool {
    (MIN_WAKE_PREFIX_LENGTH..=MAX_WAKE_PREFIX_LENGTH).contains(&prefix_length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_addresses_parse_in_every_usual_form() {
        let mac = [0x00, 0x15, 0x5D, 0x01, 0x02, 0xAB];
        for text in ["00:15:5D:01:02:AB", "00-15-5d-01-02-ab", "0015.5d01.02ab"] {
            assert_eq!(parse_mac(text), Some(mac), "{text}");
        }
        for text in [
            "",
            "00:15:5D:01:02",
            "00:15:5D:01:02:AB:CD",
            "00:15:5D:01:02:XY",
            "00:00:00:00:00:00",
            "FF:FF:FF:FF:FF:FF",
            "01:00:5E:00:00:01",
        ] {
            assert_eq!(parse_mac(text), None, "{text}");
        }
    }

    #[test]
    fn magic_packets_repeat_the_address_sixteen_times() {
        let mac = [0x00, 0x15, 0x5D, 0x01, 0x02, 0xAB];
        let packet = magic_packet(mac);
        assert_eq!(packet.len(), 102);
        assert_eq!(packet[..6], [0xFF; 6]);
        for copy in packet[6..].chunks(6) {
            assert_eq!(copy, mac);
        }
    }

    #[test]
    fn broadcast_addresses_follow_the_prefix() {
        let address = Ipv4Addr::new(192, 168, 1, 140);
        assert_eq!(
            directed_broadcast(address, 24),
            Ipv4Addr::new(192, 168, 1, 255)
        );
        assert_eq!(
            directed_broadcast(Ipv4Addr::new(172, 20, 117, 9), 20),
            Ipv4Addr::new(172, 20, 127, 255)
        );
        assert_eq!(
            directed_broadcast(Ipv4Addr::new(10, 1, 2, 3), 8),
            Ipv4Addr::new(10, 255, 255, 255)
        );
        assert!(on_subnet(
            Ipv4Addr::new(192, 168, 1, 7),
            Ipv4Addr::new(192, 168, 1, 255),
            24
        ));
        assert!(!on_subnet(
            Ipv4Addr::new(192, 168, 2, 7),
            Ipv4Addr::new(192, 168, 1, 255),
            24
        ));
        assert_eq!(subnet_mask(0), 0);
        assert_eq!(subnet_mask(32), u32::MAX);
        assert!(wake_prefix_supported(24));
        assert!(!wake_prefix_supported(31) && !wake_prefix_supported(7));
    }
}
