ALTER TABLE agents ADD COLUMN deletion_requested_at INTEGER;

CREATE INDEX IF NOT EXISTS idx_agents_pending_deletion
  ON agents(deletion_requested_at)
  WHERE deletion_requested_at IS NOT NULL;
