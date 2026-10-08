import { useCallback, useEffect, useState } from "react";
import { AuthenticationRequired, errorText } from "./http";

// Data a page loads when it opens. `load` must keep its identity between
// renders (wrap it in useCallback); a new one loads again. A locked session
// leaves the last data in place, since the workspace shows the lock instead.
export function useResource<T>(load: () => Promise<T>, fallback: string) {
  const [result, setResult] = useState<{ data: T | null; error: string | null }>({ data: null, error: null });
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let cancelled = false;
    load().then(
      (data) => {
        if (!cancelled) setResult({ data, error: null });
      },
      (error: unknown) => {
        if (cancelled || error instanceof AuthenticationRequired) return;
        setResult((current) => ({ data: current.data, error: errorText(error, fallback) }));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [load, attempt, fallback]);

  const reload = useCallback(() => setAttempt((count) => count + 1), []);

  // Replaces the data after a change the page made itself.
  const setData = useCallback((update: (current: T) => T) => {
    setResult((current) => (current.data === null ? current : { data: update(current.data), error: null }));
  }, []);

  return { data: result.data, error: result.error, reload, setData };
}
