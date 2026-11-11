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
applications are readable. It collects at a random moment between 10
seconds and an hour after the Agent starts, then at a random moment between
5.4 and 6 hours after each snapshot (`SYSTEM_INFO_INTERVAL_SECONDS`, less up
to a tenth), drawn again each time, so Agents that started together, after a
fleet update or a power cut, drift apart instead of sending together. It
collects at once when the server sends `collect_system_info`, and after an
application install or uninstall run finishes; the next one it collects on
its own is counted from then. Collecting runs on a blocking thread, one
snapshot at a time, so the control connection never waits for it; a request
that arrives while one is being collected is answered by that one.

The Agent posts the snapshot to `POST /v1/agents/{id}/system-info` with its
own credential, from a thread of its own
(`agent/src/remote/system_info/upload.rs`), not on the control connection.
A snapshot can be megabytes, which takes minutes on a slow uplink; on the
control connection it would hold up every other message, and the server,
which hears a frame only once it has arrived whole, would take the Agent
for gone after a minute (`agents::Timeouts::silence`), cut the connection,
and get the same snapshot first on the next one, so the device would never
stay online. The post has no limit in total: it gives up only when the
socket has taken nothing for a minute, and the server drops a body that has
sent nothing for a minute. Only the newest snapshot not yet delivered is
kept, in memory: a newer one replaces it, and one a restart loses is
collected again. A failed post is retried like the Agent's results, after 5
seconds, doubling to 10 minutes, and at once when the control connection
comes back or a newer snapshot arrives; a refusal other than 401, 408 or
429 drops it.

The applications are nearly all of a snapshot's bytes and seldom change, so
the Agent leaves out a list the server already holds:

- The server says which list it holds: as `stored_applications` with its
  `applications_hash` (none for no list), first thing on every connection,
  and in its answer to each snapshot, `{applications_hash, collect_again}`.
- A snapshot whose applications hash to that is sent without them and with
  `applications_unchanged` set to the hash. Until the server has said, as at
  the start of a connection, every snapshot carries its list.
- Both sides hash with `applications_hash` in `crates/protocol-types`: the
  SHA-256 of every field of every application in order, each with its length
  or whether it is set, over the sanitized list. The Agent hashes the list it
  would send, which is already sanitized and fitted; the server hashes the
  list it received after sanitizing it again, which changes nothing, so the
  two agree. Were they ever to disagree, the Agent's hash would never be the
  one the server names, and it would send every list whole, as before.

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
  changed, with the list's hash and how many rows it has in
  `device_system_info`;
- `software_names` and `software_versions`: how many devices have each
  application, and each version of it from each publisher, so the fleet's
  software is read without grouping every device's list (below);
- `device_application_changes`: what was installed, removed or updated
  between two lists, kept for 90 days. The same name, source and
  architecture is one application, and one version replacing another is an
  update. A device's first list is where its history starts.

The two counts tables change in the transaction that changes a device's
rows in `device_applications`, by what the device starts or stops having:
when a new list replaces its stored one, and when its rows are removed
(deleting the device). Nothing else writes those rows, so the counts are
always their grouping; a count that reaches 0 is removed. A device counts
once for an application however many copies it lists, and a version's count
adds up its publishers'. An application on every device has one row that
every changed list naming it adds to or takes from: each list touches only
what changed, an unchanged list nothing, and on PostgreSQL two devices'
changes to the same application wait for each other's short transaction
(in key order, so they never deadlock). SQLite stores one snapshot at a time
anyway. A deleted device's applications leave the counts when its rows are
removed, just after it is marked deleted.

A snapshot that names its list (`applications_unchanged`) does no
application work: when the hash is the one stored, the list and its history
stay as they are, and the rest of the snapshot is stored as usual. When it
isn't, after the device's rows were removed or a race, the rest is stored,
the stored list (or none) is kept, and the answer says which list the
server holds, with `collect_again`, which makes the Agent collect a new
snapshot that then carries the list. A disagreement can cost a list sent
whole, never a wrong list kept.

The public address is the one the control connection or the snapshot comes
from (behind trusted proxies, from `X-Forwarded-For`), recorded whenever the
Agent connects or posts one. A snapshot is taken whether or not the Agent's
control connection is up; the credential is what counts. When an Agent
connects and the server's snapshot is missing or
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
  optionally for one company and one [tag](devices.md#tags), and how many
  applications match. Opening an application lists its devices, each
  linking to the device's Software tab. Both lists load 100 at a time, with
  **Show more** for the next; the search asks the server 300 ms after the
  last keystroke, and only the newest search's answer is shown.

## API

| Route | |
|---|---|
| `GET /v1/agents/{id}/system-info` | The latest snapshot without its applications, with `application_count`, `public_ip`, `collected_at` and `received_at`. 404 `not_collected` before the first one. |
| `POST /v1/agents/{id}/system-info/refresh` | Asks the online Agent for a new snapshot: 202, or 409 `device_offline`. |
| `POST /v1/agents/{id}/system-info` | The Agent's own route, with its credential: `{system, applications_unchanged}`, at most 6 MiB and 64 KiB. Answers `{applications_hash, collect_again}`. 401 for a wrong credential, 409 for a device being removed, 413 for a larger body (refused as soon as it says or sends more), 408 for one that sends nothing for a minute. |
| `GET /v1/agents/{id}/applications` | The applications, by name. |
| `GET /v1/agents/{id}/application-changes?limit=` | Changes, newest first (200 by default, at most 1,000). |
| `GET /v1/system-info?q=&os=&model=&serial=&ip=&application=&company=&tag=&limit=&offset=` | `{devices, matching}`: a page of the snapshots of the devices matching every filter given, by name then ID, each as `GET /v1/agents/{id}/system-info` returns it with the device's name, platform and company, and how many devices match. Each filter matches anywhere in its field, ignoring case: `q` any of the name, operating system, model, serial number, IP addresses and host name; `model` the model or manufacturer; `ip` an adapter's or the public address; `application` an installed application's name. `company` is a company's ID or `none`; `tag` a tag's ID. 100 devices a page by default, at most 200; `limit=0` counts only. |
| `GET /v1/software?q=&company=&tag=&limit=&offset=` | `{applications, matching, truncated}`: a page of applications by name with device counts per version, or without `q` those on the most devices; how many match, and `truncated` when more follow. 200 a page by default, at most 500. Without a company or tag it reads the counts tables; with one, those devices' own rows. |
| `GET /v1/software/devices?name=&version=&unversioned=&company=&tag=&limit=&offset=` | `{devices, matching}`: a page of the copies of the application named exactly `name`, in one version if given, by device name, and how many there are. A device with two versions has two. 100 a page by default, at most 200. |

Patching uses the operating system's `id`, `id_like` and `package_managers`;
Wake-on-LAN uses the adapters' MAC addresses and subnets, which
`/v1/system-info` returns for the fleet a page at a time. Filtering,
counting and paging are the database's, so a page reads only its own
devices' snapshots; the `application` filter reads the rows of the
application names that match.

Every route but the Agent's needs `devices.view`, refreshing included: it
only makes the Agent collect again, so it isn't audited.

## Tests

`cargo test -p meshrmm-server --test system_info` exercises snapshots posted
over HTTPS beside a real control connection (one that trickles in for longer
than the control connection's silence limit, one that stalls, oversized and
unauthenticated ones), refreshes, change tracking, searches and
permissions; `cargo test -p meshrmm-agent system_info` collects the real
computer's snapshot and checks the parsers. It also pages through more
devices than a page, checks that a page costs the same statements for any
fleet, and compares the counts tables with a fresh grouping of the rows
after installs, removals, updates, deletions and enrolments; `--test
query_plans` shows that the device search without an `application` filter
and the whole fleet's software don't read `device_applications`. The
website's are in `dashboard/tests/system-info.test.mjs` and
`dashboard/tests/paged-list.test.mjs`.
