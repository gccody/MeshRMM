import assert from "node:assert/strict";
import test from "node:test";
import { MAX_CONNECTION_REASON_BYTES, isConnectionReasonValid } from "../features/session/connection-reason.ts";

test("a connection reason is optional, bounded in UTF-8 bytes, and may span lines", () => {
  assert.equal(MAX_CONNECTION_REASON_BYTES, 500);
  assert.equal(isConnectionReasonValid(""), true);
  assert.equal(isConnectionReasonValid("Printer queue\nticket 42"), true);
  assert.equal(isConnectionReasonValid("a".repeat(500)), true);
  assert.equal(isConnectionReasonValid("a".repeat(501)), false);
  assert.equal(isConnectionReasonValid("é".repeat(250)), true);
  assert.equal(isConnectionReasonValid("é".repeat(251)), false);
});

test("control characters other than newlines are rejected, as the server does", () => {
  for (const reason of ["tab\there", "bell\u0007", "delete\u007f", "c1\u0085"]) {
    assert.equal(isConnectionReasonValid(reason), false, JSON.stringify(reason));
  }
});
