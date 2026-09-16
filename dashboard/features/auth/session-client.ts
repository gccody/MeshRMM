export type BrowserSession = {
  accessToken: string;
  expiresAt: number;
  user: { id: string; email: string; firstName?: string; lastName?: string; profilePictureUrl?: string };
  organizationId?: string;
  role?: string;
  roles?: string[];
  returnTo?: string;
};
export class LoginRequiredError extends Error {}

export async function authRequest<T>(path: string, body: unknown = {}): Promise<T> {
  const response = await fetch(`/auth/${path}`, {
    method: "POST", credentials: "same-origin", cache: "no-store",
    headers: { "Content-Type": "application/json", "X-MeshRMM-Auth": "1" },
    body: JSON.stringify(body),
  });
  if (response.status === 401) throw new LoginRequiredError("Your session has expired. Please sign in again.");
  if (!response.ok) throw new Error("Session service is unavailable. Please retry.");
  return response.json() as Promise<T>;
}

// Serialize refresh rotation and logout across tabs on this origin. This also
// prevents an in-flight refresh response from restoring a cookie after logout.
export async function withSessionLock<T>(action: () => Promise<T>): Promise<T> {
  return navigator.locks ? await navigator.locks.request("meshrmm-session", action) : await action();
}
