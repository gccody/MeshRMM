// Devices' resource usage as the server reports it: the latest reading on the
// event socket, and points over a range from /v1/agents/{id}/metrics. Imports
// carry no framework, so node:test can load this module directly.

export type VolumeUsage = {
  // A drive letter ("C:") or mount point ("/").
  name: string;
  total_bytes: number;
  free_bytes: number;
};

// A device's latest report. Loads and rates average the seconds before `at`.
export type MetricsReading = {
  device_id: string;
  at: number;
  cpu_percent: number;
  memory_used_bytes: number;
  memory_total_bytes: number;
  network_received_bytes_per_second: number;
  network_sent_bytes_per_second: number;
  uptime_seconds: number;
  volumes: VolumeUsage[];
};

// Usage at one time, or averaged over the period starting at `at`.
export type MetricsPoint = {
  at: number;
  cpu_percent: number;
  // The busiest reading's load in the period.
  cpu_percent_max: number;
  memory_used_bytes: number;
  memory_total_bytes: number;
  network_received_bytes_per_second: number;
  network_sent_bytes_per_second: number;
  storage_used_bytes: number;
  storage_total_bytes: number;
};

export type MetricsRange = "live" | "hour" | "day" | "week";

export const METRICS_RANGES: { id: MetricsRange; label: string; spanMs: number }[] = [
  { id: "live", label: "Live", spanMs: 15 * 60_000 },
  { id: "hour", label: "1 hour", spanMs: 60 * 60_000 },
  { id: "day", label: "24 hours", spanMs: 24 * 60 * 60_000 },
  { id: "week", label: "7 days", spanMs: 7 * 24 * 60 * 60_000 },
];

export type DeviceMetrics = {
  range: MetricsRange;
  // How long each point covers.
  step_ms: number;
  latest: MetricsReading | null;
  points: MetricsPoint[];
};

const isAmount = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value) && value >= 0;

const POINT_FIELDS = [
  "at",
  "cpu_percent",
  "cpu_percent_max",
  "memory_used_bytes",
  "memory_total_bytes",
  "network_received_bytes_per_second",
  "network_sent_bytes_per_second",
  "storage_used_bytes",
  "storage_total_bytes",
] as const;

const READING_FIELDS = [
  "at",
  "cpu_percent",
  "memory_used_bytes",
  "memory_total_bytes",
  "network_received_bytes_per_second",
  "network_sent_bytes_per_second",
  "uptime_seconds",
] as const;

const isVolume = (value: unknown): value is VolumeUsage => {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Record<string, unknown>;
  return typeof candidate.name === "string" && isAmount(candidate.total_bytes) && isAmount(candidate.free_bytes);
};

export function parseReading(value: unknown): MetricsReading | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Record<string, unknown>;
  if (typeof candidate.device_id !== "string") return null;
  if (!READING_FIELDS.every((field) => isAmount(candidate[field]))) return null;
  if (!Array.isArray(candidate.volumes) || !candidate.volumes.every(isVolume)) return null;
  return candidate as MetricsReading;
}

const isPoint = (value: unknown): value is MetricsPoint => {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Record<string, unknown>;
  return POINT_FIELDS.every((field) => isAmount(candidate[field]));
};

// The readings in an event socket's `metrics` message, or null for any other
// message.
export function parseMetricsEvent(value: unknown): MetricsReading[] | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as { type?: unknown; readings?: unknown };
  if (candidate.type !== "metrics" || !Array.isArray(candidate.readings)) return null;
  return candidate.readings.map(parseReading).filter((reading) => reading !== null);
}

export function parseDeviceMetrics(value: unknown): DeviceMetrics | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Record<string, unknown>;
  if (!METRICS_RANGES.some((range) => range.id === candidate.range)) return null;
  if (!isAmount(candidate.step_ms) || !Array.isArray(candidate.points) || !candidate.points.every(isPoint)) return null;
  const latest = candidate.latest === null || candidate.latest === undefined ? null : parseReading(candidate.latest);
  if (candidate.latest && !latest) return null;
  return {
    range: candidate.range as MetricsRange,
    step_ms: candidate.step_ms,
    latest,
    points: candidate.points,
  };
}

// Used space across the reading's volumes, and their capacity.
export function storageOf(volumes: VolumeUsage[]) {
  return volumes.reduce(
    (sum, volume) => ({
      used: sum.used + Math.max(0, volume.total_bytes - volume.free_bytes),
      total: sum.total + volume.total_bytes,
    }),
    { used: 0, total: 0 },
  );
}

export function pointFromReading(reading: MetricsReading): MetricsPoint {
  const storage = storageOf(reading.volumes);
  return {
    at: reading.at,
    cpu_percent: reading.cpu_percent,
    cpu_percent_max: reading.cpu_percent,
    memory_used_bytes: reading.memory_used_bytes,
    memory_total_bytes: reading.memory_total_bytes,
    network_received_bytes_per_second: reading.network_received_bytes_per_second,
    network_sent_bytes_per_second: reading.network_sent_bytes_per_second,
    storage_used_bytes: storage.used,
    storage_total_bytes: storage.total,
  };
}

// The live points with `reading` added, if it is newer than the last, and
// those older than the window dropped.
export function appendLivePoint(points: MetricsPoint[], reading: MetricsReading, windowMs: number) {
  const last = points.at(-1);
  if (last && reading.at <= last.at) return points;
  return [...points.filter((point) => point.at > reading.at - windowMs), pointFromReading(reading)];
}

export const percentOf = (part: number, whole: number) => (whole > 0 ? Math.min(100, (part / whole) * 100) : 0);

// How a load or a fullness reads: a meter turns amber, then coral.
export type UsageTone = "normal" | "high" | "critical";
export const usageTone = (percent: number): UsageTone => (percent >= 95 ? "critical" : percent >= 80 ? "high" : "normal");

export function formatPercent(percent: number) {
  if (percent > 0 && percent < 1) return "<1%";
  return `${Math.round(percent)}%`;
}

const BYTE_UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

// Sizes in binary units, as Windows names them: "15.9 GB".
export function formatBytes(bytes: number) {
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < BYTE_UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  return `${value.toFixed(digits)} ${BYTE_UNITS[unit]}`;
}

const BIT_UNITS = ["bps", "Kbps", "Mbps", "Gbps", "Tbps"];

// Network rates in bits per second, as adapters are rated: "12.4 Mbps".
export function formatBitRate(bytesPerSecond: number) {
  let value = bytesPerSecond * 8;
  let unit = 0;
  while (value >= 1000 && unit < BIT_UNITS.length - 1) {
    value /= 1000;
    unit += 1;
  }
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  return `${value.toFixed(digits)} ${BIT_UNITS[unit]}`;
}

// "3 d 4 h", "5 h 12 min" or "8 min".
export function formatUptime(seconds: number) {
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);
  const days = Math.floor(hours / 24);
  if (days > 0) return `${days} d ${hours % 24} h`;
  if (hours > 0) return `${hours} h ${minutes % 60} min`;
  return `${minutes} min`;
}

// A round axis maximum at or above `value`: 1, 2 or 5 times a power of `base`.
export function niceCeiling(value: number, base = 10) {
  if (!(value > 0)) return 1;
  const magnitude = base ** Math.floor(Math.log(value) / Math.log(base));
  for (const step of [1, 2, 5, 10]) {
    if (step * magnitude >= value) return step * magnitude;
  }
  return 10 * magnitude;
}

// The SVG path through the points' values, broken where readings are missing
// for longer than `gapMs` (the device was offline).
export function linePath(
  points: MetricsPoint[],
  value: (point: MetricsPoint) => number,
  x: (at: number) => number,
  y: (value: number) => number,
  gapMs: number,
) {
  let path = "";
  let previous: number | null = null;
  for (const point of points) {
    const command = previous === null || point.at - previous > gapMs ? "M" : "L";
    path += `${command}${x(point.at).toFixed(1)},${y(value(point)).toFixed(1)}`;
    previous = point.at;
  }
  return path;
}

// The filled area under the same line, down to `baseline`.
export function areaPath(
  points: MetricsPoint[],
  value: (point: MetricsPoint) => number,
  x: (at: number) => number,
  y: (value: number) => number,
  gapMs: number,
  baseline: number,
) {
  return linePath(points, value, x, y, gapMs)
    .split("M")
    .filter(Boolean)
    .map((segment) => {
      const vertices = segment.split("L");
      const first = vertices[0].split(",")[0];
      const last = vertices.at(-1)!.split(",")[0];
      return `M${first},${baseline}L${segment}L${last},${baseline}Z`;
    })
    .join("");
}

// The point nearest `at`, by time.
export function nearestPoint(points: MetricsPoint[], at: number) {
  let nearest: MetricsPoint | null = null;
  for (const point of points) {
    if (!nearest || Math.abs(point.at - at) < Math.abs(nearest.at - at)) nearest = point;
  }
  return nearest;
}
