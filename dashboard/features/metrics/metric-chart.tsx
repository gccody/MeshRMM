import { type KeyboardEvent, type PointerEvent, type ReactNode, useEffect, useState } from "react";
import { type MetricsPoint, areaPath, linePath, nearestPoint } from "./model";

export type Series = {
  label: string;
  // The series' color: "series-1" or "series-2".
  tone: "series-1" | "series-2";
  value: (point: MetricsPoint) => number;
  format: (value: number) => string;
};

type Props = {
  title: string;
  // The current value, beside the title.
  headline?: ReactNode;
  points: MetricsPoint[];
  series: Series[];
  // The time the chart spans, in Unix milliseconds.
  domain: [number, number];
  yMax: number;
  formatTick: (value: number) => string;
  // Points further apart than this are not joined: the device was offline.
  gapMs: number;
  // Whether points are averages over a period, which the tooltip says.
  periodMs: number | null;
  // Fills the area under a single series.
  area?: boolean;
};

const HEIGHT = 168;
const MARGIN = { top: 10, right: 12, bottom: 24, left: 60 };

function formatTime(at: number, spanMs: number) {
  const date = new Date(at);
  if (spanMs > 2 * 24 * 60 * 60_000) return date.toLocaleDateString([], { weekday: "short", hour: "numeric" });
  return date.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}

function formatTooltipTime(at: number, periodMs: number | null) {
  const options: Intl.DateTimeFormatOptions = periodMs === null
    ? { hour: "numeric", minute: "2-digit", second: "2-digit" }
    : { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" };
  const start = new Date(at).toLocaleString([], options);
  if (periodMs === null) return start;
  const end = new Date(at + periodMs).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  return `${start} – ${end}`;
}

// The chart's width follows its container.
function useWidth() {
  const [element, setElement] = useState<HTMLElement | null>(null);
  const [width, setWidth] = useState(0);
  useEffect(() => {
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => setWidth(Math.floor(entry.contentRect.width)));
    observer.observe(element);
    return () => observer.disconnect();
  }, [element]);
  return { ref: setElement, width };
}

// A line chart of one measure over time, with a crosshair that reads every
// series at the nearest point, on hover and with the arrow keys.
export function MetricChart({ title, headline, points, series, domain, yMax, formatTick, gapMs, periodMs, area = false }: Props) {
  const { ref, width } = useWidth();
  const [active, setActive] = useState<number | null>(null);
  const plotWidth = Math.max(0, width - MARGIN.left - MARGIN.right);
  const plotHeight = HEIGHT - MARGIN.top - MARGIN.bottom;
  const [from, to] = domain;
  const x = (at: number) => MARGIN.left + ((at - from) / Math.max(1, to - from)) * plotWidth;
  const y = (value: number) => MARGIN.top + plotHeight - (Math.min(value, yMax) / yMax) * plotHeight;
  const baseline = MARGIN.top + plotHeight;
  const visible = points.filter((point) => point.at >= from && point.at <= to);
  const activePoint = active === null ? null : visible.find((point) => point.at === active) ?? null;
  const ticks = [0, yMax / 2, yMax];
  const tickCount = width > 520 ? 5 : 3;
  const timeTicks = width > 0 ? Array.from({ length: tickCount }, (_, index) => from + ((to - from) * index) / (tickCount - 1)) : [];

  const pick = (event: PointerEvent<SVGSVGElement>) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const at = from + ((event.clientX - bounds.left - MARGIN.left) / Math.max(1, plotWidth)) * (to - from);
    setActive(nearestPoint(visible, at)?.at ?? null);
  };
  const step = (event: KeyboardEvent<SVGSVGElement>) => {
    if (!visible.length || (event.key !== "ArrowLeft" && event.key !== "ArrowRight")) return;
    event.preventDefault();
    const index = visible.findIndex((point) => point.at === active);
    const next = index < 0 ? visible.length - 1 : Math.max(0, Math.min(visible.length - 1, index + (event.key === "ArrowLeft" ? -1 : 1)));
    setActive(visible[next].at);
  };

  return (
    <section className="metric-chart" aria-label={title}>
      <header>
        <h3>{title}</h3>
        {headline && <span className="metric-headline">{headline}</span>}
      </header>
      {series.length > 1 && (
        <ul className="chart-legend">
          {series.map((entry) => <li key={entry.label}><i className={`line-key ${entry.tone}`} aria-hidden="true" />{entry.label}</li>)}
        </ul>
      )}
      <div className="chart-frame" ref={ref}>
        {width > 0 && (
          <svg
            width={width}
            height={HEIGHT}
            viewBox={`0 0 ${width} ${HEIGHT}`}
            role="img"
            aria-label={`${title} chart. Focus it and use the left and right arrow keys to read values.`}
            tabIndex={0}
            onPointerMove={pick}
            onPointerLeave={() => setActive(null)}
            onKeyDown={step}
            onFocus={() => setActive((current) => current ?? visible.at(-1)?.at ?? null)}
            onBlur={() => setActive(null)}
          >
            {ticks.map((tick) => (
              <g key={tick} className="chart-grid">
                <line x1={MARGIN.left} x2={MARGIN.left + plotWidth} y1={y(tick)} y2={y(tick)} />
                <text x={MARGIN.left - 8} y={y(tick)} dy="0.32em" textAnchor="end">{formatTick(tick)}</text>
              </g>
            ))}
            {timeTicks.map((at, index) => (
              <text key={at} className="chart-axis" x={x(at)} y={HEIGHT - 6} textAnchor={index === 0 ? "start" : index === timeTicks.length - 1 ? "end" : "middle"}>{formatTime(at, to - from)}</text>
            ))}
            {series.map((entry) => (
              <g key={entry.label} className={entry.tone}>
                {area && <path className="chart-area" d={areaPath(visible, entry.value, x, y, gapMs, baseline)} />}
                <path className="chart-line" d={linePath(visible, entry.value, x, y, gapMs)} />
              </g>
            ))}
            {activePoint && (
              <Crosshair
                point={activePoint}
                series={series}
                x={x(activePoint.at)}
                y={y}
                top={MARGIN.top}
                bottom={baseline}
                bounds={[MARGIN.left, MARGIN.left + plotWidth]}
                label={formatTooltipTime(activePoint.at, periodMs)}
              />
            )}
          </svg>
        )}
      </div>
    </section>
  );
}

const ROW_HEIGHT = 18;
// Instrument Sans at 12px averages a little under this per character.
const CHARACTER_WIDTH = 6.6;

function Crosshair({ point, series, x, y, top, bottom, bounds, label }: {
  point: MetricsPoint;
  series: Series[];
  x: number;
  y: (value: number) => number;
  top: number;
  bottom: number;
  bounds: [number, number];
  label: string;
}) {
  const rows = series.map((entry) => ({ entry, value: entry.format(entry.value(point)) }));
  const longest = Math.max(label.length, ...rows.map((row) => row.value.length + row.entry.label.length + 4));
  const width = Math.ceil(longest * CHARACTER_WIDTH) + 24;
  const height = 14 + ROW_HEIGHT * (rows.length + 1);
  // The tooltip sits right of the crosshair unless that leaves the plot.
  const left = x + 12 + width <= bounds[1] ? x + 12 : Math.max(bounds[0], x - 12 - width);
  return (
    <g className="chart-crosshair" aria-hidden="true">
      <line x1={x} x2={x} y1={top} y2={bottom} />
      {series.map((entry) => <circle key={entry.label} className={entry.tone} cx={x} cy={y(entry.value(point))} r={4} />)}
      <g className="chart-tooltip" transform={`translate(${left},${top})`}>
        <rect width={width} height={height} rx={8} />
        <text className="tooltip-time" x={12} y={22}>{label}</text>
        {rows.map((row, index) => (
          <g key={row.entry.label} transform={`translate(12,${22 + ROW_HEIGHT * (index + 1)})`}>
            <line className={row.entry.tone} x1={0} x2={10} y1={-4} y2={-4} />
            <text x={16}><tspan className="tooltip-value">{row.value}</tspan><tspan className="tooltip-label" dx={6}>{row.entry.label}</tspan></text>
          </g>
        ))}
      </g>
    </g>
  );
}
