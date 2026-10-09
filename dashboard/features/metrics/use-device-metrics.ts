import { useEffect, useState } from "react";
import { AuthenticationRequired, type AuthorizedFetch, errorMessage } from "../../lib/http";
import {
  type DeviceMetrics,
  METRICS_RANGES,
  type MetricsRange,
  type MetricsReading,
  appendLivePoint,
  parseDeviceMetrics,
} from "./model";

// History is stored a minute at a time, so it is read again each minute.
const HISTORY_REFRESH_MS = 60_000;
const LIVE_SPAN_MS = METRICS_RANGES[0].spanMs;

type Loaded = DeviceMetrics & {
  deviceId: string;
  // When the server answered, which ends a history range's time axis.
  loadedAt: number;
};

// A device's resource usage over `range`. The live range loads once and then
// grows with the readings the event socket delivers as `latest`.
export function useDeviceMetrics({ authorizedFetch, deviceId, range, latest }: {
  authorizedFetch: AuthorizedFetch;
  deviceId: string;
  range: MetricsRange;
  latest: MetricsReading | undefined;
}) {
  const [data, setData] = useState<Loaded | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [appliedAt, setAppliedAt] = useState<number | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        const response = await authorizedFetch(`/v1/agents/${encodeURIComponent(deviceId)}/metrics?range=${range}`);
        if (!response.ok) throw new Error(await errorMessage(response, "The device’s resource usage could not be loaded."));
        const parsed = parseDeviceMetrics(await response.json());
        if (!parsed) throw new Error("The server returned invalid resource usage.");
        if (cancelled) return;
        setData({ ...parsed, deviceId, loadedAt: Date.now() });
        setError(null);
      } catch (loadError) {
        if (cancelled || loadError instanceof AuthenticationRequired) return;
        setError(loadError instanceof Error ? loadError.message : "The device’s resource usage could not be loaded.");
      }
    };
    void load();
    const timer = range === "live" ? undefined : window.setInterval(() => void load(), HISTORY_REFRESH_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [authorizedFetch, deviceId, range]);

  // Each new reading joins the live points as it arrives.
  if (latest && latest.at !== appliedAt) {
    setAppliedAt(latest.at);
    if (data && data.deviceId === deviceId) {
      setData({
        ...data,
        latest,
        points: data.range === "live" ? appendLivePoint(data.points, latest, LIVE_SPAN_MS) : data.points,
      });
    }
  }

  const current = data && data.deviceId === deviceId ? data : null;
  return {
    data: current,
    // The range on screen is still the previous one while the new one loads.
    loading: !current || current.range !== range,
    error,
  };
}
