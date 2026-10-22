// Requests to the tags API.
import { type AuthorizedFetch, expectJson, expectOk, jsonBody } from "../../lib/http";
import type { TagColor } from "../agents/types";
import type { TagSummary } from "./model";

export async function fetchTags(authorizedFetch: AuthorizedFetch) {
  return expectJson<TagSummary[]>(await authorizedFetch("/v1/tags"), "The tags could not be loaded.");
}

export async function createTag(authorizedFetch: AuthorizedFetch, tag: { name: string; color: TagColor }) {
  return expectJson<TagSummary>(await authorizedFetch("/v1/tags", jsonBody(tag)), "The tag could not be created.");
}

export async function updateTag(authorizedFetch: AuthorizedFetch, id: string, change: { name?: string; color?: TagColor }) {
  return expectJson<TagSummary>(await authorizedFetch(`/v1/tags/${encodeURIComponent(id)}`, jsonBody(change, "PATCH")), "The tag could not be saved.");
}

export async function deleteTag(authorizedFetch: AuthorizedFetch, id: string) {
  await expectOk(await authorizedFetch(`/v1/tags/${encodeURIComponent(id)}`, { method: "DELETE" }), "The tag could not be deleted.");
}

// Adds tags to devices and takes others off them; resolves to how many
// devices changed.
export async function changeDeviceTags(authorizedFetch: AuthorizedFetch, change: { deviceIds: readonly string[]; add: readonly string[]; remove: readonly string[] }) {
  const { changed } = await expectJson<{ changed: number }>(
    await authorizedFetch("/v1/device-tags", jsonBody({ device_ids: change.deviceIds, add: change.add, remove: change.remove })),
    "The tags could not be changed.",
  );
  return changed;
}
