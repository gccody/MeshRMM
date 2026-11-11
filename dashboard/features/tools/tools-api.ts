// The remote tools' requests: plain JSON, each answered by the device
// through the server.
import { type AuthorizedFetch, expectJson, jsonBody } from "../../lib/http";
import type { ProcessInfo, ProcessList, ProcessesEnded, ServiceAction, ServiceInfo, ServiceList, ServiceStartup } from "./lists";

const device = (deviceId: string) => `/v1/agents/${encodeURIComponent(deviceId)}`;

export async function listProcesses(fetch: AuthorizedFetch, deviceId: string, signal?: AbortSignal) {
  const response = await fetch(`${device(deviceId)}/processes`, { signal });
  return expectJson<ProcessList>(response, "The device’s processes couldn’t be listed.");
}

// Ends exactly the processes listed, each only while it is the one that
// started when the list said.
export async function endProcesses(fetch: AuthorizedFetch, deviceId: string, processes: ProcessInfo[], tree: boolean) {
  const targets = processes.map((process) => ({ pid: process.pid, started_at: process.started_at }));
  const response = await fetch(`${device(deviceId)}/processes/end`, jsonBody({ processes: targets, tree }));
  return expectJson<ProcessesEnded>(response, "The processes couldn’t be ended.");
}

export async function listServices(fetch: AuthorizedFetch, deviceId: string, signal?: AbortSignal) {
  const response = await fetch(`${device(deviceId)}/services`, { signal });
  return expectJson<ServiceList>(response, "The device’s services couldn’t be listed.");
}

export async function controlService(fetch: AuthorizedFetch, deviceId: string, name: string, action: ServiceAction) {
  const response = await fetch(`${device(deviceId)}/services/control`, jsonBody({ name, action }));
  return expectJson<ServiceInfo>(response, `The service couldn’t ${action === "restart" ? "be restarted" : action === "stop" ? "be stopped" : "start"}.`);
}

export async function setServiceStartup(fetch: AuthorizedFetch, deviceId: string, name: string, startup: ServiceStartup) {
  const response = await fetch(`${device(deviceId)}/services/startup`, jsonBody({ name, startup }));
  return expectJson<ServiceInfo>(response, "How the service starts couldn’t be changed.");
}
