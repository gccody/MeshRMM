//! System information from `/etc/os-release`, `/sys`, `/proc`, logind and
//! the package databases.
mod packages;

use std::collections::{BTreeSet, HashMap};
use std::ffi::CStr;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use anyhow::Context;
use meshrmm_protocol::{
    AdapterKind, DiskMedia, Hardware, Network, NetworkAdapter, OperatingSystem, PhysicalDisk,
    Processor, SignedInUser, SystemInfo,
};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::{Problems, unix, unix_ms};

pub(super) use super::unix::local_ipv4_addresses;

/// The package and update tools a distribution may have, as each is named.
const PACKAGE_MANAGERS: &[(&str, &str)] = &[
    ("apt-get", "apt"),
    ("dnf", "dnf"),
    ("yum", "yum"),
    ("zypper", "zypper"),
    ("pacman", "pacman"),
    ("flatpak", "flatpak"),
    ("snap", "snap"),
];

/// Values firmware vendors leave in DMI fields they never filled in.
const DMI_PLACEHOLDERS: &[&str] = &[
    "to be filled by o.e.m.",
    "to be filled by oem",
    "default string",
    "system serial number",
    "system product name",
    "system manufacturer",
    "system version",
    "not specified",
    "not applicable",
    "not available",
    "none",
    "n/a",
    "o.e.m.",
    "oem",
    "0",
    "00000000",
    "123456789",
    "0123456789",
];

/// Block devices that are not physical disks: loop and RAM disks, device
/// mapper and software RAID volumes, optical and floppy drives, network
/// block devices and ZFS volumes.
const VIRTUAL_BLOCK_PREFIXES: &[&str] = &[
    "loop", "ram", "zram", "dm-", "md", "sr", "fd", "nbd", "zd", "rbd",
];

/// `RTF_GATEWAY` in a route's flags.
const ROUTE_GATEWAY: u32 = 0x2;

pub(super) fn collect(problems: &mut Problems) -> SystemInfo {
    SystemInfo {
        os: operating_system(problems),
        hardware: hardware(problems),
        network: network(problems),
        users: users(),
        applications: packages::applications(problems),
        ..Default::default()
    }
}

fn operating_system(problems: &mut Problems) -> OperatingSystem {
    let release = problems
        .take(
            "operating system",
            std::fs::read_to_string("/etc/os-release")
                .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
                .context("os-release"),
        )
        .map(|text| parse_os_release(&text))
        .unwrap_or_default();
    let (kernel, machine) = problems.take("kernel", uname()).unzip();
    let field = |name: &str| release.get(name).filter(|value| !value.is_empty()).cloned();
    let mut managers: Vec<String> = PACKAGE_MANAGERS
        .iter()
        .filter(|(tool, _)| unix::find_tool(tool).is_some())
        .map(|(_, name)| (*name).to_owned())
        .collect();
    // dnf's distributions keep yum as an alias for it.
    if managers.iter().any(|name| name == "dnf") {
        managers.retain(|name| name != "yum");
    }
    OperatingSystem {
        id: field("ID").unwrap_or_else(|| "linux".to_owned()),
        id_like: field("ID_LIKE")
            .map(|like| like.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        name: field("NAME").unwrap_or_else(|| "Linux".to_owned()),
        version: field("VERSION")
            .or_else(|| field("VERSION_ID"))
            .or_else(|| field("BUILD_ID"))
            .unwrap_or_else(|| "rolling".to_owned()),
        build: None,
        kernel,
        architecture: machine.as_deref().map(architecture).unwrap_or_default(),
        // The root file system's birth time, where it records one.
        installed_at_unix_ms: std::fs::metadata("/")
            .and_then(|metadata| metadata.created())
            .ok()
            .and_then(unix_ms),
        boot_time_unix_ms: problems.take("boot time", boot_time()),
        time_zone: problems.take(
            "time zone",
            unix::time_zone().or_else(|error| {
                std::fs::read_to_string("/etc/timezone")
                    .map(|zone| zone.trim().to_owned())
                    .ok()
                    .filter(|zone| !zone.is_empty())
                    .ok_or(error)
            }),
        ),
        package_managers: managers,
        // Filled in by the shared collector.
        terminal_shells: Vec::new(),
    }
}

/// The variables of an os-release file, unquoted.
fn parse_os_release(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (name, value) = line.split_once('=')?;
            Some((name.trim().to_owned(), unquote(value.trim())))
        })
        .collect()
}

/// A shell-style value: quoted with `"` or `'`, with backslash escapes in
/// double quotes.
fn unquote(value: &str) -> String {
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return inner.to_owned();
    }
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_owned();
    };
    let mut text = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(c) = characters.next() {
        if c == '\\' {
            if let Some(escaped) = characters.next() {
                text.push(escaped);
            }
        } else {
            text.push(c);
        }
    }
    text
}

/// The kernel release and machine name.
fn uname() -> anyhow::Result<(String, String)> {
    // SAFETY: the structure is plain characters, for which zero is valid.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a writable utsname.
    if unsafe { libc::uname(&mut name) } != 0 {
        return Err(std::io::Error::last_os_error()).context("uname");
    }
    let text = |field: &[libc::c_char]| {
        // SAFETY: uname NUL-terminates every field.
        unsafe { CStr::from_ptr(field.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    Ok((text(&name.release), text(&name.machine)))
}

/// `x86_64` and `arm64` as the other platforms name them.
fn architecture(machine: &str) -> String {
    match machine {
        "aarch64" | "arm64" => "arm64".to_owned(),
        "amd64" => "x86_64".to_owned(),
        other => other.to_owned(),
    }
}

fn boot_time() -> anyhow::Result<i64> {
    let stat = std::fs::read_to_string("/proc/stat").context("/proc/stat")?;
    parse_boot_time(&stat).context("/proc/stat has no btime")
}

fn parse_boot_time(stat: &str) -> Option<i64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|seconds| seconds.trim().parse::<i64>().ok())
        .map(|seconds| seconds * 1000)
}

fn hardware(problems: &mut Problems) -> Hardware {
    let dmi = |name: &str| read_dmi(name).ok().flatten();
    let tree = |name: &str| {
        std::fs::read(Path::new("/proc/device-tree").join(name))
            .ok()
            .map(|bytes| {
                String::from_utf8_lossy(&bytes)
                    .trim_matches('\0')
                    .trim()
                    .to_owned()
            })
            .and_then(|value| meaningful(&value))
    };
    let serial_number = match read_dmi("product_serial") {
        Ok(serial) => serial,
        Err(error) => {
            problems.note("serial number", &error);
            None
        }
    }
    .or_else(|| tree("serial-number"));
    let firmware_version = firmware(
        dmi("bios_vendor").as_deref(),
        dmi("bios_version").as_deref(),
        dmi("bios_date").as_deref(),
    );
    let memory_bytes = problems.take("memory", memory_bytes()).unwrap_or(0);
    Hardware {
        manufacturer: dmi("sys_vendor").or_else(|| dmi("board_vendor")),
        model: dmi("product_name")
            .or_else(|| dmi("board_name"))
            .or_else(|| tree("model")),
        serial_number,
        firmware_version,
        processor: problems.take("processor", processor()),
        memory_bytes,
        disks: problems.take("disks", disks()).unwrap_or_default(),
    }
}

/// A DMI field, or `None` when the computer has no DMI or left the field
/// empty. Fails when the field exists but can't be read, as the serial
/// number can't but by root.
fn read_dmi(name: &str) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(Path::new("/sys/class/dmi/id").join(name)) {
        Ok(value) => Ok(meaningful(&value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("/sys/class/dmi/id/{name}")),
    }
}

/// The value, unless it is empty or a firmware placeholder.
fn meaningful(value: &str) -> Option<String> {
    let value = value.trim();
    let lower = value.to_lowercase();
    (!value.is_empty() && !DMI_PLACEHOLDERS.contains(&lower.as_str())).then(|| value.to_owned())
}

/// `vendor version (date)`, from whichever parts the firmware gives.
fn firmware(vendor: Option<&str>, version: Option<&str>, date: Option<&str>) -> Option<String> {
    let name = [vendor, version]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    match (name.is_empty(), date) {
        (true, _) => None,
        (false, Some(date)) => Some(format!("{name} ({date})")),
        (false, None) => Some(name),
    }
}

fn memory_bytes() -> anyhow::Result<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").context("/proc/meminfo")?;
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|value| value.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kibibytes| kibibytes * 1024)
        .context("/proc/meminfo has no MemTotal")
}

fn processor() -> anyhow::Result<Processor> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").context("/proc/cpuinfo")?;
    let mut cores = BTreeSet::new();
    let mut threads = 0u32;
    for entry in std::fs::read_dir("/sys/devices/system/cpu").context("/sys/devices/system/cpu")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(index) = name.to_str().and_then(|name| name.strip_prefix("cpu")) else {
            continue;
        };
        if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let path = entry.path();
        // CPUs that can't be taken offline, such as cpu0, have no `online`.
        if read_trimmed(&path.join("online")).is_some_and(|online| online == "0") {
            continue;
        }
        threads += 1;
        let topology = path.join("topology");
        let core = (
            read_trimmed(&topology.join("physical_package_id")),
            read_trimmed(&topology.join("core_id")).unwrap_or_else(|| index.to_owned()),
        );
        cores.insert(core);
    }
    anyhow::ensure!(threads > 0, "no processors are online");
    Ok(Processor {
        name: processor_name(&cpuinfo).unwrap_or_else(|| {
            uname()
                .map(|(_, machine)| machine)
                .unwrap_or_else(|_| "Unknown".to_owned())
        }),
        cores: (cores.len() as u32).clamp(1, threads),
        threads,
    })
}

/// The processor's name: `model name` on x86, and what other architectures
/// call it.
fn processor_name(cpuinfo: &str) -> Option<String> {
    ["model name", "Processor", "cpu model", "cpu"]
        .iter()
        .find_map(|field| {
            cpuinfo.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name.trim() == *field)
                    .then(|| value.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|value| !value.is_empty())
            })
        })
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

fn disks() -> anyhow::Result<Vec<PhysicalDisk>> {
    let mut disks = Vec::new();
    for entry in std::fs::read_dir("/sys/block").context("/sys/block")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if VIRTUAL_BLOCK_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            continue;
        }
        let block = entry.path();
        let size_bytes = read_trimmed(&block.join("size"))
            .and_then(|sectors| sectors.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_mul(512);
        if size_bytes == 0 {
            continue;
        }
        let device = block.join("device");
        let model = read_trimmed(&device.join("model")).filter(|model| !model.is_empty());
        let vendor = read_trimmed(&device.join("vendor"))
            .filter(|vendor| !vendor.is_empty() && vendor != "ATA");
        let model = match (vendor, model) {
            (Some(vendor), Some(model)) if !model.starts_with(&vendor) => {
                Some(format!("{vendor} {model}"))
            }
            (_, Some(model)) => Some(model),
            (vendor, None) => vendor,
        };
        let media = match read_trimmed(&block.join("queue/rotational")).as_deref() {
            Some("1") => Some(DiskMedia::Hdd),
            Some("0") => Some(DiskMedia::Ssd),
            _ => None,
        };
        let resolved = std::fs::canonicalize(&block).unwrap_or(block);
        disks.push(PhysicalDisk {
            interface: disk_interface(&name, &resolved.to_string_lossy()).map(str::to_owned),
            name,
            model,
            size_bytes,
            media,
        });
    }
    disks.sort_by(|first, second| first.name.cmp(&second.name));
    Ok(disks)
}

/// The bus a disk is attached by, from its name and its device path.
fn disk_interface(name: &str, path: &str) -> Option<&'static str> {
    let path = path.to_lowercase();
    Some(if name.starts_with("nvme") {
        "NVMe"
    } else if name.starts_with("mmcblk") {
        "MMC"
    } else if path.contains("/usb") {
        "USB"
    } else if path.contains("/virtio") {
        "Virtio"
    } else if path.contains("vmbus") || path.contains("/hv_") || name.starts_with("xvd") {
        "Virtual"
    } else if path.contains("/ata") {
        "SATA"
    } else if path.contains("/host") || path.contains("scsi") {
        "SCSI"
    } else {
        return None;
    })
}

fn network(problems: &mut Problems) -> Network {
    let hostname = problems
        .take("host name", unix::hostname())
        .unwrap_or_default();
    let mut gateways = HashMap::<String, Vec<String>>::new();
    if let Some(table) = problems.take(
        "IPv4 routes",
        std::fs::read_to_string("/proc/net/route").context("/proc/net/route"),
    ) {
        for (interface, gateway) in ipv4_gateways(&table) {
            gateways
                .entry(interface)
                .or_default()
                .push(gateway.to_string());
        }
    }
    // A kernel without IPv6 has no table.
    if let Ok(table) = std::fs::read_to_string("/proc/net/ipv6_route") {
        for (interface, gateway) in ipv6_gateways(&table) {
            gateways
                .entry(interface)
                .or_default()
                .push(gateway.to_string());
        }
    }
    let mut adapters: Vec<NetworkAdapter> = problems
        .take("network adapters", unix::interfaces())
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, interface)| !interface.loopback)
        .map(|(name, interface)| {
            let class = Path::new("/sys/class/net").join(&name);
            let kind = if class.join("wireless").exists() || class.join("phy80211").exists() {
                AdapterKind::Wireless
            } else if class.join("device").exists() {
                AdapterKind::Ethernet
            } else {
                AdapterKind::Virtual
            };
            let up = match read_trimmed(&class.join("operstate")).as_deref() {
                Some("up") => true,
                Some("unknown") | None => interface.up,
                Some(_) => false,
            };
            let speed_bits_per_second = read_trimmed(&class.join("speed"))
                .and_then(|speed| speed.parse::<i64>().ok())
                .filter(|megabits| *megabits > 0)
                .map(|megabits| megabits as u64 * 1_000_000);
            let description = std::fs::read_link(class.join("device/driver"))
                .ok()
                .and_then(|driver| Some(driver.file_name()?.to_string_lossy().into_owned()));
            NetworkAdapter {
                gateways: gateways.remove(&name).unwrap_or_default(),
                name,
                description,
                mac_address: interface.mac_address,
                kind,
                up,
                speed_bits_per_second,
                addresses: interface.addresses,
                dns_servers: Vec::new(),
            }
        })
        .collect();
    // Hardware first, so a host with many container links keeps its own
    // adapters when the list is cut short.
    adapters.sort_by_key(|adapter| {
        (
            !matches!(adapter.kind, AdapterKind::Ethernet | AdapterKind::Wireless),
            adapter.name.clone(),
        )
    });
    Network {
        fqdn: hostname.contains('.').then(|| hostname.clone()),
        hostname,
        domain: None,
        workgroup: None,
        adapters,
        dns_servers: problems
            .take("DNS servers", dns_servers())
            .unwrap_or_default(),
    }
}

/// The interface and gateway of each IPv4 default route in `/proc/net/route`,
/// whose addresses are the kernel's in-memory network-order words in hex.
fn ipv4_gateways(table: &str) -> Vec<(String, Ipv4Addr)> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (interface, destination, gateway, flags) = (
                fields.first()?,
                fields.get(1)?,
                fields.get(2)?,
                fields.get(3)?,
            );
            let flags = u32::from_str_radix(flags, 16).ok()?;
            let mask = fields.get(7).copied().unwrap_or("00000000");
            if *destination != "00000000" || mask != "00000000" || flags & ROUTE_GATEWAY == 0 {
                return None;
            }
            let gateway = Ipv4Addr::from(u32::from_str_radix(gateway, 16).ok()?.to_ne_bytes());
            (!gateway.is_unspecified()).then(|| ((*interface).to_owned(), gateway))
        })
        .collect()
}

/// The interface and next hop of each IPv6 default route in
/// `/proc/net/ipv6_route`.
fn ipv6_gateways(table: &str) -> Vec<(String, Ipv6Addr)> {
    table
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[0] != "0".repeat(32) || fields[1] != "00" {
                return None;
            }
            let flags = u32::from_str_radix(fields[8], 16).ok()?;
            let hop = Ipv6Addr::from(u128::from_str_radix(fields[4], 16).ok()?);
            (flags & ROUTE_GATEWAY != 0 && !hop.is_unspecified() && fields[9] != "lo")
                .then(|| (fields[9].to_owned(), hop))
        })
        .collect()
}

/// The system's resolvers. Behind systemd-resolved's local stub, the
/// upstream servers it forwards to.
fn dns_servers() -> anyhow::Result<Vec<String>> {
    let servers = unix::resolv_conf_servers("/etc/resolv.conf")?;
    if !servers.is_empty()
        && servers.iter().all(|server| server.starts_with("127.0.0.5"))
        && let Ok(upstream) = unix::resolv_conf_servers("/run/systemd/resolve/resolv.conf")
    {
        return Ok(upstream);
    }
    Ok(servers)
}

/// The users signed in, from logind, or from utmpx where logind isn't
/// running.
fn users() -> Vec<SignedInUser> {
    match logind_users() {
        Ok(users) => users,
        Err(error) => {
            tracing::debug!(
                error = format!("{error:#}"),
                "listing users through utmpx instead of logind"
            );
            unix::utmpx_users()
        }
    }
}

fn logind_users() -> anyhow::Result<Vec<SignedInUser>> {
    const LOGIN1: &str = "org.freedesktop.login1";
    // A hung logind mustn't hold up the snapshot.
    let bus = zbus::blocking::connection::Builder::system()
        .and_then(|builder| {
            builder
                .method_timeout(std::time::Duration::from_secs(10))
                .build()
        })
        .context("the system D-Bus")?;
    let listed: Vec<(String, u32, String, String, OwnedObjectPath)> = bus
        .call_method(
            Some(LOGIN1),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "ListSessions",
            &(),
        )
        .context("logind could not list the sessions")?
        .body()
        .deserialize()?;
    let mut users = Vec::new();
    for (_, _, user, seat, path) in listed {
        let Ok(reply) = bus.call_method(
            Some(LOGIN1),
            path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("org.freedesktop.login1.Session",),
        ) else {
            continue;
        };
        let Ok(properties) = reply.body().deserialize::<HashMap<String, OwnedValue>>() else {
            continue;
        };
        let text = |name: &str| {
            properties
                .get(name)
                .and_then(|value| String::try_from(value.try_clone().ok()?).ok())
                .unwrap_or_default()
        };
        // Login screens, and systemd's own per-user managers, are not users
        // signing in.
        if !text("Class").starts_with("user") || text("State") == "closing" {
            continue;
        }
        let remote = properties
            .get("Remote")
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false);
        let since = properties
            .get("Timestamp")
            .and_then(|value| u64::try_from(value).ok())
            .filter(|micros| *micros > 0)
            .map(|micros| (micros / 1000) as i64);
        // SSH sessions have no terminal or seat; logind names the service.
        let service = match text("Service").as_str() {
            "sshd" => "ssh".to_owned(),
            _ => String::new(),
        };
        let session = [text("TTY"), seat, service, text("Type")]
            .into_iter()
            .find(|label| !label.is_empty());
        users.push(SignedInUser {
            name: user,
            session,
            remote_host: remote
                .then(|| text("RemoteHost"))
                .filter(|host| !host.is_empty()),
            since_unix_ms: since,
        });
    }
    Ok(users)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_values_are_unquoted() {
        let ubuntu = parse_os_release(
            "PRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nVERSION=\"24.04.3 LTS (Noble Numbat)\"\nID=ubuntu\nID_LIKE=debian\n# comment=x\n",
        );
        assert_eq!(ubuntu["NAME"], "Ubuntu");
        assert_eq!(ubuntu["VERSION"], "24.04.3 LTS (Noble Numbat)");
        assert_eq!(ubuntu["ID_LIKE"], "debian");
        assert!(!ubuntu.contains_key("# comment"));
        let alma = parse_os_release(
            "NAME='AlmaLinux'\nID=\"almalinux\"\nID_LIKE=\"rhel centos fedora\"\nPRETTY_NAME=\"Alma \\\"Seafoam\\\"\"\n",
        );
        assert_eq!(alma["NAME"], "AlmaLinux");
        assert_eq!(alma["ID_LIKE"], "rhel centos fedora");
        assert_eq!(alma["PRETTY_NAME"], "Alma \"Seafoam\"");
    }

    #[test]
    fn architectures_use_the_shared_names() {
        assert_eq!(architecture("aarch64"), "arm64");
        assert_eq!(architecture("x86_64"), "x86_64");
        assert_eq!(architecture("riscv64"), "riscv64");
    }

    #[test]
    fn boot_time_comes_from_btime() {
        assert_eq!(
            parse_boot_time("cpu  1 2 3\nbtime 1700000000\nprocesses 5\n"),
            Some(1_700_000_000_000)
        );
        assert_eq!(parse_boot_time("cpu  1 2 3\n"), None);
    }

    #[test]
    fn firmware_placeholders_are_dropped() {
        assert_eq!(meaningful("To be filled by O.E.M.\n"), None);
        assert_eq!(meaningful(" Default string "), None);
        assert_eq!(meaningful("0"), None);
        assert_eq!(meaningful(""), None);
        assert_eq!(
            meaningful("Virtual Machine\n").as_deref(),
            Some("Virtual Machine")
        );
        assert_eq!(
            firmware(
                Some("Microsoft Corporation"),
                Some("Hyper-V UEFI Release v4.1"),
                Some("04/06/2022")
            )
            .as_deref(),
            Some("Microsoft Corporation Hyper-V UEFI Release v4.1 (04/06/2022)")
        );
        assert_eq!(firmware(None, Some("1.2"), None).as_deref(), Some("1.2"));
        assert_eq!(firmware(None, None, Some("01/01/2020")), None);
    }

    #[test]
    fn processor_names_cover_each_architecture() {
        let x86 = "processor\t: 0\nvendor_id\t: AuthenticAMD\nmodel name\t: AMD Ryzen 9 5950X 16-Core Processor\n";
        assert_eq!(
            processor_name(x86).as_deref(),
            Some("AMD Ryzen 9 5950X 16-Core Processor")
        );
        let arm = "Processor\t: ARMv7 Processor rev 4 (v7l)\nprocessor\t: 0\nBogoMIPS\t: 38.40\n";
        assert_eq!(
            processor_name(arm).as_deref(),
            Some("ARMv7 Processor rev 4 (v7l)")
        );
        assert_eq!(processor_name("processor\t: 0\nBogoMIPS\t: 50.00\n"), None);
    }

    #[test]
    fn disk_buses_come_from_the_device_path() {
        let interface = |name, path| disk_interface(name, path);
        assert_eq!(
            interface(
                "nvme0n1",
                "/sys/devices/pci0000:00/0000:00:01.1/0000:01:00.0/nvme/nvme0/nvme0n1"
            ),
            Some("NVMe")
        );
        assert_eq!(
            interface(
                "sda",
                "/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda"
            ),
            Some("SATA")
        );
        assert_eq!(
            interface(
                "sdb",
                "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host3/target3:0:0/3:0:0:0/block/sdb"
            ),
            Some("USB")
        );
        assert_eq!(
            interface(
                "sda",
                "/sys/devices/LNXSYSTM:00/LNXSYBUS:00/ACPI0004:00/VMBUS:00/f8b3781a-1e82-4818-a1c3-63d806ec15bb/host0/target0:0:0/0:0:0:0/block/sda"
            ),
            Some("Virtual")
        );
        assert_eq!(
            interface(
                "vda",
                "/sys/devices/pci0000:00/0000:00:04.0/virtio2/block/vda"
            ),
            Some("Virtio")
        );
        assert_eq!(
            interface(
                "mmcblk0",
                "/sys/devices/platform/emmc2bus/fe340000.mmc/mmc_host/mmc0/mmc0:0001/block/mmcblk0"
            ),
            Some("MMC")
        );
        assert_eq!(
            interface(
                "sdc",
                "/sys/devices/platform/host2/target2:0:0/2:0:0:0/block/sdc"
            ),
            Some("SCSI")
        );
    }

    #[test]
    fn default_routes_name_their_gateways() {
        let ipv4 = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            eth0\t00000000\t0100A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            eth0\t0000A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
            wlan0\t00000000\t00000000\t0001\t0\t0\t600\t00000000\t0\t0\t0\n";
        // The table holds network-order words as this computer reads them.
        let expected = Ipv4Addr::from(u32::from_str_radix("0100A8C0", 16).unwrap().to_ne_bytes());
        assert_eq!(ipv4_gateways(ipv4), [("eth0".to_owned(), expected)]);
        if cfg!(target_endian = "little") {
            assert_eq!(expected, Ipv4Addr::new(192, 168, 0, 1));
        }
        let ipv6 = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe8000000000000002155dfffe010203 00000400 00000001 00000000 00000003     eth0\n\
            fe800000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001     eth0\n\
            00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo\n";
        assert_eq!(
            ipv6_gateways(ipv6),
            [(
                "eth0".to_owned(),
                "fe80::215:5dff:fe01:203".parse().unwrap()
            )]
        );
    }

    #[test]
    fn real_readings_are_plausible() {
        let mut problems = Problems::default();
        let os = operating_system(&mut problems);
        assert!(!os.id.is_empty() && !os.version.is_empty(), "{os:?}");
        assert!(os.boot_time_unix_ms.is_some(), "{os:?}");
        let processor = processor().unwrap();
        assert!(
            processor.cores >= 1 && processor.threads >= processor.cores,
            "{processor:?}"
        );
        assert!(memory_bytes().unwrap() > 0);
        disks().unwrap();
        let network = network(&mut problems);
        assert!(!network.adapters.is_empty(), "{network:?}");
        println!(
            "{os:#?}\n{processor:#?}\n{network:#?}\n{:#?}\n{:?}",
            users(),
            problems.0
        );
    }
}
