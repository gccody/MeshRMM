ALTER TABLE companies ADD COLUMN blackout_message TEXT NOT NULL
  DEFAULT 'This machine is under maintenance by {user_name}.';
