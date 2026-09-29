// The optional reason a technician gives when a company asks its users to
// approve connections. Imports carry the .ts extension so node:test can load
// this module directly.

// The server stores at most 500 bytes of UTF-8.
export const MAX_CONNECTION_REASON_BYTES = 500;

// Newlines are allowed; other control characters are not.
// eslint-disable-next-line no-control-regex
const CONTROL_EXCEPT_NEWLINE = /[\u0000-\u0009\u000b-\u001f\u007f-\u009f]/u;

export function isConnectionReasonValid(reason: string) {
  return new TextEncoder().encode(reason).length <= MAX_CONNECTION_REASON_BYTES && !CONTROL_EXCEPT_NEWLINE.test(reason);
}
