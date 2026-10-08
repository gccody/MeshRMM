import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import { apiFetch, errorMessage } from "../../lib/http";
import { activityStorageKey } from "../session/idle-session";
import { AccountLoadError, accountLoader } from "./account-load";
import type { Account, Instance } from "./types";

// Why a signed-in browser stopped being signed in: it sat idle, the server
// ended its session, or another tab signed out.
export type LockReason = "idle" | "expired" | "elsewhere";

export type SessionState =
  | { status: "loading" }
  // `reason` is null when nobody signed in, or the user signed out here.
  // `idleTimeoutMinutes` is the signed-out account's, for explaining "idle".
  | { status: "signed-out"; reason: LockReason | null; idleTimeoutMinutes?: number }
  | { status: "signed-in"; account: Account }
  | { status: "unavailable"; message: string; retrying: boolean };

type Session = {
  instance: Instance | null;
  state: SessionState;
  account: Account | null;
  // Reads the account (and instance) again, after a change that affects them.
  // Resolves null when the session has ended.
  refresh: () => Promise<Account | null>;
  // After a response that signed this browser in.
  signedIn: () => Promise<Account | null>;
  // `reason` is why the website signed out by itself, as after inactivity.
  signOut: (reason?: LockReason) => Promise<void>;
  // The server no longer accepts the session.
  lock: (reason: LockReason) => void;
  retry: () => void;
};

// Other tabs on this origin learn of a sign-in or sign-out through these.
const SIGN_OUT_KEY = "meshrmm:sign-out";
const SIGN_IN_KEY = "meshrmm:sign-in";

const Context = createContext<Session | null>(null);

export function useSession() {
  const session = useContext(Context);
  if (!session) throw new Error("The session provider is missing.");
  return session;
}

async function fetchInstance(): Promise<Instance> {
  const response = await apiFetch("/v1/instance");
  if (!response.ok) throw new AccountLoadError(await errorMessage(response, "MeshRMM could not be reached."), response.status);
  return (await response.json()) as Instance;
}

// The account, or null when nobody is signed in.
async function fetchAccount(): Promise<Account | null> {
  const response = await apiFetch("/v1/account");
  if (response.status === 401) {
    await response.body?.cancel();
    return null;
  }
  if (!response.ok) throw new AccountLoadError(await errorMessage(response, "Your account could not be loaded."), response.status);
  return (await response.json()) as Account;
}

function broadcast(key: string) {
  try {
    window.localStorage.setItem(key, String(Date.now()));
  } catch {
    // Storage may be unavailable (private windows, quotas); tabs then don't hear.
  }
}

// A new sign-in starts the idle timer afresh, whatever an earlier session
// left in storage.
function markActive(userId: string) {
  try {
    window.localStorage.setItem(activityStorageKey(userId), String(Date.now()));
  } catch {
    // Without storage, the idle timer starts from this page load instead.
  }
}

// Holds who is signed in. Nothing is known until the browser asks the
// server, so the prerendered page and the first render show "loading".
export function SessionProvider({ children }: { children: React.ReactNode }) {
  const [instance, setInstance] = useState<Instance | null>(null);
  const [state, setState] = useState<SessionState>({ status: "loading" });
  const current = useRef(state);
  const loader = useRef<{ retry: () => void; stop: () => void } | null>(null);

  const publish = useCallback((next: SessionState) => {
    current.current = next;
    setState(next);
  }, []);

  // Ends a signed-in session for `reason`, or records that nobody is.
  const end = useCallback((reason: LockReason | null) => {
    const previous = current.current;
    publish(previous.status === "signed-in"
      ? { status: "signed-out", reason, idleTimeoutMinutes: previous.account.idle_timeout_minutes }
      : { status: "signed-out", reason: null });
  }, [publish]);

  const apply = useCallback((account: Account | null, reasonIfEnded: LockReason) => {
    if (account) publish({ status: "signed-in", account });
    else if (current.current.status !== "signed-out") end(reasonIfEnded);
  }, [end, publish]);

  useEffect(() => {
    const started = accountLoader({
      load: () => Promise.all([fetchInstance(), fetchAccount()]),
      onLoaded: ([loadedInstance, account]) => {
        setInstance(loadedInstance);
        apply(account, "expired");
      },
      onError: (error, retryInMs) => {
        publish({
          status: "unavailable",
          message: error instanceof Error ? error.message : "MeshRMM could not be reached.",
          retrying: retryInMs !== null,
        });
      },
    });
    loader.current = started;
    return () => {
      started.stop();
      loader.current = null;
    };
  }, [apply, publish]);

  const refresh = useCallback(async () => {
    const [loadedInstance, account] = await Promise.all([fetchInstance(), fetchAccount()]);
    setInstance(loadedInstance);
    apply(account, "expired");
    return account;
  }, [apply]);

  const signedIn = useCallback(async () => {
    const account = await refresh();
    if (account) {
      markActive(account.user.id);
      broadcast(SIGN_IN_KEY);
    }
    return account;
  }, [refresh]);

  const lock = useCallback((reason: LockReason) => {
    if (current.current.status === "signed-in") end(reason);
  }, [end]);

  const signOut = useCallback(async (reason?: LockReason) => {
    end(reason ?? null);
    broadcast(SIGN_OUT_KEY);
    // The cookie is HttpOnly, so only the server can remove it. A failure
    // leaves a session that expires by itself.
    await apiFetch("/v1/auth/sign-out", { method: "POST" }).then((response) => response.body?.cancel()).catch(() => {});
  }, [end]);

  useEffect(() => {
    const onStorage = (event: StorageEvent) => {
      if (event.key === SIGN_OUT_KEY) lock("elsewhere");
      else if (event.key === SIGN_IN_KEY && current.current.status !== "signed-in") void refresh().catch(() => {});
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, [lock, refresh]);

  const retry = useCallback(() => loader.current?.retry(), []);

  const value = useMemo<Session>(() => ({
    instance,
    state,
    account: state.status === "signed-in" ? state.account : null,
    refresh,
    signedIn,
    signOut,
    lock,
    retry,
  }), [instance, state, refresh, signedIn, signOut, lock, retry]);

  return <Context.Provider value={value}>{children}</Context.Provider>;
}

// Sets the tab's title: the page, then the instance's name once known.
export function useDocumentTitle(page: string) {
  const name = useContext(Context)?.instance?.name;
  useEffect(() => {
    document.title = `${page} · ${name || "MeshRMM"}`;
  }, [page, name]);
}
