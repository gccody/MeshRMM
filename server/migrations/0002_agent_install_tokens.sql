PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS agent_install_tokens (
  id TEXT PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
  company_id TEXT NOT NULL,
  created_by_user_id TEXT NOT NULL,
  platform TEXT NOT NULL CHECK (platform IN ('windows-x64')),
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  used_at INTEGER,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_agent_install_tokens_expiry
  ON agent_install_tokens(expires_at, used_at);
