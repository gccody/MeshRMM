// Requests to the applications API.
import { type AuthorizedFetch, expectJson, expectOk, jsonBody } from "../../lib/http";
import type { JobDetail, OfflineChoice } from "../automation/model";
import type { Application, ApplicationRun, DevicePackages, OutdatedPackage, RunAction, RunRequest } from "./model";

export async function fetchLibrary(authorizedFetch: AuthorizedFetch) {
  const { applications } = await expectJson<{ applications: Application[] }>(await authorizedFetch("/v1/applications"), "The application library could not be loaded.");
  return applications;
}

export async function saveApplication(authorizedFetch: AuthorizedFetch, id: string | null, input: unknown) {
  const response = id
    ? await authorizedFetch(`/v1/applications/${encodeURIComponent(id)}`, jsonBody(input, "PUT"))
    : await authorizedFetch("/v1/applications", jsonBody(input));
  return expectJson<Application>(response, "The application could not be saved.");
}

export async function deleteApplication(authorizedFetch: AuthorizedFetch, id: string) {
  await expectOk(await authorizedFetch(`/v1/applications/${encodeURIComponent(id)}`, { method: "DELETE" }), "The application could not be deleted.");
}

const device = (id: string) => `/v1/agents/${encodeURIComponent(id)}`;

export async function fetchDevicePackages(authorizedFetch: AuthorizedFetch, deviceId: string) {
  return expectJson<DevicePackages>(await authorizedFetch(`${device(deviceId)}/packages`), "The device’s applications could not be loaded.");
}

// Asks the online device to scan; returns the scan's run.
export async function requestPackageScan(authorizedFetch: AuthorizedFetch, deviceId: string) {
  return expectJson<ApplicationRun>(await authorizedFetch(`${device(deviceId)}/packages/scan`, jsonBody({})), "The device could not be asked to scan for applications.");
}

// Installs, updates or uninstalls on one online device.
export async function startApplicationRun(authorizedFetch: AuthorizedFetch, deviceId: string, request: RunRequest) {
  return expectJson<ApplicationRun>(await authorizedFetch(`${device(deviceId)}/application-runs`, jsonBody(request)), "The device could not be asked to do that.");
}

export async function fetchApplicationRuns(authorizedFetch: AuthorizedFetch, { deviceId, action, limit }: { deviceId?: string; action?: RunAction; limit?: number }) {
  const params = new URLSearchParams();
  if (deviceId) params.set("device_id", deviceId);
  if (action) params.set("action", action);
  if (limit) params.set("limit", String(limit));
  const query = params.toString();
  const { runs } = await expectJson<{ runs: ApplicationRun[] }>(await authorizedFetch(`/v1/application-runs${query ? `?${query}` : ""}`), "The application history could not be loaded.");
  return runs;
}

export async function fetchApplicationRun(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<ApplicationRun>(await authorizedFetch(`/v1/application-runs/${encodeURIComponent(id)}`), "The run could not be loaded.");
}

export async function fetchOutdated(authorizedFetch: AuthorizedFetch) {
  const { packages } = await expectJson<{ packages: OutdatedPackage[] }>(await authorizedFetch("/v1/packages/outdated"), "The outdated applications could not be loaded.");
  return packages;
}

export type JobTarget = {
  target: "devices" | "companies" | "tags" | "all";
  device_ids: string[];
  company_ids: string[];
  tag_ids: string[];
  tag_match: "any" | "all";
  offline: OfflineChoice;
  wait_hours: number;
};

// A job that installs, updates or uninstalls on many devices.
export async function startApplicationJob(authorizedFetch: AuthorizedFetch, request: RunRequest, target: JobTarget) {
  return expectJson<JobDetail>(await authorizedFetch("/v1/jobs", jsonBody({ applications: request, ...target })), "The job could not be started.");
}
