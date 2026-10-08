// Dates and user agents as the website shows them. Imports carry no
// framework, so node:test can load this module directly.

export function formatDateTime(unixMs: number | null | undefined) {
  if (unixMs === null || unixMs === undefined) return "Never";
  return new Date(unixMs).toLocaleString([], { year: "numeric", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
}

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 365 * 24 * 60 * 60_000],
  ["month", 30 * 24 * 60 * 60_000],
  ["day", 24 * 60 * 60_000],
  ["hour", 60 * 60_000],
  ["minute", 60_000],
];

// "5 minutes ago", "in 3 days", or "just now" within a minute.
export function formatRelative(unixMs: number, now = Date.now()) {
  const difference = unixMs - now;
  const format = new Intl.RelativeTimeFormat([], { numeric: "auto" });
  for (const [unit, size] of UNITS) {
    if (Math.abs(difference) >= size) return format.format(Math.round(difference / size), unit);
  }
  return "just now";
}

// "Firefox on Windows" from a user agent string; good enough to recognize
// one's own browsers, not to identify them.
export function describeUserAgent(userAgent: string | null | undefined) {
  if (!userAgent) return "Unknown browser";
  const browser = /Edg\//.test(userAgent) ? "Edge"
    : /OPR\/|Opera/.test(userAgent) ? "Opera"
    : /Firefox\//.test(userAgent) ? "Firefox"
    : /Chrome\/|CriOS\//.test(userAgent) ? "Chrome"
    : /Safari\//.test(userAgent) ? "Safari"
    : null;
  const system = /iPhone|iPad|iPod/.test(userAgent) ? "iOS"
    : /Android/.test(userAgent) ? "Android"
    : /Windows/.test(userAgent) ? "Windows"
    : /Mac OS X|Macintosh/.test(userAgent) ? "macOS"
    : /CrOS/.test(userAgent) ? "ChromeOS"
    : /Linux/.test(userAgent) ? "Linux"
    : null;
  if (browser && system) return `${browser} on ${system}`;
  return browser ?? system ?? "Unknown browser";
}
