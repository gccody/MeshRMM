import assert from "node:assert/strict";
import test from "node:test";
import { classifyHost } from "../lib/hosts.ts";
import { PLATFORM_OWNER, recordD1, startUsage, writeUsage } from "../worker/usage.ts";

test("company hostnames are metered by slug and everything else as platform usage", () => {
  assert.equal(startUsage(classifyHost("acme.meshrmm.com")).owner, "slug:acme");
  for (const hostname of ["meshrmm.com", "admin.meshrmm.com", "auth.meshrmm.com", "www.meshrmm.com", "a.b.meshrmm.com"]) {
    assert.equal(startUsage(classifyHost(hostname)).owner, PLATFORM_OWNER, hostname);
  }
});

test("each invocation writes one data point in the API Worker's format", () => {
  const points = [];
  const usage = startUsage(classifyHost("acme.meshrmm.com"));
  recordD1(usage, { rows_read: 2, rows_written: 1 });
  recordD1(usage, undefined);
  usage.owner = "company-id";
  writeUsage({ writeDataPoint: (point) => points.push(point) }, usage);
  assert.deepEqual(points, [{ indexes: ["company-id"], blobs: ["dashboard"], doubles: [1, 1, 2, 1] }]);
});

test("metering never fails a request", () => {
  const usage = startUsage(classifyHost("meshrmm.com"));
  assert.doesNotThrow(() => writeUsage(undefined, usage));
  assert.doesNotThrow(() => writeUsage({ writeDataPoint: () => { throw new Error("quota"); } }, usage));
});
