import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_APPLICATION_SORT,
  describeOs,
  formatLinkSpeed,
  installedOnParams,
  nextSort,
  parseSoftwareFilters,
  softwareParams,
  sortAdapters,
  sourceLabel,
  visibleApplications,
} from "../features/system-info/model.ts";

const app = (name, version, publisher = null, installed_at = null, source = "windows") => ({ name, version, publisher, architecture: null, installed_at, source });

const applications = [
  app("zoom", "6.2", "Zoom Video Communications", 300),
  app("7-Zip 24.08 (x64)", "24.08", "Igor Pavlov", 100),
  app("Firefox", "130.0", "Mozilla", null),
  app("firefox-esr", "115.9", null, 200, "dpkg"),
  app("Notepad++", null, "Notepad++ Team", 50),
];
const names = (list) => list.map((entry) => entry.name);

test("applications sort by name naturally, ignoring case", () => {
  assert.deepEqual(names(visibleApplications(applications, "", DEFAULT_APPLICATION_SORT)), ["7-Zip 24.08 (x64)", "Firefox", "firefox-esr", "Notepad++", "zoom"]);
  assert.deepEqual(names(visibleApplications(applications, "", { key: "name", direction: "descending" })), ["zoom", "Notepad++", "firefox-esr", "Firefox", "7-Zip 24.08 (x64)"]);
});

test("missing values sort last in either direction", () => {
  assert.deepEqual(names(visibleApplications(applications, "", { key: "installed_at", direction: "descending" })), ["zoom", "firefox-esr", "7-Zip 24.08 (x64)", "Notepad++", "Firefox"]);
  assert.deepEqual(names(visibleApplications(applications, "", { key: "installed_at", direction: "ascending" })), ["Notepad++", "7-Zip 24.08 (x64)", "firefox-esr", "zoom", "Firefox"]);
  assert.deepEqual(names(visibleApplications(applications, "", { key: "version", direction: "ascending" })).at(-1), "Notepad++");
  assert.deepEqual(names(visibleApplications(applications, "", { key: "publisher", direction: "ascending" })).at(-1), "firefox-esr");
  assert.deepEqual(names(visibleApplications(applications, "", { key: "source", direction: "ascending" }))[0], "firefox-esr");
});

test("the search matches names, publishers and versions", () => {
  assert.deepEqual(names(visibleApplications(applications, " FIREFOX ", DEFAULT_APPLICATION_SORT)), ["Firefox", "firefox-esr"]);
  assert.deepEqual(names(visibleApplications(applications, "mozilla", DEFAULT_APPLICATION_SORT)), ["Firefox"]);
  assert.deepEqual(names(visibleApplications(applications, "24.08", DEFAULT_APPLICATION_SORT)), ["7-Zip 24.08 (x64)"]);
  assert.deepEqual(visibleApplications(applications, "nothing", DEFAULT_APPLICATION_SORT), []);
});

test("clicking a heading sorts by it, and again reverses it", () => {
  assert.deepEqual(nextSort(DEFAULT_APPLICATION_SORT, "name"), { key: "name", direction: "descending" });
  assert.deepEqual(nextSort(DEFAULT_APPLICATION_SORT, "publisher"), { key: "publisher", direction: "ascending" });
  assert.deepEqual(nextSort(DEFAULT_APPLICATION_SORT, "installed_at"), { key: "installed_at", direction: "descending" });
});

test("operating systems, sources and link speeds read naturally", () => {
  assert.equal(describeOs({ name: "Windows 11 Pro", version: "24H2", build: "26100.4652" }), "Windows 11 Pro 24H2 (26100.4652)");
  assert.equal(describeOs({ name: "Arch Linux", version: "rolling", build: null }), "Arch Linux rolling");
  assert.equal(sourceLabel("windows_user"), "Windows, one user");
  assert.equal(sourceLabel("something_new"), "something_new");
  assert.equal(formatLinkSpeed(10_000_000_000), "10 Gbps");
  assert.equal(formatLinkSpeed(2_500_000_000), "2.5 Gbps");
  assert.equal(formatLinkSpeed(100_000_000), "100 Mbps");
  assert.equal(formatLinkSpeed(null), null);
});

test("hardware adapters come first, up before down", () => {
  const adapter = (name, kind, up) => ({ name, kind, up });
  const sorted = sortAdapters([adapter("vEthernet", "virtual", true), adapter("Wi-Fi", "wireless", false), adapter("Ethernet 2", "ethernet", true), adapter("Ethernet", "ethernet", true)]);
  assert.deepEqual(sorted.map((entry) => entry.name), ["Ethernet", "Ethernet 2", "Wi-Fi", "vEthernet"]);
});

test("the software search keeps its filters in the address", () => {
  assert.deepEqual(parseSoftwareFilters(new URLSearchParams("q=chrome&company=none")), { query: "chrome", company: "none" });
  assert.deepEqual(parseSoftwareFilters(null), { query: "", company: "" });
  assert.equal(parseSoftwareFilters(new URLSearchParams({ q: "x".repeat(500) })).query.length, 200);
  assert.equal(softwareParams({ query: "  7-zip ", company: "" }).toString(), "q=7-zip");
  assert.equal(softwareParams({ query: "", company: "acme" }).toString(), "company=acme");
  assert.equal(installedOnParams("Notepad++", undefined, "").toString(), "name=Notepad%2B%2B");
  assert.equal(installedOnParams("App", "1.0", "none").toString(), "name=App&version=1.0&company=none");
  assert.equal(installedOnParams("App", null, "").toString(), "name=App&unversioned=true");
});
