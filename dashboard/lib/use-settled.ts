import { useEffect, useState } from "react";

// How long a search waits after the last keystroke before asking the server.
export const SEARCH_DELAY_MS = 300;

// `value` once it has stayed the same for `delayMs`.
export function useSettled<T>(value: T, delayMs = SEARCH_DELAY_MS) {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    if (Object.is(settled, value)) return;
    const timer = window.setTimeout(() => setSettled(value), delayMs);
    return () => window.clearTimeout(timer);
  }, [value, settled, delayMs]);
  return settled;
}
