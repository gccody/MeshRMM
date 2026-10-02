import { type AuthorizedFetch, errorMessage } from "../../lib/http";
import {
  type RunAs,
  type ScriptDraft,
  type ScriptRun,
  type Toolbox,
  type ToolboxFile,
  type ToolboxScript,
  scriptBody,
} from "./model";

const json = (body: unknown): RequestInit => ({
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify(body),
});

async function expect<T>(response: Response, fallback: string): Promise<T> {
  if (!response.ok) throw new Error(await errorMessage(response, fallback));
  return (await response.json()) as T;
}

async function expectEmpty(response: Response, fallback: string) {
  if (!response.ok) throw new Error(await errorMessage(response, fallback));
}

/** The scripts and files the user may use: their own and the company's shared ones. */
export async function fetchToolbox(authorizedFetch: AuthorizedFetch) {
  return expect<Toolbox>(await authorizedFetch("/v1/toolbox"), "The toolbox could not be loaded.");
}

/** A script with its body. */
export async function fetchScript(authorizedFetch: AuthorizedFetch, id: string) {
  return expect<ToolboxScript>(
    await authorizedFetch(`/v1/toolbox/scripts/${encodeURIComponent(id)}`),
    "The script could not be loaded.",
  );
}

/**
 * Creates a script, or replaces one. Returns `null` when the user can no
 * longer see the script, as when an administrator unshares a teammate's.
 */
export async function saveScript(authorizedFetch: AuthorizedFetch, id: string | null, draft: ScriptDraft) {
  const response = await authorizedFetch(
    id ? `/v1/toolbox/scripts/${encodeURIComponent(id)}` : "/v1/toolbox/scripts",
    { method: id ? "PUT" : "POST", ...json(scriptBody(draft)) },
  );
  if (response.status === 204) return null;
  return expect<ToolboxScript>(response, "The script could not be saved.");
}

export async function deleteScript(authorizedFetch: AuthorizedFetch, id: string) {
  await expectEmpty(
    await authorizedFetch(`/v1/toolbox/scripts/${encodeURIComponent(id)}`, { method: "DELETE" }),
    "The script could not be deleted.",
  );
}

async function sha256Hex(file: Blob) {
  const digest = await crypto.subtle.digest("SHA-256", await file.arrayBuffer());
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

/** Uploads a file to the library. The server stores it only if it arrives intact. */
export async function uploadFile(authorizedFetch: AuthorizedFetch, file: File, folder: string, shared: boolean) {
  const query = new URLSearchParams({ name: file.name, folder, shared: String(shared), sha256: await sha256Hex(file) });
  return expect<ToolboxFile>(
    await authorizedFetch(`/v1/toolbox/files?${query}`, {
      method: "POST",
      headers: { "Content-Type": "application/octet-stream" },
      body: file,
    }),
    `${file.name} could not be uploaded.`,
  );
}

/** Renames, moves or shares a file. Returns `null` when the user can no longer see it. */
export async function updateFile(authorizedFetch: AuthorizedFetch, id: string, details: { name: string; folder: string; shared: boolean }) {
  const response = await authorizedFetch(`/v1/toolbox/files/${encodeURIComponent(id)}`, { method: "PUT", ...json(details) });
  if (response.status === 204) return null;
  return expect<ToolboxFile>(response, "The file could not be saved.");
}

export async function deleteFile(authorizedFetch: AuthorizedFetch, id: string) {
  await expectEmpty(
    await authorizedFetch(`/v1/toolbox/files/${encodeURIComponent(id)}`, { method: "DELETE" }),
    "The file could not be deleted.",
  );
}

/** Saves a library file to the browser's downloads. */
export async function downloadFile(authorizedFetch: AuthorizedFetch, file: ToolboxFile) {
  const response = await authorizedFetch(`/v1/toolbox/files/${encodeURIComponent(file.id)}/content`);
  if (!response.ok) throw new Error(await errorMessage(response, `${file.name} could not be downloaded.`));
  const url = URL.createObjectURL(await response.blob());
  try {
    const link = document.createElement("a");
    link.href = url;
    link.download = file.name;
    document.body.appendChild(link);
    link.click();
    link.remove();
  } finally {
    // The download has its own copy once the click is handled.
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  }
}

export async function runScript(authorizedFetch: AuthorizedFetch, deviceId: string, scriptId: string, runAs: RunAs) {
  return expect<ScriptRun>(
    await authorizedFetch(`/v1/agents/${encodeURIComponent(deviceId)}/script-runs`, {
      method: "POST",
      ...json({ script_id: scriptId, run_as: runAs }),
    }),
    "The script could not be run.",
  );
}

/** Recent runs, newest first, without their output. */
export async function fetchRuns(authorizedFetch: AuthorizedFetch, deviceId?: string) {
  const query = deviceId ? `?${new URLSearchParams({ device_id: deviceId })}` : "";
  const { runs } = await expect<{ runs: ScriptRun[] }>(
    await authorizedFetch(`/v1/script-runs${query}`),
    "Recent runs could not be loaded.",
  );
  return runs;
}

/** A run with its output. */
export async function fetchRun(authorizedFetch: AuthorizedFetch, id: string) {
  return expect<ScriptRun>(
    await authorizedFetch(`/v1/script-runs/${encodeURIComponent(id)}`),
    "The run could not be loaded.",
  );
}
