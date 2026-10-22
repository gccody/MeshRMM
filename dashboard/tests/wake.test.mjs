import assert from "node:assert/strict";
import test from "node:test";
import { outcomeLabel, policyDraft, policyDraftProblem, policyInput } from "../features/automation/model.ts";
import { relayText, wakeHeadline, wakeState, wakeTitle, wakeTone, wakeable } from "../features/wake/model.ts";

const NOW = 1_800_000_000_000;

const wake = (changes = {}) => ({
  id: "w",
  device_id: "pc",
  device_name: "Office PC",
  status: "sent",
  detail: null,
  mac_addresses: ["AA:00:00:00:00:01"],
  requested_by: "Ada",
  job_id: null,
  created_at: NOW - 60_000,
  online_at: null,
  watch_until: NOW + 9 * 60_000,
  relays: [],
  ...changes,
});

test("a sent wake waits for the device, then says whether it came online", () => {
  assert.deepEqual(wakeState(wake(), false, NOW), { kind: "waiting", until: NOW + 9 * 60_000 });
  assert.deepEqual(wakeState(wake(), true, NOW), { kind: "online", at: null }, "the event socket hears it first");
  assert.deepEqual(wakeState(wake({ online_at: NOW }), false, NOW), { kind: "online", at: NOW });
  assert.deepEqual(wakeState(wake(), false, NOW + 10 * 60_000), { kind: "not_online" });
  assert.equal(wakeTone(wakeState(wake(), false, NOW + 10 * 60_000)), "problem");
  assert.equal(wakeHeadline({ kind: "waiting", until: 0 }, "Office PC"), "Packets sent. Waiting for Office PC to come online…");
  assert.equal(wakeHeadline({ kind: "not_online" }, "Office PC"), "Office PC didn’t come online");
});

test("wakes no device could send say why, even if the device is online now", () => {
  const reason = "No online device is on Office PC's network (10.20.0.0/16).";
  assert.deepEqual(wakeState(wake({ status: "no_relay", detail: reason }), true, NOW), { kind: "no_relay", reason });
  assert.deepEqual(wakeState(wake({ status: "failed", detail: "No relay could send the packets." }), false, NOW), { kind: "failed", reason: "No relay could send the packets." });
  assert.equal(wakeState(wake({ status: "sending" }), false, NOW).kind, "sending");
  assert.equal(wakeTone({ kind: "sending" }), "pending");
  assert.equal(wakeHeadline({ kind: "no_relay", reason }, "Office PC"), "No device can wake Office PC");
});

test("relays say what they sent, from where", () => {
  const relay = { device_id: "mac", name: "Mac", broadcast: "192.168.1.255", prefix_length: 24, status: "sent", source_address: "192.168.1.140", error: null, reported_at: NOW };
  assert.equal(relayText(relay), "Sent from 192.168.1.140 to 192.168.1.255");
  assert.equal(relayText({ ...relay, status: "pending", source_address: null }), "Sending to 192.168.1.255/24…");
  assert.equal(relayText({ ...relay, status: "failed", error: "The relay didn't report back" }), "The relay didn't report back");
});

test("only offline devices are woken, and the dialog names them", () => {
  const agents = [{ id: "a", name: "A", connected: true }, { id: "b", name: "B", connected: false }];
  assert.deepEqual(wakeable(agents).map((agent) => agent.id), ["b"]);
  assert.equal(wakeTitle(["Office PC"]), "Wake Office PC");
  assert.equal(wakeTitle(["A", "B", "C"]), "Wake 3 devices");
});

test("wake jobs word their outcomes as coming online", () => {
  assert.equal(outcomeLabel("succeeded", "wake"), "Came online");
  assert.equal(outcomeLabel("no_result", "wake"), "Didn’t come online");
  assert.equal(outcomeLabel("running", "wake"), "Waiting to come online");
  assert.equal(outcomeLabel("failed", "wake"), "Failed");
  assert.equal(outcomeLabel("succeeded", "script"), "Succeeded");
  assert.equal(outcomeLabel("succeeded"), "Succeeded");
});

test("wake policies run on a schedule and never wait for offline devices", () => {
  const draft = { ...policyDraft(null, "UTC", "wake"), name: "Wake before patching", catchUpHours: "" };
  assert.equal(policyDraftProblem(draft), null, "the catch-up time doesn't apply");
  assert.equal(policyDraftProblem({ ...draft, trigger: "enroll" }), "A policy that wakes devices runs on a schedule: newly enrolled devices are online.");
  const body = policyInput(draft);
  assert.equal(body.action, "wake");
  assert.equal(body.offline, "skip");
  assert.equal(body.script_id, undefined);
});
