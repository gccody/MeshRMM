import { Activity, ArrowLeft, Building2, CircleAlert, Cpu, Gauge, Laptop, LoaderCircle, MonitorOff, Network, Package, ShieldCheck, Terminal } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { NO_COMPANY, DEFAULT_DEVICE_FILTERS, serializeDeviceFilters } from "../agents/device-filters";
import { companyName } from "../agents/model";
import { DeviceAlerts } from "../alerts/device-alerts";
import { MoveDeviceDialog } from "../companies/company-dialogs";
import { DeviceSystem, type SystemTab } from "../system-info/device-system";
import { terminalPagePath } from "../terminal/model";
import { DeviceUpdates } from "../updates/device-updates";
import { DeviceWake } from "../wake/device-wake";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { MetricChart, type Series } from "./metric-chart";
import {
  METRICS_RANGES,
  type MetricsPoint,
  type MetricsRange,
  type MetricsReading,
  formatBitRate,
  formatBytes,
  formatPercent,
  formatUptime,
  niceCeiling,
  percentOf,
  storageOf,
} from "./model";
import { UsageBar } from "./usage-meter";
import { useDeviceMetrics } from "./use-device-metrics";

const CPU: Series[] = [{ label: "CPU", tone: "series-1", value: (point) => point.cpu_percent, format: formatPercent }];
const MEMORY: Series[] = [{ label: "Used", tone: "series-1", value: (point) => point.memory_used_bytes, format: formatBytes }];
const NETWORK: Series[] = [
  { label: "Received", tone: "series-1", value: (point) => point.network_received_bytes_per_second, format: formatBitRate },
  { label: "Sent", tone: "series-2", value: (point) => point.network_sent_bytes_per_second, format: formatBitRate },
];
const STORAGE: Series[] = [{ label: "Used", tone: "series-1", value: (point) => point.storage_used_bytes, format: formatBytes }];

const isRange = (value: string | null): value is MetricsRange => METRICS_RANGES.some((range) => range.id === value);

type DeviceTab = "usage" | SystemTab | "updates";

const TABS: { id: DeviceTab; label: string; icon: typeof Gauge }[] = [
  { id: "usage", label: "Usage", icon: Gauge },
  { id: "hardware", label: "Hardware", icon: Cpu },
  { id: "software", label: "Software", icon: Package },
  { id: "network", label: "Network", icon: Network },
  { id: "updates", label: "Updates", icon: ShieldCheck },
];

const isTab = (value: string | null): value is DeviceTab => TABS.some((tab) => tab.id === value);

// `/device?id=…&tab=…`: one device's resource usage, live or over a range,
// its hardware, software and network, and its operating system updates.
export function DevicePage() {
  const { inventory, authorizedFetch, can, setPageTitle } = useWorkspace();
  const [moving, setMoving] = useState(false);
  const moveOpener = useRef<HTMLButtonElement>(null);
  const [searchParams, setSearchParams] = useSearchParams();
  const deviceId = searchParams.get("id") ?? "";
  const requestedRange = searchParams.get("range");
  const range: MetricsRange = isRange(requestedRange) ? requestedRange : "live";
  const requestedTab = searchParams.get("tab");
  const canSeeUpdates = can("updates.view");
  const tabs = TABS.filter(({ id }) => id !== "updates" || canSeeUpdates);
  const tab: DeviceTab = isTab(requestedTab) && (requestedTab !== "updates" || canSeeUpdates) ? requestedTab : "usage";
  const agent = inventory.agents.find((candidate) => candidate.id === deviceId);
  const live = inventory.metrics.get(deviceId);
  const { data, loading, error } = useDeviceMetrics({ authorizedFetch, deviceId, range, latest: live });

  useEffect(() => {
    setPageTitle(agent?.name ?? null);
    return () => setPageTitle(null);
  }, [agent?.name, setPageTitle]);

  const chooseRange = (next: MetricsRange) => {
    const params = new URLSearchParams({ id: deviceId });
    if (next !== "live") params.set("range", next);
    setSearchParams(params, { replace: true });
  };
  const tabPath = (next: DeviceTab) => {
    const params = new URLSearchParams({ id: deviceId });
    if (next !== "usage") params.set("tab", next);
    return `?${params}`;
  };

  if (inventory.hasData && !agent) {
    return (
      <div className="empty-state">
        <MonitorOff size={22} aria-hidden="true" />
        <strong>Device not found</strong>
        <span>It may have been deleted.</span>
        <Link className="secondary-button" to="/">Back to devices</Link>
      </div>
    );
  }

  const latest = agent?.connected ? live ?? data?.latest ?? null : null;
  const spanMs = METRICS_RANGES.find((entry) => entry.id === range)!.spanMs;
  const points = data?.points ?? [];
  const end = Math.max(points.at(-1)?.at ?? 0, latest?.at ?? 0, data?.loadedAt ?? 0);
  const domain: [number, number] = [end - spanMs, end];
  const stepMs = data?.step_ms ?? 5_000;
  const periodMs = range === "live" ? null : stepMs;
  const memoryTotal = Math.max(latest?.memory_total_bytes ?? 0, ...points.map((point) => point.memory_total_bytes));
  const storageTotal = Math.max(storageOf(latest?.volumes ?? []).total, ...points.map((point) => point.storage_total_bytes));
  const networkPeak = Math.max(0, ...points.flatMap((point) => [point.network_received_bytes_per_second, point.network_sent_bytes_per_second]));
  const chart = { points, domain, gapMs: stepMs * 3, periodMs };

  return (
    <>
      {tab === "usage" && <HeaderActions>
        <div className="segmented" role="group" aria-label="Time range">
          {METRICS_RANGES.map((entry) => (
            <button key={entry.id} type="button" aria-pressed={range === entry.id} onClick={() => chooseRange(entry.id)}>{entry.label}</button>
          ))}
        </div>
      </HeaderActions>}

      <div className="device-page">
        <div className="device-status">
          <Link to="/" className="back-link"><ArrowLeft size={15} aria-hidden="true" />Devices</Link>
          {agent && <StatusLine connected={agent.connected} updatingTo={agent.updating_to} latest={latest} />}
          {agent?.system && (
            <p className="status-line">
              <Laptop size={15} aria-hidden="true" />
              <span>{[agent.system.os, agent.system.model].filter(Boolean).join(" · ")}</span>
            </p>
          )}
          {agent && (
            <p className="status-line">
              <Building2 size={15} aria-hidden="true" />
              <Link to={`/${serializeDeviceFilters({ ...DEFAULT_DEVICE_FILTERS, company: agent.company_id || NO_COMPANY })}`} className="back-link">
                {companyName(inventory.companies, agent.company_id) ?? "No company"}
              </Link>
              {can("companies.manage") && <button ref={moveOpener} type="button" className="link-button" onClick={() => setMoving(true)} aria-haspopup="dialog">Move</button>}
            </p>
          )}
          {agent?.connected && (can("terminal.connect") || can("terminal.connect_system")) && (
            <a className="secondary-button device-terminal" href={terminalPagePath(agent.id)} target="_blank" rel="noreferrer" aria-label={`Open a terminal on ${agent.name} in a new tab`}>
              <Terminal size={15} aria-hidden="true" />Terminal
            </a>
          )}
        </div>
        {agent && <DeviceWake key={agent.id} agent={agent} />}

        <nav className="toolbox-tabs area-tabs device-tabs" aria-label="Device">
          {tabs.map(({ id, label, icon: Icon }) => (
            <Link key={id} to={tabPath(id)} replace aria-current={tab === id ? "page" : undefined}>
              <Icon size={16} aria-hidden="true" />{label}
            </Link>
          ))}
        </nav>

        {tab === "updates" ? agent && <DeviceUpdates key={agent.id} agent={agent} />
        : tab !== "usage" ? agent && <DeviceSystem key={agent.id} agent={agent} tab={tab} /> : <>
        {error && <div className="error-banner" role="alert"><CircleAlert size={16} aria-hidden="true" /><span>{error}</span></div>}

        {(latest || agent?.connected) && <Summary latest={latest} />}

        {agent && can("alerts.view") && <DeviceAlerts deviceId={agent.id} />}

        {!data ? (
          <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading resource usage…</strong></div>
        ) : !points.length ? (
          <div className={`empty-state compact${loading ? " refreshing" : ""}`}>
            <Activity size={22} aria-hidden="true" />
            <strong>{range === "live" ? "No live readings" : "No readings in this range"}</strong>
            <span>
              {range === "live"
                ? agent?.connected ? "An online device reports every few seconds." : "The device reports while it’s online."
                : "Each online device’s usage is kept for 7 days."}
            </span>
          </div>
        ) : (
          <>
            <div className={`metric-charts${loading ? " refreshing" : ""}`}>
              <MetricChart {...chart} title="CPU" area series={CPU} yMax={100} formatTick={(value) => `${value}%`} headline={latest && formatPercent(latest.cpu_percent)} />
              <MetricChart {...chart} title="Memory" area series={MEMORY} yMax={memoryTotal || 1} formatTick={formatBytes} headline={latest && `${formatBytes(latest.memory_used_bytes)} of ${formatBytes(latest.memory_total_bytes)}`} />
              <MetricChart {...chart} title="Network" series={NETWORK} yMax={niceCeiling(Math.max(networkPeak * 8, 1_000)) / 8} formatTick={formatBitRate} headline={latest && `↓ ${formatBitRate(latest.network_received_bytes_per_second)}  ↑ ${formatBitRate(latest.network_sent_bytes_per_second)}`} />
              <MetricChart {...chart} title="Storage" area series={STORAGE} yMax={storageTotal || 1} formatTick={formatBytes} headline={latest && `${formatBytes(storageOf(latest.volumes).used)} of ${formatBytes(storageOf(latest.volumes).total)}`} />
            </div>
            <DataTable points={points} />
          </>
        )}

        {latest && latest.volumes.length > 0 && <Volumes reading={latest} />}
        </>}
      </div>
      {moving && agent && <MoveDeviceDialog agent={agent} returnFocus={moveOpener} onClose={() => setMoving(false)} />}
    </>
  );
}

function StatusLine({ connected, updatingTo, latest }: { connected: boolean; updatingTo?: string; latest: MetricsReading | null }) {
  const tone = connected ? "online" : updatingTo ? "updating" : "offline";
  return (
    <p className="status-line">
      <i className={`led ${tone}`} aria-hidden="true" />
      <span>{connected ? "Online" : updatingTo ? `Updating to ${updatingTo}` : "Offline"}</span>
      {latest && <><span aria-hidden="true">·</span><span>Up {formatUptime(latest.uptime_seconds)}</span></>}
    </p>
  );
}

// The latest reading at a glance; an online device that hasn't reported yet
// shows that it's waiting.
function Summary({ latest }: { latest: MetricsReading | null }) {
  const storage = storageOf(latest?.volumes ?? []);
  const memory = latest ? percentOf(latest.memory_used_bytes, latest.memory_total_bytes) : 0;
  const disk = percentOf(storage.used, storage.total);
  const empty = "Waiting…";
  return (
    <dl className="metric-summary">
      <div>
        <dt>CPU</dt>
        <dd>{latest ? formatPercent(latest.cpu_percent) : empty}</dd>
        {latest && <UsageBar percent={latest.cpu_percent} />}
      </div>
      <div>
        <dt>Memory</dt>
        <dd>{latest ? formatPercent(memory) : empty}</dd>
        {latest && <><UsageBar percent={memory} /><small>{formatBytes(latest.memory_used_bytes)} of {formatBytes(latest.memory_total_bytes)}</small></>}
      </div>
      <div>
        <dt>Network</dt>
        <dd>{latest ? formatBitRate(latest.network_received_bytes_per_second + latest.network_sent_bytes_per_second) : empty}</dd>
        {latest && <small>↓ {formatBitRate(latest.network_received_bytes_per_second)} · ↑ {formatBitRate(latest.network_sent_bytes_per_second)}</small>}
      </div>
      <div>
        <dt>Storage</dt>
        <dd>{latest && storage.total ? formatPercent(disk) : empty}</dd>
        {latest && storage.total > 0 && <><UsageBar percent={disk} /><small>{formatBytes(storage.total - storage.used)} free of {formatBytes(storage.total)}</small></>}
      </div>
    </dl>
  );
}

function Volumes({ reading }: { reading: MetricsReading }) {
  return (
    <section className="volume-list" aria-label="Volumes">
      <h3>Volumes</h3>
      <ul>
        {reading.volumes.map((volume) => {
          const used = volume.total_bytes - volume.free_bytes;
          const percent = percentOf(used, volume.total_bytes);
          return (
            <li key={volume.name}>
              <strong title={volume.name}>{volume.name}</strong>
              <UsageBar percent={percent} />
              <span>{formatPercent(percent)}</span>
              <small>{formatBytes(volume.free_bytes)} free of {formatBytes(volume.total_bytes)}</small>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

// Every point the charts show, for reading exact values without a pointer.
function DataTable({ points }: { points: MetricsPoint[] }) {
  if (!points.length) return null;
  return (
    <details className="metric-table">
      <summary>Show the data as a table</summary>
      <div className="table-scroll">
        <table className="data-table">
          <thead>
            <tr><th scope="col">Time</th><th scope="col">CPU</th><th scope="col">Memory</th><th scope="col">Received</th><th scope="col">Sent</th><th scope="col">Storage used</th></tr>
          </thead>
          <tbody>
            {[...points].reverse().map((point) => (
              <tr key={point.at}>
                <th scope="row">{new Date(point.at).toLocaleString([], { month: "short", day: "numeric", hour: "numeric", minute: "2-digit", second: "2-digit" })}</th>
                <td>{formatPercent(point.cpu_percent)}</td>
                <td>{formatBytes(point.memory_used_bytes)}</td>
                <td>{formatBitRate(point.network_received_bytes_per_second)}</td>
                <td>{formatBitRate(point.network_sent_bytes_per_second)}</td>
                <td>{formatBytes(point.storage_used_bytes)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </details>
  );
}
