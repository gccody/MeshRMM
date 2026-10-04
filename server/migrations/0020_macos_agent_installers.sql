-- Installer authorizations may now be issued for macOS. SQLite cannot change
-- a CHECK constraint in place, so the table is rebuilt. Authorizations last 30
-- minutes, so unredeemed ones are not carried over.
DROP TABLE agent_install_tokens;

CREATE TABLE agent_install_tokens (
  id TEXT PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
  company_id TEXT NOT NULL,
  created_by_user_id TEXT NOT NULL,
  platform TEXT NOT NULL CHECK (platform IN ('windows-x64', 'macos')),
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  used_at INTEGER,
  device_id TEXT,
  computer_name TEXT,
  redemption_key_hash TEXT,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_agent_install_tokens_expiry
  ON agent_install_tokens(expires_at, used_at);
