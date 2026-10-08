import assert from "node:assert/strict";
import test from "node:test";
import {
  PasskeyPromptError,
  assertionJSON,
  creationOptions,
  fromBase64Url,
  promptError,
  registrationJSON,
  requestOptions,
  toBase64Url,
} from "../lib/webauthn.ts";

const bytes = (...values) => new Uint8Array(values).buffer;
const array = (buffer) => [...new Uint8Array(buffer)];

test("base64url encodes without padding and round-trips any bytes", () => {
  assert.equal(toBase64Url(bytes()), "");
  assert.equal(toBase64Url(bytes(0xfb, 0xff)), "-_8");
  assert.equal(toBase64Url(new TextEncoder().encode("hello")), "aGVsbG8");
  // A view encodes only its own bytes, not the whole buffer beneath it.
  assert.equal(toBase64Url(new Uint8Array([1, 2, 3, 4]).subarray(1, 3)), "AgM");
  const all = new Uint8Array(256).map((_, index) => index);
  for (let length = 0; length <= 256; length += 17) {
    const slice = all.slice(0, length);
    assert.deepEqual(array(fromBase64Url(toBase64Url(slice))), [...slice], `length ${length}`);
  }
  // Padded and standard base64 decode too.
  assert.deepEqual(array(fromBase64Url("-_8=")), [0xfb, 0xff]);
  assert.deepEqual(array(fromBase64Url("+/8")), [0xfb, 0xff]);
});

test("creation options decode their binary fields and keep the rest", () => {
  const options = creationOptions({
    publicKey: {
      rp: { id: "rmm.example.com", name: "Acme" },
      user: { id: "AQID", name: "ada@example.com", displayName: "Ada" },
      challenge: "BAUG",
      pubKeyCredParams: [{ type: "public-key", alg: -7 }],
      timeout: 60000,
      excludeCredentials: [{ type: "public-key", id: "Bwg", transports: ["internal"] }],
      authenticatorSelection: { residentKey: "preferred", userVerification: "preferred" },
      attestation: null,
      extensions: { credProps: true },
    },
  });
  const { publicKey } = options;
  assert.deepEqual(array(publicKey.challenge), [4, 5, 6]);
  assert.deepEqual(array(publicKey.user.id), [1, 2, 3]);
  assert.equal(publicKey.user.name, "ada@example.com");
  assert.equal(publicKey.user.displayName, "Ada");
  assert.deepEqual(array(publicKey.excludeCredentials[0].id), [7, 8]);
  assert.deepEqual(publicKey.excludeCredentials[0].transports, ["internal"]);
  assert.deepEqual(publicKey.rp, { id: "rmm.example.com", name: "Acme" });
  assert.equal(publicKey.timeout, 60000);
  assert.deepEqual(publicKey.extensions, { credProps: true });
  // A null would reach the browser as the string "null".
  assert.equal("attestation" in publicKey, false);
  assert.equal("excludeCredentials" in creationOptions({ publicKey: { challenge: "", user: { id: "", name: "a", displayName: "a" } } }).publicKey, false);
});

test("request options decode their binary fields and leave out mediation", () => {
  const options = requestOptions({
    publicKey: {
      challenge: "AQID",
      timeout: 60000,
      rpId: "rmm.example.com",
      allowCredentials: [{ type: "public-key", id: "BAU" }, { type: "public-key", id: "Bg" }],
      userVerification: "preferred",
    },
    mediation: "conditional",
  });
  assert.deepEqual(Object.keys(options), ["publicKey"]);
  assert.deepEqual(array(options.publicKey.challenge), [1, 2, 3]);
  assert.deepEqual(options.publicKey.allowCredentials.map((credential) => array(credential.id)), [[4, 5], [6]]);
  assert.equal(options.publicKey.rpId, "rmm.example.com");
  assert.equal(options.publicKey.userVerification, "preferred");
  // Passwordless prompts allow any passkey.
  const discoverable = requestOptions({ publicKey: { challenge: "AQID", allowCredentials: [] } });
  assert.deepEqual(discoverable.publicKey.allowCredentials, []);
});

test("a new passkey is sent back as JSON with base64url fields", () => {
  const credential = {
    id: "Bwg",
    rawId: bytes(7, 8),
    type: "public-key",
    response: {
      clientDataJSON: bytes(1, 2),
      attestationObject: new Uint8Array([3, 4, 5]),
      getTransports() {
        // Called as a method: the browser's needs its response as `this`.
        assert.equal(this, credential.response);
        return ["internal", "hybrid"];
      },
    },
    getClientExtensionResults: () => ({ credProps: { rk: true } }),
  };
  assert.deepEqual(registrationJSON(credential), {
    id: "Bwg",
    rawId: "Bwg",
    type: "public-key",
    response: { clientDataJSON: "AQI", attestationObject: "AwQF", transports: ["internal", "hybrid"] },
    extensions: { credProps: { rk: true } },
  });
  // Older browsers have neither transports nor extension results.
  const plain = registrationJSON({ id: "AQ", rawId: bytes(1), type: "public-key", response: { clientDataJSON: bytes(), attestationObject: bytes() } });
  assert.deepEqual(plain.response, { clientDataJSON: "", attestationObject: "" });
  assert.deepEqual(plain.extensions, {});
});

test("a passkey's answer is sent back as JSON with base64url fields", () => {
  const credential = {
    id: "Bwg",
    rawId: bytes(7, 8),
    type: "public-key",
    response: { authenticatorData: bytes(1), clientDataJSON: bytes(2), signature: bytes(3), userHandle: bytes(4) },
    getClientExtensionResults: () => ({ appid: false, buffer: bytes(9) }),
  };
  assert.deepEqual(assertionJSON(credential), {
    id: "Bwg",
    rawId: "Bwg",
    type: "public-key",
    response: { authenticatorData: "AQ", clientDataJSON: "Ag", signature: "Aw", userHandle: "BA" },
    extensions: { appid: false, buffer: "CQ" },
  });
  const withoutHandle = assertionJSON({ ...credential, response: { ...credential.response, userHandle: null }, getClientExtensionResults: undefined });
  assert.equal(withoutHandle.response.userHandle, null);
  assert.deepEqual(withoutHandle.extensions, {});
  assert.equal(JSON.parse(JSON.stringify(withoutHandle)).response.userHandle, null);
});

test("closing the passkey prompt is quiet; other failures explain themselves", () => {
  for (const name of ["NotAllowedError", "AbortError"]) {
    const error = promptError({ name }, "get");
    assert.ok(error instanceof PasskeyPromptError);
    assert.equal(error.cancelled, true, name);
  }
  const exists = promptError({ name: "InvalidStateError" }, "create");
  assert.equal(exists.cancelled, false);
  assert.match(exists.message, /already registered/);
  assert.doesNotMatch(promptError({ name: "InvalidStateError" }, "get").message, /already registered/);
  assert.match(promptError({ name: "SecurityError" }, "get").message, /address/);
  assert.match(promptError(new TypeError("bad"), "create").message, /couldn't be created/);
  assert.match(promptError(null, "get").message, /couldn't be used/);
});
