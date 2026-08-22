PRAGMA foreign_keys = ON;

ALTER TABLE companies
  ADD COLUMN dashboard_idle_timeout_minutes INTEGER NOT NULL DEFAULT 240
  CHECK (dashboard_idle_timeout_minutes BETWEEN 5 AND 1440);
