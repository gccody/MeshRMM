ALTER TABLE companies ADD COLUMN connection_notification INTEGER NOT NULL DEFAULT 1 CHECK (connection_notification IN (0, 1));
ALTER TABLE companies ADD COLUMN background_connection_notification INTEGER NOT NULL DEFAULT 0 CHECK (background_connection_notification IN (0, 1));
ALTER TABLE companies ADD COLUMN connection_notification_message TEXT NOT NULL
  DEFAULT '{user_name} has connected to this computer.';
