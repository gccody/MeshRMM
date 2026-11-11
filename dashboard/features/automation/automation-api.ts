// Requests to the jobs and automation policies API.
import { type AuthorizedFetch, expectJson, expectOk, jsonBody } from "../../lib/http";
import { type Job, type JobDevicesPage, type Outcome, type Policy, jobDevicesPath } from "./model";

export async function fetchJobs(authorizedFetch: AuthorizedFetch, policyId: string | null) {
  const query = policyId ? `?${new URLSearchParams({ policy_id: policyId })}` : "";
  const { jobs } = await expectJson<{ jobs: Job[] }>(await authorizedFetch(`/v1/jobs${query}`), "The jobs could not be loaded.");
  return jobs;
}

// The job and its counts; its devices come a page at a time.
export async function fetchJob(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<Job>(await authorizedFetch(`/v1/jobs/${encodeURIComponent(id)}`), "The job could not be loaded.");
}

// A page of the job's devices with `outcome`, or every one, after the
// device `after`.
export async function fetchJobDevices(authorizedFetch: AuthorizedFetch, id: string, outcome: Outcome | null, after: string | null, limit?: number) {
  return expectJson<JobDevicesPage>(await authorizedFetch(jobDevicesPath(id, outcome, after, limit)), "The job's devices could not be loaded.");
}

// What POST /v1/jobs takes: a script and account, or a file and folder, on
// the devices, and what happens to offline ones.
export type StartJobInput = {
  script_id?: string;
  run_as?: "user" | "system";
  file_id?: string;
  destination?: "user" | "public";
  target: "devices" | "companies" | "tags" | "all";
  device_ids: string[];
  company_ids: string[];
  tag_ids: string[];
  tag_match: "any" | "all";
  offline: "skip" | "wait";
  wait_hours: number;
};

export async function startJob(authorizedFetch: AuthorizedFetch, input: StartJobInput) {
  return expectJson<Job>(await authorizedFetch("/v1/jobs", jsonBody(input)), "The job could not be started.");
}

export async function cancelJob(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<Job>(await authorizedFetch(`/v1/jobs/${encodeURIComponent(id)}/cancel`, jsonBody({})), "The job could not be cancelled.");
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

// Runs the policy now; returns its job, or for a maintenance policy how
// many windows it set.
export async function runPolicy(authorizedFetch: AuthorizedFetch, id: string) {
  return expectJson<Job | { maintenance: { devices: number; ends_at: number | null } }>(await authorizedFetch(`/v1/automation-policies/${encodeURIComponent(id)}/run`, jsonBody({})), "The policy could not be run.");
}
