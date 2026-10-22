//! A computer's operating system, hardware, network and installed software,
//! which an Agent collects when it starts, every
//! [`SYSTEM_INFO_INTERVAL_SECONDS`], and when the server asks, and sends on
//! its control connection as one snapshot.
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// How often an Agent collects a new snapshot on its own.
pub const SYSTEM_INFO_INTERVAL_SECONDS: u64 = 6 * 60 * 60;
/// The most applications a snapshot lists.
pub const MAX_APPLICATIONS: usize = 10_000;
/// The most adapters, disks, users, addresses per adapter, and problems.
pub const MAX_SYSTEM_INFO_ITEMS: usize = 64;
/// The longest text, in characters, any field of a snapshot carries.
pub const MAX_SYSTEM_INFO_TEXT_CHARS: usize = 256;
/// The largest snapshot an Agent sends, as JSON. An Agent drops applications
/// from a larger one, and says so in its problems.
pub const MAX_SYSTEM_INFO_BYTES: usize = 6 * 1024 * 1024;

/// One snapshot of a computer. Parts the Agent could not read are left
/// empty, and say why in [`SystemInfo::problems`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemInfo {
    /// When the Agent collected it, by the device's clock.
    pub collected_at_unix_ms: i64,
    pub os: OperatingSystem,
    pub hardware: Hardware,
    pub network: Network,
    /// The users signed in to the computer, locally or remotely.
    #[serde(default)]
    pub users: Vec<SignedInUser>,
    /// Installed applications and packages, by name.
    #[serde(default)]
    pub applications: Vec<Application>,
    /// What could not be collected, such as `applications (rpm): …`.
    #[serde(default)]
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingSystem {
    /// `windows`, `macos`, or the Linux distribution's os-release `ID`, such
    /// as `ubuntu` or `almalinux`.
    pub id: String,
    /// The os-release `ID_LIKE` of a Linux distribution, such as `debian`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub id_like: Vec<String>,
    /// Such as `Windows 11 Pro`, `macOS` or `Ubuntu`.
    pub name: String,
    /// Such as `24H2`, `15.6.1` or `24.04.3 LTS (Noble Numbat)`.
    pub version: String,
    /// Such as `26100.4652` on Windows or `24G90` on macOS.
    #[serde(default)]
    pub build: Option<String>,
    /// Such as `10.0.26100`, `Darwin 24.6.0` or `6.8.0-79-generic`.
    #[serde(default)]
    pub kernel: Option<String>,
    /// `x86_64` or `arm64`, or what else the kernel names.
    pub architecture: String,
    #[serde(default)]
    pub installed_at_unix_ms: Option<i64>,
    #[serde(default)]
    pub boot_time_unix_ms: Option<i64>,
    /// An IANA name, such as `Europe/Berlin`, or on Windows its own, such as
    /// `W. Europe Standard Time`.
    #[serde(default)]
    pub time_zone: Option<String>,
    /// The update and package tools present, such as `windows_update`,
    /// `winget`, `softwareupdate`, `homebrew`, `apt`, `dnf` or `pacman`.
    #[serde(default)]
    pub package_managers: Vec<String>,
    /// The shells a remote terminal can run here.
    #[serde(default)]
    pub terminal_shells: Vec<crate::TerminalShell>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub serial_number: Option<String>,
    /// The BIOS, UEFI or system firmware version.
    #[serde(default)]
    pub firmware_version: Option<String>,
    #[serde(default)]
    pub processor: Option<Processor>,
    /// Installed physical memory.
    pub memory_bytes: u64,
    /// Physical disks, as opposed to the volumes on them.
    #[serde(default)]
    pub disks: Vec<PhysicalDisk>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Processor {
    pub name: String,
    /// Physical cores of every processor package together.
    pub cores: u32,
    /// Logical processors.
    pub threads: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalDisk {
    /// How the system names it, such as `PhysicalDrive0`, `disk0` or `nvme0n1`.
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    pub size_bytes: u64,
    #[serde(default)]
    pub media: Option<DiskMedia>,
    /// The bus it is attached by, such as `NVMe`, `SATA`, `USB` or `Virtual`.
    #[serde(default)]
    pub interface: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskMedia {
    Ssd,
    Hdd,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    pub hostname: String,
    #[serde(default)]
    pub fqdn: Option<String>,
    /// The Active Directory or DNS domain the computer has joined.
    #[serde(default)]
    pub domain: Option<String>,
    /// The Windows workgroup of a computer that joined no domain.
    #[serde(default)]
    pub workgroup: Option<String>,
    #[serde(default)]
    pub adapters: Vec<NetworkAdapter>,
    /// The resolvers the whole system uses, where they aren't per adapter.
    #[serde(default)]
    pub dns_servers: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAdapter {
    /// Such as `Ethernet`, `en0` or `enp3s0`.
    pub name: String,
    /// The hardware's name, such as `Intel(R) Ethernet Connection I219-V`.
    #[serde(default)]
    pub description: Option<String>,
    /// Upper case and colon separated, such as `00:15:5D:01:02:03`.
    #[serde(default)]
    pub mac_address: Option<String>,
    pub kind: AdapterKind,
    /// The link is up.
    pub up: bool,
    #[serde(default)]
    pub speed_bits_per_second: Option<u64>,
    #[serde(default)]
    pub addresses: Vec<AdapterAddress>,
    #[serde(default)]
    pub gateways: Vec<String>,
    #[serde(default)]
    pub dns_servers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Ethernet,
    Wireless,
    /// Software adapters: bridges, tunnels, VPNs and virtual switches.
    Virtual,
    #[default]
    #[serde(other)]
    Other,
}

/// An IP address on an adapter, and its subnet as a prefix length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterAddress {
    pub address: String,
    pub prefix_length: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedInUser {
    /// Such as `DOMAIN\name` on Windows, or the account name.
    pub name: String,
    /// Such as `Console`, `RDP-Tcp#2`, `seat0` or `pts/1`.
    #[serde(default)]
    pub session: Option<String>,
    /// Where a remote user is connected from.
    #[serde(default)]
    pub remote_host: Option<String>,
    #[serde(default)]
    pub since_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Application {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
    /// Such as `x86_64`, `x64` or `noarch`, where the source records it.
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub installed_at_unix_ms: Option<i64>,
    pub source: ApplicationSource,
}

/// Where the Agent found an application, which is also what installs and
/// updates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationSource {
    /// A Windows program installed for every user.
    Windows,
    /// A Windows program installed for one user.
    WindowsUser,
    /// A macOS application bundle.
    MacosApp,
    Dpkg,
    Rpm,
    Pacman,
    Flatpak,
    Snap,
}

impl ApplicationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::WindowsUser => "windows_user",
            Self::MacosApp => "macos_app",
            Self::Dpkg => "dpkg",
            Self::Rpm => "rpm",
            Self::Pacman => "pacman",
            Self::Flatpak => "flatpak",
            Self::Snap => "snap",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "windows" => Self::Windows,
            "windows_user" => Self::WindowsUser,
            "macos_app" => Self::MacosApp,
            "dpkg" => Self::Dpkg,
            "rpm" => Self::Rpm,
            "pacman" => Self::Pacman,
            "flatpak" => Self::Flatpak,
            "snap" => Self::Snap,
            _ => return None,
        })
    }
}

/// Text as a snapshot may carry it: without control characters or
/// surrounding space, and at most [`MAX_SYSTEM_INFO_TEXT_CHARS`] long.
pub fn clean_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_SYSTEM_INFO_TEXT_CHARS)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// [`clean_text`], or `None` when nothing is left.
pub fn clean_optional(text: Option<&str>) -> Option<String> {
    text.map(clean_text).filter(|text| !text.is_empty())
}

fn clean_field(field: &mut String) {
    *field = clean_text(field);
}

fn clean_option(field: &mut Option<String>) {
    *field = clean_optional(field.as_deref());
}

fn clean_list(list: &mut Vec<String>) {
    for item in list.iter_mut() {
        clean_field(item);
    }
    list.retain(|item| !item.is_empty());
    list.dedup();
    list.truncate(MAX_SYSTEM_INFO_ITEMS);
}

/// The address in its canonical form, or `None` if it is not one.
fn clean_ip(text: &str) -> Option<String> {
    // Windows and Linux can name an IPv6 address's zone, as in `fe80::1%12`.
    let address = text.trim().split('%').next()?;
    address.parse::<IpAddr>().ok().map(|ip| ip.to_string())
}

fn clean_ips(list: &mut Vec<String>) {
    *list = list.iter().filter_map(|text| clean_ip(text)).collect();
    list.dedup();
    list.truncate(MAX_SYSTEM_INFO_ITEMS);
}

/// A MAC address as `AA:BB:CC:DD:EE:FF`, or `None` for anything else,
/// including the all-zero address of adapters without one.
pub fn clean_mac(text: &str) -> Option<String> {
    let digits: Vec<u8> = text
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.'))
        .map(|c| c.to_digit(16).map(|digit| digit as u8))
        .collect::<Option<_>>()?;
    if digits.len() != 12 || digits.iter().all(|digit| *digit == 0) {
        return None;
    }
    Some(
        digits
            .chunks(2)
            .map(|pair| format!("{:X}{:X}", pair[0], pair[1]))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

impl SystemInfo {
    /// The snapshot with every text and list bounded, impossible values
    /// dropped and applications in a stable order, so whatever a peer sent
    /// is safe to store and show.
    pub fn sanitized(mut self) -> Self {
        let os = &mut self.os;
        clean_field(&mut os.id);
        clean_list(&mut os.id_like);
        clean_field(&mut os.name);
        clean_field(&mut os.version);
        clean_option(&mut os.build);
        clean_option(&mut os.kernel);
        clean_field(&mut os.architecture);
        clean_option(&mut os.time_zone);
        clean_list(&mut os.package_managers);
        let mut shells = Vec::new();
        for shell in os.terminal_shells.drain(..) {
            if !shells.contains(&shell) {
                shells.push(shell);
            }
        }
        os.terminal_shells = shells;

        let hardware = &mut self.hardware;
        clean_option(&mut hardware.manufacturer);
        clean_option(&mut hardware.model);
        clean_option(&mut hardware.serial_number);
        clean_option(&mut hardware.firmware_version);
        if let Some(processor) = &mut hardware.processor {
            clean_field(&mut processor.name);
        }
        for disk in &mut hardware.disks {
            clean_field(&mut disk.name);
            clean_option(&mut disk.model);
            clean_option(&mut disk.interface);
        }
        hardware.disks.retain(|disk| !disk.name.is_empty());
        hardware.disks.truncate(MAX_SYSTEM_INFO_ITEMS);

        let network = &mut self.network;
        clean_field(&mut network.hostname);
        clean_option(&mut network.fqdn);
        clean_option(&mut network.domain);
        clean_option(&mut network.workgroup);
        clean_ips(&mut network.dns_servers);
        for adapter in &mut network.adapters {
            clean_field(&mut adapter.name);
            clean_option(&mut adapter.description);
            adapter.mac_address = adapter.mac_address.as_deref().and_then(clean_mac);
            adapter.addresses = adapter
                .addresses
                .iter()
                .filter_map(|entry| {
                    let address = clean_ip(&entry.address)?;
                    let limit = if address.contains(':') { 128 } else { 32 };
                    (entry.prefix_length <= limit).then_some(AdapterAddress {
                        address,
                        prefix_length: entry.prefix_length,
                    })
                })
                .collect();
            adapter.addresses.truncate(MAX_SYSTEM_INFO_ITEMS);
            clean_ips(&mut adapter.gateways);
            clean_ips(&mut adapter.dns_servers);
        }
        network.adapters.retain(|adapter| !adapter.name.is_empty());
        network.adapters.truncate(MAX_SYSTEM_INFO_ITEMS);

        for user in &mut self.users {
            clean_field(&mut user.name);
            clean_option(&mut user.session);
            clean_option(&mut user.remote_host);
        }
        self.users.retain(|user| !user.name.is_empty());
        self.users.truncate(MAX_SYSTEM_INFO_ITEMS);

        for application in &mut self.applications {
            clean_field(&mut application.name);
            clean_option(&mut application.version);
            clean_option(&mut application.publisher);
            clean_option(&mut application.architecture);
        }
        self.applications
            .retain(|application| !application.name.is_empty());
        self.applications.sort_by(|first, second| {
            first
                .name
                .to_lowercase()
                .cmp(&second.name.to_lowercase())
                .then_with(|| first.cmp(second))
        });
        self.applications.dedup();
        if self.applications.len() > MAX_APPLICATIONS {
            self.problems.push(format!(
                "applications: only the first {MAX_APPLICATIONS} of {} are listed",
                self.applications.len()
            ));
            self.applications.truncate(MAX_APPLICATIONS);
        }
        clean_list(&mut self.problems);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SystemInfo {
        SystemInfo {
            collected_at_unix_ms: 1_700_000_000_000,
            os: OperatingSystem {
                id: "ubuntu".into(),
                id_like: vec!["debian".into()],
                name: "Ubuntu".into(),
                version: "24.04.3 LTS (Noble Numbat)".into(),
                build: None,
                kernel: Some("6.8.0-79-generic".into()),
                architecture: "x86_64".into(),
                installed_at_unix_ms: Some(1_690_000_000_000),
                boot_time_unix_ms: Some(1_699_990_000_000),
                time_zone: Some("Europe/Berlin".into()),
                package_managers: vec!["apt".into(), "snap".into()],
                terminal_shells: vec![crate::TerminalShell::Login, crate::TerminalShell::Bash],
            },
            hardware: Hardware {
                manufacturer: Some("Microsoft Corporation".into()),
                model: Some("Virtual Machine".into()),
                serial_number: Some("1234-5678".into()),
                firmware_version: Some("Hyper-V UEFI Release v4.1".into()),
                processor: Some(Processor {
                    name: "AMD Ryzen 9 5950X 16-Core Processor".into(),
                    cores: 4,
                    threads: 8,
                }),
                memory_bytes: 8 << 30,
                disks: vec![PhysicalDisk {
                    name: "sda".into(),
                    model: Some("Virtual Disk".into()),
                    size_bytes: 128 << 30,
                    media: Some(DiskMedia::Ssd),
                    interface: Some("SCSI".into()),
                }],
            },
            network: Network {
                hostname: "ubuntu2404".into(),
                fqdn: None,
                domain: None,
                workgroup: None,
                adapters: vec![NetworkAdapter {
                    name: "eth0".into(),
                    description: None,
                    mac_address: Some("00:15:5D:01:02:03".into()),
                    kind: AdapterKind::Ethernet,
                    up: true,
                    speed_bits_per_second: Some(10_000_000_000),
                    addresses: vec![AdapterAddress {
                        address: "172.20.1.5".into(),
                        prefix_length: 20,
                    }],
                    gateways: vec!["172.20.0.1".into()],
                    dns_servers: vec![],
                }],
                dns_servers: vec!["172.20.0.1".into()],
            },
            users: vec![SignedInUser {
                name: "mesh".into(),
                session: Some("seat0".into()),
                remote_host: None,
                since_unix_ms: Some(1_699_990_100_000),
            }],
            applications: vec![Application {
                name: "openssl".into(),
                version: Some("3.0.13-0ubuntu3.5".into()),
                publisher: Some("Ubuntu Developers".into()),
                architecture: Some("amd64".into()),
                installed_at_unix_ms: None,
                source: ApplicationSource::Dpkg,
            }],
            problems: vec![],
        }
    }

    #[test]
    fn snapshots_round_trip_as_plain_json() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(json["applications"][0]["source"], "dpkg");
        assert_eq!(json["hardware"]["disks"][0]["media"], "ssd");
        assert_eq!(json["network"]["adapters"][0]["kind"], "ethernet");
        assert_eq!(
            serde_json::from_value::<SystemInfo>(json).unwrap(),
            sample()
        );
        let unknown: AdapterKind = serde_json::from_str(r#""infiniband""#).unwrap();
        assert_eq!(unknown, AdapterKind::Other);
    }

    #[test]
    fn a_clean_snapshot_is_unchanged() {
        assert_eq!(sample().sanitized(), sample());
    }

    #[test]
    fn sanitizing_bounds_and_cleans_everything() {
        let mut wild = sample();
        wild.os.name = format!("  Ubuntu\n{}  ", "x".repeat(400));
        wild.hardware.serial_number = Some(" \t ".into());
        wild.network.adapters[0].mac_address = Some("00-00-00-00-00-00".into());
        wild.network.adapters[0].addresses = vec![
            AdapterAddress {
                address: "fe80::1%12".into(),
                prefix_length: 64,
            },
            AdapterAddress {
                address: "10.0.0.1".into(),
                prefix_length: 33,
            },
            AdapterAddress {
                address: "not an address".into(),
                prefix_length: 8,
            },
        ];
        wild.network.dns_servers = vec!["1.1.1.1".into(), "1.1.1.1".into(), "bad".into()];
        wild.applications = (0..MAX_APPLICATIONS + 5)
            .rev()
            .map(|index| Application {
                name: format!("app {index:05}"),
                version: Some(String::new()),
                publisher: None,
                architecture: None,
                installed_at_unix_ms: None,
                source: ApplicationSource::Rpm,
            })
            .chain([Application {
                name: "\u{7}".into(),
                version: None,
                publisher: None,
                architecture: None,
                installed_at_unix_ms: None,
                source: ApplicationSource::Rpm,
            }])
            .collect();
        let clean = wild.sanitized();
        assert!(clean.os.name.starts_with("Ubuntu"));
        assert_eq!(clean.os.name.chars().count(), MAX_SYSTEM_INFO_TEXT_CHARS);
        assert_eq!(clean.hardware.serial_number, None);
        let adapter = &clean.network.adapters[0];
        assert_eq!(adapter.mac_address, None);
        assert_eq!(
            adapter.addresses,
            [AdapterAddress {
                address: "fe80::1".into(),
                prefix_length: 64
            }]
        );
        assert_eq!(clean.network.dns_servers, ["1.1.1.1"]);
        assert_eq!(clean.applications.len(), MAX_APPLICATIONS);
        assert_eq!(clean.applications[0].name, "app 00000");
        assert_eq!(clean.applications[0].version, None);
        assert_eq!(clean.problems.len(), 1, "{:?}", clean.problems);
    }

    #[test]
    fn mac_addresses_are_normalized() {
        assert_eq!(
            clean_mac("00-15-5d-0a-0B-0c").as_deref(),
            Some("00:15:5D:0A:0B:0C")
        );
        assert_eq!(
            clean_mac("0015.5d0a.0b0c").as_deref(),
            Some("00:15:5D:0A:0B:0C")
        );
        assert_eq!(clean_mac("00:15:5d"), None);
        assert_eq!(clean_mac("zz:15:5d:0a:0b:0c"), None);
    }

    #[test]
    fn sources_round_trip() {
        for source in [
            ApplicationSource::Windows,
            ApplicationSource::WindowsUser,
            ApplicationSource::MacosApp,
            ApplicationSource::Dpkg,
            ApplicationSource::Rpm,
            ApplicationSource::Pacman,
            ApplicationSource::Flatpak,
            ApplicationSource::Snap,
        ] {
            assert_eq!(ApplicationSource::parse(source.as_str()), Some(source));
            assert_eq!(
                serde_json::to_value(source).unwrap(),
                serde_json::Value::String(source.as_str().into())
            );
        }
    }
}
