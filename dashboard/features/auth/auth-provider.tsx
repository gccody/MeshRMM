"use client";

import { createContext, useCallback, useContext, useEffect, useRef, useState } from "react";
import { activityStorageKey } from "../session/idle-session";
import { authRequest, LoginRequiredError, withSessionLock, type BrowserSession } from "./session-client";

export { LoginRequiredError } from "./session-client";
export const LOGIN_ATTEMPT_KEY = "meshrmm:workos-login-attempt";
export const AUTH_REFRESH_FAILED_EVENT = "meshrmm:workos-refresh-failed";
const SIGN_OUT_KEY = "meshrmm:sign-out";
type SignInOptions = { organizationId?: string; invitationToken?: string; state?: { returnTo?: string } };
type AuthContext = {
  isLoading: boolean;
  user: BrowserSession["user"] | null;
  organizationId?: string;
  role?: string;
  roles?: string[];
  getAccessToken: () => Promise<string>;
  signIn: (options?: SignInOptions) => Promise<void>;
  signOut: (options?: { navigate?: boolean; returnTo?: string }) => Promise<void>;
};
const Context = createContext<AuthContext | null>(null);
export function useAuth() {
  const value = useContext(Context);
  if (!value) throw new Error("Authentication provider is unavailable");
  return value;
}

export function AuthProvider({ children, enabled }: { children: React.ReactNode; enabled: boolean }) {
  const [session, setSession] = useState<BrowserSession | null>(null);
  const [isLoading, setLoading] = useState(true);
  const [startupError, setStartupError] = useState<string | null>(null);
  const current = useRef<BrowserSession | null>(null);
  const pending = useRef<Promise<BrowserSession> | null>(null);
  const generation = useRef(0);
  const callback = useRef<Promise<BrowserSession> | null>(null);
  const publish = useCallback((value: BrowserSession | null) => {
    current.current = value;
    setSession(value);
  }, []);
  const refresh = useCallback((): Promise<BrowserSession> => {
    if (pending.current) return pending.current;
    const version = generation.current;
    const request = withSessionLock(async () => {
      if (version !== generation.current) throw new LoginRequiredError();
      return authRequest<BrowserSession>("session");
    }).then((value) => {
      if (version !== generation.current) throw new LoginRequiredError();
      publish(value);
      return value;
    }).catch((error: unknown) => {
      if (error instanceof LoginRequiredError && version === generation.current) {
        const wasSignedIn = Boolean(current.current);
        publish(null);
        if (wasSignedIn) window.dispatchEvent(new Event(AUTH_REFRESH_FAILED_EVENT));
      }
      throw error;
    }).finally(() => { if (pending.current === request) pending.current = null; });
    pending.current = request;
    return request;
  }, [publish]);

  useEffect(() => {
    let cancelled = false;
    async function initialize() {
      try {
        if (!enabled) return;
        const params = new URLSearchParams(window.location.search);
        if (params.has("code") || params.has("error")) {
          if (params.has("error")) throw new Error("Sign-in was not completed. Please try again.");
          callback.current ??= withSessionLock(() => authRequest<BrowserSession>("callback", { code: params.get("code"), state: params.get("state") }));
          const value = await callback.current;
          if (cancelled) return;
          window.sessionStorage.removeItem(LOGIN_ATTEMPT_KEY);
          if (value.organizationId) window.localStorage.setItem(activityStorageKey(value.organizationId), String(Date.now()));
          // A navigation ensures the restored route and its server-rendered view
          // agree, and removes the authorization code from the address bar.
          window.location.replace(value.returnTo ?? "/");
          return;
        }
        await refresh();
      } catch (error) {
        if (!cancelled && !(error instanceof LoginRequiredError)) setStartupError(error instanceof Error ? error.message : "Unable to restore your session.");
      } finally { if (!cancelled) setLoading(false); }
    }
    void initialize();
    return () => { cancelled = true; };
  }, [enabled, refresh]);

  useEffect(() => {
    if (!session) return;
    let timer: ReturnType<typeof setTimeout>;
    let cancelled = false;
    const renew = () => {
      void refresh().catch((error: unknown) => {
        if (!cancelled && !(error instanceof LoginRequiredError)) timer = setTimeout(renew, 30_000);
      });
    };
    timer = setTimeout(renew, Math.max(1000, session.expiresAt - Date.now() - 60_000));
    return () => { cancelled = true; clearTimeout(timer); };
  }, [session, refresh]);

  useEffect(() => {
    const signedOut = (event: StorageEvent) => {
      if (event.key !== SIGN_OUT_KEY) return;
      generation.current++;
      publish(null);
      window.dispatchEvent(new Event(AUTH_REFRESH_FAILED_EVENT));
    };
    window.addEventListener("storage", signedOut);
    return () => window.removeEventListener("storage", signedOut);
  }, [publish]);

  const getAccessToken = useCallback(async () => {
    if (current.current && current.current.expiresAt > Date.now() + 60_000) return current.current.accessToken;
    try { return (await refresh()).accessToken; }
    catch (error) {
      if (!(error instanceof LoginRequiredError) && current.current && current.current.expiresAt > Date.now()) return current.current.accessToken;
      throw error;
    }
  }, [refresh]);
  const signIn = useCallback(async (options: SignInOptions = {}) => {
    const result = await authRequest<{ url: string }>("login", { invitationToken: options.invitationToken, returnTo: options.state?.returnTo ?? "/" });
    window.location.assign(result.url);
  }, []);
  const signOut = useCallback(async (options: { navigate?: boolean; returnTo?: string } = {}) => {
    generation.current++;
    publish(null);
    window.localStorage.setItem(SIGN_OUT_KEY, crypto.randomUUID());
    await withSessionLock(() => authRequest("logout"));
    if (options.navigate !== false) window.location.assign("https://meshrmm.com");
  }, [publish]);

  if (startupError) return (
    <main className="platform-auth"><section className="signed-out-card">
      <h1>Unable to restore your session</h1><p>{startupError}</p>
      <button className="primary-button" onClick={() => window.location.reload()}>Retry</button>
      <button className="secondary-button" onClick={() => void signIn()}>Sign in</button>
    </section></main>
  );
  return <Context.Provider value={{ isLoading, user: session?.user ?? null, organizationId: session?.organizationId, role: session?.role, roles: session?.roles, getAccessToken, signIn, signOut }}>{children}</Context.Provider>;
}
