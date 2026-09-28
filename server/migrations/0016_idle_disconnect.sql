-- NULL never disconnects idle remote sessions.
ALTER TABLE companies ADD COLUMN idle_disconnect_minutes INTEGER CHECK (idle_disconnect_minutes IS NULL OR idle_disconnect_minutes > 0);
ALTER TABLE companies ADD COLUMN allow_idle_disconnect_override INTEGER NOT NULL DEFAULT 1 CHECK (allow_idle_disconnect_override IN (0, 1));
