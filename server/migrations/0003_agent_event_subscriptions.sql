PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS agent_event_subscriptions (
  token_hash TEXT PRIMARY KEY CHECK (length(token_hash) = 64),
  company_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  used_at INTEGER,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_agent_event_subscriptions_expiry
  ON agent_event_subscriptions(expires_at, used_at);
