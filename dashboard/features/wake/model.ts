// Wake-on-LAN requests as the server sends them, and how the website words
// them. See docs/wake-on-lan.md. Imports carry the .ts extension so
// node:test can load this module directly.
import type { Agent } from "../agents/types.ts";

export type WakeStatus = "sending" | "sent" | "failed" | "no_relay";
export type RelayStatus = "pending" | "sent" | "failed";

// One relay of a wake request, on one subnet it shares with the device.
export type WakeRelay = {
  device_id: string;
  name: string;
  broadcast: string;
  prefix_length: number;
  status: RelayStatus;
  // The relay's address it sent from.
  source_address: string | null;
  error: string | null;
  reported_at: number | null;
};

// POST /v1/agents/{id}/wake, GET /v1/wakes?ids=
export type Wake = {
  id: string;
  device_id: string;
  device_name: string | null;
  status: WakeStatus;
  // Why there is no relay, or why every relay failed.
  detail: string | null;
  mac_addresses: string[];
  requested_by: string;
  job_id: string | null;
  created_at: number;
  // When the device came online afterwards, if it did in time.
  online_at: number | null;
  // Until when the device coming online counts as woken.
  watch_until: number;
  relays: WakeRelay[];
};

// POST /v1/wakes
export type WokenMany = {
  wakes: Wake[];
  skipped: { device_id: string; reason: "online" | "not_found" }[];
};

// How a wake stands for the person who asked: the device's own presence
// counts too, since the event socket hears it come online first.
export type WakeState =
  | { kind: "sending" }
  | { kind: "no_relay"; reason: string }
  | { kind: "failed"; reason: string }
  | { kind: "waiting"; until: number }
  | { kind: "online"; at: number | null }
  | { kind: "not_online" };

export function wakeState(wake: Wake, connected: boolean, now: number): WakeState {
  if (wake.online_at !== null || (connected && wake.status !== "no_relay" && wake.status !== "failed")) {
    return { kind: "online", at: wake.online_at };
  }
  switch (wake.status) {
    case "sending": return { kind: "sending" };
    case "no_relay": return { kind: "no_relay", reason: wake.detail ?? "No device can wake it." };
    case "failed": return { kind: "failed", reason: wake.detail ?? "No relay could send the packets." };
    default: return now < wake.watch_until ? { kind: "waiting", until: wake.watch_until } : { kind: "not_online" };
  }
}

export type WakeTone = "pending" | "success" | "problem";

export function wakeTone(state: WakeState): WakeTone {
  switch (state.kind) {
    case "online": return "success";
    case "sending":
    case "waiting": return "pending";
    default: return "problem";
  }
}

// The state in a short headline.
export function wakeHeadline(state: WakeState, deviceName: string) {
  switch (state.kind) {
    case "sending": return `Asking devices on ${deviceName}’s network to wake it…`;
    case "no_relay": return `No device can wake ${deviceName}`;
    case "failed": return `The wake packets couldn’t be sent`;
    case "waiting": return `Packets sent. Waiting for ${deviceName} to come online…`;
    case "online": return `${deviceName} is online`;
    case "not_online": return `${deviceName} didn’t come online`;
  }
}

// What didn't coming online most often means.
export const NOT_ONLINE_HINT =
  "Check that Wake-on-LAN is turned on in its firmware and network adapter, that it's on mains power, and on Windows that Fast Startup is off.";

// A relay's part, in words: "Sent from 192.168.1.140 to 192.168.1.255".
export function relayText(relay: WakeRelay) {
  const network = `${relay.broadcast}/${relay.prefix_length}`;
  switch (relay.status) {
    case "pending": return `Sending to ${network}…`;
    case "sent": return relay.source_address ? `Sent from ${relay.source_address} to ${relay.broadcast}` : `Sent to ${relay.broadcast}`;
    case "failed": return relay.error ?? "Failed";
  }
}

// Whether a relay may still report.
export const isSettling = (wake: Wake) => wake.status === "sending" || wake.relays.some((relay) => relay.status === "pending");

// The chosen devices that can be woken: offline ones.
export const wakeable = (agents: readonly Agent[]) => agents.filter((agent) => !agent.connected);

// "Wake Office PC", or "Wake 3 devices".
export function wakeTitle(names: readonly string[]) {
  return names.length === 1 ? `Wake ${names[0]}` : `Wake ${names.length} devices`;
}
