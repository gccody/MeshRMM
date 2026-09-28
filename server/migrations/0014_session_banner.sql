ALTER TABLE companies ADD COLUMN session_banner INTEGER NOT NULL DEFAULT 1 CHECK (session_banner IN (0, 1));
