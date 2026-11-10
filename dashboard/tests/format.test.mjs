import assert from "node:assert/strict";
import test from "node:test";
import { formatDateTime, formatRelative, timeFormat } from "../lib/format.ts";
import { formatDate } from "../features/system-info/model.ts";

// A zone with daylight saving time, set before anything is formatted.
process.env.TZ = "America/New_York";

// Counts the formatters built from here on. Date's own toLocaleString builds
// its formatter without going through these.
const built = { dateTime: 0, relative: 0 };
const { DateTimeFormat, RelativeTimeFormat } = Intl;
Intl.DateTimeFormat = class extends DateTimeFormat {
  constructor(...args) {
    super(...args);
    built.dateTime++;
  }
};
Intl.RelativeTimeFormat = class extends RelativeTimeFormat {
  constructor(...args) {
    super(...args);
    built.relative++;
  }
};

const TIMES = [
  0,
  -1,
  1_700_000_000_000,
  1_700_000_000_999.75,
  Date.UTC(2026, 2, 8, 6, 59, 59), // a minute before the clocks go forward
  Date.UTC(2026, 2, 8, 7, 0, 0),
  Date.UTC(2026, 10, 1, 5, 30, 0), // in the hour that happens twice
  Date.UTC(2026, 11, 31, 23, 59, 59),
  Date.UTC(1969, 6, 20, 20, 17),
  -62_135_596_800_000, // year 1
  8.64e15,
  -8.64e15,
  8.64e15 + 1,
  Number.NaN,
  Number.POSITIVE_INFINITY,
];

// Each page's options, with the Date method it used to call.
const FORMATS = [
  ["toLocaleString", { year: "numeric", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }],
  ["toLocaleString", { month: "short", day: "numeric", hour: "numeric", minute: "2-digit", second: "2-digit" }],
  ["toLocaleString", { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }],
  ["toLocaleString", { hour: "numeric", minute: "2-digit", second: "2-digit" }],
  ["toLocaleTimeString", { hour: "numeric", minute: "2-digit" }],
  ["toLocaleDateString", { weekday: "short", hour: "numeric" }],
  ["toLocaleDateString", { year: "numeric", month: "short", day: "numeric", timeZone: "UTC" }],
];

test("a kept formatter writes each time exactly as Date's toLocale methods did", () => {
  for (const [method, options] of FORMATS) {
    const format = timeFormat(options);
    for (const at of TIMES) {
      assert.equal(format(at), new Date(at)[method]([], options), `${method} ${JSON.stringify(options)} at ${at}`);
    }
  }
  for (const at of TIMES) {
    assert.equal(formatDateTime(at), new Date(at).toLocaleString([], FORMATS[0][1]));
    assert.equal(formatDate(at), new Date(at).toLocaleDateString([], FORMATS[6][1]));
  }
  assert.equal(formatDateTime(null), "Never");
  assert.equal(formatDate(undefined), "—");
});

// formatRelative as it was, building its formatter on every call.
function oldRelative(unixMs, now) {
  const units = [["year", 365 * 24 * 60 * 60_000], ["month", 30 * 24 * 60 * 60_000], ["day", 24 * 60 * 60_000], ["hour", 60 * 60_000], ["minute", 60_000]];
  const difference = unixMs - now;
  const format = new RelativeTimeFormat([], { numeric: "auto" });
  for (const [unit, size] of units) {
    if (Math.abs(difference) >= size) return format.format(Math.round(difference / size), unit);
  }
  return "just now";
}

test("relative times read exactly as they did", () => {
  const now = 1_700_000_000_000;
  const minute = 60_000;
  const offsets = [0, 1, 59_999, minute, 89_999, 90_000, 5 * minute, 59 * minute, 60 * minute, 90 * minute, 23 * 60 * minute,
    24 * 60 * minute, 36 * 60 * minute, 3 * 24 * 60 * minute, 29 * 24 * 60 * minute, 30 * 24 * 60 * minute, 45 * 24 * 60 * minute,
    364 * 24 * 60 * minute, 365 * 24 * 60 * minute, 800 * 24 * 60 * minute, 50 * 365 * 24 * 60 * minute];
  for (const offset of offsets) {
    for (const at of [now - offset, now + offset]) assert.equal(formatRelative(at, now), oldRelative(at, now), `${at - now} ms`);
  }
  assert.equal(formatRelative(now - 24 * 60 * minute, now), "yesterday", "numeric: auto words it");
});

test("each formatter is built once, on first use, however many rows it formats", () => {
  const before = { ...built };
  const format = timeFormat({ hour: "numeric" });
  assert.equal(built.dateTime, before.dateTime, "not when it is made");
  for (let row = 0; row < 500; row++) format(1_700_000_000_000 + row * 60_000);
  assert.equal(built.dateTime, before.dateTime + 1);

  const shared = built.dateTime;
  for (let row = 0; row < 500; row++) formatDateTime(1_700_000_000_000 + row * 60_000);
  for (let row = 0; row < 500; row++) formatDateTime(1_700_000_000_000 + row * 60_000);
  assert.ok(built.dateTime - shared <= 1, `${built.dateTime - shared} built for formatDateTime`);

  const relative = built.relative;
  for (let row = 0; row < 500; row++) formatRelative(1_700_000_000_000 - row * 60_000, 1_700_000_000_000);
  assert.ok(built.relative - relative <= 1, `${built.relative - relative} built for formatRelative`);
  assert.equal(built.relative, 1, "one in all, for every test in this file");
  assert.ok(built.dateTime <= FORMATS.length + 3, `${built.dateTime} built in all`);
});
