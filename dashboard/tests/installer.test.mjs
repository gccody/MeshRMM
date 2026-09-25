import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import {
  ENROLLMENT_MAGIC,
  enrolledInstaller,
  publishedChecksum,
  sha256Hex,
} from "../features/enrollment/installer.ts";

const digest = "ab".repeat(32);

test("the published checksum is the first field of a sha256sum line", () => {
  assert.equal(publishedChecksum(`${digest}  meshrmm-agent-windows-x64.exe\n`), digest);
  assert.equal(publishedChecksum(`  ${digest.toUpperCase()}\n`), digest);
  assert.equal(publishedChecksum(""), null);
  assert.equal(publishedChecksum("ab".repeat(31)), null);
  assert.equal(publishedChecksum(`${"zz".repeat(32)}  file.exe`), null);
});

test("the digest is lowercase hex SHA-256", async () => {
  const data = new TextEncoder().encode("MeshRMM");
  assert.equal(await sha256Hex(data.buffer), createHash("sha256").update(data).digest("hex"));
});

test("the enrollment follows the binary with its length and magic at the end", async () => {
  const binary = new Uint8Array([0x4d, 0x5a, 1, 2, 3]);
  const bootstrap = { server: "https://acme.meshrmm.com", install_token: "t0ken", expires_at_unix_ms: 1_700_000_000_000 };
  const installer = enrolledInstaller(binary.buffer, bootstrap);
  assert.equal(installer.type, "application/vnd.microsoft.portable-executable");

  const bytes = new Uint8Array(await installer.arrayBuffer());
  const magic = new TextEncoder().encode(ENROLLMENT_MAGIC);
  const config = new TextEncoder().encode(JSON.stringify(bootstrap));
  assert.equal(bytes.length, binary.length + config.length + 8 + magic.length);
  assert.deepEqual(bytes.subarray(0, binary.length), binary);
  assert.deepEqual(bytes.subarray(bytes.length - magic.length), magic);

  const lengthOffset = bytes.length - magic.length - 8;
  const configLength = new DataView(bytes.buffer, lengthOffset, 8).getBigUint64(0, true);
  assert.equal(configLength, BigInt(config.length));
  const configBytes = bytes.subarray(lengthOffset - config.length, lengthOffset);
  assert.deepEqual(JSON.parse(new TextDecoder().decode(configBytes)), bootstrap);
});
