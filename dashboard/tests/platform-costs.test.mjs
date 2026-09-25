import assert from "node:assert/strict";
import test from "node:test";
import { formatPreciseUsd, formatQuantity, formatRate, formatUsage, formatUsd, monthLabel, recentMonths } from "../features/platform/costs.ts";

test("the month picker offers the current and previous two UTC months", () => {
  assert.deepEqual(recentMonths(new Date("2026-09-25T18:00:00Z")), ["2026-09", "2026-08", "2026-07"]);
  assert.deepEqual(recentMonths(new Date("2026-01-31T23:59:59Z")), ["2026-01", "2025-12", "2025-11"]);
  assert.equal(monthLabel("2026-09"), "September 2026");
});

test("totals show cents but never hide a nonzero charge", () => {
  assert.equal(formatUsd(0), "$0.00");
  assert.equal(formatUsd(0.0001), "<$0.01");
  assert.equal(formatUsd(1234.567), "$1,234.57");
});

test("line items keep small usage charges readable", () => {
  assert.equal(formatPreciseUsd(0), "$0.00");
  assert.equal(formatPreciseUsd(1.2e-7), "$0.00000012");
  assert.equal(formatPreciseUsd(0.0123), "$0.012");
  assert.equal(formatPreciseUsd(0.5), "$0.50");
  assert.equal(formatRate({ usd: 0.001, per_label: "1M rows" }), "$0.001 / 1M rows");
  assert.equal(formatRate({ usd: 125, per_label: "connection" }), "$125.00 / connection");
  assert.equal(formatQuantity(1234.5), "1,235");
  assert.equal(formatQuantity(1.2345), "1.23");
  assert.equal(formatQuantity(0.000123), "0.000123");
  assert.equal(formatUsage({ quantity: 1, unit: "connections" }), "1 connection");
  assert.equal(formatUsage({ quantity: 2, unit: "connections" }), "2 connections");
  assert.equal(formatUsage({ quantity: 1, unit: "GB-s" }), "1 GB-s");
});
