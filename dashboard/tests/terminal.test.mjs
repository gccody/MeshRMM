import assert from "node:assert/strict";
import test from "node:test";
import {
  ACK_THRESHOLD_BYTES,
  AckCounter,
  MAX_INPUT_BYTES,
  accountLabel,
  accountOptions,
  ackMessage,
  clampSize,
  closeReason,
  initialChoice,
  inputMessages,
  isEnded,
  isTooLarge,
  MAX_PASTE_BYTES,
  isValidSize,
  parseRememberedChoice,
  parseTerminalEvent,
  resizeMessage,
  shellOptions,
  statusText,
  terminalPagePath,
  terminalSocketUrl,
} from "../features/terminal/model.ts";

test("devices offer the shells they reported, or their platform's defaults", () => {
  assert.deepEqual(shellOptions("windows", undefined), ["powershell", "cmd"]);
  assert.deepEqual(shellOptions("linux", null), ["login", "sh"]);
  assert.deepEqual(shellOptions("macos", []), ["login", "sh"]);
  assert.deepEqual(shellOptions("windows", ["powershell", "pwsh", "cmd"]), ["powershell", "pwsh", "cmd"]);
  // Unknown, foreign and repeated entries are dropped.
  assert.deepEqual(shellOptions("linux", ["login", "bash", "fish", "cmd", "bash", 3]), ["login", "bash"]);
  assert.deepEqual(shellOptions("windows", ["bash"]), ["powershell", "cmd"]);
});

test("accounts follow the permissions, the device's own first", () => {
  const can = (granted) => (permission) => granted.includes(permission);
  assert.deepEqual(accountOptions(can(["terminal.connect", "terminal.connect_system"])), ["system", "user"]);
  assert.deepEqual(accountOptions(can(["terminal.connect"])), ["user"]);
  assert.deepEqual(accountOptions(can(["terminal.connect_system"])), ["system"]);
  assert.deepEqual(accountOptions(can(["sessions.connect"])), []);
  assert.equal(accountLabel("system", "windows"), "SYSTEM");
  assert.equal(accountLabel("system", "macos"), "root");
  assert.equal(accountLabel("user", "linux"), "Signed-in user");
});

test("a new terminal starts from the remembered choice while it is still offered", () => {
  assert.deepEqual(initialChoice(["login", "zsh"], ["system", "user"], null), { shell: "login", account: "system" });
  assert.deepEqual(initialChoice(["login", "zsh"], ["system", "user"], { shell: "zsh", account: "user" }), { shell: "zsh", account: "user" });
  assert.deepEqual(initialChoice(["login"], ["user"], { shell: "zsh", account: "system" }), { shell: "login", account: "user" });
  assert.equal(initialChoice([], ["user"], null), null);
  assert.equal(initialChoice(["login"], [], null), null);
  assert.deepEqual(parseRememberedChoice('{"shell":"pwsh","account":"user"}'), { shell: "pwsh", account: "user" });
  assert.deepEqual(parseRememberedChoice('{"shell":"fish","account":"admin"}'), { shell: undefined, account: undefined });
  assert.equal(parseRememberedChoice("not json"), null);
  assert.equal(parseRememberedChoice(null), null);
});

test("sizes stay within what the server accepts", () => {
  assert.equal(isValidSize(80, 24), true);
  assert.equal(isValidSize(1000, 500), true);
  assert.equal(isValidSize(1001, 24), false);
  assert.equal(isValidSize(80, 0), false);
  assert.equal(isValidSize(80.5, 24), false);
  assert.deepEqual(clampSize(120.7, 40.2), { columns: 120, rows: 40 });
  assert.deepEqual(clampSize(5000, 900), { columns: 1000, rows: 500 });
  assert.deepEqual(clampSize(0, 0), { columns: 80, rows: 24 });
  assert.deepEqual(clampSize(Number.NaN, 24), { columns: 80, rows: 24 });
});

test("input is split into UTF-8 messages without cutting a character", () => {
  assert.deepEqual(inputMessages(""), []);
  assert.deepEqual([...inputMessages("ls\r")[0]], [108, 115, 13]);
  const text = "é".repeat(MAX_INPUT_BYTES);
  const messages = inputMessages(text);
  assert.equal(messages.reduce((total, message) => total + message.length, 0), MAX_INPUT_BYTES * 2);
  for (const message of messages) {
    assert.ok(message.length <= MAX_INPUT_BYTES);
    // Every message decodes on its own.
    assert.doesNotThrow(() => new TextDecoder("utf-8", { fatal: true }).decode(message));
  }
  // A three-byte character straddling the limit moves to the next message.
  const straddling = inputMessages(`ab${"€"}`, false, 4);
  assert.deepEqual(straddling.map((message) => message.length), [2, 3]);
  // Binary input keeps one byte per character.
  assert.deepEqual([...inputMessages("\x1b[M\xff", true)[0]], [27, 91, 77, 255]);
});

test("pastes over 1 MiB are refused whole", () => {
  assert.equal(isTooLarge(inputMessages("a".repeat(MAX_PASTE_BYTES))), false);
  assert.equal(isTooLarge(inputMessages("a".repeat(MAX_PASTE_BYTES + 1))), true);
});

test("drawn output is acknowledged in batches, and the rest after a pause", () => {
  const acks = new AckCounter();
  assert.equal(acks.flush(), null);
  assert.equal(acks.drawn(1000), null);
  assert.equal(acks.pending, 1000);
  assert.equal(acks.drawn(ACK_THRESHOLD_BYTES), ACK_THRESHOLD_BYTES + 1000);
  assert.equal(acks.pending, 0);
  assert.equal(acks.drawn(10), null);
  // The total, not the increment.
  assert.equal(acks.flush(), ACK_THRESHOLD_BYTES + 1010);
  assert.equal(acks.flush(), null);
  assert.equal(ackMessage(42), '{"type":"ack","bytes":42}');
  assert.equal(resizeMessage(120, 40), '{"type":"resize","columns":120,"rows":40}');
});

test("the device's events are read strictly", () => {
  assert.deepEqual(parseTerminalEvent('{"type":"started","account":"root","shell":"/bin/zsh"}'), { type: "started", account: "root", shell: "/bin/zsh" });
  assert.deepEqual(parseTerminalEvent('{"type":"failed","message":"zsh is not installed"}'), { type: "failed", message: "zsh is not installed" });
  assert.deepEqual(parseTerminalEvent('{"type":"exited","code":3}'), { type: "exited", code: 3 });
  assert.deepEqual(parseTerminalEvent('{"type":"exited","code":null}'), { type: "exited", code: null });
  assert.equal(parseTerminalEvent('{"type":"started"}'), null);
  assert.equal(parseTerminalEvent('{"type":"other"}'), null);
  assert.equal(parseTerminalEvent("garbage"), null);
});

test("statuses and close codes read as sentences", () => {
  assert.equal(statusText({ state: "connecting" }), "Connecting…");
  assert.equal(statusText({ state: "connected", account: "NT AUTHORITY\\SYSTEM", shell: "powershell.exe" }), "Connected · NT AUTHORITY\\SYSTEM · powershell.exe");
  assert.equal(statusText({ state: "exited", code: 0 }), "Exited (code 0)");
  assert.equal(statusText({ state: "exited", code: null }), "Exited");
  assert.equal(statusText({ state: "disconnected", reason: "The device disconnected" }), "Disconnected: The device disconnected");
  assert.equal(statusText({ state: "failed", message: "Nobody is signed in." }), "Couldn’t start: Nobody is signed in.");
  assert.equal(isEnded({ state: "connected", account: "", shell: "" }), false);
  assert.equal(isEnded({ state: "exited", code: 0 }), true);
  assert.equal(closeReason(4008, "closed after 30 minutes without input"), "Closed after 30 minutes without input");
  assert.equal(closeReason(4009, ""), "The device disconnected");
  assert.equal(closeReason(4001, ""), "Sign in again");
  assert.equal(closeReason(1006, ""), "The connection to the server was lost");
});

test("terminal addresses", () => {
  assert.equal(terminalSocketUrl({ protocol: "https:", host: "rmm.example.com" }, "a/b"), "wss://rmm.example.com/v1/terminals/a%2Fb");
  assert.equal(terminalSocketUrl({ protocol: "http:", host: "localhost:5173" }, "x"), "ws://localhost:5173/v1/terminals/x");
  assert.equal(terminalPagePath("dev 1"), "/terminal?id=dev%201");
});
