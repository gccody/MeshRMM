// What the server says about itself and the signed-in account.

// GET /v1/instance: what the website needs before anyone signs in.
export type Instance = {
  name: string;
  // No account exists yet; first-run setup creates the administrator.
  setup_required: boolean;
  sign_in: {
    password: boolean;
    // Whether "forgot password" can email a reset link.
    password_reset_email: boolean;
  };
  password_min_length: number;
};

export type Permission =
  | "devices.view"
  | "devices.enroll"
  | "devices.delete"
  | "devices.rotate_credentials"
  | "sessions.connect"
  | "sessions.connect_background"
  | "sessions.close_any"
  | "scripts.run"
  | "scripts.manage_shared"
  | "files.deliver"
  | "files.manage_shared"
  | "users.manage"
  | "roles.manage"
  | "settings.manage"
  | "authentication.manage"
  | "audit.view";

export type RoleRef = { id: string; name: string };

// GET /v1/account
export type Account = {
  user: {
    id: string;
    email: string;
    display_name: string;
    has_password: boolean;
    created_at: number;
    last_sign_in_at: number | null;
  };
  two_factor: {
    enabled: boolean;
    // The instance requires it of password sign-ins.
    required: boolean;
    // Nothing but account security works until it is set up.
    enrollment_required: boolean;
    recovery_codes_remaining: number;
  };
  roles: RoleRef[];
  permissions: Permission[];
  is_administrator: boolean;
  session: { id: string; auth_method: string; created_at: number; expires_at: number };
  // Sign the browser out after this long without activity.
  idle_timeout_minutes: number;
  // Device users approve each connection, so Connect asks for a reason.
  connection_approval: boolean;
};

// The body of a response that signs the browser in.
export type SignInResult =
  | { status: "signed_in"; two_factor_enrollment_required: boolean }
  | { status: "second_factor_required"; challenge: string; methods: string[] };

export const can = (account: Pick<Account, "permissions"> | null | undefined, permission: Permission) =>
  Boolean(account?.permissions.includes(permission));

export const canAny = (account: Pick<Account, "permissions"> | null | undefined, permissions: readonly Permission[]) =>
  permissions.some((permission) => can(account, permission));
