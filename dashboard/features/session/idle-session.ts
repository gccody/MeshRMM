export const DEFAULT_IDLE_TIMEOUT_MINUTES = 4 * 60;
export const MIN_IDLE_TIMEOUT_MINUTES = 5;
export const MAX_IDLE_TIMEOUT_MINUTES = 24 * 60;

const ACTIVITY_KEY_PREFIX = "pulsermm:dashboard-activity:";

export function activityStorageKey(organizationId: string) {
  return `${ACTIVITY_KEY_PREFIX}${organizationId}`;
}

export function timeoutMilliseconds(timeoutMinutes: number) {
  const validTimeout =
    Number.isInteger(timeoutMinutes) &&
    timeoutMinutes >= MIN_IDLE_TIMEOUT_MINUTES &&
    timeoutMinutes <= MAX_IDLE_TIMEOUT_MINUTES
      ? timeoutMinutes
      : DEFAULT_IDLE_TIMEOUT_MINUTES;
  return validTimeout * 60 * 1_000;
}

export function hasIdleTimeoutElapsed(
  lastActivityAt: number,
  timeoutMinutes: number,
  now = Date.now(),
) {
  return now - lastActivityAt >= timeoutMilliseconds(timeoutMinutes);
}

export function formatIdleTimeout(timeoutMinutes: number) {
  if (timeoutMinutes < 60) return `${timeoutMinutes} minutes`;
  const hours = timeoutMinutes / 60;
  return Number.isInteger(hours)
    ? `${hours} ${hours === 1 ? "hour" : "hours"}`
    : `${hours.toFixed(1)} hours`;
}

export function readLastActivity(organizationId: string, fallback = Date.now()) {
  const stored = Number(window.localStorage.getItem(activityStorageKey(organizationId)));
  return Number.isFinite(stored) && stored > 0 ? stored : fallback;
}
