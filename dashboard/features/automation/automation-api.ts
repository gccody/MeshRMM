// Requests to the jobs and automation policies API.
import { type AuthorizedFetch, expectJson, expectOk, jsonBody } from "../../lib/http";
import type { Job, JobDetail, Policy } from "./model";

export async function fetchJobs(authorizedFetch: AuthorizedFetch, policyId: string | null) {
  const query = policyId ? `?${new URLSearchParams({ policy_id: policyId })}` : "";
  const { jobs } = await expectJson<{ jobs: Job[] }>(await authorizedFetch(`/v1/jobs${query}`), "The jobs could not be loaded.");
  return jobs;
}

export async function fetchJob(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<JobDetail>(await authorizedFetch(`/v1/jobs/${encodeURIComponent(id)}`), "The job could not be loaded.");
}

// What POST /v1/jobs takes: a script and account, or a file and folder, on
// the devices, and what happens to offline ones.
export type StartJobInput = {
  script_id?: string;
  run_as?: "user" | "system";
  file_id?: string;
  destination?: "user" | "public";
  target: "devices" | "companies" | "all";
  device_ids: string[];
  company_ids: string[];
  offline: "skip" | "wait";
  wait_hours: number;
};

export async function startJob(authorizedFetch: AuthorizedFetch, input: StartJobInput) {
  return expectJson<JobDetail>(await authorizedFetch("/v1/jobs", jsonBody(input)), "The job could not be started.");
}

export async function cancelJob(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<JobDetail>(await authorizedFetch(`/v1/jobs/${encodeURIComponent(id)}/cancel`, jsonBody({})), "The job could not be cancelled.");
}

export async function fetchPolicies(authorizedFetch: AuthorizedFetch) {
  const { policies } = await expectJson<{ policies: Policy[] }>(await authorizedFetch("/v1/automation-policies"), "The policies could not be loaded.");
  return policies;
}

export async function savePolicy(authorizedFetch: AuthorizedFetch, id: string | null, input: unknown) {
  const response = id
    ? await authorizedFetch(`/v1/automation-policies/${encodeURIComponent(id)}`, jsonBody(input, "PUT"))
    : await authorizedFetch("/v1/automation-policies", jsonBody(input));
  return expectJson<Policy>(response, "The policy could not be saved.");
}

export async function deletePolicy(authorizedFetch: AuthorizedFetch, id: string) {
  await expectOk(await authorizedFetch(`/v1/automation-policies/${encodeURIComponent(id)}`, { method: "DELETE" }), "The policy could not be deleted.");
}

// Starts a job of the policy now; returns the job.
export async function runPolicy(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<Job>(await authorizedFetch(`/v1/automation-policies/${encodeURIComponent(id)}/run`, jsonBody({})), "The policy could not be run.");
}
