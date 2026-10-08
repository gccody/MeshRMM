// Audit events and how the website words them. Imports carry the .ts
// extension so node:test can load this module directly.

export type AuditEvent = {
  id: string;
  actor_user_id: string | null;
  // Who did it, as recorded then: an email, "anonymous" or "command line".
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
  "account.session_end": "Signed out a session",
  "user.create": "Created a user",
  "user.update": "Changed a user",
  "user.delete": "Deleted a user",
  "user.sign_out": "Signed a user out everywhere",
  "user.two_factor_reset": "Reset a user's two-factor authentication",
  "user.password_reset_create": "Made a password reset link",
  "user.password_reset_request": "Requested a password reset",
  "user.password_reset_complete": "Reset their password",
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

const TARGET_LABELS: Record<string, string> = {
  user: "User",
  role: "Role",
  invitation: "Invitation",
  agent: "Device",
  agent_installer: "Installer",
  toolbox_script: "Script",
  toolbox_file: "File",
  settings: "Settings",
};

export function targetLabel(event: Pick<AuditEvent, "actor_user_id" | "target_type" | "target_id" | "metadata">) {
  const kind = TARGET_LABELS[event.target_type] ?? event.target_type;
  if (event.target_type === "settings") return kind;
  // Sign-ins and account changes name the user who acted.
  if (event.target_type === "user" && event.target_id === event.actor_user_id) return "Own account";
  const { name, email } = event.metadata;
  const detail = typeof name === "string" ? name : typeof email === "string" ? email : event.target_id;
  return `${kind}: ${detail}`;
}
