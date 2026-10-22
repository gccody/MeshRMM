// Requests to the device notes API.
import { type AuthorizedFetch, expectJson, jsonBody } from "../../lib/http";
import type { Notes, RevisionText } from "./model";

const notesPath = (deviceId: string) => `/v1/agents/${encodeURIComponent(deviceId)}/notes`;

export async function fetchNotes(authorizedFetch: AuthorizedFetch, deviceId: string) {
  return expectJson<Notes>(await authorizedFetch(notesPath(deviceId)), "The device’s notes could not be loaded.");
}

export async function fetchRevision(authorizedFetch: AuthorizedFetch, deviceId: string, revision: number) {
  return expectJson<RevisionText>(await authorizedFetch(`${notesPath(deviceId)}/${revision}`), "That version of the notes could not be loaded.");
}

// Saves the notes over `revision`, the one the edit started from. Fails with
// the code `notes_changed` when someone saved a newer one.
export async function saveNotes(authorizedFetch: AuthorizedFetch, deviceId: string, body: string, revision: number) {
  return expectJson<Notes>(await authorizedFetch(notesPath(deviceId), jsonBody({ body, revision }, "PUT")), "The notes could not be saved.");
}
