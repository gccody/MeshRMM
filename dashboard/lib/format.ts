// Dates and user agents as the website shows them. Imports carry no
// framework, so node:test can load this module directly.

// The furthest from 1970 a Date can be, either way.
const MAX_TIME_MS = 8.64e15;

// Formats times as Date's toLocaleString, toLocaleDateString or
// toLocaleTimeString would with these options, in the browser's language and
// time zone. Building an Intl formatter costs far more than formatting with
// one, and tables format a time for every row they draw, so the formatter is
// built once, on first use: not at import, which prerendering and node:test
// do too.
export function timeFormat(options: Intl.DateTimeFormatOptions) {
  let format: Intl.DateTimeFormat | undefined;
  return (unixMs: number) => {
    // What toLocaleString says of a time no Date can hold; the formatter
    // would throw.
    if (!(Math.abs(unixMs) <= MAX_TIME_MS)) return "Invalid Date";
    format ??= new Intl.DateTimeFormat([], options);
    return format.format(unixMs);
  };
}

const dateTime = timeFormat({ year: "numeric", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });

export function formatDateTime(unixMs: number | null | undefined) {
  if (unixMs === null || unixMs === undefined) return "Never";
  return dateTime(unixMs);
}

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 365 * 24 * 60 * 60_000],
  ["month", 30 * 24 * 60 * 60_000],
  ["day", 24 * 60 * 60_000],
  ["hour", 60 * 60_000],
  ["minute", 60_000],
];

// Built on first use, as above.
let relative: Intl.RelativeTimeFormat | undefined;

// "5 minutes ago", "in 3 days", or "just now" within a minute.
export function formatRelative(unixMs: number, now = Date.now()) {
  const difference = unixMs - now;
  for (const [unit, size] of UNITS) {
    if (Math.abs(difference) >= size) {
      relative ??= new Intl.RelativeTimeFormat([], { numeric: "auto" });
      return relative.format(Math.round(difference / size), unit);
    }
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
