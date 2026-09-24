import assert from "node:assert/strict";
import test from "node:test";
import { classifyHost } from "../lib/hosts.ts";
import { fixtureSql } from "../scripts/seed-local.mjs";

const production = {
  "meshrmm.com": { surface: "marketing", rootDomain: "meshrmm.com" },
  "admin.meshrmm.com": { surface: "platform", rootDomain: "meshrmm.com" },
  "auth.meshrmm.com": { surface: "auth", rootDomain: "meshrmm.com" },
  "www.meshrmm.com": { surface: "www", rootDomain: "meshrmm.com" },
  "acme.meshrmm.com": { surface: "tenant", rootDomain: "meshrmm.com", slug: "acme" },
  "a1-b2.meshrmm.com": { surface: "tenant", rootDomain: "meshrmm.com", slug: "a1-b2" },
  "api.meshrmm.com": { surface: "unknown" },
  "downloads.meshrmm.com": { surface: "unknown" },
  "a.meshrmm.com": { surface: "unknown" },
  "-acme.meshrmm.com": { surface: "unknown" },
  "a.b.meshrmm.com": { surface: "unknown" },
  "meshrmm.com.evil.test": { surface: "unknown" },
  "evilmeshrmm.com": { surface: "unknown" },
  "localhost": { surface: "unknown" },
  "acme.localhost": { surface: "unknown" },
};

test("production hosts are classified the same with or without the development setting", () => {
  for (const devRootDomain of [undefined, "", "localhost", "meshrmm.com", "example.com"]) {
    for (const [hostname, expected] of Object.entries(production)) {
      if (devRootDomain === "localhost" && hostname.endsWith("localhost")) continue;
      assert.deepEqual(classifyHost(hostname, devRootDomain), expected, `${hostname} with ${devRootDomain}`);
    }
  }
});

test("only MESHRMM_DEV_ROOT_DOMAIN=localhost maps localhost names", () => {
  const dev = (hostname) => classifyHost(hostname, "localhost");
  assert.deepEqual(dev("localhost"), { surface: "marketing", rootDomain: "localhost" });
  assert.deepEqual(dev("admin.localhost"), { surface: "platform", rootDomain: "localhost" });
  assert.deepEqual(dev("www.localhost"), { surface: "www", rootDomain: "localhost" });
  assert.deepEqual(dev("acme.localhost"), { surface: "tenant", rootDomain: "localhost", slug: "acme" });
  assert.deepEqual(dev("api.localhost"), { surface: "unknown" });
  assert.deepEqual(dev("a.b.localhost"), { surface: "unknown" });
  assert.deepEqual(dev("notlocalhost"), { surface: "unknown" });
  // Any other value, such as a real domain, is ignored.
  assert.deepEqual(classifyHost("acme.example.com", "example.com"), { surface: "unknown" });
  assert.deepEqual(classifyHost("example.com", "example.com"), { surface: "unknown" });
});

test("the local seed accepts only a valid slug and WorkOS organization ID", () => {
  const sql = fixtureSql("acme", "org_01ABC", 1000);
  assert.match(sql, /VALUES \('local-acme', 'acme \(local\)', 1000, 'acme', 'active', 'org_01ABC', 1000\)/);
  assert.match(sql, /'acme\.localhost', 'local-acme', 'primary', 1000/);
  for (const [slug, organization] of [["a'; DROP TABLE companies; --", "org_1"], ["acme", "org_1'"], ["Acme", "org_1"], ["acme", "user_1"]]) {
    assert.throws(() => fixtureSql(slug, organization, 1000));
  }
});
