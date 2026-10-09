import { type MetricsReading, formatBytes, formatPercent, percentOf, usageTone } from "./model";

// A bar that fills with a load or fullness, and turns amber, then coral, as it
// nears the top. Drawn in SVG, since the website's pages carry no inline
// styles.
export function UsageBar({ percent }: { percent: number }) {
  const width = Math.max(0, Math.min(100, percent));
  return (
    <svg className={`usage-bar ${usageTone(percent)}`} aria-hidden="true">
      <rect className="usage-track" width="100%" height="6" rx="3" />
      {width > 0 && <rect className="usage-fill" width={`${Math.max(width, 3).toFixed(1)}%`} height="6" rx="3" />}
    </svg>
  );
}

// An online device's CPU load and memory use, under its tile on the wall.
export function DeviceUsage({ reading }: { reading: MetricsReading }) {
  const memory = percentOf(reading.memory_used_bytes, reading.memory_total_bytes);
  const memoryDetail = `${formatBytes(reading.memory_used_bytes)} of ${formatBytes(reading.memory_total_bytes)}`;
  return (
    <dl className="device-usage">
      <div title={`CPU ${formatPercent(reading.cpu_percent)}`}>
        <dt>CPU</dt>
        <dd><UsageBar percent={reading.cpu_percent} /><span>{formatPercent(reading.cpu_percent)}</span></dd>
      </div>
      <div title={`Memory ${memoryDetail}`}>
        <dt>RAM</dt>
        <dd><UsageBar percent={memory} /><span>{formatPercent(memory)}</span></dd>
      </div>
    </dl>
  );
}
