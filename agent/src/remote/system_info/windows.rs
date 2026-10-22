//! The Windows snapshot, from the registry, the firmware's SMBIOS tables,
//! the storage and IP helper APIs, and the Terminal Services session list.
use meshrmm_protocol::SystemInfo;

use super::Problems;

mod applications;
mod disks;
mod hardware;
mod network;
mod os;
mod registry;
mod smbios;
mod users;

/// The IPv4 addresses of the adapters that are up.
pub(super) fn local_ipv4_addresses() -> anyhow::Result<Vec<super::LocalAddress>> {
    Ok(network::adapters()?
        .into_iter()
        .filter(|adapter| adapter.up)
        .flat_map(|adapter| {
            adapter.addresses.into_iter().filter_map(move |address| {
                Some(super::LocalAddress {
                    interface: adapter.name.clone(),
                    address: address.address.parse().ok()?,
                    prefix_length: address.prefix_length,
                })
            })
        })
        .collect())
}

pub(super) fn collect(problems: &mut Problems) -> SystemInfo {
    let os = os::collect(problems);
    let applications = applications::list(problems, &os.architecture);
    SystemInfo {
        hardware: hardware::collect(problems),
        network: network::collect(problems),
        users: problems
            .take("signed-in users", users::list())
            .unwrap_or_default(),
        applications,
        os,
        ..Default::default()
    }
}
