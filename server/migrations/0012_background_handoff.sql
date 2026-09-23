ALTER TABLE remote_handoffs ADD COLUMN start_in_background INTEGER NOT NULL DEFAULT 0 CHECK (start_in_background IN (0, 1));
