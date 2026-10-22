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
