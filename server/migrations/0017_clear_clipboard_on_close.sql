ALTER TABLE companies ADD COLUMN clear_clipboard_on_close INTEGER NOT NULL DEFAULT 1 CHECK (clear_clipboard_on_close IN (0, 1));
ALTER TABLE companies ADD COLUMN allow_clear_clipboard_override INTEGER NOT NULL DEFAULT 1 CHECK (allow_clear_clipboard_override IN (0, 1));
