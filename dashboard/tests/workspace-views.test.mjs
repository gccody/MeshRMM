import assert from "node:assert/strict";
import test from "node:test";
import { VIEW_COPY, VIEW_PATHS, viewForPath } from "../features/workspace/views.ts";

test("maps each workspace route to its view", () => {
  assert.equal(viewForPath("/"), "agents");
  assert.equal(viewForPath("/settings"), "settings");
  assert.equal(viewForPath("/toolbox"), "toolbox");
  assert.equal(viewForPath("/users"), "team");
  assert.equal(viewForPath("/authentication"), "sso");
  for (const [view, path] of Object.entries(VIEW_PATHS)) {
    assert.equal(viewForPath(path), view);
    assert.ok(VIEW_COPY[view].title);
  }
});

test("ignores a trailing slash", () => {
  assert.equal(viewForPath("/settings/"), "settings");
  assert.equal(viewForPath("/users//"), "team");
  assert.equal(viewForPath("//"), "agents");
});

test("treats unknown or missing paths as Devices", () => {
  assert.equal(viewForPath("/unknown"), "agents");
  assert.equal(viewForPath("/settings/extra"), "agents");
  assert.equal(viewForPath(""), "agents");
  assert.equal(viewForPath(null), "agents");
  assert.equal(viewForPath(undefined), "agents");
});
