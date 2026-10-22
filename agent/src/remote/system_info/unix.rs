//! What macOS and Linux read the same way: interface addresses, the host
//! name, the time zone, signed-in users, resolvers and the package tools.
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use meshrmm_protocol::{AdapterAddress, SignedInUser};

/// Where package tools live. The service's own `PATH` can be minimal, so it
/// isn't searched.
const TOOL_DIRECTORIES: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/usr/local/bin",
    "/opt/homebrew/bin",
    "/snap/bin",
];

/// An interface as `getifaddrs` lists it.
#[derive(Debug, Default)]
pub(super) struct Interface {
    pub(super) up: bool,
    pub(super) loopback: bool,
    pub(super) mac_address: Option<String>,
    pub(super) addresses: Vec<AdapterAddress>,
}

/// Every interface by name, with its addresses and, where the system lists
/// it here, its hardware address.
pub(super) fn interfaces() -> anyhow::Result<BTreeMap<String, Interface>> {
    let mut list = std::ptr::null_mut();
    // SAFETY: on success `list` points to a list freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Err(std::io::Error::last_os_error()).context("getifaddrs");
    }
    let mut interfaces = BTreeMap::<String, Interface>::new();
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: entries stay valid until the list is freed.
        let current = unsafe { &*entry };
        entry = current.ifa_next;
        if current.ifa_name.is_null() {
            continue;
        }
        // SAFETY: the name is NUL-terminated.
        let name = unsafe { CStr::from_ptr(current.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let interface = interfaces.entry(name).or_default();
        let flags = current.ifa_flags as libc::c_int;
        interface.up |= flags & libc::IFF_UP != 0 && flags & libc::IFF_RUNNING != 0;
        interface.loopback |= flags & libc::IFF_LOOPBACK != 0;
        if current.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: a non-null address starts with its family.
        let family = i32::from(unsafe { (*current.ifa_addr).sa_family });
        match family {
            libc::AF_INET | libc::AF_INET6 => {
                // SAFETY: the address and mask are of the family's type.
                let address = unsafe { socket_address(current.ifa_addr) };
                let prefix_length = if current.ifa_netmask.is_null() {
                    None
                } else {
                    // SAFETY: as above; a mask has its address's family.
                    unsafe { socket_address(current.ifa_netmask) }.map(|mask| prefix_length(&mask))
                };
                if let Some(address) = address {
                    interface.addresses.push(AdapterAddress {
                        address: address.to_string(),
                        prefix_length: prefix_length.unwrap_or(if address.is_ipv4() {
                            32
                        } else {
                            128
                        }),
                    });
                }
            }
            #[cfg(target_os = "macos")]
            libc::AF_LINK => {
                // SAFETY: an AF_LINK address is a sockaddr_dl.
                let link = unsafe { &*current.ifa_addr.cast::<libc::sockaddr_dl>() };
                if link.sdl_alen == 6 {
                    let start = usize::from(link.sdl_nlen);
                    // SAFETY: sdl_data holds the name and then the address,
                    // which can run past the declared array's end.
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            link.sdl_data.as_ptr().cast::<u8>().add(start),
                            6,
                        )
                    };
                    interface.mac_address = Some(format_mac(bytes));
                }
            }
            #[cfg(target_os = "linux")]
            libc::AF_PACKET => {
                // SAFETY: an AF_PACKET address is a sockaddr_ll.
                let link = unsafe { &*current.ifa_addr.cast::<libc::sockaddr_ll>() };
                if link.sll_halen == 6 {
                    interface.mac_address = Some(format_mac(&link.sll_addr[..6]));
                }
            }
            _ => {}
        }
    }
    // SAFETY: the list came from getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(list) };
    Ok(interfaces)
}

/// The IP address in an AF_INET or AF_INET6 `sockaddr`.
///
/// # Safety
/// `address` must point to a `sockaddr_in` or `sockaddr_in6`, as its family says.
unsafe fn socket_address(address: *const libc::sockaddr) -> Option<std::net::IpAddr> {
    // SAFETY: the caller's guarantee.
    unsafe {
        match i32::from((*address).sa_family) {
            libc::AF_INET => {
                let address = &*address.cast::<libc::sockaddr_in>();
                Some(std::net::Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)).into())
            }
            libc::AF_INET6 => {
                let address = &*address.cast::<libc::sockaddr_in6>();
                Some(std::net::Ipv6Addr::from(address.sin6_addr.s6_addr).into())
            }
            _ => None,
        }
    }
}

/// The number of leading one bits in a subnet mask.
fn prefix_length(mask: &std::net::IpAddr) -> u8 {
    match mask {
        std::net::IpAddr::V4(mask) => u32::from(*mask).leading_ones() as u8,
        std::net::IpAddr::V6(mask) => u128::from(*mask).leading_ones() as u8,
    }
}

pub(super) fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub(super) fn hostname() -> anyhow::Result<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer's length is passed with it.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("gethostname");
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    Ok(String::from_utf8_lossy(&buffer[..end]).into_owned())
}

/// The IANA name of the system time zone, from the zone file
/// `/etc/localtime` links to.
pub(super) fn time_zone() -> anyhow::Result<String> {
    let target = std::fs::read_link("/etc/localtime").context("/etc/localtime")?;
    zone_name(&target).with_context(|| format!("/etc/localtime links to {}", target.display()))
}

fn zone_name(target: &Path) -> Option<String> {
    let text = target.to_str()?;
    let (_, name) = text.split_once("zoneinfo/")?;
    // Some distributions keep a `posix/` or `right/` copy of the database.
    let name = name
        .strip_prefix("posix/")
        .or_else(|| name.strip_prefix("right/"))
        .unwrap_or(name);
    (!name.is_empty()).then(|| name.to_owned())
}

/// The users with a login session in utmpx: consoles, terminals and SSH.
pub(super) fn utmpx_users() -> Vec<SignedInUser> {
    let mut users = Vec::new();
    // SAFETY: utmpx is read on this thread from start to end; the entries
    // getutxent returns stay valid until the next call.
    unsafe {
        libc::setutxent();
        loop {
            let entry = libc::getutxent();
            if entry.is_null() {
                break;
            }
            let entry = &*entry;
            if entry.ut_type != libc::USER_PROCESS {
                continue;
            }
            let text = |field: &[libc::c_char]| {
                let bytes: Vec<u8> = field
                    .iter()
                    .map(|c| *c as u8)
                    .take_while(|byte| *byte != 0)
                    .collect();
                String::from_utf8_lossy(&bytes).into_owned()
            };
            let name = text(&entry.ut_user);
            if name.is_empty() {
                continue;
            }
            let host = text(&entry.ut_host);
            // 32 bits on Linux, 64 on macOS.
            #[allow(clippy::useless_conversion)]
            let seconds = i64::from(entry.ut_tv.tv_sec);
            users.push(SignedInUser {
                name,
                session: Some(text(&entry.ut_line)).filter(|line| !line.is_empty()),
                remote_host: Some(host).filter(|host| !host.is_empty() && !host.starts_with(':')),
                since_unix_ms: (seconds > 0).then(|| seconds * 1000),
            });
        }
        libc::endutxent();
    }
    users
}

/// The `nameserver` lines of a resolv.conf.
pub(super) fn resolv_conf_servers(path: &str) -> anyhow::Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| path.to_owned())?;
    Ok(parse_resolv_conf(&text))
}

fn parse_resolv_conf(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

/// The path of a tool in the usual directories, if it is installed.
pub(super) fn find_tool(name: &str) -> Option<PathBuf> {
    TOOL_DIRECTORIES
        .iter()
        .map(|directory| Path::new(directory).join(name))
        .find(|path| path.is_file())
}

/// Runs a tool and returns its standard output, failing if it runs longer
/// than `timeout` or exits with an error.
#[cfg(target_os = "linux")]
pub(super) fn run_tool(
    path: &Path,
    args: &[&str],
    timeout: std::time::Duration,
) -> anyhow::Result<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use anyhow::bail;

    let mut child = Command::new(path)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {}", path.display()))?;
    let mut stdout = child.stdout.take().context("no standard output")?;
    let mut stderr = child.stderr.take().context("no standard error")?;
    // Reading on other threads keeps a full pipe from stalling the tool.
    let output = std::thread::spawn(move || {
        let mut text = Vec::new();
        stdout.read_to_end(&mut text).map(|_| text)
    });
    let errors = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.read_to_end(&mut text);
        text
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} took longer than {timeout:?}", path.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = output
        .join()
        .map_err(|_| anyhow::anyhow!("reading the output failed"))??;
    let errors = errors.join().unwrap_or_default();
    if !status.success() {
        bail!(
            "{} failed ({status}): {}",
            path.display(),
            String::from_utf8_lossy(&errors).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_masks_give_prefix_lengths() {
        assert_eq!(prefix_length(&"255.255.240.0".parse().unwrap()), 20);
        assert_eq!(prefix_length(&"255.255.255.255".parse().unwrap()), 32);
        assert_eq!(prefix_length(&"ffff:ffff:ffff:ffff::".parse().unwrap()), 64);
    }

    #[test]
    fn zone_names_come_from_the_link() {
        let name = |path: &str| zone_name(Path::new(path));
        assert_eq!(
            name("/var/db/timezone/zoneinfo/America/New_York").as_deref(),
            Some("America/New_York")
        );
        assert_eq!(
            name("../usr/share/zoneinfo/posix/Europe/Berlin").as_deref(),
            Some("Europe/Berlin")
        );
        assert_eq!(name("/usr/share/zoneinfo/UTC").as_deref(), Some("UTC"));
        assert_eq!(name("/etc/zone"), None);
    }

    #[test]
    fn resolvers_come_from_nameserver_lines() {
        let text = "# generated\nnameserver 127.0.0.53\nsearch lan\nnameserver  fe80::1%en0 \noptions edns0\n";
        assert_eq!(parse_resolv_conf(text), ["127.0.0.53", "fe80::1%en0"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tools_run_with_a_timeout() {
        use std::time::Duration;

        let shell = Path::new("/bin/sh");
        assert_eq!(
            run_tool(shell, &["-c", "echo hello"], Duration::from_secs(5)).unwrap(),
            "hello\n"
        );
        let failed = run_tool(
            shell,
            &["-c", "echo bad >&2; exit 3"],
            Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();
        assert!(failed.contains("bad"), "{failed}");
        assert!(run_tool(shell, &["-c", "sleep 5"], Duration::from_millis(100)).is_err());
    }

    #[test]
    fn this_computer_has_interfaces_and_a_name() {
        let interfaces = interfaces().unwrap();
        assert!(interfaces.values().any(|interface| interface.loopback));
        assert!(!hostname().unwrap().is_empty());
    }
}
