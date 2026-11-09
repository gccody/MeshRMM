// Requests to the monitors API.
import { type AuthorizedFetch, expectJson, expectOk, jsonBody } from "../../lib/http";
import type { DeviceMonitor, RemediationAttempt } from "./model";

export async function fetchDeviceMonitors(authorizedFetch: AuthorizedFetch, deviceId: string) {
  return expectJson<DeviceMonitor[]>(
    await authorizedFetch(`/v1/agents/${encodeURIComponent(deviceId)}/monitors`),
    "The device's monitors could not be loaded.",
  );
}

// Has the device run the monitors now: those of `ruleIds`, or every one.
export async function checkMonitors(authorizedFetch: AuthorizedFetch, deviceId: string, ruleIds: string[]) {
  await expectOk(
    await authorizedFetch(`/v1/agents/${encodeURIComponent(deviceId)}/monitors/check`, jsonBody({ rule_ids: ruleIds })),
    "The device could not be asked to check.",
  );
}

export async function fetchRemediations(authorizedFetch: AuthorizedFetch, alertId: string) {
  return expectJson<RemediationAttempt[]>(
    await authorizedFetch(`/v1/alerts/${encodeURIComponent(alertId)}/remediations`),
    "The self-healing attempts could not be loaded.",
  );
}
