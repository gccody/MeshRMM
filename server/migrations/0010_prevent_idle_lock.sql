ALTER TABLE companies ADD COLUMN prevent_idle_lock INTEGER NOT NULL DEFAULT 1 CHECK (prevent_idle_lock IN (0, 1));
ALTER TABLE companies ADD COLUMN allow_idle_override INTEGER NOT NULL DEFAULT 1 CHECK (allow_idle_override IN (0, 1));
