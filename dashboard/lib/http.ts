export class AuthenticationRequired extends Error {}

// Calls a control-plane path with the current access token.
export type AuthorizedFetch = (path: string, init?: RequestInit) => Promise<Response>;

export const normalizeServer = (server: string) =>
  server.trim().replace(/\/+$/, "");

export async function errorMessage(response: Response, fallback: string) {
  try {
    const body: unknown = await response.json();
    if (
      body &&
      typeof body === "object" &&
      "error" in body &&
      typeof body.error === "string"
    ) {
      return body.error || fallback;
    }
    return fallback;
  } catch {
    return fallback;
  }
}
