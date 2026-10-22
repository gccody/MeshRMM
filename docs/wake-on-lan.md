# Wake-on-LAN

An offline device can be woken from the website. MeshRMM has no way into a
sleeping or powered-off computer's network, so another device on the same
local network does it: the server asks online Agents there (the **relays**)
to broadcast a Wake-on-LAN magic packet for the offline device, and the
device's network adapter powers the computer on when it sees it.

## Waking a device

- **Wake device** in an offline device's **⋯** menu on the device wall, or
  **Wake** on its page.
- **Wake N offline** in the selection bar when ticked devices include
  offline ones. Online ones among them are left alone.
- A [policy](automation.md#policies) whose action is **Wake devices**, for
  example a few minutes before a patch window.

The dialog opens with the request already sent. For each device it shows
which relays sent the packets, from which of their addresses and to which
broadcast address, or why none could, then watches for the device to come
online: **is online** once its Agent connects, or **didn't come online**
after 10 minutes, with what to check. Closing the dialog doesn't stop
anything. The device page shows the device's last wake of the past day the
same way, with who asked.

When no device can wake it, the dialog says why:

- the device's Agent never sent its [network details](hardware-and-software.md),
  so its hardware addresses aren't known;
- its last network details have no wired or wireless adapter with a hardware
  (MAC) address, or no IPv4 address on a local network;
- no online device is on its network. Magic packets are broadcasts, which
  routers don't forward, so a device with a MeshRMM Agent must be online on
  each network whose computers are to be woken.

## Choosing the relays

The server keeps each device's last network details after it goes offline:
its adapters' hardware addresses, kinds (wired, wireless, virtual) and IPv4
addresses with their prefix lengths, and the public address its Agent last
connected from (`device_system_info`, `device_ip_addresses`). For an
offline device it:

1. takes the hardware addresses of its adapters that aren't virtual,
   wired ones first, at most 8;
2. takes its last IPv4 addresses, leaving out loopback, link-local, /31 and
   /32 addresses and subnets wider than /8;
3. finds the online devices with an address whose subnet (by that device's
   own prefix length) holds one of those addresses. A subnet that is
   virtual at both ends, such as two hosts' identical Docker bridges, never
   counts;
4. if any of them connect from the public address the offline device last
   connected from, keeps only those: the others are most likely at another
   site that happens to use the same private addresses;
5. ranks the rest by whether the subnet is on a hardware adapter at either
   end, then by whether they are in the offline device's company, and uses
   the first three.

Each relay sends on every subnet it shares with the device. Devices behind
the same NAT (a Hyper-V or VMware virtual switch, say) are woken through
other devices behind it, and devices on the office network through devices
on the office network.

## What a relay sends

The server sends each relay one `send_wake_packets` command over its
control connection with a packet request per device: the device's hardware
addresses, and the subnet as its directed broadcast address and prefix
length. The relay finds its own current address on that subnet (it may
have changed since its last report), and for each hardware address
broadcasts the 102-byte magic packet (six `0xFF` bytes, then the hardware
address sixteen times) over UDP to ports 9 and 7, both at the subnet's
directed broadcast address and at 255.255.255.255. Its socket is bound to its
address on the subnet, so a computer on several networks sends out of the
right one. It answers with `wake_packets_sent`: per request the address it
sent from, or why it couldn't.

| | Windows | macOS | Linux |
|---|---|---|---|
| Relays | Yes | Yes | Yes |
| UDP magic packets to ports 9 and 7 | Yes | Yes | Yes |
| Sent from the address on the subnet | Yes (bound socket) | Yes (bound socket and `IP_BOUND_IF`, since macOS sends 255.255.255.255 out of the primary interface otherwise) | Yes (bound socket) |
| Ethernet frame of type 0x0842 too | No | No | Yes (raw packet socket, as root) |

## What the device needs

Wake-on-LAN depends on the sleeping computer, not on MeshRMM:

- **Firmware**: Wake-on-LAN (often "Power on by PCI-E", "Wake on LAN" or
  "Resume by LAN") turned on in the BIOS or UEFI setup. Some computers only
  wake from sleep, not from off; most need mains power, and laptops often
  wake only on mains power with a wired connection.
- **Network adapter**: Wake-on-LAN turned on in the adapter's driver
  (Windows: Device Manager → the adapter → **Power Management** "Allow this
  device to wake the computer" and "Only allow a magic packet", and
  **Advanced** "Wake on Magic Packet"; Linux: `ethtool -s <interface> wol g`,
  made persistent by the network manager; macOS: **Wake for network
  access** in Energy settings).
- **Windows Fast Startup**: shutting down with Fast Startup on is a
  hibernation that many adapters can't wake from. Turn it off (Control
  Panel → Power Options → **Choose what the power buttons do**) to wake
  computers that were shut down.
- **Modern Standby (S0ix)**: computers that sleep this way stay on the
  network in a low-power state and wake on their adapter's own patterns;
  many ignore magic packets while asleep, and Wi-Fi rarely wakes anything.
  Prefer a wired connection.
- **Network**: the packet must reach the adapter. Switches deliver
  broadcasts within a network; a router between the relay and the device
  stops them. Some managed networks drop directed broadcasts; the
  255.255.255.255 copy covers the relay's own network.

Virtual machines are woken by their host's management tools, not by magic
packets; Hyper-V ignores them.

## Outcomes

A wake request is `sending` until its relays report, `sent` once one of them
sent the packets, `failed` when none could (each relay's error is kept), and
`no_relay` when there was none. A relay that hasn't reported within 15
seconds failed. If the device's Agent connects within 10 minutes of the
request, the request records when (`online_at`): that is what counts as the
device having woken. A device can of course come online by itself in that
time.

In a policy's job, online devices are skipped (**Online already**), each
offline device is woken (**Waiting to come online**), and its outcome is
**Came online** when its Agent connects within 10 minutes, **Didn't come
online** after that, or **Failed** with the reason when no relay could send
the packets. Wake jobs never wait for offline devices, and a wake policy
can't run on enrollment, as a new device is online.

## Permissions and audit

`devices.wake` (Technician has it) wakes devices. Seeing wake requests needs
`devices.view`. Saving a wake policy needs `automation.manage` and
`devices.wake`.

Each device a user wakes is recorded as `agent.wake`, with the request's ID,
the relays, the hardware addresses, and its status or why it had no relay.
A policy's wakes are recorded once as its job's `job.start`. Wake requests
are kept for 30 days.

## API

| Route | |
|---|---|
| `POST /v1/agents/{id}/wake` | Wakes the offline device. `201` with the wake request once its relays reported, or after 5 seconds; `409 device_online` when it is online. A request with no relay is `201` with `status: "no_relay"` and the reason in `detail`. (`devices.wake`) |
| `POST /v1/wakes` | `{device_ids}` (1 to 200): wakes the offline ones. `201` with `{wakes, skipped: [{device_id, reason: "online" \| "not_found"}]}`. (`devices.wake`) |
| `GET /v1/wakes?ids=a,b` | Wake requests by ID. (`devices.view`) |
| `GET /v1/agents/{id}/wakes?limit=` | The device's latest wake requests, newest first (5 by default, at most 50). (`devices.view`) |

A wake request reads `{id, device_id, device_name, status, detail,
mac_addresses, requested_by, job_id, created_at, online_at, watch_until,
relays: [{device_id, name, broadcast, prefix_length, status, source_address,
error, reported_at}]}`. A job's devices carry `wake_id`.

## How it's built

- `crates/protocol-types/src/wake.rs`: `WakePacket`, `WakePacketResult`, the
  magic packet, hardware address parsing and subnet arithmetic.
  `AgentCommand::SendWakePackets` and `AgentStatusMessage::WakePacketsSent`.
- `agent/src/remote/wake.rs`: the relay. It lists the computer's current
  IPv4 addresses (`system_info::local_ipv4_addresses`) and sends on a
  blocking thread; the coordinator reports the results.
- `server/src/wake/`: `relays.rs` chooses relays; `mod.rs` plans, records
  and sends wake requests, records relays' reports, fails relays that don't
  report (and, at startup, those a restart interrupted), and marks devices
  that come online. The coordinator calls it when an Agent connects and when
  a relay reports.
- Tables: `wake_requests` and `wake_relays`; `device_ip_addresses.kind`.
- Jobs: kind `wake`, joined to `wake_requests` by `job_id` and device;
  policies: action `wake`. See [automation](automation.md).
- `server/src/api/wake.rs`; the website's `dashboard/features/wake/`.

## Tests

- `cargo test -p meshrmm-protocol-types wake` and `cargo test -p
  meshrmm-agent wake`: the packet, parsing, subnets, and a real send on the
  loopback interface.
- `cargo test -p meshrmm-server wake` (unit tests of relay choice and
  plans) and `--test wake`: waking through a relay, devices coming online,
  no relay, failing relays, permissions, waking many, and wake policies and
  their jobs.
- `dashboard/tests/wake.test.mjs`.
- To see the packets on a network, capture on another computer there:
  `sudo tcpdump -i <interface> -X 'udp port 9 or udp port 7'`.
