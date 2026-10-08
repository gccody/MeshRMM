// Passkeys (WebAuthn). The server sends its prompts as JSON with every binary
// field in base64url, and takes the browser's answer the same way; these
// functions convert between that JSON and what navigator.credentials takes
// and returns. The conversions need nothing but atob and btoa, so node:test
// can load this module directly.

type Base64Url = string;

type CredentialDescriptorJSON = { type: string; id: Base64Url; transports?: string[] };

// A prompt to create a passkey (webauthn-rs's CreationChallengeResponse).
export type CreationOptionsJSON = {
  publicKey: {
    challenge: Base64Url;
    user: { id: Base64Url; name: string; displayName: string };
    excludeCredentials?: CredentialDescriptorJSON[];
    [option: string]: unknown;
  };
};

// A prompt to use a passkey (webauthn-rs's RequestChallengeResponse).
export type RequestOptionsJSON = {
  publicKey: {
    challenge: Base64Url;
    allowCredentials?: CredentialDescriptorJSON[];
    [option: string]: unknown;
  };
  mediation?: string;
};

// A server prompt and the token that answers it.
export type PasskeyPrompt<T> = { ceremony: string; options: T };

export type RegistrationJSON = {
  id: string;
  rawId: Base64Url;
  type: string;
  response: { clientDataJSON: Base64Url; attestationObject: Base64Url; transports?: string[] };
  extensions: Record<string, unknown>;
};

export type AssertionJSON = {
  id: string;
  rawId: Base64Url;
  type: string;
  response: { authenticatorData: Base64Url; clientDataJSON: Base64Url; signature: Base64Url; userHandle: Base64Url | null };
  extensions: Record<string, unknown>;
};

type Binary = ArrayBuffer | ArrayBufferView;

// The parts of a PublicKeyCredential the server needs.
type CredentialLike<Response> = {
  id: string;
  rawId: Binary;
  type: string;
  response: Response;
  getClientExtensionResults?: () => object;
};

export type RegistrationCredential = CredentialLike<{
  clientDataJSON: Binary;
  attestationObject: Binary;
  getTransports?: () => string[];
}>;

export type AssertionCredential = CredentialLike<{
  authenticatorData: Binary;
  clientDataJSON: Binary;
  signature: Binary;
  userHandle: Binary | null;
}>;

const bytesOf = (data: Binary) =>
  ArrayBuffer.isView(data) ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength) : new Uint8Array(data);

export function toBase64Url(data: Binary): Base64Url {
  let binary = "";
  for (const byte of bytesOf(data)) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

// Accepts padded or unpadded base64url, and standard base64.
export function fromBase64Url(text: Base64Url): ArrayBuffer {
  const base64 = text.replace(/-/g, "+").replace(/_/g, "/").replace(/=+$/, "");
  const binary = atob(base64.padEnd(Math.ceil(base64.length / 4) * 4, "="));
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index++) bytes[index] = binary.charCodeAt(index);
  return bytes.buffer;
}

// The server leaves out most unset options, but a null that slipped through
// would reach the browser as the string "null" or the number 0.
function withoutNulls<T>(value: T): T {
  if (Array.isArray(value)) return value.map(withoutNulls) as T;
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value).filter(([, entry]) => entry !== null).map(([key, entry]) => [key, withoutNulls(entry)]),
  ) as T;
}

const descriptor = (credential: CredentialDescriptorJSON) => ({ ...credential, id: fromBase64Url(credential.id) });

export function creationOptions({ publicKey }: CreationOptionsJSON): CredentialCreationOptions {
  const { challenge, user, excludeCredentials, ...rest } = withoutNulls(publicKey);
  return {
    publicKey: {
      ...rest,
      challenge: fromBase64Url(challenge),
      user: { ...user, id: fromBase64Url(user.id) },
      ...(excludeCredentials && { excludeCredentials: excludeCredentials.map(descriptor) }),
    } as PublicKeyCredentialCreationOptions,
  };
}

// `mediation` is left out: the website asks for a passkey only when someone
// chooses to use one.
export function requestOptions({ publicKey }: RequestOptionsJSON): CredentialRequestOptions {
  const { challenge, allowCredentials, ...rest } = withoutNulls(publicKey);
  return {
    publicKey: {
      ...rest,
      challenge: fromBase64Url(challenge),
      ...(allowCredentials && { allowCredentials: allowCredentials.map(descriptor) }),
    } as PublicKeyCredentialRequestOptions,
  };
}

// Extension results are plain values, except for the odd buffer.
function jsonExtensions(value: unknown): unknown {
  if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) return toBase64Url(value);
  if (Array.isArray(value)) return value.map(jsonExtensions);
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(Object.entries(value).map(([key, entry]) => [key, jsonExtensions(entry)]));
}

const extensionsOf = (credential: CredentialLike<unknown>) =>
  jsonExtensions(credential.getClientExtensionResults?.() ?? {}) as Record<string, unknown>;

export function registrationJSON(credential: RegistrationCredential): RegistrationJSON {
  const { response } = credential;
  const transports = response.getTransports?.();
  return {
    id: credential.id,
    rawId: toBase64Url(credential.rawId),
    type: credential.type,
    response: {
      clientDataJSON: toBase64Url(response.clientDataJSON),
      attestationObject: toBase64Url(response.attestationObject),
      ...(transports && { transports }),
    },
    extensions: extensionsOf(credential),
  };
}

export function assertionJSON(credential: AssertionCredential): AssertionJSON {
  const { response } = credential;
  return {
    id: credential.id,
    rawId: toBase64Url(credential.rawId),
    type: credential.type,
    response: {
      authenticatorData: toBase64Url(response.authenticatorData),
      clientDataJSON: toBase64Url(response.clientDataJSON),
      signature: toBase64Url(response.signature),
      userHandle: response.userHandle ? toBase64Url(response.userHandle) : null,
    },
    extensions: extensionsOf(credential),
  };
}

// A passkey prompt that ended without a passkey. `cancelled` means the
// person closed it (or it timed out), which needs no message.
export class PasskeyPromptError extends Error {
  readonly cancelled: boolean;

  constructor(message: string, cancelled = false) {
    super(message);
    this.cancelled = cancelled;
  }
}

// What a failure from navigator.credentials means to the person using it.
export function promptError(error: unknown, purpose: "create" | "get"): PasskeyPromptError {
  const name = error && typeof error === "object" && "name" in error ? String(error.name) : "";
  switch (name) {
    case "NotAllowedError":
    case "AbortError":
      return new PasskeyPromptError("The passkey prompt was closed.", true);
    case "InvalidStateError":
      if (purpose === "create") return new PasskeyPromptError("That passkey is already registered to your account. Use another device or security key.");
      break;
    case "SecurityError":
      return new PasskeyPromptError("Passkeys don't work at this address. Open MeshRMM at its usual address and try again.");
    case "NotSupportedError":
      return new PasskeyPromptError("This browser or device can't use passkeys here.");
  }
  return new PasskeyPromptError(purpose === "create" ? "The passkey couldn't be created. Try again." : "The passkey couldn't be used. Try again.");
}

// Whether this browser can create and use passkeys at all.
export function passkeysSupported() {
  return typeof window !== "undefined" && typeof window.PublicKeyCredential === "function" && Boolean(navigator.credentials);
}

// Asks the browser to create a passkey for the server's prompt.
export async function createPasskey(options: CreationOptionsJSON): Promise<RegistrationJSON> {
  let credential: Credential | null;
  try {
    credential = await navigator.credentials.create(creationOptions(options));
  } catch (error) {
    throw promptError(error, "create");
  }
  if (!credential) throw new PasskeyPromptError("The passkey prompt was closed.", true);
  return registrationJSON(credential as unknown as RegistrationCredential);
}

// Asks the browser for a passkey that answers the server's prompt.
export async function getPasskey(options: RequestOptionsJSON): Promise<AssertionJSON> {
  let credential: Credential | null;
  try {
    credential = await navigator.credentials.get(requestOptions(options));
  } catch (error) {
    throw promptError(error, "get");
  }
  if (!credential) throw new PasskeyPromptError("The passkey prompt was closed.", true);
  return assertionJSON(credential as unknown as AssertionCredential);
}
