// Requests to the MeshRMM API. The website and API share one origin, so the
// session cookie authenticates every request. The server refuses a request
// that changes something unless it carries X-MeshRMM-Request, which a page
// on another origin cannot add.

// The session ended; the workspace locks itself before this is thrown.
export class AuthenticationRequired extends Error {}

// Calls an API path with the session cookie.
export type AuthorizedFetch = (path: string, init?: RequestInit) => Promise<Response>;

// A failed request, with the server's message and `code` when it sent one.
export class RequestError extends Error {
  readonly status: number;
  readonly code: string | undefined;

  constructor(message: string, status: number, code?: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

const SAFE_METHODS = new Set(["GET", "HEAD", "OPTIONS"]);

export function apiFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  if (!SAFE_METHODS.has((init.method ?? "GET").toUpperCase())) headers.set("X-MeshRMM-Request", "1");
  return fetch(path, { cache: "no-store", credentials: "same-origin", ...init, headers });
}

// A JSON request body.
export const jsonBody = (body: unknown, method = "POST"): RequestInit => ({
  method,
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify(body),
});

// The server words errors as sentence fragments ("the code is incorrect");
// the website shows them as sentences.
export function asSentence(message: string) {
  const text = message.trim();
  if (!text) return text;
  const capitalized = text[0].toUpperCase() + text.slice(1);
  return /[.!?…]$/u.test(capitalized) ? capitalized : `${capitalized}.`;
}

async function errorBody(response: Response): Promise<{ error?: string; code?: string }> {
  try {
    const body: unknown = await response.json();
    if (!body || typeof body !== "object") return {};
    const { error, code } = body as Record<string, unknown>;
    return {
      error: typeof error === "string" ? error : undefined,
      code: typeof code === "string" ? code : undefined,
    };
  } catch {
    return {};
  }
}

export async function errorMessage(response: Response, fallback: string) {
  const { error } = await errorBody(response);
  return error ? asSentence(error) : fallback;
}

export async function requestError(response: Response, fallback: string) {
  const { error, code } = await errorBody(response);
  return new RequestError(error ? asSentence(error) : fallback, response.status, code);
}

// The response's JSON, or a RequestError for a failed one.
export async function expectJson<T>(response: Response, fallback: string): Promise<T> {
  if (!response.ok) throw await requestError(response, fallback);
  return (await response.json()) as T;
}

// Nothing to read, or a RequestError for a failed response.
export async function expectOk(response: Response, fallback: string) {
  if (!response.ok) throw await requestError(response, fallback);
  await response.body?.cancel();
}

export const errorText = (error: unknown, fallback: string) =>
  error instanceof Error && error.message ? error.message : fallback;
