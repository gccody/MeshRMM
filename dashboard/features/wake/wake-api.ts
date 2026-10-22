// Requests to the Wake-on-LAN API.
import { type AuthorizedFetch, expectJson, jsonBody } from "../../lib/http";
import type { Wake, WokenMany } from "./model";

// Wakes one offline device; resolves once its relays reported, or after a
// few seconds.
export async function wakeDevice(authorizedFetch: AuthorizedFetch, deviceId: string) {
  return expectJson<Wake>(await authorizedFetch(`/v1/agents/${encodeURIComponent(deviceId)}/wake`, jsonBody({})), "The device could not be woken.");
}

// Wakes the chosen devices that are offline.
export async function wakeDevices(authorizedFetch: AuthorizedFetch, deviceIds: readonly string[]) {
  return expectJson<WokenMany>(await authorizedFetch("/v1/wakes", jsonBody({ device_ids: deviceIds })), "The devices could not be woken.");
}

export async function fetchWakes(authorizedFetch: AuthorizedFetch, ids: readonly string[]) {
  const { wakes } = await expectJson<{ wakes: Wake[] }>(await authorizedFetch(`/v1/wakes?${new URLSearchParams({ ids: ids.join(",") })}`), "The wake requests could not be loaded.");
  return wakes;
}

// The device's latest wake requests, newest first.
export async function fetchRecentWakes(authorizedFetch: AuthorizedFetch, deviceId: string, limit = 1) {
  const { wakes } = await expectJson<{ wakes: Wake[] }>(await authorizedFetch(`/v1/agents/${encodeURIComponent(deviceId)}/wakes?limit=${limit}`), "The device’s wake requests could not be loaded.");
  return wakes;
}
