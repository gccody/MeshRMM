// Requests to the custom fields API.
import { type AuthorizedFetch, expectJson, expectOk, jsonBody } from "../../lib/http";
import type { AppliesTo, FieldDefinition, FieldFilter, FieldJson, FieldType, FieldValue, ScriptAccess } from "./model";

// Whose values: a device's or a company's.
export type FieldOwner = { kind: "device" | "company"; id: string };

const ownerPath = ({ kind, id }: FieldOwner) => `/v1/${kind === "device" ? "agents" : "companies"}/${encodeURIComponent(id)}/custom-fields`;

export async function fetchDefinitions(authorizedFetch: AuthorizedFetch) {
  const { fields } = await expectJson<{ fields: FieldDefinition[] }>(await authorizedFetch("/v1/custom-fields"), "The custom fields could not be loaded.");
  return fields;
}

export type DefinitionInput = {
  label: string;
  description: string;
  options: string[];
  applies_to: AppliesTo;
  script_access: ScriptAccess;
};

export async function createDefinition(authorizedFetch: AuthorizedFetch, input: DefinitionInput & { key: string; type: FieldType }) {
  return expectJson<FieldDefinition>(await authorizedFetch("/v1/custom-fields", jsonBody(input)), "The field could not be created.");
}

export async function updateDefinition(authorizedFetch: AuthorizedFetch, id: string, input: DefinitionInput) {
  return expectJson<FieldDefinition>(await authorizedFetch(`/v1/custom-fields/${encodeURIComponent(id)}`, jsonBody(input, "PUT")), "The field could not be saved.");
}

export async function deleteDefinition(authorizedFetch: AuthorizedFetch, id: string) {
  await expectOk(await authorizedFetch(`/v1/custom-fields/${encodeURIComponent(id)}`, { method: "DELETE" }), "The field could not be deleted.");
}

export async function reorderDefinitions(authorizedFetch: AuthorizedFetch, ids: string[]) {
  const { fields } = await expectJson<{ fields: FieldDefinition[] }>(await authorizedFetch("/v1/custom-fields/order", jsonBody({ ids }, "PUT")), "The fields could not be reordered.");
  return fields;
}

export async function fetchValues(authorizedFetch: AuthorizedFetch, owner: FieldOwner) {
  const { fields } = await expectJson<{ fields: FieldValue[] }>(await authorizedFetch(ownerPath(owner)), "The fields could not be loaded.");
  return fields;
}

export async function saveValue(authorizedFetch: AuthorizedFetch, owner: FieldOwner, fieldId: string, value: FieldJson) {
  return expectJson<FieldValue>(await authorizedFetch(`${ownerPath(owner)}/${encodeURIComponent(fieldId)}`, jsonBody({ value }, "PUT")), "The value could not be saved.");
}

// A secret's value; the server records who revealed it.
export async function revealValue(authorizedFetch: AuthorizedFetch, owner: FieldOwner, fieldId: string) {
  return expectJson<{ value: string; source: "device" | "company" }>(
    await authorizedFetch(`${ownerPath(owner)}/${encodeURIComponent(fieldId)}/reveal`, jsonBody({})),
    "The value could not be revealed.",
  );
}

export async function setOnDevices(authorizedFetch: AuthorizedFetch, fieldId: string, deviceIds: string[], value: FieldJson) {
  return expectJson<{ changed: number }>(
    await authorizedFetch("/v1/device-custom-fields", jsonBody({ field: fieldId, device_ids: deviceIds, value })),
    "The field could not be set on the devices.",
  );
}

// The IDs of the devices the filter matches.
export async function matchingDevices(authorizedFetch: AuthorizedFetch, filter: FieldFilter) {
  const params = new URLSearchParams({ op: filter.op });
  if (filter.value) params.set("value", filter.value);
  const { device_ids } = await expectJson<{ device_ids: string[] }>(
    await authorizedFetch(`/v1/custom-fields/${encodeURIComponent(filter.key)}/devices?${params}`),
    "The devices could not be filtered by the field.",
  );
  return device_ids;
}
