// Single sign-on and directory sync settings as the server sends them, and
// the SSO form's request. Imports carry the .ts extension so node:test can
// load this module directly.

export type GroupRole = { group: string; role_id: string };

export type SsoProvider = {
  enabled: boolean;
  display_name: string;
  issuer_url: string;
  client_id: string;
  // The secret itself is never sent back.
  has_client_secret: boolean;
  scopes: string;
  auto_provision: boolean;
  default_role_id: string | null;
  require_verified_email: boolean;
  groups_claim: string | null;
  group_roles: GroupRole[];
  updated_at: number;
};

// GET /v1/settings/sso
export type SsoSettings = {
  // What the administrator registers at the provider.
  redirect_uri: string;
  provider: SsoProvider | null;
};

export type ScimToken = { id: string; name: string; created_at: number; last_used_at: number | null };

export type CreatedScimToken = ScimToken & { secret: string };

export type ScimGroup = {
  id: string;
  display_name: string;
  external_id: string | null;
  role_id: string | null;
  member_count: number;
};

// GET /v1/settings/scim
export type ScimSettings = { base_url: string; tokens: ScimToken[]; groups: ScimGroup[] };

export const DEFAULT_SCOPES = "openid email profile";
export const MAX_SSO_NAME_LENGTH = 80;

export type SsoForm = {
  enabled: boolean;
  displayName: string;
  issuerUrl: string;
  clientId: string;
  // A new secret; empty keeps the stored one unless `removeSecret`.
  clientSecret: string;
  removeSecret: boolean;
  scopes: string;
  autoProvision: boolean;
  // Empty for none.
  defaultRoleId: string;
  requireVerifiedEmail: boolean;
  groupsClaim: string;
  groupRoles: GroupRole[];
};

// The form for a provider, or a new one's defaults.
export function ssoForm(provider: SsoProvider | null): SsoForm {
  return {
    enabled: provider?.enabled ?? true,
    displayName: provider?.display_name ?? "",
    issuerUrl: provider?.issuer_url ?? "",
    clientId: provider?.client_id ?? "",
    clientSecret: "",
    removeSecret: false,
    scopes: provider?.scopes ?? DEFAULT_SCOPES,
    autoProvision: provider?.auto_provision ?? false,
    defaultRoleId: provider?.default_role_id ?? "",
    requireVerifiedEmail: provider?.require_verified_email ?? true,
    groupsClaim: provider?.groups_claim ?? "",
    groupRoles: provider?.group_roles ?? [],
  };
}

// What's wrong with the form before the server sees it, or null.
export function ssoFormProblem(form: SsoForm): string | null {
  const name = form.displayName.trim();
  if (!name) return "Enter the provider's name, as the sign-in button shows it.";
  if ([...name].length > MAX_SSO_NAME_LENGTH) return `The provider's name must be at most ${MAX_SSO_NAME_LENGTH} characters.`;
  if (!form.issuerUrl.trim()) return "Enter the issuer URL.";
  if (!form.clientId.trim()) return "Enter the client ID.";
  if (form.groupsClaim.trim()) {
    const groups = form.groupRoles.map((mapping) => mapping.group.trim());
    if (groups.some((group) => !group)) return "Enter a group name for each group mapping, or remove it.";
    if (form.groupRoles.some((mapping) => !mapping.role_id)) return "Choose a role for each group mapping.";
  }
  return null;
}

// PUT /v1/settings/sso. Leaving `client_secret` out keeps the stored
// secret; null removes it.
export function ssoUpdate(form: SsoForm): Record<string, unknown> {
  const groupsClaim = form.groupsClaim.trim();
  const body: Record<string, unknown> = {
    enabled: form.enabled,
    display_name: form.displayName.trim(),
    issuer_url: form.issuerUrl.trim(),
    client_id: form.clientId.trim(),
    scopes: form.scopes.trim() || DEFAULT_SCOPES,
    auto_provision: form.autoProvision,
    default_role_id: form.defaultRoleId || null,
    require_verified_email: form.requireVerifiedEmail,
    groups_claim: groupsClaim || null,
    // Mappings mean nothing without a claim to read groups from.
    group_roles: groupsClaim ? form.groupRoles.map(({ group, role_id }) => ({ group: group.trim(), role_id })) : [],
  };
  if (form.clientSecret) body.client_secret = form.clientSecret;
  else if (form.removeSecret) body.client_secret = null;
  return body;
}
