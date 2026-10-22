import { type AuthorizedFetch, expectJson, requestError } from "../../lib/http";
import type { Application, ApplicationChange, FleetApplication, InstalledOn, Snapshot } from "./model";

const device = (id: string) => `/v1/agents/${encodeURIComponent(id)}`;

// The device's latest snapshot, or null until it sent one.
export async function fetchSnapshot(authorizedFetch: AuthorizedFetch, deviceId: string): Promise<Snapshot | null> {
  const response = await authorizedFetch(`${device(deviceId)}/system-info`);
  if (response.status === 404) {
    const error = await requestError(response, "The device’s system information could not be loaded.");
    if (error.code === "not_collected") return null;
    throw error;
  }
  return expectJson<Snapshot>(response, "The device’s system information could not be loaded.");
}

export async function fetchApplications(authorizedFetch: AuthorizedFetch, deviceId: string) {
  const response = await authorizedFetch(`${device(deviceId)}/applications`);
  return (await expectJson<{ applications: Application[] }>(response, "The device’s applications could not be loaded.")).applications;
}

export async function fetchChanges(authorizedFetch: AuthorizedFetch, deviceId: string) {
  const response = await authorizedFetch(`${device(deviceId)}/application-changes?limit=200`);
  return (await expectJson<{ changes: ApplicationChange[] }>(response, "The device’s application changes could not be loaded.")).changes;
}

// Asks the online Agent for a new snapshot.
export async function requestRefresh(authorizedFetch: AuthorizedFetch, deviceId: string) {
  const response = await authorizedFetch(`${device(deviceId)}/system-info/refresh`, { method: "POST" });
  if (!response.ok) throw await requestError(response, "The device could not be asked for its system information.");
  await response.body?.cancel();
}

export async function fetchSoftware(authorizedFetch: AuthorizedFetch, params: URLSearchParams) {
  const query = params.toString();
  const response = await authorizedFetch(`/v1/software${query ? `?${query}` : ""}`);
  return expectJson<{ applications: FleetApplication[]; truncated: boolean }>(response, "The applications could not be loaded.");
}

export async function fetchInstalledOn(authorizedFetch: AuthorizedFetch, params: URLSearchParams) {
  const response = await authorizedFetch(`/v1/software/devices?${params}`);
  return (await expectJson<{ devices: InstalledOn[] }>(response, "The devices could not be loaded.")).devices;
}

