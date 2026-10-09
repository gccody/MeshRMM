-- The MeshRMM schema for SQLite. migrations/postgres holds the same schema for
-- PostgreSQL; every change must be made to both (tests/schema_parity.rs checks
-- that they define the same tables and columns).
--
-- Conventions shared by both backends:
-- * Times are Unix milliseconds in 64-bit integers.
-- * Identifiers are text (UUIDs or 64-character hex token hashes).
-- * Booleans are BOOLEAN in PostgreSQL and 0/1 integers here.
-- * One install serves one company, so no table has a tenant column.

-- Instance-wide settings and remote-session policy. Exactly one row.
CREATE TABLE settings (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  instance_name TEXT NOT NULL DEFAULT 'MeshRMM' CHECK (length(instance_name) BETWEEN 1 AND 120),
  dashboard_idle_timeout_minutes INTEGER NOT NULL DEFAULT 240
    CHECK (dashboard_idle_timeout_minutes BETWEEN 5 AND 1440),
  blackout_message TEXT NOT NULL DEFAULT 'This machine is under maintenance by {user_name}.',
  display_border INTEGER NOT NULL DEFAULT 1 CHECK (display_border IN (0, 1)),
  prevent_idle_lock INTEGER NOT NULL DEFAULT 1 CHECK (prevent_idle_lock IN (0, 1)),
  allow_idle_override INTEGER NOT NULL DEFAULT 1 CHECK (allow_idle_override IN (0, 1)),
  session_banner INTEGER NOT NULL DEFAULT 1 CHECK (session_banner IN (0, 1)),
  connection_notification INTEGER NOT NULL DEFAULT 1 CHECK (connection_notification IN (0, 1)),
  background_connection_notification INTEGER NOT NULL DEFAULT 0
    CHECK (background_connection_notification IN (0, 1)),
  connection_notification_message TEXT NOT NULL DEFAULT '{user_name} has connected to this computer.',
  idle_disconnect_minutes INTEGER CHECK (idle_disconnect_minutes IS NULL OR idle_disconnect_minutes > 0),
  allow_idle_disconnect_override INTEGER NOT NULL DEFAULT 1
    CHECK (allow_idle_disconnect_override IN (0, 1)),
  clear_clipboard_on_close INTEGER NOT NULL DEFAULT 1 CHECK (clear_clipboard_on_close IN (0, 1)),
  allow_clear_clipboard_override INTEGER NOT NULL DEFAULT 1
    CHECK (allow_clear_clipboard_override IN (0, 1)),
  connection_approval INTEGER NOT NULL DEFAULT 0 CHECK (connection_approval IN (0, 1)),
  connection_approval_message TEXT NOT NULL DEFAULT '{user_name} would like to connect.',
  connection_approval_timeout_seconds INTEGER NOT NULL DEFAULT 30
    CHECK (connection_approval_timeout_seconds BETWEEN 5 AND 300),
  connection_approval_lock_idle_seconds INTEGER NOT NULL DEFAULT 60
    CHECK (connection_approval_lock_idle_seconds BETWEEN 0 AND 3600),
  require_two_factor INTEGER NOT NULL DEFAULT 0 CHECK (require_two_factor IN (0, 1)),
  password_min_length INTEGER NOT NULL DEFAULT 12 CHECK (password_min_length BETWEEN 8 AND 128),
  session_lifetime_hours INTEGER NOT NULL DEFAULT 720 CHECK (session_lifetime_hours BETWEEN 1 AND 8760),
  smtp_host TEXT,
  smtp_port INTEGER CHECK (smtp_port IS NULL OR smtp_port BETWEEN 1 AND 65535),
  smtp_security TEXT NOT NULL DEFAULT 'starttls' CHECK (smtp_security IN ('starttls', 'tls', 'none')),
  smtp_username TEXT,
  smtp_password_encrypted BLOB,
  smtp_from TEXT,
  updated_at INTEGER NOT NULL,
  updated_by_user_id TEXT
);

INSERT INTO settings (id, updated_at) VALUES (1, 0);

CREATE TABLE users (
  id TEXT NOT NULL PRIMARY KEY,
  -- Stored lowercased, so uniqueness ignores case.
  email TEXT NOT NULL UNIQUE CHECK (length(email) BETWEEN 3 AND 254),
  display_name TEXT NOT NULL CHECK (length(display_name) BETWEEN 1 AND 120),
  -- Argon2id PHC string; NULL for accounts that only sign in through SSO.
  password_hash TEXT,
  password_changed_at INTEGER,
  disabled INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0, 1)),
  oidc_subject TEXT UNIQUE,
  scim_external_id TEXT UNIQUE,
  -- SCIM created or changed the account, so the identity provider manages it.
  scim_managed INTEGER NOT NULL DEFAULT 0 CHECK (scim_managed IN (0, 1)),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  last_sign_in_at INTEGER
);

CREATE TABLE user_totp (
  user_id TEXT NOT NULL PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
  -- Encrypted with the instance key.
  secret_encrypted BLOB NOT NULL,
  -- NULL until the user proves the authenticator works.
  confirmed_at INTEGER,
  -- The last accepted time step, so a code cannot be replayed.
  last_used_step INTEGER,
  created_at INTEGER NOT NULL
);

CREATE TABLE user_recovery_codes (
  id TEXT NOT NULL PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  code_hash TEXT NOT NULL CHECK (length(code_hash) = 64),
  used_at INTEGER,
  created_at INTEGER NOT NULL
);

CREATE INDEX idx_user_recovery_codes_user ON user_recovery_codes (user_id);

CREATE TABLE user_passkeys (
  id TEXT NOT NULL PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  credential_id BLOB NOT NULL UNIQUE,
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
  -- The serialized WebAuthn credential, including its signature counter.
  passkey_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER
);

CREATE INDEX idx_user_passkeys_user ON user_passkeys (user_id);

CREATE TABLE user_sessions (
  id TEXT NOT NULL PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  auth_method TEXT NOT NULL CHECK (auth_method IN ('password', 'passkey', 'oidc')),
  created_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  -- When the user last completed a second factor or passkey, for step-up checks.
  verified_at INTEGER,
  ip TEXT,
  user_agent TEXT
);

CREATE INDEX idx_user_sessions_user ON user_sessions (user_id);
CREATE INDEX idx_user_sessions_expiry ON user_sessions (expires_at);

CREATE TABLE roles (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 80),
  description TEXT NOT NULL DEFAULT '' CHECK (length(description) <= 500),
  -- 'administrator' or 'technician' for the two roles every install starts with.
  builtin TEXT UNIQUE CHECK (builtin IS NULL OR builtin IN ('administrator', 'technician')),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE role_permissions (
  role_id TEXT NOT NULL REFERENCES roles (id) ON DELETE CASCADE,
  permission TEXT NOT NULL,
  PRIMARY KEY (role_id, permission)
);

-- Every install starts with these two roles. The Administrator holds every
-- permission implicitly, so it has no role_permissions rows; the Technician
-- is an editable default.
INSERT INTO roles (id, name, description, builtin, created_at, updated_at) VALUES
  ('administrator', 'Administrator', 'Every permission, including users, roles and settings.', 'administrator', 0, 0),
  ('technician', 'Technician', 'Enrolls and connects to devices and runs toolbox scripts and files.', 'technician', 0, 0);

INSERT INTO role_permissions (role_id, permission) VALUES
  ('technician', 'devices.view'),
  ('technician', 'devices.enroll'),
  ('technician', 'sessions.connect'),
  ('technician', 'sessions.connect_background'),
  ('technician', 'scripts.run'),
  ('technician', 'files.deliver');

CREATE TABLE user_roles (
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  role_id TEXT NOT NULL REFERENCES roles (id) ON DELETE CASCADE,
  PRIMARY KEY (user_id, role_id)
);

CREATE INDEX idx_user_roles_role ON user_roles (role_id);

CREATE TABLE invitations (
  id TEXT NOT NULL PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
  email TEXT NOT NULL CHECK (length(email) BETWEEN 3 AND 254),
  created_by_user_id TEXT,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  accepted_at INTEGER,
  revoked_at INTEGER
);

CREATE INDEX idx_invitations_expiry ON invitations (expires_at);

CREATE TABLE invitation_roles (
  invitation_id TEXT NOT NULL REFERENCES invitations (id) ON DELETE CASCADE,
  role_id TEXT NOT NULL REFERENCES roles (id) ON DELETE CASCADE,
  PRIMARY KEY (invitation_id, role_id)
);

CREATE TABLE password_resets (
  id TEXT NOT NULL PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  -- NULL when the user asked for the reset; otherwise the administrator.
  created_by_user_id TEXT,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  used_at INTEGER
);

CREATE INDEX idx_password_resets_expiry ON password_resets (expires_at);

-- The OIDC identity provider. At most one row.
CREATE TABLE oidc_provider (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
  display_name TEXT NOT NULL CHECK (length(display_name) BETWEEN 1 AND 80),
  issuer_url TEXT NOT NULL,
  client_id TEXT NOT NULL,
  client_secret_encrypted BLOB,
  scopes TEXT NOT NULL DEFAULT 'openid email profile',
  -- Create an account on first sign-in for users the IdP vouches for.
  auto_provision INTEGER NOT NULL DEFAULT 0 CHECK (auto_provision IN (0, 1)),
  default_role_id TEXT REFERENCES roles (id) ON DELETE SET NULL,
  -- Link or create accounts by email only when the provider says the address
  -- is verified. Without that, whoever controls the provider's email claim
  -- could take over the matching account.
  require_verified_email INTEGER NOT NULL DEFAULT 1 CHECK (require_verified_email IN (0, 1)),
  -- The claim listing the user's groups; oidc_group_roles maps them to roles.
  groups_claim TEXT,
  updated_at INTEGER NOT NULL
);

-- Members of an SSO group (named in the groups claim) hold these roles.
CREATE TABLE oidc_group_roles (
  group_name TEXT NOT NULL CHECK (length(group_name) BETWEEN 1 AND 256),
  role_id TEXT NOT NULL REFERENCES roles (id) ON DELETE CASCADE,
  PRIMARY KEY (group_name, role_id)
);

-- The groups the user's last SSO sign-in reported.
CREATE TABLE user_oidc_groups (
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  group_name TEXT NOT NULL,
  PRIMARY KEY (user_id, group_name)
);

CREATE INDEX idx_user_oidc_groups_group ON user_oidc_groups (group_name);

CREATE TABLE scim_tokens (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 80),
  token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
  created_by_user_id TEXT,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at INTEGER
);

CREATE TABLE scim_groups (
  id TEXT NOT NULL PRIMARY KEY,
  display_name TEXT NOT NULL UNIQUE CHECK (length(display_name) BETWEEN 1 AND 256),
  external_id TEXT UNIQUE,
  -- Members of the group hold this role while they stay in it.
  role_id TEXT REFERENCES roles (id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE scim_group_members (
  group_id TEXT NOT NULL REFERENCES scim_groups (id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  PRIMARY KEY (group_id, user_id)
);

CREATE INDEX idx_scim_group_members_user ON scim_group_members (user_id);

-- The roles each user holds: assigned to them, or through an identity
-- provider's group (a SCIM group, or a group in the SSO groups claim).
CREATE VIEW effective_user_roles AS
  SELECT user_id, role_id FROM user_roles
  UNION
  SELECT m.user_id, g.role_id
    FROM scim_group_members AS m JOIN scim_groups AS g ON g.id = m.group_id
    WHERE g.role_id IS NOT NULL
  UNION
  SELECT u.user_id, r.role_id
    FROM user_oidc_groups AS u JOIN oidc_group_roles AS r ON r.group_name = u.group_name;

CREATE TABLE audit_events (
  id TEXT NOT NULL PRIMARY KEY,
  -- NULL when the server, SCIM, or an unauthenticated request acted.
  actor_user_id TEXT,
  actor_label TEXT NOT NULL,
  action TEXT NOT NULL,
  target_type TEXT NOT NULL,
  target_id TEXT NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  ip TEXT,
  created_at INTEGER NOT NULL
);

CREATE INDEX idx_audit_events_created ON audit_events (created_at);
CREATE INDEX idx_audit_events_actor ON audit_events (actor_user_id, created_at);

CREATE TABLE agents (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
  auth_token_hash TEXT NOT NULL CHECK (length(auth_token_hash) = 64),
  -- A rotated credential the Agent has not used yet; using it promotes it.
  pending_auth_token_hash TEXT CHECK (pending_auth_token_hash IS NULL OR length(pending_auth_token_hash) = 64),
  -- The same credential encrypted with the instance key, so it can be sent
  -- again until the Agent uses it. Cleared with the hash.
  pending_auth_token_encrypted BLOB,
  created_by_user_id TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deletion_requested_at INTEGER
);

CREATE INDEX idx_agents_name ON agents (name);

-- Each device's resource usage, one row per minute its Agent reported in.
-- Rates and loads are averages over the minute's samples; storage is the
-- minute's last sample, summed across the device's volumes.
CREATE TABLE device_metrics (
  device_id TEXT NOT NULL,
  -- The minute's start, in Unix milliseconds.
  minute INTEGER NOT NULL,
  samples INTEGER NOT NULL CHECK (samples > 0),
  cpu_percent REAL NOT NULL,
  cpu_percent_max REAL NOT NULL,
  memory_used_bytes INTEGER NOT NULL,
  memory_total_bytes INTEGER NOT NULL,
  network_received_bytes_per_second INTEGER NOT NULL,
  network_sent_bytes_per_second INTEGER NOT NULL,
  storage_used_bytes INTEGER NOT NULL,
  storage_total_bytes INTEGER NOT NULL,
  PRIMARY KEY (device_id, minute)
);

CREATE INDEX idx_device_metrics_minute ON device_metrics (minute);

CREATE TABLE agent_install_tokens (
  id TEXT NOT NULL PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
  created_by_user_id TEXT NOT NULL,
  platform TEXT NOT NULL CHECK (platform IN ('windows-x64', 'macos')),
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  used_at INTEGER,
  -- Kept after redemption so an interrupted install can redeem again.
  device_id TEXT,
  computer_name TEXT,
  redemption_key_hash TEXT
);

CREATE INDEX idx_agent_install_tokens_expiry ON agent_install_tokens (expires_at);

CREATE TABLE remote_handoffs (
  token_hash TEXT NOT NULL PRIMARY KEY CHECK (length(token_hash) = 64),
  device_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  start_in_background INTEGER NOT NULL DEFAULT 0 CHECK (start_in_background IN (0, 1)),
  reason TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  used_at INTEGER
);

CREATE INDEX idx_remote_handoffs_expiry ON remote_handoffs (expires_at);

-- Live remote sessions, persisted so a viewer and Agent can resume one after
-- the server restarts. A device has at most one; `expires_at` is its idle
-- deadline, which activity moves forward. The record holds the session's
-- signaling tokens, so it is sealed with the instance key.
CREATE TABLE remote_sessions (
  id TEXT NOT NULL PRIMARY KEY,
  device_id TEXT NOT NULL UNIQUE,
  user_id TEXT NOT NULL,
  record_encrypted BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
);

CREATE INDEX idx_remote_sessions_user ON remote_sessions (user_id);
CREATE INDEX idx_remote_sessions_expiry ON remote_sessions (expires_at);

CREATE TABLE toolbox_scripts (
  id TEXT NOT NULL PRIMARY KEY,
  owner_user_id TEXT NOT NULL,
  shared INTEGER NOT NULL DEFAULT 0 CHECK (shared IN (0, 1)),
  folder TEXT NOT NULL DEFAULT '' CHECK (length(CAST(folder AS BLOB)) <= 255),
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
  description TEXT NOT NULL DEFAULT '' CHECK (length(CAST(description AS BLOB)) <= 1000),
  language TEXT NOT NULL CHECK (language IN ('powershell', 'cmd', 'shell')),
  body TEXT NOT NULL CHECK (length(CAST(body AS BLOB)) BETWEEN 1 AND 131072),
  timeout_seconds INTEGER NOT NULL DEFAULT 300 CHECK (timeout_seconds BETWEEN 10 AND 3600),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  updated_by_user_id TEXT NOT NULL
);

CREATE INDEX idx_toolbox_scripts_owner ON toolbox_scripts (owner_user_id, shared);

CREATE TABLE toolbox_files (
  id TEXT NOT NULL PRIMARY KEY,
  owner_user_id TEXT NOT NULL,
  shared INTEGER NOT NULL DEFAULT 0 CHECK (shared IN (0, 1)),
  folder TEXT NOT NULL DEFAULT '' CHECK (length(CAST(folder AS BLOB)) <= 255),
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
  size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
  sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  updated_by_user_id TEXT NOT NULL
);

CREATE INDEX idx_toolbox_files_owner ON toolbox_files (owner_user_id, shared);

CREATE TABLE script_runs (
  id TEXT NOT NULL PRIMARY KEY,
  device_id TEXT NOT NULL,
  script_id TEXT NOT NULL,
  script_name TEXT NOT NULL,
  language TEXT NOT NULL CHECK (language IN ('powershell', 'cmd', 'shell')),
  requested_by_user_id TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('dashboard', 'session')),
  run_as TEXT NOT NULL CHECK (run_as IN ('system', 'user')),
  timeout_seconds INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'completed', 'failed', 'timed_out')),
  ran_as TEXT,
  exit_code INTEGER,
  stdout TEXT NOT NULL DEFAULT '',
  stderr TEXT NOT NULL DEFAULT '',
  output_truncated INTEGER NOT NULL DEFAULT 0 CHECK (output_truncated IN (0, 1)),
  error TEXT,
  created_at INTEGER NOT NULL,
  completed_at INTEGER
);

CREATE INDEX idx_script_runs_device ON script_runs (device_id, created_at);
CREATE INDEX idx_script_runs_requester ON script_runs (requested_by_user_id, created_at);
CREATE INDEX idx_script_runs_created ON script_runs (created_at);

CREATE TABLE file_deliveries (
  id TEXT NOT NULL PRIMARY KEY,
  device_id TEXT NOT NULL,
  file_id TEXT NOT NULL,
  file_name TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  requested_by_user_id TEXT NOT NULL,
  destination TEXT NOT NULL CHECK (destination IN ('user', 'public')),
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'delivered', 'failed')),
  path TEXT,
  error TEXT,
  created_at INTEGER NOT NULL,
  completed_at INTEGER
);

CREATE INDEX idx_file_deliveries_device ON file_deliveries (device_id, created_at);
CREATE INDEX idx_file_deliveries_created ON file_deliveries (created_at);
