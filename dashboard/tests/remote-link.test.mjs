import assert from "node:assert/strict";
import test from "node:test";
import { remoteViewerLink } from "../features/session/remote-link.ts";

test("the viewer link carries the handoff, server and device", () => {
  const link = new URL(remoteViewerLink("ab".repeat(32), "https://acme.meshrmm.com", "3f2a9c1e-0b7d"));
  assert.equal(link.protocol, "meshrmm:");
  assert.equal(link.host, "connect");
  assert.equal(link.searchParams.get("handoff"), "ab".repeat(32));
  assert.equal(link.searchParams.get("server"), "https://acme.meshrmm.com");
  assert.equal(link.searchParams.get("device"), "3f2a9c1e-0b7d");
});

test("the viewer link encodes reserved characters", () => {
  const link = new URL(remoteViewerLink("t", "https://x.example/a?b=c&d", "a&b=c"));
  assert.equal(link.searchParams.get("server"), "https://x.example/a?b=c&d");
  assert.equal(link.searchParams.get("device"), "a&b=c");
});
