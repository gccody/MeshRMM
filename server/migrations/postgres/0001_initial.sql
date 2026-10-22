-- The MeshRMM schema for PostgreSQL. migrations/sqlite holds the same schema
-- for SQLite; every change must be made to both (tests/schema_parity.rs checks
-- that they define the same tables and columns).
--
-- Conventions shared by both backends:
-- * Times are Unix milliseconds in 64-bit integers.
-- * Identifiers are text (UUIDs or 64-character hex token hashes).
-- * Booleans are BOOLEAN here and 0/1 integers in SQLite. Every integer is
--   BIGINT, so one Rust type (i64) decodes it from either backend.
-- * One install serves one company, so no table has a tenant column.

-- Instance-wide settings and remote-session policy. Exactly one row.
CREATE TABLE settings (
  id BIGINT PRIMARY KEY CHECK (id = 1),
  instance_name TEXT NOT NULL DEFAULT 'MeshRMM' CHECK (length(instance_name) BETWEEN 1 AND 120),
  dashboard_idle_timeout_minutes BIGINT NOT NULL DEFAULT 240
    CHECK (dashboard_idle_timeout_minutes BETWEEN 5 AND 1440),
  blackout_message TEXT NOT NULL DEFAULT 'This machine is under maintenance by {user_name}.',
  display_border BOOLEAN NOT NULL DEFAULT TRUE,
  prevent_idle_lock BOOLEAN NOT NULL DEFAULT TRUE,
  allow_idle_override BOOLEAN NOT NULL DEFAULT TRUE,
  session_banner BOOLEAN NOT NULL DEFAULT TRUE,
  connection_notification BOOLEAN NOT NULL DEFAULT TRUE,
  background_connection_notification BOOLEAN NOT NULL DEFAULT FALSE,
  connection_notification_message TEXT NOT NULL DEFAULT '{user_name} has connected to this computer.',
  idle_disconnect_minutes BIGINT CHECK (idle_disconnect_minutes IS NULL OR idle_disconnect_minutes > 0),
  allow_idle_disconnect_override BOOLEAN NOT NULL DEFAULT TRUE,
  clear_clipboard_on_close BOOLEAN NOT NULL DEFAULT TRUE,
  allow_clear_clipboard_override BOOLEAN NOT NULL DEFAULT TRUE,
  connection_approval BOOLEAN NOT NULL DEFAULT FALSE,
  connection_approval_message TEXT NOT NULL DEFAULT '{user_name} would like to connect.',
  connection_approval_timeout_seconds BIGINT NOT NULL DEFAULT 30
    CHECK (connection_approval_timeout_seconds BETWEEN 5 AND 300),
  connection_approval_lock_idle_seconds BIGINT NOT NULL DEFAULT 60
    CHECK (connection_approval_lock_idle_seconds BETWEEN 0 AND 3600),
  require_two_factor BOOLEAN NOT NULL DEFAULT FALSE,
  password_min_length BIGINT NOT NULL DEFAULT 12 CHECK (password_min_length BETWEEN 8 AND 128),
  session_lifetime_hours BIGINT NOT NULL DEFAULT 720 CHECK (session_lifetime_hours BETWEEN 1 AND 8760),
  -- How often online devices scan for operating system updates; 0 leaves
  -- scans to users and policies.
  update_scan_interval_hours BIGINT NOT NULL DEFAULT 24
    CHECK (update_scan_interval_hours BETWEEN 0 AND 168),
  -- Remote terminals with no input for this long are closed.
  terminal_idle_timeout_minutes BIGINT NOT NULL DEFAULT 30
    CHECK (terminal_idle_timeout_minutes BETWEEN 5 AND 480),
  smtp_host TEXT,
  smtp_port BIGINT CHECK (smtp_port IS NULL OR smtp_port BETWEEN 1 AND 65535),
  smtp_security TEXT NOT NULL DEFAULT 'starttls' CHECK (smtp_security IN ('starttls', 'tls', 'none')),
  smtp_username TEXT,
  smtp_password_encrypted BYTEA,
  smtp_from TEXT,
  updated_at BIGINT NOT NULL,
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
  password_changed_at BIGINT,
  disabled BOOLEAN NOT NULL DEFAULT FALSE,
  oidc_subject TEXT UNIQUE,
  scim_external_id TEXT UNIQUE,
  -- SCIM created or changed the account, so the identity provider manages it.
  scim_managed BOOLEAN NOT NULL DEFAULT FALSE,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  last_sign_in_at BIGINT
);

CREATE TABLE user_totp (
  user_id TEXT NOT NULL PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
  -- Encrypted with the instance key.
  secret_encrypted BYTEA NOT NULL,
  -- NULL until the user proves the authenticator works.
  confirmed_at BIGINT,
  -- The last accepted time step, so a code cannot be replayed.
  last_used_step BIGINT,
  created_at BIGINT NOT NULL
);

CREATE TABLE user_recovery_codes (
  id TEXT NOT NULL PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  code_hash TEXT NOT NULL CHECK (length(code_hash) = 64),
  used_at BIGINT,
  created_at BIGINT NOT NULL
);

CREATE INDEX idx_user_recovery_codes_user ON user_recovery_codes (user_id);

CREATE TABLE user_passkeys (
  id TEXT NOT NULL PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  credential_id BYTEA NOT NULL UNIQUE,
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
  -- The serialized WebAuthn credential, including its signature counter.
  passkey_json TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  last_used_at BIGINT
);

CREATE INDEX idx_user_passkeys_user ON user_passkeys (user_id);

CREATE TABLE user_sessions (
  id TEXT NOT NULL PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  auth_method TEXT NOT NULL CHECK (auth_method IN ('password', 'passkey', 'oidc')),
  created_at BIGINT NOT NULL,
  last_seen_at BIGINT NOT NULL,
  expires_at BIGINT NOT NULL,
  -- When the user last completed a second factor or passkey, for step-up checks.
  verified_at BIGINT,
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
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
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
  ('technician', 'Technician', 'Enrolls, connects to and wakes devices, opens terminals, runs toolbox scripts and files, installs updates, and responds to alerts.', 'technician', 0, 0);

INSERT INTO role_permissions (role_id, permission) VALUES
  ('technician', 'devices.view'),
  ('technician', 'devices.enroll'),
  ('technician', 'devices.wake'),
  ('technician', 'sessions.connect'),
  ('technician', 'sessions.connect_background'),
  ('technician', 'terminal.connect'),
  ('technician', 'terminal.connect_system'),
  ('technician', 'scripts.run'),
  ('technician', 'files.deliver'),
  ('technician', 'alerts.view'),
  ('technician', 'alerts.respond'),
  ('technician', 'updates.view'),
  ('technician', 'updates.install');

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
  created_at BIGINT NOT NULL,
  expires_at BIGINT NOT NULL,
  accepted_at BIGINT,
  revoked_at BIGINT
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
  created_at BIGINT NOT NULL,
  expires_at BIGINT NOT NULL,
  used_at BIGINT
);

CREATE INDEX idx_password_resets_expiry ON password_resets (expires_at);

-- The OIDC identity provider. At most one row.
CREATE TABLE oidc_provider (
  id BIGINT PRIMARY KEY CHECK (id = 1),
  enabled BOOLEAN NOT NULL DEFAULT FALSE,
  display_name TEXT NOT NULL CHECK (length(display_name) BETWEEN 1 AND 80),
  issuer_url TEXT NOT NULL,
  client_id TEXT NOT NULL,
  client_secret_encrypted BYTEA,
  scopes TEXT NOT NULL DEFAULT 'openid email profile',
  -- Create an account on first sign-in for users the IdP vouches for.
  auto_provision BOOLEAN NOT NULL DEFAULT FALSE,
  default_role_id TEXT REFERENCES roles (id) ON DELETE SET NULL,
  -- Link or create accounts by email only when the provider says the address
  -- is verified. Without that, whoever controls the provider's email claim
  -- could take over the matching account.
  require_verified_email BOOLEAN NOT NULL DEFAULT TRUE,
  -- The claim listing the user's groups; oidc_group_roles maps them to roles.
  groups_claim TEXT,
  updated_at BIGINT NOT NULL
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
  created_at BIGINT NOT NULL,
  last_used_at BIGINT,
  revoked_at BIGINT
);

CREATE TABLE scim_groups (
  id TEXT NOT NULL PRIMARY KEY,
  display_name TEXT NOT NULL UNIQUE CHECK (length(display_name) BETWEEN 1 AND 256),
  external_id TEXT UNIQUE,
  -- Members of the group hold this role while they stay in it.
  role_id TEXT REFERENCES roles (id) ON DELETE SET NULL,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
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
  created_at BIGINT NOT NULL
);

CREATE INDEX idx_audit_events_created ON audit_events (created_at);
CREATE INDEX idx_audit_events_actor ON audit_events (actor_user_id, created_at);

-- The companies devices are sorted into, such as an MSP's customers.
CREATE TABLE companies (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 120),
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE TABLE agents (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
  -- The operating system the Agent reported when it enrolled.
  platform TEXT NOT NULL CHECK (platform IN ('windows', 'macos', 'linux')),
  auth_token_hash TEXT NOT NULL CHECK (length(auth_token_hash) = 64),
  -- A rotated credential the Agent has not used yet; using it promotes it.
  pending_auth_token_hash TEXT CHECK (pending_auth_token_hash IS NULL OR length(pending_auth_token_hash) = 64),
  -- The same credential encrypted with the instance key, so it can be sent
  -- again until the Agent uses it. Cleared with the hash.
  pending_auth_token_encrypted BYTEA,
  created_by_user_id TEXT,
  -- NULL while the device belongs to no company.
  company_id TEXT REFERENCES companies (id) ON DELETE SET NULL,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  deletion_requested_at BIGINT
);

CREATE INDEX idx_agents_name ON agents (name);
CREATE INDEX idx_agents_company ON agents (company_id);

-- Each device's resource usage, one row per minute its Agent reported in.
-- Rates and loads are averages over the minute's samples; storage is the
-- minute's last sample, summed across the device's volumes.
CREATE TABLE device_metrics (
  device_id TEXT NOT NULL,
  -- The minute's start, in Unix milliseconds.
  minute BIGINT NOT NULL,
  samples BIGINT NOT NULL CHECK (samples > 0),
  cpu_percent DOUBLE PRECISION NOT NULL,
  cpu_percent_max DOUBLE PRECISION NOT NULL,
  memory_used_bytes BIGINT NOT NULL,
  memory_total_bytes BIGINT NOT NULL,
  network_received_bytes_per_second BIGINT NOT NULL,
  network_sent_bytes_per_second BIGINT NOT NULL,
  storage_used_bytes BIGINT NOT NULL,
  storage_total_bytes BIGINT NOT NULL,
  PRIMARY KEY (device_id, minute)
);

CREATE INDEX idx_device_metrics_minute ON device_metrics (minute);

-- The latest hardware and software snapshot each device's Agent sent. What
-- the website and the API search on are columns; the rest of the snapshot,
-- except its applications, is the Agent's JSON in `details`.
CREATE TABLE device_system_info (
  device_id TEXT NOT NULL PRIMARY KEY,
  -- When the Agent collected it, by the device's clock, and when the server
  -- received it, by its own.
  collected_at BIGINT NOT NULL,
  received_at BIGINT NOT NULL,
  -- The address the Agent's control connection last came from.
  public_ip TEXT,
  os_id TEXT NOT NULL,
  os_name TEXT NOT NULL,
  os_version TEXT NOT NULL,
  os_build TEXT,
  architecture TEXT NOT NULL,
  manufacturer TEXT,
  model TEXT,
  serial_number TEXT,
  hostname TEXT NOT NULL,
  -- Identifies the applications list, so an unchanged one isn't rewritten.
  applications_hash TEXT NOT NULL,
  details TEXT NOT NULL
);

-- The IP addresses on each device's network adapters, from its snapshot.
CREATE TABLE device_ip_addresses (
  device_id TEXT NOT NULL,
  adapter TEXT NOT NULL,
  address TEXT NOT NULL,
  prefix_length BIGINT NOT NULL CHECK (prefix_length BETWEEN 0 AND 128),
  mac_address TEXT,
  -- The adapter's kind: Wake-on-LAN relays broadcast from hardware adapters'
  -- subnets in preference to virtual ones'.
  kind TEXT NOT NULL CHECK (kind IN ('ethernet', 'wireless', 'virtual', 'other')),
  PRIMARY KEY (device_id, adapter, address)
);

CREATE INDEX idx_device_ip_addresses_address ON device_ip_addresses (address);

-- The applications and packages installed on each device, from its snapshot.
CREATE TABLE device_applications (
  device_id TEXT NOT NULL,
  name TEXT NOT NULL,
  version TEXT,
  publisher TEXT,
  architecture TEXT,
  installed_at BIGINT,
  source TEXT NOT NULL CHECK (source IN ('windows', 'windows_user', 'macos_app', 'dpkg', 'rpm', 'pacman', 'flatpak', 'snap'))
);

CREATE INDEX idx_device_applications_device ON device_applications (device_id);
CREATE INDEX idx_device_applications_name ON device_applications (name);

-- Applications installed, removed or updated between a device's snapshots.
CREATE TABLE device_application_changes (
  id TEXT NOT NULL PRIMARY KEY,
  device_id TEXT NOT NULL,
  -- When the server received the snapshot that showed the change.
  observed_at BIGINT NOT NULL,
  change TEXT NOT NULL CHECK (change IN ('installed', 'removed', 'updated')),
  name TEXT NOT NULL,
  version TEXT,
  -- The version an update replaced.
  previous_version TEXT,
  source TEXT NOT NULL
);

CREATE INDEX idx_device_application_changes_device ON device_application_changes (device_id, observed_at);
CREATE INDEX idx_device_application_changes_observed ON device_application_changes (observed_at);

CREATE TABLE agent_install_tokens (
  id TEXT NOT NULL PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
  created_by_user_id TEXT NOT NULL,
  platform TEXT NOT NULL CHECK (platform IN ('windows-x64', 'windows-arm64', 'macos', 'linux')),
  -- The company the device it enrolls joins.
  company_id TEXT REFERENCES companies (id) ON DELETE SET NULL,
  created_at BIGINT NOT NULL,
  expires_at BIGINT NOT NULL,
  used_at BIGINT,
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
  start_in_background BOOLEAN NOT NULL DEFAULT FALSE,
  reason TEXT NOT NULL DEFAULT '',
  created_at BIGINT NOT NULL,
  expires_at BIGINT NOT NULL,
  used_at BIGINT
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
  record_encrypted BYTEA NOT NULL,
  created_at BIGINT NOT NULL,
  expires_at BIGINT NOT NULL
);

CREATE INDEX idx_remote_sessions_user ON remote_sessions (user_id);
CREATE INDEX idx_remote_sessions_expiry ON remote_sessions (expires_at);

CREATE TABLE toolbox_scripts (
  id TEXT NOT NULL PRIMARY KEY,
  owner_user_id TEXT NOT NULL,
  shared BOOLEAN NOT NULL DEFAULT FALSE,
  folder TEXT NOT NULL DEFAULT '' CHECK (octet_length(folder) <= 255),
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
  description TEXT NOT NULL DEFAULT '' CHECK (octet_length(description) <= 1000),
  language TEXT NOT NULL CHECK (language IN ('powershell', 'cmd', 'shell')),
  body TEXT NOT NULL CHECK (octet_length(body) BETWEEN 1 AND 131072),
  timeout_seconds BIGINT NOT NULL DEFAULT 300 CHECK (timeout_seconds BETWEEN 10 AND 3600),
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  updated_by_user_id TEXT NOT NULL
);

CREATE INDEX idx_toolbox_scripts_owner ON toolbox_scripts (owner_user_id, shared);

CREATE TABLE toolbox_files (
  id TEXT NOT NULL PRIMARY KEY,
  owner_user_id TEXT NOT NULL,
  shared BOOLEAN NOT NULL DEFAULT FALSE,
  folder TEXT NOT NULL DEFAULT '' CHECK (octet_length(folder) <= 255),
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
  size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0),
  sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
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
  -- 'job' for a device's run of a job (jobs.id in job_id).
  source TEXT NOT NULL CHECK (source IN ('dashboard', 'session', 'job')),
  job_id TEXT,
  run_as TEXT NOT NULL CHECK (run_as IN ('system', 'user')),
  timeout_seconds BIGINT NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'completed', 'failed', 'timed_out', 'lost')),
  ran_as TEXT,
  exit_code BIGINT,
  stdout TEXT NOT NULL DEFAULT '',
  stderr TEXT NOT NULL DEFAULT '',
  output_truncated BOOLEAN NOT NULL DEFAULT FALSE,
  error TEXT,
  created_at BIGINT NOT NULL,
  completed_at BIGINT
);

CREATE INDEX idx_script_runs_device ON script_runs (device_id, created_at);
CREATE INDEX idx_script_runs_requester ON script_runs (requested_by_user_id, created_at);
CREATE INDEX idx_script_runs_created ON script_runs (created_at);
CREATE INDEX idx_script_runs_job ON script_runs (job_id, device_id);

CREATE TABLE file_deliveries (
  id TEXT NOT NULL PRIMARY KEY,
  device_id TEXT NOT NULL,
  file_id TEXT NOT NULL,
  file_name TEXT NOT NULL,
  size_bytes BIGINT NOT NULL,
  requested_by_user_id TEXT NOT NULL,
  destination TEXT NOT NULL CHECK (destination IN ('user', 'public')),
  -- The job this delivery is a device's part of, if any.
  job_id TEXT,
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'delivered', 'failed')),
  path TEXT,
  error TEXT,
  created_at BIGINT NOT NULL,
  completed_at BIGINT
);

CREATE INDEX idx_file_deliveries_device ON file_deliveries (device_id, created_at);
CREATE INDEX idx_file_deliveries_created ON file_deliveries (created_at);
CREATE INDEX idx_file_deliveries_job ON file_deliveries (job_id, device_id);

-- Where notifications go: email to chosen users and everyone holding chosen
-- roles, or a webhook. Alert rules name the channels they notify.
CREATE TABLE notification_channels (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 80),
  kind TEXT NOT NULL CHECK (kind IN ('email', 'webhook')),
  enabled BOOLEAN NOT NULL DEFAULT TRUE,
  -- A webhook's URL, and the secret its requests are signed with, sealed
  -- with the instance key.
  webhook_url TEXT,
  webhook_secret_encrypted BYTEA,
  -- How the last delivery went: NULL error when it succeeded.
  last_delivery_at BIGINT,
  last_delivery_error TEXT,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE TABLE notification_channel_users (
  channel_id TEXT NOT NULL REFERENCES notification_channels (id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  PRIMARY KEY (channel_id, user_id)
);

CREATE TABLE notification_channel_roles (
  channel_id TEXT NOT NULL REFERENCES notification_channels (id) ON DELETE CASCADE,
  role_id TEXT NOT NULL REFERENCES roles (id) ON DELETE CASCADE,
  PRIMARY KEY (channel_id, role_id)
);

-- What raises alerts. `threshold` is a percentage, or gigabytes for a disk
-- space rule in 'gb'; `duration_minutes` is how long a device must be offline
-- or a load sustained; `days` how long updates or a restart must be pending,
-- and `update_category` which updates count ('any' or a category).
CREATE TABLE alert_rules (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 120),
  severity TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
  enabled BOOLEAN NOT NULL DEFAULT TRUE,
  condition TEXT NOT NULL CHECK (condition IN ('device_offline', 'disk_space', 'cpu', 'memory', 'script_failed', 'updates_pending', 'update_install_failed', 'reboot_pending')),
  threshold DOUBLE PRECISION,
  threshold_unit TEXT CHECK (threshold_unit IS NULL OR threshold_unit IN ('percent', 'gb')),
  duration_minutes BIGINT,
  -- For script_failed: a script that exits with a code other than 0 failed too.
  nonzero_exit_fails BOOLEAN NOT NULL DEFAULT FALSE,
  update_category TEXT CHECK (update_category IS NULL OR update_category IN ('any', 'security', 'critical', 'definition', 'driver', 'feature_pack', 'service_pack', 'update_rollup', 'upgrade', 'tool', 'other')),
  days BIGINT CHECK (days IS NULL OR days BETWEEN 1 AND 365),
  -- Every device, or only those of the companies in alert_rule_companies.
  all_companies BOOLEAN NOT NULL DEFAULT TRUE,
  notify_on_resolve BOOLEAN NOT NULL DEFAULT FALSE,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  updated_by_user_id TEXT
);

CREATE TABLE alert_rule_companies (
  rule_id TEXT NOT NULL REFERENCES alert_rules (id) ON DELETE CASCADE,
  company_id TEXT NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
  PRIMARY KEY (rule_id, company_id)
);

CREATE TABLE alert_rule_channels (
  rule_id TEXT NOT NULL REFERENCES alert_rules (id) ON DELETE CASCADE,
  channel_id TEXT NOT NULL REFERENCES notification_channels (id) ON DELETE CASCADE,
  PRIMARY KEY (rule_id, channel_id)
);

-- Alerts rules raised. A rule has at most one unresolved alert per device and
-- subject (the volume of a disk space alert; '' otherwise). Resolved alerts
-- are kept for a while, then purged.
CREATE TABLE alerts (
  id TEXT NOT NULL PRIMARY KEY,
  rule_id TEXT NOT NULL REFERENCES alert_rules (id) ON DELETE CASCADE,
  device_id TEXT NOT NULL,
  subject TEXT NOT NULL DEFAULT '',
  state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'acknowledged', 'resolved')),
  -- What tripped the rule: a percentage, gigabytes or minutes, and in words.
  value DOUBLE PRECISION,
  detail TEXT NOT NULL,
  -- The latest failed run of a script_failed alert, and how many failed.
  script_run_id TEXT,
  -- The failed install of an update_install_failed alert.
  update_run_id TEXT,
  occurrences BIGINT NOT NULL DEFAULT 1,
  raised_at BIGINT NOT NULL,
  last_occurred_at BIGINT NOT NULL,
  acknowledged_at BIGINT,
  acknowledged_by_user_id TEXT,
  acknowledged_by_label TEXT,
  resolved_at BIGINT,
  -- NULL user when the server resolved it.
  resolved_by_user_id TEXT,
  resolved_by_label TEXT,
  -- 'cleared': the condition passed; 'manual': a user resolved it;
  -- 'not_applicable': the rule was turned off or no longer covers the device.
  resolution TEXT CHECK (resolution IS NULL OR resolution IN ('cleared', 'manual', 'not_applicable'))
);

CREATE UNIQUE INDEX idx_alerts_unresolved ON alerts (rule_id, device_id, subject) WHERE state <> 'resolved';
CREATE INDEX idx_alerts_device ON alerts (device_id, raised_at);
CREATE INDEX idx_alerts_raised ON alerts (raised_at);
CREATE INDEX idx_alerts_resolved ON alerts (resolved_at);

-- A script run or file delivery on many devices at once. The job keeps what
-- it sends, so every device gets the same script even if the library
-- changes while it waits for offline devices. See docs/automation.md.
CREATE TABLE jobs (
  id TEXT NOT NULL PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('script', 'file', 'update_scan', 'update_install', 'wake')),
  -- The toolbox script or file, and its name when the job started. Update
  -- jobs have an empty item_id and say what they do in item_name.
  item_id TEXT NOT NULL,
  item_name TEXT NOT NULL,
  -- Scripts: the interpreter, body, timeout and account.
  language TEXT CHECK (language IS NULL OR language IN ('powershell', 'cmd', 'shell')),
  body TEXT,
  timeout_seconds BIGINT,
  run_as TEXT CHECK (run_as IS NULL OR run_as IN ('system', 'user')),
  -- Files: the folder they go to.
  destination TEXT CHECK (destination IS NULL OR destination IN ('user', 'public')),
  -- Update installs: the categories installed (comma-separated; NULL for
  -- every pending update), and when the computer restarts afterwards.
  update_categories TEXT,
  reboot TEXT CHECK (reboot IS NULL OR reboot IN ('never', 'if_required', 'always')),
  reboot_delay_minutes BIGINT,
  -- Which devices it was started on, and that in words, such as "Acme, Globex".
  target TEXT NOT NULL CHECK (target IN ('devices', 'companies', 'all')),
  target_label TEXT NOT NULL,
  -- Offline devices are skipped, or waited for until expires_at.
  offline TEXT NOT NULL CHECK (offline IN ('skip', 'wait')),
  expires_at BIGINT NOT NULL,
  -- Who started it: a user, or the user whose access a policy runs with.
  requested_by_user_id TEXT NOT NULL,
  -- The policy that started it, and its name then.
  policy_id TEXT,
  policy_name TEXT,
  created_at BIGINT NOT NULL,
  -- When the last device finished, ran out of time or was cancelled.
  finished_at BIGINT
);

CREATE INDEX idx_jobs_created ON jobs (created_at);
CREATE INDEX idx_jobs_policy ON jobs (policy_id, created_at);
CREATE INDEX idx_jobs_finished ON jobs (finished_at);

-- Each device a job covers. 'queued': online, its run starts soon;
-- 'waiting': offline, it runs when the device connects; 'started': its run or
-- delivery has the job's ID; 'skipped': the device has no interpreter for the
-- script; 'missed': it was offline and the job didn't wait for it, or it
-- didn't connect in time; 'cancelled'.
CREATE TABLE job_devices (
  job_id TEXT NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
  device_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('queued', 'waiting', 'started', 'skipped', 'missed', 'cancelled')),
  -- Why it was skipped or missed.
  detail TEXT,
  updated_at BIGINT NOT NULL,
  PRIMARY KEY (job_id, device_id)
);

CREATE INDEX idx_job_devices_state ON job_devices (state, device_id);

-- Scripts and update scans and installs the server runs by itself: on a
-- schedule, or on each device that enrolls. Each time it fires it starts a job.
CREATE TABLE automation_policies (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 120),
  enabled BOOLEAN NOT NULL DEFAULT TRUE,
  -- 'script' runs a shared toolbox script as run_as; 'update_scan' scans
  -- for operating system updates; 'update_install' installs the pending
  -- updates in update_categories (comma-separated; NULL for all), then
  -- restarts as reboot says.
  action TEXT NOT NULL DEFAULT 'script' CHECK (action IN ('script', 'update_scan', 'update_install', 'wake')),
  script_id TEXT,
  run_as TEXT CHECK (run_as IS NULL OR run_as IN ('system', 'user')),
  update_categories TEXT,
  reboot TEXT CHECK (reboot IS NULL OR reboot IN ('never', 'if_required', 'always')),
  reboot_delay_minutes BIGINT,
  -- Every device, the companies in automation_policy_companies, or the
  -- devices in automation_policy_devices.
  target TEXT NOT NULL CHECK (target IN ('devices', 'companies', 'all')),
  -- 'interval' every interval_minutes; 'daily' at time_of_day; 'weekly' at
  -- time_of_day on weekdays (a bit per day, Monday = 1 to Sunday = 64);
  -- 'once' at run_at; all in time_zone. 'enroll' when a device enrolls.
  trigger_type TEXT NOT NULL CHECK (trigger_type IN ('interval', 'daily', 'weekly', 'once', 'enroll')),
  interval_minutes BIGINT,
  time_of_day TEXT,
  weekdays BIGINT,
  run_at TEXT,
  time_zone TEXT,
  -- Offline devices are skipped, or caught up when they connect within
  -- catch_up_hours (and before the policy fires again).
  offline TEXT NOT NULL CHECK (offline IN ('skip', 'wait')),
  catch_up_hours BIGINT NOT NULL,
  -- Whose access it runs with: the user who last saved it.
  owner_user_id TEXT NOT NULL,
  -- When it fires next; NULL when it's off, fires on enrollment, or has
  -- nothing left to do. Interval schedules count from interval_anchor_at.
  next_run_at BIGINT,
  interval_anchor_at BIGINT,
  last_run_at BIGINT,
  last_job_id TEXT,
  -- Why it last didn't fire when it was due, and when.
  last_problem TEXT,
  last_problem_at BIGINT,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);

CREATE INDEX idx_automation_policies_next ON automation_policies (next_run_at);

CREATE TABLE automation_policy_companies (
  policy_id TEXT NOT NULL REFERENCES automation_policies (id) ON DELETE CASCADE,
  company_id TEXT NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
  PRIMARY KEY (policy_id, company_id)
);

CREATE TABLE automation_policy_devices (
  policy_id TEXT NOT NULL REFERENCES automation_policies (id) ON DELETE CASCADE,
  device_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
  PRIMARY KEY (policy_id, device_id)
);

-- Channels told when one of the policy's jobs finishes with failures.
CREATE TABLE automation_policy_channels (
  policy_id TEXT NOT NULL REFERENCES automation_policies (id) ON DELETE CASCADE,
  channel_id TEXT NOT NULL REFERENCES notification_channels (id) ON DELETE CASCADE,
  PRIMARY KEY (policy_id, channel_id)
);

-- Each device's latest update scan. See docs/patch-management.md.
CREATE TABLE device_update_status (
  device_id TEXT NOT NULL PRIMARY KEY,
  -- The update service or package manager; NULL until a scan names one, or
  -- when the device has none the Agent supports (supported = 0).
  manager TEXT CHECK (manager IS NULL OR manager IN ('windows_update', 'softwareupdate', 'apt', 'dnf', 'yum', 'pacman')),
  supported BOOLEAN NOT NULL DEFAULT TRUE,
  -- The last scan that worked, by the device's clock.
  scanned_at BIGINT,
  -- When the last scan report arrived, worked or not, and why it failed.
  received_at BIGINT,
  scan_error TEXT,
  reboot_pending BOOLEAN NOT NULL DEFAULT FALSE,
  -- When a scan first said the device must restart.
  reboot_pending_since BIGINT,
  -- When the server last asked the Agent for a scan on its own schedule.
  scan_requested_at BIGINT
);

-- The updates each device's latest scan found pending. first_seen_at is
-- when a scan first listed the update, kept across scans.
CREATE TABLE device_pending_updates (
  device_id TEXT NOT NULL,
  update_id TEXT NOT NULL,
  title TEXT NOT NULL,
  kb TEXT,
  version TEXT,
  current_version TEXT,
  category TEXT NOT NULL CHECK (category IN ('security', 'critical', 'definition', 'driver', 'feature_pack', 'service_pack', 'update_rollup', 'upgrade', 'tool', 'other')),
  severity TEXT CHECK (severity IS NULL OR severity IN ('critical', 'important', 'moderate', 'low')),
  size_bytes BIGINT,
  reboot_required BOOLEAN NOT NULL DEFAULT FALSE,
  released_at BIGINT,
  source TEXT,
  first_seen_at BIGINT NOT NULL,
  PRIMARY KEY (device_id, update_id)
);

CREATE INDEX idx_device_pending_updates_category ON device_pending_updates (category, device_id);

-- Scans and installs the server asked a device for: by a user, or as a
-- device's part of a job (job_id). Installs are the device's update history.
CREATE TABLE update_runs (
  id TEXT NOT NULL PRIMARY KEY,
  device_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('scan', 'install')),
  -- Who asked: a user, or the user whose access a policy runs with, and the
  -- policy's name then.
  requested_by_user_id TEXT NOT NULL,
  policy_name TEXT,
  job_id TEXT,
  -- Installs: the chosen updates (a JSON array of IDs) or categories
  -- (comma-separated), neither for every pending update, and the restart.
  update_ids TEXT,
  update_categories TEXT,
  reboot TEXT CHECK (reboot IS NULL OR reboot IN ('never', 'if_required', 'always')),
  reboot_delay_minutes BIGINT,
  -- 'lost': the Agent never reported.
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'succeeded', 'partial', 'failed', 'lost')),
  progress TEXT,
  progress_percent BIGINT,
  error TEXT,
  installed BIGINT NOT NULL DEFAULT 0,
  failed BIGINT NOT NULL DEFAULT 0,
  reboot_required BOOLEAN NOT NULL DEFAULT FALSE,
  -- When the Agent set the computer to restart, by its clock, or why it
  -- couldn't.
  reboot_scheduled_at BIGINT,
  reboot_error TEXT,
  created_at BIGINT NOT NULL,
  completed_at BIGINT
);

CREATE INDEX idx_update_runs_device ON update_runs (device_id, created_at);
CREATE INDEX idx_update_runs_created ON update_runs (created_at);
CREATE INDEX idx_update_runs_job ON update_runs (job_id, device_id);
CREATE INDEX idx_update_runs_status ON update_runs (status);

-- How each update of an install went.
CREATE TABLE update_run_items (
  run_id TEXT NOT NULL REFERENCES update_runs (id) ON DELETE CASCADE,
  update_id TEXT NOT NULL,
  title TEXT NOT NULL,
  kb TEXT,
  category TEXT NOT NULL CHECK (category IN ('security', 'critical', 'definition', 'driver', 'feature_pack', 'service_pack', 'update_rollup', 'upgrade', 'tool', 'other')),
  outcome TEXT NOT NULL CHECK (outcome IN ('installed', 'failed')),
  error TEXT,
  PRIMARY KEY (run_id, update_id)
);

-- Wake-on-LAN: each asks online devices on an offline device's network (the
-- relays) to broadcast magic packets for it, by a user or as the device's
-- part of a job (job_id).
CREATE TABLE wake_requests (
  id TEXT NOT NULL PRIMARY KEY,
  device_id TEXT NOT NULL,
  requested_by_user_id TEXT NOT NULL,
  job_id TEXT,
  -- 'sending': relays haven't all reported; 'sent': at least one sent the
  -- packets; 'failed': none did; 'no_relay': there was none, or nothing to
  -- wake the device with, as detail says.
  status TEXT NOT NULL CHECK (status IN ('sending', 'sent', 'failed', 'no_relay')),
  detail TEXT,
  -- The hardware addresses woken, comma-separated.
  mac_addresses TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  -- When the device's Agent connected within a few minutes of the packets.
  online_at BIGINT
);

CREATE INDEX idx_wake_requests_device ON wake_requests (device_id, created_at);
CREATE INDEX idx_wake_requests_created ON wake_requests (created_at);
CREATE INDEX idx_wake_requests_job ON wake_requests (job_id, device_id);

-- The relays of each wake request, one per subnet each shares with the
-- device, and what each reported.
CREATE TABLE wake_relays (
  wake_id TEXT NOT NULL REFERENCES wake_requests (id) ON DELETE CASCADE,
  relay_device_id TEXT NOT NULL,
  relay_name TEXT NOT NULL,
  broadcast TEXT NOT NULL,
  prefix_length BIGINT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending', 'sent', 'failed')),
  -- The relay's address it sent from.
  source_address TEXT,
  error TEXT,
  reported_at BIGINT,
  PRIMARY KEY (wake_id, relay_device_id, broadcast)
);
