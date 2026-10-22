# Hardware and software

Every Agent sends the server a snapshot of its computer: the operating
system, the hardware, the network and the installed applications. The
website shows it on each device's page, searches the device wall by it, and
answers "which devices have this application, in which version" across the
fleet. Seeing any of it needs `devices.view`.

## What a snapshot holds

The wire format is `SystemInfo` in `crates/protocol-types/src/system_info.rs`.

- **Operating system:** name, version, build, kernel, architecture, install
  date, last boot, time zone, an `id` (`windows`, `macos`, or the Linux
  distribution's os-release `ID` and `ID_LIKE`), and the update and package
  tools present (`windows_update`, `winget`, `chocolatey`, `softwareupdate`,
  `homebrew`, `macports`, `apt`, `dnf`, `yum`, `zypper`, `pacman`,
  `flatpak`, `snap`).
- **Hardware:** manufacturer, model, serial number, firmware version,
  processor with its cores and threads, installed memory, and the physical
  disks with their model, size, SSD or hard disk, and bus.
- **Network:** host name, full DNS name, Windows domain or workgroup, the
  system's DNS servers, and each adapter's name, description, MAC address,
  kind (Ethernet, Wi-Fi or virtual), link state and speed, IPv4 and IPv6
  addresses with prefix lengths, gateways and DNS servers.
- **Signed-in users:** name, session (console, Remote Desktop, TTY, SSH),
  where a remote one connects from, and since when.
- **Applications:** name, version, publisher, architecture, install date and
  source (`windows`, `windows_user`, `macos_app`, `dpkg`, `rpm`, `pacman`,
  `flatpak`, `snap`).
- **Problems:** the parts the Agent could not read, such as a package tool
  that failed. The rest of the snapshot still arrives.

Every text is bounded to 256 characters, lists to 64 entries (10,000
applications), and the whole snapshot to 6 MiB of JSON; the Agent drops
applications from the end of a larger one and says so in its problems.

## Collecting

The coordinator collects: the Windows service's SYSTEM worker, the Mac's
root daemon and the Linux root service, so firmware details and every user's
applications are readable. It collects 10 seconds after the Agent starts,
every 6 hours after that (`SYSTEM_INFO_INTERVAL_SECONDS`), and when the
server sends `collect_system_info`. Collecting runs on a blocking thread, one
snapshot at a time, so the control connection never waits for it; a request
that arrives while one is being collected is answered by that one. The
snapshot goes to the server as the `system_info` status message on the
control connection (`agent/src/remote/system_info.rs`).

Each platform reads the operating system directly and runs a tool only where
no stable file format exists:

| | Windows | macOS | Linux |
|---|---|---|---|
| Operating system | Registry `CurrentVersion` (`ProductName` reads Windows 11 from build 22000), `IsWow64Process2` | `SystemVersion.plist`, `uname`, `/var/db/.AppleSetupDone` for the install date | `/etc/os-release`, `uname`, the birth time of `/` for the install date |
| Manufacturer, model, serial, firmware | SMBIOS tables (`GetSystemFirmwareTable`), the baseboard when the system entry is an OEM placeholder | IOKit: `IOPlatformSerialNumber`, the device tree's product name and firmware version | `/sys/class/dmi/id` (the serial needs root), the board when the product is a placeholder, `/proc/device-tree` on boards without DMI |
| Processor, memory | Registry, `GetLogicalProcessorInformationEx`, `GetPhysicallyInstalledSystemMemory` | `sysctl` | `/proc/cpuinfo`, sysfs topology, `/proc/meminfo` |
| Disks | `\\.\PhysicalDriveN` storage properties | IOKit whole-disk media, without disk images and APFS containers | `/sys/block`, without loop, RAM, device-mapper and RAID devices |
| Adapters | `GetAdaptersAddresses`; adapters without the hardware flag, such as Hyper-V switches, are virtual | `getifaddrs`, SystemConfiguration for names, Wi-Fi, gateways and DNS | `getifaddrs`, `/sys/class/net`, `/proc/net/route` and `ipv6_route`, resolv.conf (systemd-resolved's real servers rather than its stub) |
| Domain or workgroup | `NetGetJoinInformation` | Not collected | Not collected |
| Signed-in users | `WTSEnumerateSessions` | utmpx | logind, utmpx without it |
| Applications | Uninstall keys: 64-bit and 32-bit machine-wide, and each loaded user hive | `.app` bundles in `/Applications` and each `~/Applications`, publisher from the code signature | dpkg's status file, pacman's database, `rpm -qa`, `flatpak list`, `snap list` |

Windows leaves out system components and updates (`SystemComponent`,
`ParentKeyName`, update release types). macOS names an application as Finder
does, by its bundle's file name, and leaves out `/System/Applications`, which
come with the operating system. See [platform parity](platform-parity.md#hardware-and-software)
for what each platform can't report.

## Storing

`server/src/system_info/` keeps each device's latest snapshot only:

- `device_system_info`: the columns the website and API search (operating
  system, architecture, manufacturer, model, serial number, host name), when
  the Agent collected it by the device's clock and when the server received
  it by its own, the public address the Agent connects from, and the rest of
  the snapshot as JSON;
- `device_ip_addresses`: each adapter's addresses, prefix lengths, MAC and
  kind, which [Wake-on-LAN](wake-on-lan.md) chooses relays by;
- `device_applications`: the applications, rewritten only when the list
  changed;
- `device_application_changes`: what was installed, removed or updated
  between two snapshots, kept for 90 days. The same name, source and
  architecture is one application, and one version replacing another is an
  update. A device's first snapshot is where its history starts.

The public address is the one the control connection comes from (behind
trusted proxies, from `X-Forwarded-For`), recorded whenever the Agent
connects. When an Agent connects and the server's snapshot is missing or
more than 7 hours old, the server asks for a new one. Deleting a device
deletes everything it sent.

## The website

- **Device page:** the **Hardware**, **Software** and **Network** tabs beside
  **Usage**, with when the snapshot was collected and **Refresh**, which asks
  an online device for a new one. The page updates when it arrives. Software
  is searchable and sortable by name, version, publisher, install date and
  source, and lists the recent changes below.
- **Device wall:** each tile shows the operating system, and the search box
  also matches the operating system, manufacturer, model, serial number and
  IP addresses, public ones included. The device list's event socket carries
  this summary (`system` on each device), so it updates live.
- **Software** (`/software`): the applications on the most devices, or those
  whose name matches a search, with how many devices have each version,
  optionally for one company. Opening an application lists its devices, each
  linking to the device's Software tab.

## API

| Route | |
|---|---|
| `GET /v1/agents/{id}/system-info` | The latest snapshot without its applications, with `application_count`, `public_ip`, `collected_at` and `received_at`. 404 `not_collected` before the first one. |
| `POST /v1/agents/{id}/system-info/refresh` | Asks the online Agent for a new snapshot: 202, or 409 `device_offline`. |
| `GET /v1/agents/{id}/applications` | The applications, by name. |
| `GET /v1/agents/{id}/application-changes?limit=` | Changes, newest first (200 by default, at most 1,000). |
| `GET /v1/system-info?q=&os=&model=&serial=&ip=&application=&company=` | The snapshots of the devices matching every filter given. Each matches anywhere in its field, ignoring case: `q` any of the name, operating system, model, serial number, IP addresses and host name; `model` the model or manufacturer; `ip` an adapter's or the public address; `application` an installed application's name. `company` is a company's ID or `none`. |
| `GET /v1/software?q=&company=&limit=` | Applications by name with device counts per version, or without `q` those on the most devices; `truncated` when more matched. |
| `GET /v1/software/devices?name=&version=&unversioned=&company=` | The devices that have the application named exactly `name`, in one version if given. |

Patching uses the operating system's `id`, `id_like` and `package_managers`;
Wake-on-LAN uses the adapters' MAC addresses and subnets, which
`/v1/system-info` returns for every device at once.

Every route needs `devices.view`, refreshing included: it only makes the
Agent collect again, so it isn't audited.

## Tests

`cargo test -p meshrmm-server --test system_info` exercises snapshots over a
real control connection, refreshes, change tracking, searches and
permissions; `cargo test -p meshrmm-agent system_info` collects the real
computer's snapshot and checks the parsers. The website's are in
`dashboard/tests/system-info.test.mjs`.
