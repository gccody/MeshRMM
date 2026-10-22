//! Choosing the online devices that wake an offline one.
//!
//! Magic packets are broadcasts, which routers don't forward, so a relay
//! must have an address on a subnet that holds one of the offline device's
//! last known IPv4 addresses. Every online device with one is a candidate.
//! When some of them connect from the public address the offline device last
//! connected from, only those are used: the others are most likely at
//! another site that happens to use the same private addresses. Candidates
//! are then ranked by whether the subnet is on a hardware adapter at either
//! end, rather than a virtual switch, bridge or tunnel, then by company, and
//! up to [`super::MAX_RELAYS`] are used. A subnet that is virtual at both
//! ends, such as two hosts' identical container bridges, never counts.
use std::{collections::BTreeMap, net::Ipv4Addr};

use meshrmm_protocol_types::{AdapterKind, directed_broadcast, wake_prefix_supported};

/// An IPv4 address on one of a device's adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub address: Ipv4Addr,
    pub prefix_length: u8,
    pub kind: AdapterKind,
}

impl Address {
    /// Whether the address can be on a subnet a relay broadcasts on.
    pub fn usable(&self) -> bool {
        let ip = self.address;
        wake_prefix_supported(self.prefix_length)
            && !ip.is_loopback()
            && !ip.is_link_local()
            && !ip.is_unspecified()
            && !ip.is_multicast()
            && !ip.is_broadcast()
            && directed_broadcast(ip, self.prefix_length) != ip
    }

    fn hardware(&self) -> bool {
        self.kind != AdapterKind::Virtual
    }
}

/// An online device that may relay.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub device_id: String,
    pub name: String,
    pub public_ip: Option<String>,
    pub company_id: Option<String>,
    pub addresses: Vec<Address>,
}

/// The offline device, as it was last seen.
#[derive(Debug, Clone)]
pub struct Target<'a> {
    pub device_id: &'a str,
    pub public_ip: Option<&'a str>,
    pub company_id: Option<&'a str>,
    pub addresses: &'a [Address],
}

/// A relay and the subnets it broadcasts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub device_id: String,
    pub name: String,
    /// Each subnet's directed broadcast address and prefix length.
    pub subnets: Vec<(Ipv4Addr, u8)>,
}

/// A candidate that shares a subnet with the target: its rank (whether it
/// is at the same site, how many ends of the subnet are hardware, whether
/// it is in the same company), itself and the subnets.
type Match<'a> = ((bool, u8, bool), &'a Candidate, Vec<(Ipv4Addr, u8)>);

/// Up to `limit` relays for `target`, best first.
pub fn choose(target: &Target<'_>, candidates: &[Candidate], limit: usize) -> Vec<Chosen> {
    let targets = target
        .addresses
        .iter()
        .filter(|address| address.usable())
        .collect::<Vec<_>>();
    // Rank (higher is better) and subnets of each candidate that shares one.
    let mut matches: Vec<Match<'_>> = Vec::new();
    for candidate in candidates {
        if candidate.device_id == target.device_id {
            continue;
        }
        let mut best_hardware = None;
        let mut subnets = BTreeMap::new();
        for relay in candidate
            .addresses
            .iter()
            .filter(|address| address.usable())
        {
            let broadcast = directed_broadcast(relay.address, relay.prefix_length);
            for wanted in &targets {
                if directed_broadcast(wanted.address, relay.prefix_length) != broadcast
                    || !(relay.hardware() || wanted.hardware())
                {
                    continue;
                }
                let hardware = u8::from(relay.hardware()) + u8::from(wanted.hardware());
                best_hardware = best_hardware.max(Some(hardware));
                subnets.insert(broadcast, relay.prefix_length);
            }
        }
        let Some(hardware) = best_hardware else {
            continue;
        };
        let same_site =
            target.public_ip.is_some() && candidate.public_ip.as_deref() == target.public_ip;
        let same_company =
            target.company_id.is_some() && candidate.company_id.as_deref() == target.company_id;
        matches.push((
            (same_site, hardware, same_company),
            candidate,
            subnets.into_iter().collect(),
        ));
    }
    if matches.iter().any(|((same_site, _, _), _, _)| *same_site) {
        matches.retain(|((same_site, _, _), _, _)| *same_site);
    }
    matches.sort_by(|(left_rank, left, _), (right_rank, right, _)| {
        right_rank
            .cmp(left_rank)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.device_id.cmp(&right.device_id))
    });
    matches
        .into_iter()
        .take(limit)
        .map(|(_, candidate, subnets)| Chosen {
            device_id: candidate.device_id.clone(),
            name: candidate.name.clone(),
            subnets,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str, prefix_length: u8, kind: AdapterKind) -> Address {
        Address {
            address: text.parse().unwrap(),
            prefix_length,
            kind,
        }
    }

    fn lan(text: &str) -> Address {
        address(text, 24, AdapterKind::Ethernet)
    }

    fn candidate(name: &str, public_ip: &str, addresses: Vec<Address>) -> Candidate {
        Candidate {
            device_id: name.to_lowercase(),
            name: name.to_owned(),
            public_ip: Some(public_ip.to_owned()),
            company_id: None,
            addresses,
        }
    }

    fn names(chosen: &[Chosen]) -> Vec<&str> {
        chosen.iter().map(|relay| relay.name.as_str()).collect()
    }

    #[test]
    fn relays_share_the_targets_subnet() {
        let addresses = [lan("192.168.1.23")];
        let target = Target {
            device_id: "offline",
            public_ip: Some("203.0.113.5"),
            company_id: None,
            addresses: &addresses,
        };
        let candidates = [
            candidate("Mac", "203.0.113.5", vec![lan("192.168.1.140")]),
            candidate(
                "VM",
                "203.0.113.5",
                vec![address("172.20.112.7", 20, AdapterKind::Ethernet)],
            ),
            // Given the offline device's old address.
            candidate("PC", "203.0.113.5", vec![lan("192.168.1.23")]),
        ];
        let chosen = choose(&target, &candidates, 3);
        assert_eq!(names(&chosen), ["Mac", "PC"]);
        assert_eq!(chosen[0].subnets, [("192.168.1.255".parse().unwrap(), 24)]);
    }

    #[test]
    fn relays_at_the_same_site_come_first_and_alone() {
        let addresses = [lan("10.0.0.20")];
        let target = Target {
            device_id: "pc",
            public_ip: Some("203.0.113.5"),
            company_id: None,
            addresses: &addresses,
        };
        let elsewhere = candidate("Elsewhere", "198.51.100.1", vec![lan("10.0.0.9")]);
        let here = candidate("Here", "203.0.113.5", vec![lan("10.0.0.30")]);
        assert_eq!(
            names(&choose(&target, &[elsewhere.clone(), here], 3)),
            ["Here"]
        );
        // Without one at the site, any on the subnet will do.
        assert_eq!(names(&choose(&target, &[elsewhere], 3)), ["Elsewhere"]);
    }

    #[test]
    fn hardware_subnets_rank_first_and_virtual_pairs_never_count() {
        let addresses = [
            lan("192.168.1.23"),
            address("172.17.0.1", 16, AdapterKind::Virtual),
        ];
        let target = Target {
            device_id: "pc",
            public_ip: None,
            company_id: Some("acme"),
            addresses: &addresses,
        };
        let docker = candidate(
            "Docker",
            "x",
            vec![address("172.17.0.1", 16, AdapterKind::Virtual)],
        );
        let switch = candidate(
            "Hyper-V host",
            "x",
            vec![address("192.168.1.9", 24, AdapterKind::Virtual)],
        );
        let mut wired = candidate("Wired", "x", vec![lan("192.168.1.10")]);
        wired.company_id = Some("acme".into());
        let other = candidate("Another", "x", vec![lan("192.168.1.11")]);
        let chosen = choose(&target, &[docker, switch, other, wired], 2);
        assert_eq!(names(&chosen), ["Wired", "Another"]);
    }

    #[test]
    fn unusable_addresses_and_the_target_itself_are_ignored() {
        let addresses = [
            address("169.254.3.4", 16, AdapterKind::Ethernet),
            address("127.0.0.1", 8, AdapterKind::Ethernet),
            address("10.9.8.7", 32, AdapterKind::Ethernet),
        ];
        let target = Target {
            device_id: "pc",
            public_ip: None,
            company_id: None,
            addresses: &addresses,
        };
        let candidates = [
            candidate(
                "Link-local",
                "x",
                vec![address("169.254.9.9", 16, AdapterKind::Ethernet)],
            ),
            candidate(
                "Loopback",
                "x",
                vec![address("127.0.0.1", 8, AdapterKind::Ethernet)],
            ),
            Candidate {
                device_id: "pc".into(),
                ..candidate("Itself", "x", vec![lan("10.9.8.7")])
            },
        ];
        assert!(choose(&target, &candidates, 3).is_empty());
        assert!(!lan("192.168.1.255").usable(), "a broadcast address");
        assert!(lan("192.168.1.1").usable());
    }
}
