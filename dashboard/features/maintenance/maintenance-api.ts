// Requests to the maintenance windows API.
import { type AuthorizedFetch, expectJson, jsonBody } from "../../lib/http";
import type { DeviceWindows, MaintenanceTarget, MaintenanceWindow, Showing, WindowList } from "./model";
import { windowsPath } from "./model";

export async function fetchWindows(authorizedFetch: AuthorizedFetch, showing: Showing, filters: { device?: string; company?: string; policy?: string } = {}) {
  return expectJson<WindowList>(await authorizedFetch(windowsPath(showing, filters)), "The maintenance windows could not be loaded.");
}

// The device's open windows and its latest past ones.
export async function fetchDeviceWindows(authorizedFetch: AuthorizedFetch, deviceId: string) {
  return expectJson<DeviceWindows>(await authorizedFetch(`/v1/agents/${encodeURIComponent(deviceId)}/maintenance-windows`), "The device’s maintenance windows could not be loaded.");
}

// Puts the devices `body` covers in maintenance: a window each.
export async function startMaintenance(authorizedFetch: AuthorizedFetch, body: Record<string, unknown>) {
  return expectJson<{ created: number; windows: MaintenanceWindow[] }>(
    await authorizedFetch("/v1/maintenance-windows", jsonBody(body)),
    "The devices could not be put in maintenance.",
  );
}

// Ends a window now, or cancels it if it hasn't started.
export async function endWindow(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<MaintenanceWindow>(await authorizedFetch(`/v1/maintenance-windows/${encodeURIComponent(id)}/end`, jsonBody({})), "The maintenance window could not be ended.");
}

// Ends the windows in force on the devices `target` covers, and with
// `upcoming` cancels those to come. Resolves to how many ended.
export async function endMaintenance(authorizedFetch: AuthorizedFetch, target: MaintenanceTarget, upcoming = false) {
  const { ended } = await expectJson<{ ended: number }>(
    await authorizedFetch("/v1/maintenance-windows/end", jsonBody({ ...target, upcoming })),
    "Maintenance could not be ended.",
  );
  return ended;
}

// Changes an open window's end (null for until someone ends it), reason or
// what it holds back.
export async function updateWindow(authorizedFetch: AuthorizedFetch, id: string, change: { ends_at?: number | null; reason?: string; automation?: string }) {
  return expectJson<MaintenanceWindow>(await authorizedFetch(`/v1/maintenance-windows/${encodeURIComponent(id)}`, jsonBody(change, "PATCH")), "The maintenance window could not be changed.");
}
