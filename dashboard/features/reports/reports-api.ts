// Requests to the reports API.
import { type AuthorizedFetch, expectJson, expectOk, jsonBody } from "../../lib/http";
import type { Recipient, Report, ReportList, Schedule, Templates } from "./model";

export async function fetchTemplates(authorizedFetch: AuthorizedFetch) {
  return expectJson<Templates>(await authorizedFetch("/v1/report-templates"), "The report templates could not be loaded.");
}

export async function fetchReports(authorizedFetch: AuthorizedFetch, before: number | null = null) {
  const query = before === null ? "" : `?before=${before}`;
  return expectJson<ReportList>(await authorizedFetch(`/v1/reports${query}`), "The reports could not be loaded.");
}

export async function fetchReport(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<Report>(await authorizedFetch(`/v1/reports/${encodeURIComponent(id)}`), "The report could not be loaded.");
}

export async function runReport(authorizedFetch: AuthorizedFetch, input: unknown) {
  return expectJson<Report>(await authorizedFetch("/v1/reports", jsonBody(input)), "The report could not be started.");
}

export async function deleteReport(authorizedFetch: AuthorizedFetch, id: string) {
  await expectOk(await authorizedFetch(`/v1/reports/${encodeURIComponent(id)}`, { method: "DELETE" }), "The report could not be deleted.");
}

export async function fetchSchedules(authorizedFetch: AuthorizedFetch) {
  return expectJson<{ schedules: Schedule[] }>(await authorizedFetch("/v1/report-schedules"), "The report schedules could not be loaded.");
}

export async function fetchRecipients(authorizedFetch: AuthorizedFetch) {
  return expectJson<{ users: Recipient[] }>(await authorizedFetch("/v1/report-schedules/recipients"), "The users could not be loaded.");
}

export async function saveSchedule(authorizedFetch: AuthorizedFetch, id: string | null, input: unknown) {
  const response = id
    ? await authorizedFetch(`/v1/report-schedules/${encodeURIComponent(id)}`, jsonBody(input, "PUT"))
    : await authorizedFetch("/v1/report-schedules", jsonBody(input));
  return expectJson<Schedule>(response, "The schedule could not be saved.");
}

export async function deleteSchedule(authorizedFetch: AuthorizedFetch, id: string) {
  await expectOk(await authorizedFetch(`/v1/report-schedules/${encodeURIComponent(id)}`, { method: "DELETE" }), "The schedule could not be deleted.");
}

export async function runSchedule(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<Report>(await authorizedFetch(`/v1/report-schedules/${encodeURIComponent(id)}/run`, jsonBody({})), "The schedule could not be run.");
}
