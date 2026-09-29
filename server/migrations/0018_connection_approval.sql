ALTER TABLE companies ADD COLUMN connection_approval INTEGER NOT NULL DEFAULT 0 CHECK (connection_approval IN (0, 1));
ALTER TABLE companies ADD COLUMN connection_approval_message TEXT NOT NULL
  DEFAULT '{user_name} would like to connect.';
ALTER TABLE companies ADD COLUMN connection_approval_timeout_seconds INTEGER NOT NULL DEFAULT 30
  CHECK (connection_approval_timeout_seconds BETWEEN 5 AND 300);
ALTER TABLE companies ADD COLUMN connection_approval_lock_idle_seconds INTEGER NOT NULL DEFAULT 60
  CHECK (connection_approval_lock_idle_seconds BETWEEN 0 AND 3600);
ALTER TABLE remote_handoffs ADD COLUMN reason TEXT NOT NULL DEFAULT '';
