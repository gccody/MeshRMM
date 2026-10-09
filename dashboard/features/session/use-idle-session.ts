
import { useEffect, useRef } from "react";
import {
  activityStorageKey,
  hasIdleTimeoutElapsed,
  readLastActivity,
  timeoutMilliseconds,
} from "./idle-session";

const STORAGE_WRITE_INTERVAL_MS = 30_000;
const MAX_TIMER_DELAY_MS = 2_147_000_000;
const ACTIVITY_EVENTS: (keyof WindowEventMap)[] = [
  "keydown",
  "pointerdown",
  "touchstart",
  "wheel",
];

type Options = {
  enabled: boolean;
  userId?: string | null;
  timeoutMinutes: number;
  onTimeout: () => void;
};

// Tracks the user's activity in this tab and others, and calls `onTimeout`
// once the idle timeout passes. Returns the cleanup, unless it already passed.
function watchIdleSession(
  userId: string,
  timeoutMinutes: number,
  onTimeout: () => void,
): (() => void) | undefined {
  const storageKey = activityStorageKey(userId);
  let lastActivityAt = readLastActivity(userId);
  let lastStorageWriteAt = lastActivityAt;
  let expired = false;
  let timer: number | undefined;

  const expire = () => {
    if (expired) return;
    expired = true;
    if (timer !== undefined) window.clearTimeout(timer);
    onTimeout();
  };

  const schedule = () => {
    if (expired) return;
    if (timer !== undefined) window.clearTimeout(timer);
    const remaining = lastActivityAt + timeoutMilliseconds(timeoutMinutes) - Date.now();
    if (remaining <= 0) {
      expire();
      return;
    }
    timer = window.setTimeout(expire, Math.min(remaining, MAX_TIMER_DELAY_MS));
  };

  const recordActivity = () => {
    const now = Date.now();
    if (hasIdleTimeoutElapsed(lastActivityAt, timeoutMinutes, now)) {
      expire();
      return;
    }
    lastActivityAt = now;
    if (now - lastStorageWriteAt >= STORAGE_WRITE_INTERVAL_MS) {
      window.localStorage.setItem(storageKey, String(now));
      lastStorageWriteAt = now;
    }
    schedule();
  };

  const handleVisibility = () => {
    if (!document.hidden) recordActivity();
  };

  const handleStorage = (event: StorageEvent) => {
    if (event.key !== storageKey) return;
    const nextActivity = Number(event.newValue);
    if (Number.isFinite(nextActivity) && nextActivity > lastActivityAt) {
      lastActivityAt = nextActivity;
      lastStorageWriteAt = nextActivity;
      schedule();
    }
  };

  if (hasIdleTimeoutElapsed(lastActivityAt, timeoutMinutes)) {
    expire();
    return;
  }
  window.localStorage.setItem(storageKey, String(lastActivityAt));
  for (const eventName of ACTIVITY_EVENTS) {
    window.addEventListener(eventName, recordActivity, { passive: true, capture: true });
  }
  document.addEventListener("visibilitychange", handleVisibility);
  window.addEventListener("storage", handleStorage);
  schedule();

  return () => {
    if (timer !== undefined) window.clearTimeout(timer);
    if (!expired) window.localStorage.setItem(storageKey, String(lastActivityAt));
    for (const eventName of ACTIVITY_EVENTS) {
      window.removeEventListener(eventName, recordActivity, { capture: true });
    }
    document.removeEventListener("visibilitychange", handleVisibility);
    window.removeEventListener("storage", handleStorage);
  };
}

export function useIdleSession({
  enabled,
  userId,
  timeoutMinutes,
  onTimeout,
}: Options) {
  const onTimeoutRef = useRef(onTimeout);

  useEffect(() => {
    onTimeoutRef.current = onTimeout;
  }, [onTimeout]);

  useEffect(() => {
    if (!enabled || !userId) return;
    return watchIdleSession(userId, timeoutMinutes, () => onTimeoutRef.current());
  }, [enabled, userId, timeoutMinutes]);
}
