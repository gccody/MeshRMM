ALTER TABLE companies ADD COLUMN display_border INTEGER NOT NULL DEFAULT 1 CHECK (display_border IN (0, 1));
