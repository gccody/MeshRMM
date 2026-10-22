// Requests to the alerts API.
import { type AuthorizedFetch, expectJson, expectOk, jsonBody } from "../../lib/http";
import type { Alert, AlertList, AlertRule, Channel, ChannelList, Recipients } from "./model";

export async function fetchAlerts(authorizedFetch: AuthorizedFetch, query: string) {
  return expectJson<AlertList>(await authorizedFetch(`/v1/alerts${query ? `?${query}` : ""}`), "The alerts could not be loaded.");
}

export async function respondToAlert(authorizedFetch: AuthorizedFetch, alert: Alert, action: "acknowledge" | "resolve") {
  const response = await authorizedFetch(`/v1/alerts/${encodeURIComponent(alert.id)}/${action}`, jsonBody({}));
  return expectJson<Alert>(response, action === "acknowledge" ? "The alert could not be acknowledged." : "The alert could not be resolved.");
}

export async function fetchRules(authorizedFetch: AuthorizedFetch) {
  return expectJson<AlertRule[]>(await authorizedFetch("/v1/alert-rules"), "The alert rules could not be loaded.");
}

export async function saveRule(authorizedFetch: AuthorizedFetch, id: string | null, input: unknown) {
  const response = id
    ? await authorizedFetch(`/v1/alert-rules/${encodeURIComponent(id)}`, jsonBody(input, "PUT"))
    : await authorizedFetch("/v1/alert-rules", jsonBody(input));
  return expectJson<AlertRule>(response, "The rule could not be saved.");
}

export async function deleteRule(authorizedFetch: AuthorizedFetch, id: string) {
  await expectOk(await authorizedFetch(`/v1/alert-rules/${encodeURIComponent(id)}`, { method: "DELETE" }), "The rule could not be deleted.");
}

export async function fetchChannels(authorizedFetch: AuthorizedFetch) {
  return expectJson<ChannelList>(await authorizedFetch("/v1/notification-channels"), "The notification channels could not be loaded.");
}

export async function fetchRecipients(authorizedFetch: AuthorizedFetch) {
  return expectJson<Recipients>(await authorizedFetch("/v1/notification-channels/recipients"), "The users and roles could not be loaded.");
}

export async function saveChannel(authorizedFetch: AuthorizedFetch, id: string | null, input: unknown) {
  const response = id
    ? await authorizedFetch(`/v1/notification-channels/${encodeURIComponent(id)}`, jsonBody(input, "PUT"))
    : await authorizedFetch("/v1/notification-channels", jsonBody(input));
  return expectJson<Channel>(response, "The channel could not be saved.");
}

export async function deleteChannel(authorizedFetch: AuthorizedFetch, id: string) {
  await expectOk(await authorizedFetch(`/v1/notification-channels/${encodeURIComponent(id)}`, { method: "DELETE" }), "The channel could not be deleted.");
}

export async function testChannel(authorizedFetch: AuthorizedFetch, id: string) {
  const response = await authorizedFetch(`/v1/notification-channels/${encodeURIComponent(id)}/test`, jsonBody({}));
  return expectJson<{ delivered: boolean; error: string | null }>(response, "The test could not be sent.");
}
