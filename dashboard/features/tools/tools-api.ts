// The remote tools' requests: plain JSON, each answered by the device
// through the server.
import { type AuthorizedFetch, expectJson, expectOk, jsonBody } from "../../lib/http";
import type { ProcessInfo, ProcessList, ProcessesEnded, ServiceAction, ServiceInfo, ServiceList, ServiceStartup } from "./lists";
import type { RegistryData, RegistryKey, RegistryListing, RegistryValue } from "./registry";

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

const keyQuery = (key: RegistryKey) => new URLSearchParams({ root: key.root, path: key.path, view: key.view });

export async function listRegistryKey(fetch: AuthorizedFetch, deviceId: string, key: RegistryKey, signal?: AbortSignal) {
  const response = await fetch(`${device(deviceId)}/registry?${keyQuery(key)}`, { signal });
  return expectJson<RegistryListing>(response, "The key couldn’t be opened.");
}

export async function getRegistryValue(fetch: AuthorizedFetch, deviceId: string, key: RegistryKey, name: string) {
  const query = keyQuery(key);
  query.set("name", name);
  const response = await fetch(`${device(deviceId)}/registry/value?${query}`);
  return expectJson<RegistryValue>(response, "The value couldn’t be read.");
}

// Each change answers once the device has made it.
async function changeRegistry(fetch: AuthorizedFetch, deviceId: string, path: string, body: object, fallback: string) {
  const response = await fetch(`${device(deviceId)}/registry/${path}`, jsonBody(body));
  await expectOk(response, fallback);
}

export const createRegistryKey = (fetch: AuthorizedFetch, deviceId: string, key: RegistryKey) =>
  changeRegistry(fetch, deviceId, "keys", key, "The key couldn’t be created.");

export const deleteRegistryKey = (fetch: AuthorizedFetch, deviceId: string, key: RegistryKey, recursive: boolean) =>
  changeRegistry(fetch, deviceId, "keys/delete", { ...key, recursive }, "The key couldn’t be deleted.");

export async function setRegistryValue(fetch: AuthorizedFetch, deviceId: string, key: RegistryKey, name: string, data: RegistryData, create: boolean) {
  const response = await fetch(`${device(deviceId)}/registry/values`, jsonBody({ ...key, name, ...data, create }));
  return expectJson<RegistryValue>(response, "The value couldn’t be saved.");
}

export const renameRegistryValue = (fetch: AuthorizedFetch, deviceId: string, key: RegistryKey, from: string, to: string) =>
  changeRegistry(fetch, deviceId, "values/rename", { ...key, from, to }, "The value couldn’t be renamed.");

export const deleteRegistryValue = (fetch: AuthorizedFetch, deviceId: string, key: RegistryKey, name: string) =>
  changeRegistry(fetch, deviceId, "values/delete", { ...key, name }, "The value couldn’t be deleted.");
