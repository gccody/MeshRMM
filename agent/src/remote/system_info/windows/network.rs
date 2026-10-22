//! The computer's names and domain, and its network adapters with their
//! addresses, gateways and resolvers.
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Context, bail};
use meshrmm_protocol::{AdapterAddress, AdapterKind, Network, NetworkAdapter};
use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_MORE_DATA, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST,
    GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::NetworkManagement::NetManagement::{
    NETSETUP_JOIN_STATUS, NetApiBufferFree, NetGetJoinInformation, NetSetupDomainName,
    NetSetupWorkgroupName,
};
use windows::Win32::Networking::WinSock::SOCKET_ADDRESS;
use windows::Win32::System::SystemInformation::{
    COMPUTER_NAME_FORMAT, ComputerNameDnsFullyQualified, ComputerNameDnsHostname,
    ComputerNameNetBIOS, GetComputerNameExW,
};
use windows::core::{PCWSTR, PWSTR};

use super::super::Problems;

/// `IF_TYPE_*` values from ipifcons.h.
const IF_TYPE_ETHERNET: u32 = 6;
const IF_TYPE_PPP: u32 = 23;
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_PROP_VIRTUAL: u32 = 53;
const IF_TYPE_IEEE80211: u32 = 71;
const IF_TYPE_TUNNEL: u32 = 131;
/// The `AF_*` families of a `SOCKADDR`.
const AF_INET: u16 = 2;
const AF_INET6: u16 = 23;

pub(super) fn collect(problems: &mut Problems) -> Network {
    let hostname = computer_name(ComputerNameDnsHostname)
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(|| problems.take("host name", computer_name(ComputerNameNetBIOS)))
        .unwrap_or_default();
    let fqdn = computer_name(ComputerNameDnsFullyQualified)
        .ok()
        .filter(|name| name.contains('.') && !name.eq_ignore_ascii_case(&hostname));
    let (domain, workgroup) = problems.take("domain", join()).unwrap_or_default();
    Network {
        hostname,
        fqdn,
        domain,
        workgroup,
        adapters: problems
            .take("network adapters", adapters())
            .unwrap_or_default(),
        // Windows keeps resolvers per adapter.
        dns_servers: Vec::new(),
    }
}

fn computer_name(format: COMPUTER_NAME_FORMAT) -> anyhow::Result<String> {
    let mut length = 0u32;
    // SAFETY: no buffer asks for the length, which includes the NUL.
    let _ = unsafe { GetComputerNameExW(format, None, &mut length) };
    let mut name = vec![0u16; length.max(1) as usize];
    // SAFETY: the buffer holds `length` units.
    unsafe { GetComputerNameExW(format, Some(PWSTR(name.as_mut_ptr())), &mut length) }
        .context("GetComputerNameExW")?;
    Ok(String::from_utf16_lossy(&name[..length as usize]))
}

/// The Active Directory domain the computer joined, or else its workgroup.
fn join() -> anyhow::Result<(Option<String>, Option<String>)> {
    let mut name = PWSTR::null();
    let mut status = NETSETUP_JOIN_STATUS::default();
    // SAFETY: both out pointers are valid; the name is freed below.
    let result = unsafe { NetGetJoinInformation(PCWSTR::null(), &mut name, &mut status) };
    if result != 0 {
        bail!("NetGetJoinInformation failed with {result}");
    }
    // SAFETY: the API returned a NUL-terminated name, or none.
    let text = (!name.is_null())
        .then(|| unsafe { name.to_string() }.ok())
        .flatten()
        .filter(|text| !text.is_empty());
    if !name.is_null() {
        // SAFETY: the buffer came from NetGetJoinInformation.
        unsafe { NetApiBufferFree(Some(name.0.cast())) };
    }
    Ok(match status {
        status if status == NetSetupDomainName => (text, None),
        status if status == NetSetupWorkgroupName => (None, text),
        _ => (None, None),
    })
}

pub(super) fn adapters() -> anyhow::Result<Vec<NetworkAdapter>> {
    // Hyper-V virtual switches and VPNs show up as Ethernet; only hardware
    // adapters have the hardware flag.
    let mut hardware = HashSet::new();
    crate::remote::metrics::windows::for_each_hardware_interface(|row| {
        // SAFETY: a LUID is always readable as its value.
        hardware.insert(unsafe { row.InterfaceLuid.Value });
    })
    .context("GetIfTable2")?;

    let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
    let mut size = 32 * 1024u32;
    let mut buffer;
    let mut attempts = 0;
    loop {
        // u64s keep the records aligned.
        buffer = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: the buffer holds `size` bytes.
        let result = unsafe {
            GetAdaptersAddresses(0, flags, None, Some(buffer.as_mut_ptr().cast()), &mut size)
        };
        attempts += 1;
        match result {
            code if code == ERROR_SUCCESS.0 => break,
            code if (code == ERROR_BUFFER_OVERFLOW.0 || code == ERROR_MORE_DATA.0)
                && attempts < 4 => {}
            code => bail!("GetAdaptersAddresses failed with {code}"),
        }
    }

    let mut adapters = Vec::new();
    let mut entry = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !entry.is_null() {
        // SAFETY: the list lives in `buffer` until it is dropped.
        let adapter = unsafe { &*entry };
        entry = adapter.Next;
        if adapter.IfType == IF_TYPE_SOFTWARE_LOOPBACK {
            continue;
        }
        // SAFETY: the LUID is always readable as its value.
        let is_hardware = hardware.contains(&unsafe { adapter.Luid.Value });
        let up = adapter.OperStatus == IfOperStatusUp;
        let mut addresses = Vec::new();
        let mut unicast = adapter.FirstUnicastAddress;
        while !unicast.is_null() {
            // SAFETY: as above.
            let address = unsafe { &*unicast };
            unicast = address.Next;
            if let Some(ip) = ip_address(&address.Address) {
                addresses.push(AdapterAddress {
                    address: ip.to_string(),
                    prefix_length: address.OnLinkPrefixLength,
                });
            }
        }
        // Hidden virtual adapters that are down and unused are noise.
        if !is_hardware && !up && addresses.is_empty() {
            continue;
        }
        let mut gateways = Vec::new();
        let mut gateway = adapter.FirstGatewayAddress;
        while !gateway.is_null() {
            // SAFETY: as above.
            let current = unsafe { &*gateway };
            gateway = current.Next;
            gateways.extend(ip_address(&current.Address).map(|ip| ip.to_string()));
        }
        let mut dns_servers = Vec::new();
        let mut server = adapter.FirstDnsServerAddress;
        while !server.is_null() {
            // SAFETY: as above.
            let current = unsafe { &*server };
            server = current.Next;
            dns_servers.extend(
                ip_address(&current.Address)
                    .filter(|ip| !is_placeholder_resolver(ip))
                    .map(|ip| ip.to_string()),
            );
        }
        let length = (adapter.PhysicalAddressLength as usize).min(adapter.PhysicalAddress.len());
        let mac = &adapter.PhysicalAddress[..length];
        adapters.push(NetworkAdapter {
            // SAFETY: the names are NUL-terminated, or null.
            name: unsafe { adapter.FriendlyName.to_string() }.unwrap_or_default(),
            description: unsafe { adapter.Description.to_string() }.ok(),
            mac_address: (length == 6).then(|| {
                mac.iter()
                    .map(|byte| format!("{byte:02X}"))
                    .collect::<Vec<_>>()
                    .join(":")
            }),
            kind: kind(adapter.IfType, is_hardware),
            up,
            speed_bits_per_second: Some(adapter.TransmitLinkSpeed)
                .filter(|speed| *speed != 0 && *speed != u64::MAX),
            addresses,
            gateways,
            dns_servers,
        });
    }
    Ok(adapters)
}

fn kind(if_type: u32, hardware: bool) -> AdapterKind {
    match if_type {
        IF_TYPE_ETHERNET | IF_TYPE_IEEE80211 if !hardware => AdapterKind::Virtual,
        IF_TYPE_ETHERNET => AdapterKind::Ethernet,
        IF_TYPE_IEEE80211 => AdapterKind::Wireless,
        IF_TYPE_TUNNEL | IF_TYPE_PPP | IF_TYPE_PROP_VIRTUAL => AdapterKind::Virtual,
        _ => AdapterKind::Other,
    }
}

/// Windows lists these site-local IPv6 resolvers on adapters that have
/// none configured.
fn is_placeholder_resolver(ip: &IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if v6.segments()[..7] == [0xfec0, 0, 0, 0xffff, 0, 0, 0]
        && (1..=3).contains(&v6.segments()[7]))
}

fn ip_address(address: &SOCKET_ADDRESS) -> Option<IpAddr> {
    if address.lpSockaddr.is_null() || address.iSockaddrLength <= 0 {
        return None;
    }
    // SAFETY: the address is `iSockaddrLength` bytes long.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            address.lpSockaddr.cast::<u8>(),
            address.iSockaddrLength as usize,
        )
    };
    parse_sockaddr(bytes)
}

/// The address in a `SOCKADDR_IN` or `SOCKADDR_IN6`.
fn parse_sockaddr(bytes: &[u8]) -> Option<IpAddr> {
    let family = u16::from_le_bytes(bytes.get(..2)?.try_into().ok()?);
    match family {
        AF_INET => {
            let octets: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
            Some(Ipv4Addr::from(octets).into())
        }
        AF_INET6 => {
            let octets: [u8; 16] = bytes.get(8..24)?.try_into().ok()?;
            Some(Ipv6Addr::from(octets).into())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_addresses_are_parsed() {
        let mut v4 = vec![2, 0, 0x1f, 0x90, 192, 168, 1, 152];
        v4.extend([0; 8]);
        assert_eq!(parse_sockaddr(&v4), Some("192.168.1.152".parse().unwrap()));
        let mut v6 = vec![23, 0, 0, 0, 0, 0, 0, 0];
        v6.extend("fe80::1234".parse::<Ipv6Addr>().unwrap().octets());
        v6.extend([5, 0, 0, 0]);
        assert_eq!(parse_sockaddr(&v6), Some("fe80::1234".parse().unwrap()));
        assert_eq!(parse_sockaddr(&[2, 0, 0]), None);
        assert_eq!(parse_sockaddr(&[17, 0, 0, 0, 1, 2, 3, 4]), None);
    }

    #[test]
    fn virtual_ethernet_is_virtual() {
        assert_eq!(kind(IF_TYPE_ETHERNET, true), AdapterKind::Ethernet);
        assert_eq!(kind(IF_TYPE_ETHERNET, false), AdapterKind::Virtual);
        assert_eq!(kind(IF_TYPE_IEEE80211, true), AdapterKind::Wireless);
        assert_eq!(kind(IF_TYPE_TUNNEL, false), AdapterKind::Virtual);
        assert_eq!(kind(144, true), AdapterKind::Other);
    }

    #[test]
    fn placeholder_resolvers_are_dropped() {
        assert!(is_placeholder_resolver(
            &"fec0:0:0:ffff::1".parse().unwrap()
        ));
        assert!(is_placeholder_resolver(
            &"fec0:0:0:ffff::3".parse().unwrap()
        ));
        assert!(!is_placeholder_resolver(
            &"fec0:0:0:ffff::4".parse().unwrap()
        ));
        assert!(!is_placeholder_resolver(&"1.1.1.1".parse().unwrap()));
    }
}
