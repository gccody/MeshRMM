export class AuthenticationRedirectStarted extends Error {}

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

