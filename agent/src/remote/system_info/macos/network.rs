//! Network adapters from the interface list and SystemConfiguration: the
//! Mac's hardware ports with their display names, plus any other interface
//! with a routable address, such as a VPN tunnel, and each one's routers and
//! the system's resolvers.
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::Context;
use meshrmm_protocol::{AdapterAddress, AdapterKind, Network, NetworkAdapter};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};

use super::super::{Problems, unix};
use super::{array, dictionary, lookup, text};

#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    fn SCNetworkInterfaceCopyAll() -> Option<NonNull<CFArray>>;
    fn SCNetworkInterfaceGetBSDName(interface: *const c_void) -> *const CFString;
    fn SCNetworkInterfaceGetLocalizedDisplayName(interface: *const c_void) -> *const CFString;
    fn SCNetworkInterfaceGetInterfaceType(interface: *const c_void) -> *const CFString;
    fn SCNetworkInterfaceGetHardwareAddressString(interface: *const c_void) -> *const CFString;
    fn SCDynamicStoreCreate(
        allocator: *const c_void,
        name: &CFString,
        callout: *const c_void,
        context: *mut c_void,
    ) -> Option<NonNull<CFType>>;
    fn SCDynamicStoreCopyKeyList(store: &CFType, pattern: &CFString) -> Option<NonNull<CFArray>>;
    fn SCDynamicStoreCopyValue(store: &CFType, key: &CFString) -> Option<NonNull<CFType>>;
}

/// A hardware port SystemConfiguration knows, such as `Wi-Fi` on `en0`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Port {
    display_name: Option<String>,
    /// `IEEE80211`, `Ethernet`, `Bridge`, …
    kind: Option<String>,
    /// The port's own hardware address.
    mac_address: Option<String>,
}

/// What macOS gives processes it hides hardware addresses from, instead of
/// the address.
const HIDDEN_MAC: &str = "02:00:00:00:00:00";

pub(super) fn collect(problems: &mut Problems) -> Network {
    let hostname = problems
        .take("host name", unix::hostname())
        .unwrap_or_default();
    let (hostname, fqdn) = host_names(&hostname);
    let ports = problems.take("network ports", ports()).unwrap_or_default();
    let store = problems.take("network state", store());
    let routers = store.as_ref().map(routers).unwrap_or_default();
    let dns_servers = store
        .as_ref()
        .and_then(dns_servers)
        .filter(|servers| !servers.is_empty())
        .or_else(|| unix::resolv_conf_servers("/etc/resolv.conf").ok())
        .unwrap_or_default();
    let interfaces = problems
        .take("network adapters", unix::interfaces())
        .unwrap_or_default();
    Network {
        hostname,
        fqdn,
        domain: None,
        workgroup: None,
        adapters: adapters(interfaces, &ports, &routers),
        dns_servers,
    }
}

/// The host name without the `.local` Bonjour suffix macOS adds, and the
/// fully qualified name when the Mac has a real DNS name.
fn host_names(name: &str) -> (String, Option<String>) {
    if let Some(short) = name.strip_suffix(".local") {
        return (short.to_owned(), None);
    }
    match name.split_once('.') {
        Some((short, _)) if !short.is_empty() => (short.to_owned(), Some(name.to_owned())),
        _ => (name.to_owned(), None),
    }
}

fn adapters(
    interfaces: BTreeMap<String, unix::Interface>,
    ports: &HashMap<String, Port>,
    routers: &HashMap<String, Vec<String>>,
) -> Vec<NetworkAdapter> {
    let mut adapters: Vec<NetworkAdapter> = interfaces
        .into_iter()
        .filter(|(name, interface)| listed(name, interface, ports.get(name)))
        .map(|(name, interface)| {
            let port = ports.get(&name);
            NetworkAdapter {
                description: port.and_then(|port| port.display_name.clone()),
                kind: kind(&name, port),
                mac_address: mac_address(interface.mac_address, port),
                up: interface.up,
                speed_bits_per_second: None,
                gateways: routers.get(&name).cloned().unwrap_or_default(),
                dns_servers: Vec::new(),
                addresses: interface.addresses,
                name,
            }
        })
        .collect();
    // Hardware ports first, in the system's order of their names.
    adapters.sort_by_key(|adapter| (adapter.kind == AdapterKind::Virtual, adapter.name.clone()));
    adapters
}

/// The address the interface uses, or when macOS hides it, the port's
/// hardware address.
fn mac_address(interface: Option<String>, port: Option<&Port>) -> Option<String> {
    let known = |address: &String| !address.eq_ignore_ascii_case(HIDDEN_MAC);
    interface
        .filter(known)
        .or_else(|| port?.mac_address.clone().filter(known))
}

/// Whether an interface is worth listing: a hardware port, or any other
/// interface with an address beyond link-local, such as a VPN tunnel. The
/// many system interfaces with neither (AirDrop's `awdl0`, `llw0`, `anpi*`,
/// `gif0`, `stf0`, idle `utun*` tunnels) and loopback are left out.
fn listed(name: &str, interface: &unix::Interface, port: Option<&Port>) -> bool {
    if interface.loopback {
        return false;
    }
    port.is_some() && !name.starts_with("awdl") && !name.starts_with("llw")
        || interface.addresses.iter().any(routable)
}

fn routable(address: &AdapterAddress) -> bool {
    match address.address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => !ip.is_link_local() && !ip.is_loopback(),
        Ok(std::net::IpAddr::V6(ip)) => !ip.is_unicast_link_local() && !ip.is_loopback(),
        Err(_) => false,
    }
}

fn kind(name: &str, port: Option<&Port>) -> AdapterKind {
    match port.and_then(|port| port.kind.as_deref()) {
        Some("IEEE80211") => return AdapterKind::Wireless,
        Some("Bridge" | "VLAN" | "Bond" | "PPP" | "IPSec" | "VPN") => return AdapterKind::Virtual,
        _ => {}
    }
    let virtual_prefixes = [
        "utun", "ipsec", "ppp", "bridge", "gif", "stf", "vmnet", "feth", "tap", "tun", "vlan",
        "bond", "awdl", "llw", "anpi",
    ];
    if virtual_prefixes
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        AdapterKind::Virtual
    } else if name.starts_with("en") {
        AdapterKind::Ethernet
    } else {
        AdapterKind::Other
    }
}

/// SystemConfiguration's hardware ports by BSD name.
fn ports() -> anyhow::Result<HashMap<String, Port>> {
    // SAFETY: the function returns a +1 reference or null.
    let all = unsafe { SCNetworkInterfaceCopyAll() }.context("no network interfaces")?;
    // SAFETY: the reference is owned.
    let all = unsafe { CFRetained::from_raw(all) };
    let mut ports = HashMap::new();
    for index in 0..all.len() {
        // SAFETY: the index is in bounds; the element is an SCNetworkInterface
        // the array keeps alive while it is used here.
        let interface = unsafe { all.value_at_index(index as isize) };
        // SAFETY: the getters return unretained strings owned by the interface.
        let (name, display_name, kind, mac_address) = unsafe {
            (
                borrowed_string(SCNetworkInterfaceGetBSDName(interface)),
                borrowed_string(SCNetworkInterfaceGetLocalizedDisplayName(interface)),
                borrowed_string(SCNetworkInterfaceGetInterfaceType(interface)),
                borrowed_string(SCNetworkInterfaceGetHardwareAddressString(interface)),
            )
        };
        if let Some(name) = name {
            ports.insert(
                name,
                Port {
                    display_name,
                    kind,
                    mac_address,
                },
            );
        }
    }
    Ok(ports)
}

/// # Safety
/// `string` must be null or a valid CFString.
unsafe fn borrowed_string(string: *const CFString) -> Option<String> {
    // SAFETY: the caller's guarantee.
    let string = unsafe { string.as_ref() }?.to_string();
    (!string.is_empty()).then_some(string)
}

fn store() -> anyhow::Result<CFRetained<CFType>> {
    let name = CFString::from_static_str("MeshRMM system information");
    // SAFETY: a store without a callout; the result is a +1 reference.
    let store = unsafe {
        SCDynamicStoreCreate(
            std::ptr::null(),
            &name,
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    }
    .context("SCDynamicStoreCreate failed")?;
    // SAFETY: the reference is owned.
    Ok(unsafe { CFRetained::from_raw(store) })
}

fn value(store: &CFType, key: &str) -> Option<CFRetained<CFType>> {
    // SAFETY: the store and key are valid; the value is a +1 reference.
    let value = unsafe { SCDynamicStoreCopyValue(store, &CFString::from_str(key)) }?;
    // SAFETY: the reference is owned.
    Some(unsafe { CFRetained::from_raw(value) })
}

/// Each interface's routers, from its network services' IPv4 and IPv6 state.
fn routers(store: &CFRetained<CFType>) -> HashMap<String, Vec<String>> {
    let pattern = CFString::from_static_str("State:/Network/Service/[^/]+/IPv[46]");
    // SAFETY: the store and pattern are valid; the list is a +1 reference.
    let Some(keys) = (unsafe { SCDynamicStoreCopyKeyList(store, &pattern) }) else {
        return HashMap::new();
    };
    // SAFETY: the reference is owned, and the list holds strings.
    let keys: CFRetained<CFArray<CFString>> =
        unsafe { CFRetained::cast_unchecked(CFRetained::from_raw(keys)) };
    let mut routers = HashMap::<String, Vec<String>>::new();
    for key in keys.to_vec() {
        let Some(state) = value(store, &key.to_string()).and_then(dictionary) else {
            continue;
        };
        let interface = lookup(&state, "InterfaceName").and_then(|value| text(&value));
        let router = lookup(&state, "Router").and_then(|value| text(&value));
        if let (Some(interface), Some(router)) = (interface, router) {
            let list = routers.entry(interface).or_default();
            if !list.contains(&router) {
                list.push(router);
            }
        }
    }
    routers
}

/// The resolvers the system uses, as `scutil --dns` lists first.
fn dns_servers(store: &CFRetained<CFType>) -> Option<Vec<String>> {
    let dns = value(store, "State:/Network/Global/DNS").and_then(dictionary)?;
    let servers = array(lookup(&dns, "ServerAddresses")?)?;
    Some(
        servers
            .to_vec()
            .iter()
            .filter_map(|server| text(server))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(addresses: &[&str], loopback: bool) -> unix::Interface {
        unix::Interface {
            up: true,
            loopback,
            mac_address: None,
            addresses: addresses
                .iter()
                .map(|address| AdapterAddress {
                    address: (*address).into(),
                    prefix_length: 64,
                })
                .collect(),
        }
    }

    fn port(kind: &str) -> Port {
        Port {
            display_name: Some(kind.into()),
            kind: Some(kind.into()),
            mac_address: Some("b0:be:83:35:44:7c".into()),
        }
    }

    #[test]
    fn hidden_hardware_addresses_come_from_the_port() {
        let wifi = port("IEEE80211");
        assert_eq!(
            mac_address(Some("AE:16:3D:0D:A5:15".into()), Some(&wifi)).as_deref(),
            Some("AE:16:3D:0D:A5:15")
        );
        assert_eq!(
            mac_address(Some(HIDDEN_MAC.into()), Some(&wifi)).as_deref(),
            Some("b0:be:83:35:44:7c")
        );
        assert_eq!(mac_address(Some(HIDDEN_MAC.into()), None), None);
    }

    #[test]
    fn ports_and_routable_interfaces_are_listed() {
        let wifi = port("IEEE80211");
        assert!(listed("en0", &interface(&[], false), Some(&wifi)));
        assert!(listed(
            "utun4",
            &interface(&["fe80::1", "100.64.1.2"], false),
            None
        ));
        assert!(!listed("utun0", &interface(&["fe80::1"], false), None));
        assert!(!listed(
            "awdl0",
            &interface(&["fe80::1"], false),
            Some(&port("Ethernet"))
        ));
        assert!(!listed("lo0", &interface(&["127.0.0.1"], true), None));
        assert!(!listed("anpi0", &interface(&[], false), None));
    }

    #[test]
    fn kinds_come_from_the_port_then_the_name() {
        assert_eq!(kind("en0", Some(&port("IEEE80211"))), AdapterKind::Wireless);
        assert_eq!(kind("en1", Some(&port("Ethernet"))), AdapterKind::Ethernet);
        assert_eq!(kind("bridge0", Some(&port("Bridge"))), AdapterKind::Virtual);
        assert_eq!(kind("utun3", None), AdapterKind::Virtual);
        assert_eq!(kind("en5", None), AdapterKind::Ethernet);
        assert_eq!(kind("xyz0", None), AdapterKind::Other);
    }

    #[test]
    fn bonjour_names_are_not_fully_qualified() {
        assert_eq!(
            host_names("Grants-MacBook-Air.local"),
            ("Grants-MacBook-Air".into(), None)
        );
        assert_eq!(
            host_names("mac1.corp.example.com"),
            ("mac1".into(), Some("mac1.corp.example.com".into()))
        );
        assert_eq!(host_names("studio"), ("studio".into(), None));
    }
}
