import assert from "node:assert/strict";
import test from "node:test";
import {
  parseAlertEvent,
  resolutionLabel,
  ruleForm,
  ruleFormProblem,
  ruleInput,
  withCondition,
} from "../features/alerts/model.ts";
import {
  applyMonitorUpdate,
  formatSeconds,
  monitorForm,
  monitorStatus,
  needsScriptsRun,
  parseEventIds,
  remediationChoices,
  scriptsRunReason,
} from "../features/monitors/model.ts";

const named = (condition) => ({ ...withCondition(ruleForm(null), condition), name: "Watch" });

test("monitor rules send their check, and only theirs", () => {
  const form = named("service");
  assert.equal(ruleFormProblem(form), "Enter the service's name.");
  const service = { ...form, monitor: { ...form.monitor, name: " Spooler ", automatic: true, failures: "2" } };
  assert.equal(ruleFormProblem(service), null);
  assert.deepEqual(ruleInput(service).monitor, {
    interval_seconds: 300,
    timeout_seconds: 30,
    failures: 2,
    check: { kind: "service", name: "Spooler", automatic: true },
  });
  assert.equal(ruleInput(service).threshold, null);
  assert.equal(ruleInput(named("cpu")).monitor, null, "other rules send no monitor");

  const process = named("process");
  const busy = { ...process, monitor: { ...process.monitor, name: "sqlservr.exe", alertWhen: "cpu_above", percent: "90", minutes: "5" } };
  assert.deepEqual(ruleInput(busy).monitor.check, { kind: "process", name: "sqlservr.exe", alert_when: "cpu_above", percent: 90, megabytes: null, minutes: 5 });
  assert.equal(ruleFormProblem({ ...busy, monitor: { ...busy.monitor, percent: "100" } }), "Enter a CPU threshold of 1 to 99%.");
  assert.equal(ruleFormProblem({ ...busy, monitor: { ...busy.monitor, name: "C:\\sql\\sqlservr.exe" } }), "Enter the process's file name, without a folder.");

  const events = named("event_log");
  const disk = { ...events, monitor: { ...events.monitor, source: "disk", eventIds: "51, 7 11,7", message: " bad block " } };
  assert.deepEqual(ruleInput(disk).monitor.check, { kind: "event_log", log: "System", source: "disk", event_ids: [7, 11, 51], levels: ["critical", "error"], message: "bad block" });
  assert.equal(ruleFormProblem({ ...disk, monitor: { ...disk.monitor, eventIds: "7, 70000" } }), "Enter event IDs from 0 to 65535, separated by commas.");
  assert.equal(ruleFormProblem({ ...disk, monitor: { ...disk.monitor, source: "x' or '1" } }), "The source can't contain quotes, < > or &.");

  const port = named("tcp_port");
  assert.equal(ruleFormProblem({ ...port, monitor: { ...port.monitor, host: "db.example; reboot", port: "5432" } }), "Enter a host name or IP address.");
  assert.equal(ruleFormProblem({ ...port, monitor: { ...port.monitor, host: "db.example", port: "0" } }), "Enter a port from 1 to 65535.");
  assert.deepEqual(ruleInput({ ...port, monitor: { ...port.monitor, host: " [::1] ", port: "5432" } }).monitor.check, { kind: "tcp_port", host: "[::1]", port: 5432 });

  const web = named("http");
  assert.equal(ruleFormProblem({ ...web, monitor: { ...web.monitor, url: "ftp://x" } }), "Enter an address starting with http:// or https://.");
  const health = { ...web, monitor: { ...web.monitor, url: "https://intranet/health", status: "204", keyword: "ok", verifyTls: false } };
  assert.deepEqual(ruleInput(health).monitor.check, { kind: "http", url: "https://intranet/health", status: 204, keyword: "ok", verify_tls: false });

  const script = named("script_check");
  assert.equal(script.monitor.timeout, "60");
  assert.equal(ruleFormProblem(script), "Choose the script to run.");
  assert.equal(ruleFormProblem({ ...script, monitor: { ...script.monitor, scriptId: "s", timeout: "5" } }), "Enter a timeout of 10 to 3,600 seconds.");
  assert.deepEqual(ruleInput({ ...script, monitor: { ...script.monitor, scriptId: "s", runAs: "user" } }).monitor.check, { kind: "script_check", script_id: "s", run_as: "user" });
});

test("saved monitors fill the form back", () => {
  const monitor = { interval_seconds: 60, timeout_seconds: 10, failures: 3, check: { kind: "certificate", host: "mail.example", port: 993, days: 7 } };
  const form = monitorForm(monitor, "certificate");
  assert.deepEqual([form.interval, form.timeout, form.failures, form.host, form.port, form.days], [60, "10", "3", "mail.example", "993", "7"]);
  const rule = ruleForm({
    id: "r", name: "Mail", severity: "warning", enabled: true, condition: "certificate", threshold: null, threshold_unit: null,
    duration_minutes: null, nonzero_exit_fails: false, target: "all", company_ids: [], tag_ids: [], tag_match: "any",
    channel_ids: ["c"], notify_on_resolve: false, monitor, description: "", created_at: 0, updated_at: 0,
    remediation: { action: "script", script_id: "s", run_as: "system", attempts: 2, retry_minutes: 10 }, notify_when: "remediation_failed",
  });
  assert.equal(rule.monitor.host, "mail.example");
  assert.deepEqual(rule.remediation, { enabled: true, action: "script", scriptId: "s", runAs: "system", attempts: "2", retryMinutes: "10", notifyWhen: "remediation_failed" });
  assert.deepEqual(ruleInput(rule).remediation, { action: "script", script_id: "s", run_as: "system", attempts: 2, retry_minutes: 10 });
  assert.equal(ruleInput(rule).notify_when, "remediation_failed");
});

test("self-healing offers what fits the condition", () => {
  assert.deepEqual(remediationChoices("service", "not_running"), ["restart_service", "script"]);
  assert.deepEqual(remediationChoices("process", "not_running"), ["script"]);
  assert.deepEqual(remediationChoices("process", "memory_above"), ["end_process", "script"]);
  assert.deepEqual(remediationChoices("disk_space", "not_running"), ["script"]);
  assert.deepEqual(remediationChoices("device_offline", "not_running"), []);
  assert.deepEqual(remediationChoices("script_failed", "not_running"), []);
  assert.deepEqual(remediationChoices("app_install_failed", "not_running"), []);
  assert.deepEqual(remediationChoices("app_updates_pending", "not_running"), ["script"]);

  const form = named("service");
  const healing = { ...form, monitor: { ...form.monitor, name: "nginx" }, remediation: { ...form.remediation, enabled: true } };
  assert.equal(healing.remediation.action, "restart_service");
  assert.deepEqual(ruleInput(healing).remediation, { action: "restart_service", script_id: null, run_as: null, attempts: 3, retry_minutes: 5 });
  assert.equal(ruleFormProblem({ ...healing, remediation: { ...healing.remediation, action: "script" } }), "Choose the script self-healing runs.");
  assert.equal(ruleFormProblem({ ...healing, remediation: { ...healing.remediation, attempts: "11" } }), "Enter 1 to 10 attempts.");
  // Switching to a process that must run takes ending it off the table.
  const ending = { ...named("process"), monitor: { ...named("process").monitor, name: "app", alertWhen: "running" }, remediation: { ...healing.remediation, action: "end_process" } };
  assert.equal(ruleFormProblem(ending), null);
  assert.equal(ruleFormProblem({ ...ending, monitor: { ...ending.monitor, alertWhen: "not_running" } }), "Choose what self-healing does.");
  // Without self-healing, the rule notifies when raised.
  assert.deepEqual([ruleInput(form).remediation, ruleInput(form).notify_when], [null, "raised"]);
});

test("monitor results arrive on the event socket and update the device's list", () => {
  const state = { rule_id: "r1", device_id: "d", checked_at: 5, received_at: 6, passing: false, failures: 2, value: null, detail: "down", output: "" };
  const update = { type: "monitor_update", device_id: "d", states: [state] };
  assert.deepEqual(parseAlertEvent(update), update);
  assert.equal(parseAlertEvent({ type: "monitor_update", device_id: "d", states: [{ rule_id: "r" }] }), null);
  const monitors = [
    { rule_id: "r1", problem: null, state: null, failures_before_alert: 3 },
    { rule_id: "r2", problem: null, state: null, failures_before_alert: 1 },
  ];
  const applied = applyMonitorUpdate(monitors, update);
  assert.equal(applied[0].state, state);
  assert.equal(applied[1].state, null);
  assert.deepEqual(monitorStatus(applied[0]), { label: "Failing (2 of 3)", tone: "pending" });
  assert.deepEqual(monitorStatus({ ...applied[0], failures_before_alert: 2 }), { label: "Failing", tone: "problem" });
  assert.deepEqual(monitorStatus(applied[1]), { label: "Not checked yet", tone: "pending" });
  assert.deepEqual(monitorStatus({ ...applied[1], problem: "Windows devices can't run it" }), { label: "Not checked here", tone: "muted" });
  assert.deepEqual(monitorStatus({ ...applied[0], state: { ...state, passing: true } }), { label: "Passing", tone: "success" });
});

test("small helpers word monitors", () => {
  assert.equal(resolutionLabel({ resolution: "remediated", resolved_by: null }), "Resolved itself after self-healing");
  assert.equal(formatSeconds(30), "30 seconds");
  assert.equal(formatSeconds(300), "5 minutes");
  assert.equal(formatSeconds(3600), "1 hour");
  assert.deepEqual(parseEventIds(""), []);
  assert.equal(parseEventIds("7, x"), null);
});

test("checks that reach past the device's services and processes need scripts.run", () => {
  for (const condition of ["event_log", "ping", "tcp_port", "http", "certificate", "script_check"]) {
    assert.equal(needsScriptsRun(condition), true, condition);
    assert.match(scriptsRunReason(condition), /needs the scripts\.run permission/);
  }
  for (const condition of ["service", "process", "cpu", "device_offline"]) {
    assert.equal(needsScriptsRun(condition), false, condition);
    assert.equal(scriptsRunReason(condition), null);
  }
  assert.match(scriptsRunReason("ping"), /^This check probes hosts from inside the device’s network/);
});
