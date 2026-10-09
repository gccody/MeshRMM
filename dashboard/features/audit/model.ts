// Audit events and how the website words them. Imports carry the .ts
// extension so node:test can load this module directly.

export type AuditEvent = {
  id: string;
  actor_user_id: string | null;
  // Who did it, as recorded then: an email, "anonymous", "command line", or
  // an identity provider such as "SCIM (Okta)" or "SSO (Okta)", which has
  // no user ID.
  actor_label: string;
  action: string;
  target_type: string;
  target_id: string;
  metadata: Record<string, unknown>;
  ip: string | null;
  created_at: number;
};

export type AuditPage = { events: AuditEvent[]; next?: string };

// The filter's choices: action prefixes, which the server matches when they
// end in a dot.
export const AUDIT_CATEGORIES: readonly { prefix: string; label: string }[] = [
  { prefix: "", label: "Everything" },
  { prefix: "auth.", label: "Sign-ins" },
  { prefix: "account.", label: "Account security" },
  { prefix: "user.", label: "Users" },
  { prefix: "invitation.", label: "Invitations" },
  { prefix: "role.", label: "Roles" },
  { prefix: "settings.", label: "Settings" },
  { prefix: "scim.", label: "Directory sync" },
  { prefix: "agent.", label: "Devices" },
  { prefix: "agent_installer.", label: "Enrollment" },
  { prefix: "remote.", label: "Remote sessions" },
  { prefix: "script.", label: "Script runs" },
  { prefix: "file.", label: "File deliveries" },
  { prefix: "toolbox.", label: "Toolbox library" },
];

const ACTION_LABELS: Record<string, string> = {
  "setup.complete": "Set up the server",
  "auth.sign_in": "Signed in",
  "auth.sign_in_failed": "Failed to sign in",
  "auth.sign_out": "Signed out",
  "account.update": "Changed their name",
  "account.password_change": "Changed their password",
  "account.two_factor_enable": "Turned on two-factor authentication",
  "account.two_factor_disable": "Turned off two-factor authentication",
  "account.recovery_codes_replace": "Replaced their recovery codes",
  "account.passkey_add": "Added a passkey",
  "account.passkey_rename": "Renamed a passkey",
  "account.passkey_remove": "Removed a passkey",
  "account.session_end": "Signed out a session",
  "user.create": "Created a user",
  "user.update": "Changed a user",
  "user.delete": "Deleted a user",
  "user.sign_out": "Signed a user out everywhere",
  "user.two_factor_reset": "Reset a user's two-factor authentication",
  "user.password_reset_create": "Made a password reset link",
  "user.password_reset_request": "Requested a password reset",
  "user.password_reset_complete": "Reset their password",
  "user.sso_link": "Linked an account to its SSO identity",
  "user.sso_unlink": "Unlinked a user's SSO identity",
  "user.provision": "Created an account at first sign-in",
  "invitation.create": "Invited a user",
  "invitation.renew": "Renewed an invitation",
  "invitation.revoke": "Revoked an invitation",
  "invitation.accept": "Accepted an invitation",
  "role.create": "Created a role",
  "role.update": "Changed a role",
  "role.delete": "Deleted a role",
  "settings.update": "Changed settings",
  "settings.authentication_update": "Changed the sign-in policy",
  "settings.smtp_update": "Changed email settings",
  "settings.smtp_delete": "Turned off email",
  "settings.sso_update": "Changed single sign-on",
  "settings.sso_delete": "Removed single sign-on",
  "scim.token_create": "Made a SCIM token",
  "scim.token_revoke": "Revoked a SCIM token",
  "scim.group_role_update": "Changed the role a SCIM group grants",
  "scim.user_create": "Created a user through SCIM",
  "scim.user_update": "Changed a user through SCIM",
  "scim.user_delete": "Deleted a user through SCIM",
  "scim.group_create": "Added a SCIM group",
  "scim.group_update": "Changed a SCIM group",
  "scim.group_delete": "Deleted a SCIM group",
  "agent_installer.issue": "Created an Agent installer",
  "agent_installer.redeem": "Enrolled a device",
  "agent.delete": "Deleted a device",
  "agent.rotate_credential": "Rotated a device's credential",
  "remote.handoff_create": "Started connecting to a device",
  "remote.session_create": "Started a remote session",
  "remote.session_close": "Closed a remote session",
  "script.run": "Ran a script",
  "file.deliver": "Sent a file to a device",
  "toolbox.script_create": "Added a script",
  "toolbox.script_update": "Changed a script",
  "toolbox.script_delete": "Deleted a script",
  "toolbox.file_upload": "Uploaded a file",
  "toolbox.file_update": "Changed a file",
  "toolbox.file_delete": "Deleted a file",
};

// "Signed in", or the action itself for one this website doesn't know.
export function actionLabel(action: string) {
  return ACTION_LABELS[action] ?? action;
}

const SIGN_IN_METHODS: Record<string, string> = {
  password: "with a password",
  passkey: "with a passkey",
  oidc: "with SSO",
};

const SIGN_IN_FAILURES: Record<string, string> = {
  password: "wrong password",
  totp: "wrong authenticator code",
  recovery_code: "wrong recovery code",
  passkey: "passkey not accepted",
  account_disabled: "account disabled",
};

// The action, with how a sign-in happened or why it failed when the event
// says: "Signed in with a passkey", "Failed to sign in: account disabled".
export function eventLabel(event: Pick<AuditEvent, "action" | "metadata">) {
  const label = actionLabel(event.action);
  const { method, reason, source } = event.metadata;
  if (event.action === "auth.sign_in" && typeof method === "string" && SIGN_IN_METHODS[method]) return `${label} ${SIGN_IN_METHODS[method]}`;
  if (event.action === "auth.sign_in_failed" && typeof reason === "string" && SIGN_IN_FAILURES[reason]) return `${label}: ${SIGN_IN_FAILURES[reason]}`;
  if (event.action === "user.provision" && source === "sso") return `${label} with SSO`;
  return label;
}

const TARGET_LABELS: Record<string, string> = {
  user: "User",
  role: "Role",
  invitation: "Invitation",
  agent: "Device",
  agent_installer: "Installer",
  toolbox_script: "Script",
  toolbox_file: "File",
  scim_group: "SCIM group",
  scim_token: "SCIM token",
  settings: "Settings",
};

// `deviceNames` names devices still in the inventory, rather than their IDs.
export function targetLabel(event: Pick<AuditEvent, "actor_user_id" | "target_type" | "target_id" | "metadata">, deviceNames?: ReadonlyMap<string, string>) {
  const kind = TARGET_LABELS[event.target_type] ?? event.target_type;
  if (event.target_type === "settings") return kind;
  // Sign-ins and account changes name the user who acted.
  if (event.target_type === "user" && event.target_id === event.actor_user_id) return "Own account";
  const { name, email, display_name: displayName } = event.metadata;
  const detail = typeof name === "string" ? name
    : typeof email === "string" ? email
    : typeof displayName === "string" ? displayName
    : (event.target_type === "agent" && deviceNames?.get(event.target_id)) || event.target_id;
  return `${kind}: ${detail}`;
}
