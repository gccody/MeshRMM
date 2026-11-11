import assert from "node:assert/strict";
import test from "node:test";
import {
  applicationDraft,
  applicationDraftProblem,
  applicationInput,
  applicationsFor,
  describeSource,
  describeUpdate,
  filterPackages,
  installersChanged,
  chosenUpdates,
  joinDevicePage,
  namesText,
  outdatedDevicesQuery,
  outdatedLabel,
  outdatedQuery,
  outdatedSummary,
  packageKey,
  platformsOf,
  itemTone,
  runCounts,
  runTone,
  sourceDraft,
  uninstallableInventory,
  validPackageId,
  validVersion,
  versionChange,
} from "../features/applications/model.ts";
import { JOB_KIND_LABELS, SAVE_PERMISSION, describePolicyAction, isApplicationJob, policyDraft, policyDraftProblem, policyInput } from "../features/automation/model.ts";
import { CONDITION_LABELS, clearsItself, ruleForm, ruleFormProblem, ruleInput, usesDays, withCondition } from "../features/alerts/model.ts";
import { canView, navViewFor, viewForPath } from "../features/workspace/views.ts";

const firefox = {
  id: "a1",
  name: "Firefox",
  description: "",
  pinned_version: null,
  sources: [
    { source: "winget", platform: "windows", package_id: "Mozilla.Firefox" },
    { source: "homebrew_cask", platform: "macos", package_id: "firefox" },
  ],
  created_at: 1,
  updated_at: 1,
  installed: 3,
  outdated: 1,
  policies: [],
};

test("package IDs are checked as the server checks them", () => {
  assert.ok(validPackageId("winget", "Mozilla.Firefox"));
  assert.ok(validPackageId("winget", "9NBLGGH4NNS1"));
  assert.ok(validPackageId("winget", "Python.Python.3.12"));
  assert.ok(validPackageId("homebrew", "node@20"));
  assert.ok(validPackageId("homebrew_cask", "homebrew/cask-fonts/font-fira-code"));
  assert.ok(validPackageId("flatpak", "org.mozilla.firefox"));
  assert.ok(validPackageId("snap", "code"));
  for (const [source, id] of [
    ["winget", ""],
    ["winget", "--all"],
    ["winget", "-h"],
    ["winget", "Git.Git; calc"],
    ["winget", "Git Git"],
    ["chocolatey", "git&calc"],
    ["chocolatey", "a+b"],
    ["homebrew", "../etc"],
    ["flatpak", "org.mozilla..firefox"],
    ["snap", "Code"],
    ["snap", "code.x"],
    ["winget", "a".repeat(201)],
  ]) assert.equal(validPackageId(source, id), false, `${source} ${id}`);
  assert.ok(validVersion("1.2.3-beta+4"));
  assert.ok(validVersion("2:1.0~rc1"));
  assert.equal(validVersion("-1"), false);
  assert.equal(validVersion("1 2"), false);
  assert.equal(validVersion(""), false);
});

test("the library editor checks drafts and builds the server's input", () => {
  const draft = { ...applicationDraft(null), name: " Firefox " };
  draft.sources = [{ ...draft.sources[0], packageId: " Mozilla.Firefox " }];
  assert.equal(applicationDraftProblem(draft), null);
  assert.deepEqual(applicationInput(draft), {
    name: "Firefox",
    description: "",
    pinned_version: null,
    sources: [{ source: "winget", package_id: "Mozilla.Firefox" }],
  });

  const custom = sourceDraft({ source: "custom" });
  assert.equal(custom.platform, "windows");
  const installer = { ...custom, fileId: "f1", installArguments: " /S ", detection: "registry", detectionKey: "HKLM\\SOFTWARE\\7-Zip" };
  const withInstaller = { ...draft, pinnedVersion: "24.08", sources: [installer] };
  assert.equal(applicationDraftProblem(withInstaller), null);
  assert.deepEqual(applicationInput(withInstaller).sources, [{
    source: "custom",
    platform: "windows",
    file_id: "f1",
    install_arguments: "/S",
    uninstall_command: null,
    detection: { kind: "registry", key: "HKLM\\SOFTWARE\\7-Zip" },
  }]);
  assert.equal(applicationInput(withInstaller).pinned_version, "24.08");
  const named = { ...installer, detection: "application", detectionName: "7-Zip*", detectionMinVersion: "24.08" };
  assert.deepEqual(applicationInput({ ...draft, sources: [named] }).sources[0].detection, { kind: "application", name: "7-Zip*", min_version: "24.08" });

  const problems = [
    { name: "" },
    { name: "x".repeat(121) },
    { pinnedVersion: "1 2" },
    { sources: [] },
    { sources: Array.from({ length: 13 }, (_, index) => ({ ...draft.sources[0], packageId: `App.N${index}` })) },
    { sources: [{ ...draft.sources[0], packageId: "" }] },
    { sources: [{ ...draft.sources[0], packageId: "--silent" }] },
    { sources: [draft.sources[0], { ...draft.sources[0], packageId: "mozilla.firefox" }] },
    { sources: [{ ...installer, fileId: "" }] },
    { sources: [{ ...installer, installArguments: "/S\n& calc" }] },
    { sources: [{ ...installer, uninstallCommand: "x".repeat(2001) }] },
    { sources: [{ ...installer, detectionKey: "SOFTWARE\\7-Zip" }] },
    { sources: [{ ...installer, platform: "macos" }] },
    { sources: [{ ...named, detectionName: "" }] },
    { sources: [{ ...named, detectionMinVersion: "v 1" }] },
    { sources: [{ ...installer, detection: "file", detectionPath: "" }] },
  ];
  for (const change of problems) assert.ok(applicationDraftProblem({ ...draft, ...change }), JSON.stringify(change));
});

test("applications are edited as they are stored", () => {
  const draft = applicationDraft({
    ...firefox,
    pinned_version: "128.0",
    sources: [...firefox.sources, { source: "custom", platform: "linux", package_id: "", file_id: "f", file_name: "app.deb", detection: { kind: "file", path: "/opt/app" }, uninstall_command: "apt-get remove -y app" }],
  });
  assert.equal(draft.pinnedVersion, "128.0");
  assert.equal(draft.sources.length, 3);
  assert.equal(draft.sources[1].platform, "macos");
  assert.equal(draft.sources[2].detection, "file");
  assert.equal(draft.sources[2].detectionPath, "/opt/app");
  assert.equal(applicationDraftProblem(draft), null);
  assert.equal(applicationInput(draft).sources[2].uninstall_command, "apt-get remove -y app");
});

test("adding or changing a custom installer is told apart, as it needs scripts.run", () => {
  const vendor = {
    ...firefox,
    sources: [...firefox.sources, { source: "custom", platform: "windows", package_id: "", file_id: "f", file_name: "vendor.msi", install_arguments: "/qn", detection: { kind: "application", name: "Vendor*" } }],
  };
  const draft = applicationDraft(vendor);
  assert.equal(installersChanged(vendor, draft), false, "unchanged");
  assert.equal(installersChanged(vendor, { ...draft, name: "Renamed", sources: [draft.sources[2], { ...draft.sources[0], packageId: "Other.App" }] }), false, "packages and order");
  assert.equal(installersChanged(vendor, { ...draft, sources: draft.sources.slice(0, 2) }), false, "removed");
  const custom = draft.sources[2];
  for (const change of [{ installArguments: "/qn & calc" }, { uninstallCommand: "calc" }, { fileId: "g" }, { platform: "linux" }, { detectionName: "Other*" }, { detection: "file", detectionPath: "C:\\App\\app.exe" }]) {
    assert.equal(installersChanged(vendor, { ...draft, sources: [...draft.sources.slice(0, 2), { ...custom, ...change }] }), true, JSON.stringify(change));
  }
  assert.equal(installersChanged(null, draft), true, "a new application with one");
  assert.equal(installersChanged(null, applicationDraft(firefox)), false, "packages only");
});

test("library applications, packages and runs read as words", () => {
  assert.deepEqual(platformsOf(firefox), ["windows", "macos"]);
  assert.deepEqual(applicationsFor([firefox], "linux"), []);
  assert.equal(applicationsFor([firefox], "macos").length, 1);
  assert.equal(describeSource(firefox.sources[0]), "winget Mozilla.Firefox");
  assert.equal(describeSource({ source: "custom", platform: "windows", package_id: "", file_id: "f" }), "Deleted file");
  assert.equal(namesText(["Firefox"]), "Firefox");
  assert.equal(namesText(["Firefox", "7-Zip"]), "Firefox and 7-Zip");
  assert.equal(namesText(["a", "b", "c"]), "3 applications");
  assert.equal(describeUpdate("all", []), "Update all applications");
  assert.equal(describeUpdate("only", ["Git"]), "Update Git");
  assert.equal(describeUpdate("except", ["Git", "Zoom"]), "Update all applications except Git and Zoom");
  assert.equal(versionChange({ version: "1.0", available_version: "1.1" }), "1.0 → 1.1");
  assert.equal(versionChange({ version: "1.0", available_version: null }), "1.0");
  assert.equal(packageKey({ source: "winget", package_id: "Git.Git" }), "winget:Git.Git");
  assert.equal(runTone("succeeded"), "success");
  assert.equal(runTone("pending"), "pending");
  for (const status of ["partial", "failed", "lost"]) assert.equal(runTone(status), "problem");
  assert.equal(runTone("skipped"), "muted");
  assert.deepEqual(["succeeded", "unchanged", "skipped", "failed"].map(itemTone), ["success", "success", "muted", "problem"]);
});

test("the fleet's outdated packages are asked for a page at a time", () => {
  // The search is trimmed and left out when empty; the first page has no offset.
  assert.equal(outdatedQuery("", 0), "limit=100");
  assert.equal(outdatedQuery("  fire fox ", 200, 50), "query=fire+fox&limit=50&offset=200");
  const git = { source: "winget", package_id: "Git.Git" };
  assert.equal(outdatedDevicesQuery(git, null), "package=winget%3AGit.Git&limit=100");
  assert.equal(outdatedDevicesQuery(git, "d2", 2), "package=winget%3AGit.Git&limit=2&after=d2");

  const device = (id) => ({ device_id: id, device_name: id, version: "1", available_version: "2", outdated_since: 1 });
  const first = { devices: [device("d1"), device("d2")], next: "d2" };
  const second = joinDevicePage(first, "d2", { devices: [device("d2"), device("d3")], next: null });
  assert.deepEqual(second.devices.map((shown) => shown.device_id), ["d1", "d2", "d3"], "a device already shown isn't shown again");
  assert.equal(second.next, null, "the last page ends the list");
  assert.equal(joinDevicePage(second, "d2", { devices: [device("d4")], next: null }), second, "a page asked after another than the last one shown changes nothing");

  // Pinned packages can't be chosen, so these count every update chosen.
  assert.equal(chosenUpdates([{ devices: 3 }, { devices: 2 }]), 5);
  assert.equal(chosenUpdates([]), 0);
  assert.equal(outdatedLabel(["Firefox"]), "Firefox is outdated");
  assert.equal(outdatedLabel(["Firefox", "7-Zip"]), "any of Firefox and 7-Zip is outdated");
  assert.equal(outdatedLabel([]), "an application that isn't pinned is outdated");
  assert.equal(outdatedSummary(1, false), "1 application, on the most devices first");
  assert.equal(outdatedSummary(3, true), "3 applications match");
});

test("device packages filter", () => {
  const packages = [
    { source: "winget", package_id: "Git.Git", name: "Git", version: "2.44", available_version: "2.45", outdated_since: 5, application_id: null, application_name: null, held: false },
    { source: "chocolatey", package_id: "7zip", name: "7-Zip", version: "24.08", available_version: null, outdated_since: null, application_id: "a", application_name: "Archiver", held: false },
  ];
  assert.deepEqual(filterPackages(packages, "", true).map((pkg) => pkg.name), ["Git"]);
  assert.deepEqual(filterPackages(packages, "archiver", false).map((pkg) => pkg.name), ["7-Zip"]);
  assert.deepEqual(filterPackages(packages, "GIT.git", false).map((pkg) => pkg.name), ["Git"]);
  assert.deepEqual(uninstallableInventory([{ name: "a", source: "windows" }, { name: "b", source: "windows_user" }, { name: "c", source: "dpkg" }]).map((program) => program.name), ["a"]);
});

test("application policies check their applications and become the server's input", () => {
  const base = { ...policyDraft(null, "UTC", "app_install"), name: "Browsers" };
  assert.ok(policyDraftProblem(base), "an install policy names applications");
  const install = { ...base, applicationIds: ["a1"] };
  assert.equal(policyDraftProblem(install), null);
  assert.deepEqual(policyInput(install).applications, { application_ids: ["a1"], packages: [], mode: "all" });
  assert.equal(policyInput(install).action, "app_install");

  const update = { ...base, action: "app_update" };
  assert.equal(policyDraftProblem(update), null, "update all");
  assert.deepEqual(policyInput({ ...update, applicationIds: ["a1"], packages: "winget:Git.Git" }).applications, { application_ids: [], packages: [], mode: "all" }, "updating all names nothing");
  assert.ok(policyDraftProblem({ ...update, updateMode: "only" }));
  assert.ok(policyDraftProblem({ ...update, updateMode: "except", packages: "Git.Git" }));
  assert.ok(policyDraftProblem({ ...update, updateMode: "except", packages: "winget:--all" }));
  const except = { ...update, updateMode: "except", applicationIds: ["a1"], packages: " winget:Git.Git \n\nhomebrew_cask:zoom\n" };
  assert.equal(policyDraftProblem(except), null);
  assert.deepEqual(policyInput(except).applications, { application_ids: ["a1"], packages: ["winget:Git.Git", "homebrew_cask:zoom"], mode: "except" });

  const edited = policyDraft({ ...policyDraft(null, "UTC"), action: "app_update", applications: { application_ids: ["a1"], packages: ["snap:code"], mode: "only" } }, "UTC");
  assert.equal(edited.packages, "snap:code");
  assert.equal(edited.updateMode, "only");
  assert.deepEqual(edited.applicationIds, ["a1"]);

  assert.equal(describePolicyAction({ action: "app_install", script_name: null, applications: { application_ids: ["a", "b"], packages: [], mode: "all" } }), "Install 2 applications");
  assert.equal(describePolicyAction({ action: "app_update", script_name: null, applications: { application_ids: [], packages: [], mode: "all" } }), "Update all applications");
  assert.equal(describePolicyAction({ action: "app_update", script_name: null, applications: { application_ids: ["a"], packages: [], mode: "except" } }), "Update all applications except 1 application");
  assert.equal(SAVE_PERMISSION.app_install, "applications.deploy");
  assert.equal(SAVE_PERMISSION.app_update, "applications.deploy");
  assert.equal(JOB_KIND_LABELS.app_uninstall, "Application uninstall");
  assert.ok(isApplicationJob("app_update"));
  assert.equal(isApplicationJob("update_install"), false);
});

test("application alert rules take days and failures stay", () => {
  assert.equal(CONDITION_LABELS.app_install_failed, "Application install failed");
  assert.equal(clearsItself("app_install_failed"), false);
  assert.equal(clearsItself("app_updates_pending"), true);
  assert.ok(usesDays("app_updates_pending"));
  const form = { ...withCondition(ruleForm(null), "app_updates_pending"), name: "Stale apps" };
  assert.equal(ruleFormProblem(form), null);
  assert.equal(ruleInput(form).days, 14);
  assert.equal(ruleInput(form).update_category, null);
  assert.ok(ruleFormProblem({ ...form, days: "0" }));
  const failed = { ...withCondition(ruleForm(null), "app_install_failed"), name: "Failed installs" };
  assert.equal(ruleFormProblem(failed), null);
  assert.equal(ruleInput(failed).days, null);
});

test("the Applications page is for those who may see applications", () => {
  const account = (permissions) => ({ permissions, is_administrator: false });
  assert.equal(viewForPath("/applications"), "applications");
  assert.equal(navViewFor("applications"), "applications");
  assert.equal(canView(account(["applications.view"]), "applications"), true);
  assert.equal(canView(account(["updates.view"]), "applications"), false);
  assert.equal(canView(account(["applications.deploy"]), "jobs"), true);
});

test("a run's counts say what it did, and when it had nothing to do", () => {
  const run = (action, counts, status = "succeeded") => ({ action, status, succeeded: 0, unchanged: 0, skipped: 0, failed: 0, ...counts });
  assert.equal(runCounts(run("update", { unchanged: 2 })), "Nothing to do: 2 applications already up to date");
  assert.equal(runCounts(run("install", { unchanged: 1 })), "Nothing to do: 1 application already installed");
  assert.equal(runCounts(run("uninstall", { unchanged: 3 })), "Nothing to do: 3 applications already not installed");
  assert.equal(runCounts(run("update", {})), "Nothing to do: nothing was outdated");
  assert.equal(runCounts(run("install", {}, "failed")), "");
  assert.equal(runCounts(run("install", { skipped: 2 }, "skipped")), "2 skipped");
  assert.equal(runCounts(run("update", { succeeded: 2, unchanged: 1, skipped: 1, failed: 1 }, "partial")), "2 done, 1 already up to date, 1 skipped, 1 failed");
});
