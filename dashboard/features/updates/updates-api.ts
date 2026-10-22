// Requests to the updates API.
import { type AuthorizedFetch, expectJson, jsonBody } from "../../lib/http";
import type { JobDetail } from "../automation/model";
import { type DeviceUpdateSummary, type DeviceUpdates, type InstallChoice, type RebootBehavior, type RunKind, type UpdateCategory, type UpdateRun, installBody } from "./model";

const device = (id: string) => `/v1/agents/${encodeURIComponent(id)}/updates`;

export async function fetchDeviceUpdates(authorizedFetch: AuthorizedFetch, deviceId: string) {
  return expectJson<DeviceUpdates>(await authorizedFetch(device(deviceId)), "The device’s updates could not be loaded.");
}

// Asks the online device to scan; returns the scan's run.
export async function requestScan(authorizedFetch: AuthorizedFetch, deviceId: string) {
  return expectJson<UpdateRun>(await authorizedFetch(`${device(deviceId)}/scan`, jsonBody({})), "The device could not be asked to scan for updates.");
}

export async function startInstall(authorizedFetch: AuthorizedFetch, deviceId: string, choice: InstallChoice) {
  return expectJson<UpdateRun>(await authorizedFetch(`${device(deviceId)}/install`, jsonBody(installBody(choice))), "The install could not be started.");
}

export async function fetchRuns(authorizedFetch: AuthorizedFetch, { deviceId, kind, limit }: { deviceId?: string; kind?: RunKind; limit?: number }) {
  const params = new URLSearchParams();
  if (deviceId) params.set("device_id", deviceId);
  if (kind) params.set("kind", kind);
  if (limit) params.set("limit", String(limit));
  const query = params.toString();
  const { runs } = await expectJson<{ runs: UpdateRun[] }>(await authorizedFetch(`/v1/update-runs${query ? `?${query}` : ""}`), "The update history could not be loaded.");
  return runs;
}

export async function fetchRun(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<UpdateRun>(await authorizedFetch(`/v1/update-runs/${encodeURIComponent(id)}`), "The install could not be loaded.");
}

export async function fetchFleet(authorizedFetch: AuthorizedFetch) {
  const { devices } = await expectJson<{ devices: DeviceUpdateSummary[] }>(await authorizedFetch("/v1/updates/devices"), "The devices’ updates could not be loaded.");
  return devices;
}

// A job that scans, or installs updates, on chosen devices.
export type UpdateJobInput =
  | { action: "scan" }
  | { action: "install"; categories: UpdateCategory[]; reboot: RebootBehavior; reboot_delay_minutes: number };

export async function startUpdateJob(authorizedFetch: AuthorizedFetch, deviceIds: readonly string[], updates: UpdateJobInput, offline: { offline: "skip" | "wait"; wait_hours: number }) {
  const body = { updates, target: "devices", device_ids: [...deviceIds], company_ids: [], ...offline };
  return expectJson<JobDetail>(await authorizedFetch("/v1/jobs", jsonBody(body)), updates.action === "scan" ? "The scan could not be started." : "The install could not be started.");
}
