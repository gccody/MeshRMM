// Renew authorization, not inventory. The backend keeps the same WebSocket and
// revision stream; expiry or any renewal failure requires a fresh subscription.
type AuthorizedFetch = (path: string, init?: RequestInit) => Promise<Response>;

export function subscriptionRenewal(
  authorizedFetch: AuthorizedFetch,
  disconnect: () => void,
) {
  let stopped = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let connectionId: string | undefined;
  const abort = new AbortController();

  const schedule = (expiresAt: unknown) => {
    if (typeof expiresAt !== "number" || !Number.isSafeInteger(expiresAt) || expiresAt <= Date.now()) {
      throw new Error("Invalid subscription authorization deadline");
    }
    if (timer !== undefined) clearTimeout(timer);
    timer = setTimeout(() => { void renew(); }, Math.max(0, expiresAt - Date.now() - 60_000));
  };

  const renew = async () => {
    if (stopped || !connectionId) return;
    try {
      const response = await authorizedFetch("/v1/agents/events/subscriptions/renew", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ connection_id: connectionId }),
        signal: abort.signal,
      });
      if (stopped) return;
      if (!response.ok) throw new Error("Subscription authorization could not be renewed");
      const data = await response.json() as { expires_at_unix_ms?: unknown };
      if (!stopped) schedule(data.expires_at_unix_ms);
    } catch {
      if (!stopped) disconnect();
    }
  };

  return {
    accept(value: unknown): boolean {
      if (!value || typeof value !== "object" || !("type" in value) || value.type !== "authorization") return false;
      const authorization = value as { connection_id?: unknown; expires_at_unix_ms?: unknown };
      if (stopped) return true;
      try {
        if (typeof authorization.connection_id !== "string" || !/^[a-f0-9]{64}$/i.test(authorization.connection_id)) {
          throw new Error("Invalid subscription connection ID");
        }
        connectionId = authorization.connection_id;
        schedule(authorization.expires_at_unix_ms);
      } catch {
        disconnect();
      }
      return true;
    },
    stop() {
      stopped = true;
      if (timer !== undefined) clearTimeout(timer);
      abort.abort();
    },
  };
}
