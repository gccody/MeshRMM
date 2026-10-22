// The file browser's requests. Listings and changes are plain JSON; an
// upload's bytes go with XMLHttpRequest, the one browser API that reports
// how much of a request body has been sent.
import { type AuthorizedFetch, RequestError, expectJson, expectOk, jsonBody } from "../../lib/http";
import type { DownloadView, FileAccount, FileBatch, FileEntry, FileListing, FileRoots, FileSort, UploadView } from "./model";

const device = (deviceId: string) => `/v1/agents/${encodeURIComponent(deviceId)}/files`;

export type ListRequest = {
  path: string;
  account: FileAccount;
  offset: number;
  limit: number;
  sort: FileSort;
  descending: boolean;
  hidden: boolean;
  filter: string;
  refresh: boolean;
};

export async function listRoots(fetch: AuthorizedFetch, deviceId: string, account: FileAccount) {
  const response = await fetch(`${device(deviceId)}/roots?run_as=${account}`);
  return expectJson<FileRoots>(response, "The device’s drives couldn’t be listed.");
}

export async function listFolder(fetch: AuthorizedFetch, deviceId: string, request: ListRequest, signal?: AbortSignal) {
  const params = new URLSearchParams({
    path: request.path,
    run_as: request.account,
    offset: String(request.offset),
    limit: String(request.limit),
    sort: request.sort,
    order: request.descending ? "desc" : "asc",
  });
  if (request.hidden) params.set("hidden", "true");
  if (request.filter.trim()) params.set("filter", request.filter.trim());
  if (request.refresh) params.set("refresh", "true");
  const response = await fetch(`${device(deviceId)}?${params}`, { signal });
  return expectJson<FileListing>(response, "The folder couldn’t be listed.");
}

type Placed = { path: string; entry: FileEntry };

export async function createFolder(fetch: AuthorizedFetch, deviceId: string, account: FileAccount, path: string) {
  const response = await fetch(`${device(deviceId)}/folders`, jsonBody({ path, run_as: account }));
  return expectJson<Placed>(response, "The folder couldn’t be created.");
}

export async function renameEntry(fetch: AuthorizedFetch, deviceId: string, account: FileAccount, path: string, name: string) {
  const response = await fetch(`${device(deviceId)}/rename`, jsonBody({ path, name, run_as: account }));
  return expectJson<Placed>(response, "It couldn’t be renamed.");
}

export async function moveEntries(fetch: AuthorizedFetch, deviceId: string, account: FileAccount, paths: string[], destination: string) {
  const response = await fetch(`${device(deviceId)}/move`, jsonBody({ paths, destination, run_as: account }));
  return expectJson<FileBatch>(response, "They couldn’t be moved.");
}

export async function deleteEntries(fetch: AuthorizedFetch, deviceId: string, account: FileAccount, paths: string[], trash: boolean) {
  const response = await fetch(`${device(deviceId)}/delete`, jsonBody({ paths, trash, run_as: account }));
  return expectJson<FileBatch>(response, "They couldn’t be deleted.");
}

export async function createDownload(fetch: AuthorizedFetch, deviceId: string, account: FileAccount, paths: string[]) {
  const response = await fetch(`${device(deviceId)}/downloads`, jsonBody({ paths, run_as: account }));
  return expectJson<DownloadView>(response, "The download couldn’t start.");
}

export async function downloadProgress(fetch: AuthorizedFetch, id: string) {
  const response = await fetch(`/v1/file-downloads/${encodeURIComponent(id)}/progress`);
  return expectJson<DownloadView>(response, "The download’s progress couldn’t be read.");
}

// Hands a download to the browser, which saves it with its own progress
// and can resume it.
export function startBrowserDownload(url: string) {
  const link = document.createElement("a");
  link.href = url;
  link.download = "";
  link.rel = "noreferrer";
  document.body.append(link);
  link.click();
  link.remove();
}

export async function createUpload(
  fetch: AuthorizedFetch,
  deviceId: string,
  account: FileAccount,
  path: string,
  size: number,
  overwrite: boolean,
) {
  const response = await fetch(`${device(deviceId)}/uploads`, jsonBody({ path, size, overwrite, run_as: account }));
  return expectJson<UploadView>(response, "The upload couldn’t start.");
}

export async function uploadStatus(fetch: AuthorizedFetch, id: string) {
  const response = await fetch(`/v1/file-uploads/${encodeURIComponent(id)}`);
  return expectJson<UploadView>(response, "The upload’s progress couldn’t be read.");
}

export async function finishUpload(fetch: AuthorizedFetch, id: string, sha256: string) {
  const response = await fetch(`/v1/file-uploads/${encodeURIComponent(id)}/finish`, jsonBody({ sha256 }));
  return expectJson<Placed>(response, "The upload couldn’t be finished.");
}

export async function cancelUpload(fetch: AuthorizedFetch, id: string) {
  const response = await fetch(`/v1/file-uploads/${encodeURIComponent(id)}`, { method: "DELETE" });
  await expectOk(response, "The upload couldn’t be cancelled.");
}

// Sends `body`, the upload's bytes from `offset`. `onProgress` gets the
// bytes of `body` sent so far. Resolves with how much the device has.
export function sendUpload(
  id: string,
  offset: number,
  body: Blob,
  onProgress: (sent: number) => void,
  signal: AbortSignal,
): Promise<UploadView> {
  return new Promise((resolve, reject) => {
    const request = new XMLHttpRequest();
    request.open("PUT", `/v1/file-uploads/${encodeURIComponent(id)}?offset=${offset}`);
    request.setRequestHeader("X-MeshRMM-Request", "1");
    request.setRequestHeader("Content-Type", "application/octet-stream");
    request.responseType = "json";
    request.upload.onprogress = (event) => onProgress(event.loaded);
    request.onload = () => {
      const body = request.response as (UploadView & { error?: string; code?: string }) | null;
      if (request.status >= 200 && request.status < 300 && body) {
        resolve(body);
      } else {
        const message = body?.error ? body.error[0].toUpperCase() + body.error.slice(1) : "The upload stopped.";
        reject(new RequestError(/[.!?]$/u.test(message) ? message : `${message}.`, request.status, body?.code));
      }
    };
    request.onerror = () => reject(new RequestError("The connection to the server dropped.", 0, "network"));
    request.onabort = () => reject(new DOMException("The upload was stopped.", "AbortError"));
    signal.addEventListener("abort", () => request.abort(), { once: true });
    request.send(body);
  });
}
