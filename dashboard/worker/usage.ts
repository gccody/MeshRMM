// Per-company usage for the platform cost report; see docs/cost-tracking.md.
// Each invocation writes one data point in the API Worker's format: index1 is
// the owner, blob1 the source, and doubles are billable requests, invocations,
// D1 rows read and D1 rows written.
import type { Host } from "../lib/hosts";

// Requests no company can be charged for: marketing pages, the admin console
// and sign-in. Company hostnames are recorded by slug until the company is
// looked up; the cost report maps slugs to companies.
export const PLATFORM_OWNER = "_platform";

export type Usage = { owner: string; d1RowsRead: number; d1RowsWritten: number };

type D1Meta = { rows_read?: number; rows_written?: number } | undefined;

export function startUsage(host: Host): Usage {
  return {
    owner: host.surface === "tenant" ? `slug:${host.slug}` : PLATFORM_OWNER,
    d1RowsRead: 0,
    d1RowsWritten: 0,
  };
}

export function recordD1(usage: Usage, meta: D1Meta) {
  usage.d1RowsRead += meta?.rows_read ?? 0;
  usage.d1RowsWritten += meta?.rows_written ?? 0;
}

export function writeUsage(dataset: AnalyticsEngineDataset | undefined, usage: Usage) {
  try {
    dataset?.writeDataPoint({
      indexes: [usage.owner],
      blobs: ["dashboard"],
      doubles: [1, 1, usage.d1RowsRead, usage.d1RowsWritten],
    });
  } catch {
    // Metering must never fail a request.
  }
}
