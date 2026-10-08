import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import {
  VIEWER_DOWNLOADS,
  VIEWER_PLATFORMS,
  detectViewerPlatform,
} from "../features/session/viewer-downloads.ts";
import {
  LAUNCH_DETECT_MS,
  LAUNCH_WATCH_MS,
  isOpeningExternalLink,
  openExternalLink,
  watchViewerLaunch,
} from "../features/session/viewer-launch.ts";

const WINDOWS_CHROME = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const MAC_SAFARI = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
const LINUX_FIREFOX = "Mozilla/5.0 (X11; Linux x86_64; rv:142.0) Gecko/20100101 Firefox/142.0";
const ANDROID_CHROME = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Mobile Safari/537.36";

test("the viewer build follows the browser's platform", () => {
  const cases = [
    ["Windows Chrome", { userAgent: WINDOWS_CHROME, maxTouchPoints: 0 }, "windows-x64"],
    ["UA-CH Windows with a reduced user agent", { userAgent: "Mozilla/5.0", userAgentData: { platform: "Windows" } }, "windows-x64"],
    ["Windows touch laptop", { userAgent: WINDOWS_CHROME, maxTouchPoints: 10 }, "windows-x64"],
    ["macOS Safari", { userAgent: MAC_SAFARI, maxTouchPoints: 0 }, "macos-arm64"],
    ["macOS with a touch-emulating extension", { userAgent: MAC_SAFARI, maxTouchPoints: 1 }, "macos-arm64"],
    ["iPad asking for the desktop site", { userAgent: MAC_SAFARI, maxTouchPoints: 5 }, null],
    ["Linux", { userAgent: LINUX_FIREFOX, maxTouchPoints: 0 }, null],
    ["Android", { userAgent: ANDROID_CHROME, maxTouchPoints: 5, userAgentData: { platform: "Android" } }, null],
    ["no navigator (server render)", null, null],
  ];
  for (const [name, nav, expected] of cases) {
    assert.equal(detectViewerPlatform(nav), expected, name);
  }
});

test("viewer downloads are the builds the release publishes", async () => {
  const releaseAssets = await readFile(new URL("../../scripts/release-artifacts.mjs", import.meta.url), "utf8");
  for (const platform of VIEWER_PLATFORMS) {
    const { href } = VIEWER_DOWNLOADS[platform];
    assert.match(href, /^\/downloads\/[^/]+$/);
    assert.ok(releaseAssets.includes(`"client-${platform}": "${href.slice("/downloads/".length)}"`), href);
  }
});

function fakeEnv({ visibilityState = "visible", focused = true } = {}) {
  const document = Object.assign(new EventTarget(), {
    visibilityState,
    hasFocus: () => focused,
  });
  return {
    window: new EventTarget(),
    document,
    setTimeout: (callback, delayMs) => setTimeout(callback, delayMs),
    clearTimeout: (handle) => clearTimeout(handle),
  };
}

function watch(env) {
  const outcomes = [];
  const cancel = watchViewerLaunch(env, (outcome) => outcomes.push(outcome));
  return { outcomes, cancel };
}

test("losing focus soon after the link opens is a handoff", (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const env = fakeEnv();
  const { outcomes } = watch(env);
  t.mock.timers.tick(1_000);
  env.window.dispatchEvent(new Event("blur"));
  assert.deepEqual(outcomes, ["handed-off"]);
  // Later signals and timers report nothing more.
  env.window.dispatchEvent(new Event("blur"));
  t.mock.timers.tick(LAUNCH_WATCH_MS);
  assert.deepEqual(outcomes, ["handed-off"]);
});

test("pagehide and a hidden page also count as a handoff", (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const hidden = fakeEnv();
  const first = watch(hidden);
  hidden.document.visibilityState = "hidden";
  hidden.document.dispatchEvent(new Event("visibilitychange"));
  assert.deepEqual(first.outcomes, ["handed-off"]);

  const unloaded = fakeEnv();
  const second = watch(unloaded);
  unloaded.window.dispatchEvent(new Event("pagehide"));
  assert.deepEqual(second.outcomes, ["handed-off"]);

  // Becoming visible is not a signal.
  const shown = fakeEnv();
  const third = watch(shown);
  shown.document.dispatchEvent(new Event("visibilitychange"));
  assert.deepEqual(third.outcomes, []);
});

test("no signal is reported as not detected, and a later one upgrades it", (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const env = fakeEnv();
  const { outcomes } = watch(env);
  t.mock.timers.tick(LAUNCH_DETECT_MS - 1);
  assert.deepEqual(outcomes, []);
  t.mock.timers.tick(1);
  assert.deepEqual(outcomes, ["not-detected"]);
  t.mock.timers.tick(30_000 - LAUNCH_DETECT_MS);
  env.window.dispatchEvent(new Event("blur"));
  assert.deepEqual(outcomes, ["not-detected", "handed-off"]);
});

test("signals after the handoff token expires are ignored", (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const env = fakeEnv();
  const { outcomes } = watch(env);
  t.mock.timers.tick(61_000);
  env.window.dispatchEvent(new Event("blur"));
  env.window.dispatchEvent(new Event("pagehide"));
  assert.deepEqual(outcomes, ["not-detected"]);
});

test("a cancelled watch reports nothing", (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const env = fakeEnv();
  const { outcomes, cancel } = watch(env);
  t.mock.timers.tick(500);
  cancel();
  env.window.dispatchEvent(new Event("blur"));
  t.mock.timers.tick(LAUNCH_WATCH_MS);
  assert.deepEqual(outcomes, []);
  cancel();
});

test("a page that is already hidden or unfocused cannot tell", (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  for (const state of [{ visibilityState: "hidden" }, { focused: false }]) {
    const env = fakeEnv(state);
    const { outcomes } = watch(env);
    env.window.dispatchEvent(new Event("blur"));
    t.mock.timers.tick(LAUNCH_WATCH_MS);
    assert.deepEqual(outcomes, ["unknown"]);
  }
});

test("leave-page guards can tell an external link from leaving", () => {
  const seen = [];
  openExternalLink("meshrmm://connect?handoff=x", { assign: (url) => seen.push([url, isOpeningExternalLink()]) });
  assert.deepEqual(seen, [["meshrmm://connect?handoff=x", true]]);
  assert.equal(isOpeningExternalLink(), false);
  assert.throws(() => openExternalLink("meshrmm://x", { assign: () => { throw new Error("blocked"); } }), /blocked/);
  assert.equal(isOpeningExternalLink(), false);
});
