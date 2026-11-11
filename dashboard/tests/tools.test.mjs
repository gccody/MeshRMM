import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_PROCESS_SORT,
  NUMERIC_PROCESS_KEYS,
  errorHelp,
  formatCpu,
  nextSort,
  processMatches,
  processTree,
  processTreeOf,
  serviceActions,
  sortProcesses,
  startupLabel,
  visibleServices,
} from "../features/tools/lists.ts";
import { canManageTool, canViewTool, parseTool, toolsPagePath, unavailableReason } from "../features/tools/model.ts";

const process = (pid, parent, name, extra = {}) => ({
  pid,
  parent_pid: parent,
  name,
  cpu_percent: 0,
  memory_bytes: 0,
  started_at: pid,
  ...extra,
});

const PROCESSES = [
  process(1, undefined, "systemd"),
  process(10, 1, "sshd", { cpu_percent: 2, memory_bytes: 50 }),
  process(11, 10, "bash", { user: "ada", command_line: "-bash" }),
  process(12, 11, "python3", { cpu_percent: 40, command_line: "python3 -m http.server" }),
  process(13, 11, "vim"),
  process(20, 1, "cron", { memory_bytes: 900 }),
  // Its parent ID now names a process that started after it.
  process(5, 20, "orphan"),
];

test("processes sort by any column, largest first for numbers", () => {
  const byCpu = sortProcesses(PROCESSES, DEFAULT_PROCESS_SORT).map((each) => each.name);
  assert.deepEqual(byCpu.slice(0, 2), ["python3", "sshd"]);
  const byName = sortProcesses(PROCESSES, { key: "name", descending: false }).map((each) => each.name);
  assert.deepEqual(byName.slice(0, 3), ["bash", "cron", "orphan"]);
  assert.deepEqual(nextSort({ key: "cpu", descending: true }, "cpu", NUMERIC_PROCESS_KEYS), { key: "cpu", descending: false });
  assert.deepEqual(nextSort({ key: "cpu", descending: true }, "memory", NUMERIC_PROCESS_KEYS), { key: "memory", descending: true });
  assert.deepEqual(nextSort({ key: "cpu", descending: true }, "name", NUMERIC_PROCESS_KEYS), { key: "name", descending: false });
});

test("a search matches names, IDs, users and command lines", () => {
  assert.equal(processMatches(PROCESSES[3], "HTTP.SERVER"), true);
  assert.equal(processMatches(PROCESSES[2], "ada"), true);
  assert.equal(processMatches(PROCESSES[4], "13"), true);
  assert.equal(processMatches(PROCESSES[4], "nginx"), false);
  assert.equal(processMatches(PROCESSES[4], "  "), true);
});

test("the tree follows parents, and a search keeps a match's ancestors", () => {
  const sort = { key: "pid", descending: false };
  const rows = processTree(PROCESSES, sort);
  assert.deepEqual(
    rows.map((row) => [row.process.pid, row.depth]),
    [[1, 0], [10, 1], [11, 2], [12, 3], [13, 3], [20, 1], [5, 0]],
  );
  assert.equal(rows.find((row) => row.process.pid === 11).children, 2);
  const collapsed = processTree(PROCESSES, sort, "", new Set([11]));
  assert.equal(collapsed.some((row) => row.process.pid === 12), false);
  const found = processTree(PROCESSES, sort, "vim");
  assert.deepEqual(found.map((row) => row.process.pid), [1, 10, 11, 13]);
});

test("ending a tree lists every process in it, children first", () => {
  assert.deepEqual(processTreeOf(PROCESSES, 10).map((each) => each.pid), [12, 13, 11, 10]);
  // The orphan isn't cron's, though its parent ID says so.
  assert.deepEqual(processTreeOf(PROCESSES, 20).map((each) => each.pid), [20]);
  assert.deepEqual(processTreeOf(PROCESSES, 999), []);
  // A loop of parents ends.
  const looped = [process(2, 3, "a", { started_at: 0 }), process(3, 2, "b", { started_at: 0 })];
  assert.deepEqual(processTreeOf(looped, 2).map((each) => each.pid), [3, 2]);
});

test("services filter, search and sort", () => {
  const services = [
    { name: "cron.service", display_name: "cron.service", state: "running", startup: "automatic", account: "root" },
    { name: "Spooler", display_name: "Print Spooler", state: "stopped", startup: "manual", description: "Queues print jobs" },
    { name: "web", display_name: "Web", state: "failed", startup: "disabled", account: "www-data" },
  ];
  assert.deepEqual(visibleServices(services, "running", "", { key: "name", descending: false }).map((each) => each.name), ["cron.service"]);
  assert.deepEqual(visibleServices(services, "stopped", "", { key: "name", descending: false }).map((each) => each.name), ["Spooler", "web"]);
  assert.deepEqual(visibleServices(services, "all", "print jobs", { key: "name", descending: false }).map((each) => each.name), ["Spooler"]);
  assert.deepEqual(visibleServices(services, "all", "", { key: "state", descending: false }).map((each) => each.name), ["cron.service", "web", "Spooler"]);
  assert.deepEqual(visibleServices(services, "all", "", { key: "startup", descending: true }).map((each) => each.name), ["web", "Spooler", "cron.service"]);
});

test("a service offers what its state allows, never stopping the Agent", () => {
  assert.deepEqual(serviceActions({ state: "running" }), ["stop", "restart"]);
  assert.deepEqual(serviceActions({ state: "stopped", startup: "manual" }), ["start"]);
  assert.deepEqual(serviceActions({ state: "stopped", startup: "disabled" }), []);
  assert.deepEqual(serviceActions({ state: "stopping" }), []);
  assert.deepEqual(serviceActions({ state: "running", protected: true }), []);
});

test("startup types are named in each platform's words", () => {
  assert.equal(startupLabel("automatic_delayed", "windows"), "Automatic (Delayed Start)");
  assert.equal(startupLabel("disabled", "linux"), "Masked");
  assert.equal(startupLabel("manual", "linux"), "Disabled (on demand)");
  assert.equal(startupLabel("automatic", "macos"), "Enabled");
});

test("tools follow permissions and have pages of their own", () => {
  const can = (held) => (permission) => held.includes(permission);
  assert.equal(canViewTool(can(["tools.processes.view"]), "processes"), true);
  assert.equal(canManageTool(can(["tools.processes.view"]), "processes"), false);
  assert.equal(canViewTool(can(["tools.processes.view"]), "services"), false);
  assert.equal(toolsPagePath("a b", "services"), "/tools?id=a%20b&tool=services");
  assert.equal(parseTool("services"), "services");
  assert.equal(parseTool("nope"), null);
  for (const platform of ["windows", "macos", "linux"]) {
    assert.equal(unavailableReason("processes", platform), null);
    assert.equal(unavailableReason("services", platform), null);
  }
});

test("numbers and errors read plainly", () => {
  assert.equal(formatCpu(0.01), "0%");
  assert.equal(formatCpu(3.14), "3.1%");
  assert.equal(formatCpu(55.5), "56%");
  assert.match(errorHelp("device_timeout"), /refresh/);
  assert.equal(errorHelp("access_denied"), null);
});
