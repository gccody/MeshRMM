"use client";

import { useAuth } from "@workos-inc/authkit-react";
import { LoaderCircle, ShieldCheck } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { LOGIN_ATTEMPT_KEY } from "../providers";

const LOGIN_ATTEMPT_TTL_MS = 30_000;

export default function LoginRoute() {
  const { isLoading, user, signIn } = useAuth();
  const started = useRef(false);
  const [isPaused, setIsPaused] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (isLoading || started.current) return;

    if (user) {
      window.sessionStorage.removeItem(LOGIN_ATTEMPT_KEY);
      window.location.replace("/");
      return;
    }

    started.current = true;
    const now = Date.now();
    const lastAttempt = Number(window.sessionStorage.getItem(LOGIN_ATTEMPT_KEY));

    if (Number.isFinite(lastAttempt) && now - lastAttempt < LOGIN_ATTEMPT_TTL_MS) {
      window.setTimeout(() => setIsPaused(true), 0);
      return;
    }

    window.sessionStorage.setItem(LOGIN_ATTEMPT_KEY, String(now));
    void signIn({ state: { returnTo: "/" } }).catch((signInError: unknown) => {
      window.sessionStorage.removeItem(LOGIN_ATTEMPT_KEY);
      setError(signInError instanceof Error ? signInError.message : "WorkOS sign-in could not be started.");
    });
  }, [isLoading, signIn, user]);

  const retry = () => {
    window.sessionStorage.removeItem(LOGIN_ATTEMPT_KEY);
    window.location.reload();
  };

  return (
    <main className="login-route">
      <section className="signed-out-card">
        <div className="modal-icon"><ShieldCheck size={22} /></div>
        <p className="eyebrow">Secure company access</p>
        <h1>{isPaused ? "Sign-in paused" : error ? "Sign-in unavailable" : "Preparing secure sign-in"}</h1>
        <p>
          {isPaused
            ? "PulseRMM stopped a repeated authentication redirect before it could loop. You can safely retry when ready."
            : error ?? "Redirecting to WorkOS to authenticate your PulseRMM account."}
        </p>
        {isPaused || error ? (
          <div className="login-actions">
            <button className="primary-button" onClick={retry}><ShieldCheck size={16} /> Retry sign-in</button>
            <button className="secondary-button" onClick={() => window.location.replace("/")}>Return home</button>
          </div>
        ) : (
          <div className="login-progress" role="status"><LoaderCircle size={18} className="spin" /> Connecting to WorkOS</div>
        )}
      </section>
    </main>
  );
}
