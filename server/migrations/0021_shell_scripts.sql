-- Toolbox scripts may also be shell scripts, which Mac Agents run with zsh.
-- SQLite cannot change a CHECK constraint in place, so both tables are
-- rebuilt with their rows.
CREATE TABLE toolbox_scripts_new (
  id TEXT PRIMARY KEY,
  company_id TEXT NOT NULL,
  owner_user_id TEXT NOT NULL,
  shared INTEGER NOT NULL DEFAULT 0 CHECK (shared IN (0, 1)),
  folder TEXT NOT NULL DEFAULT '' CHECK (length(CAST(folder AS BLOB)) <= 255),
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
  description TEXT NOT NULL DEFAULT '' CHECK (length(CAST(description AS BLOB)) <= 1000),
  language TEXT NOT NULL CHECK (language IN ('powershell', 'cmd', 'shell')),
  body TEXT NOT NULL CHECK (length(CAST(body AS BLOB)) BETWEEN 1 AND 131072),
  timeout_seconds INTEGER NOT NULL DEFAULT 300 CHECK (timeout_seconds BETWEEN 10 AND 3600),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  updated_by_user_id TEXT NOT NULL,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT;
INSERT INTO toolbox_scripts_new SELECT * FROM toolbox_scripts;
DROP TABLE toolbox_scripts;
ALTER TABLE toolbox_scripts_new RENAME TO toolbox_scripts;
CREATE INDEX idx_toolbox_scripts_company
  ON toolbox_scripts(company_id, owner_user_id);

CREATE TABLE script_runs_new (
  id TEXT PRIMARY KEY,
  company_id TEXT NOT NULL,
  device_id TEXT NOT NULL,
  script_id TEXT NOT NULL,
  script_name TEXT NOT NULL,
  language TEXT NOT NULL CHECK (language IN ('powershell', 'cmd', 'shell')),
  requested_by_user_id TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('dashboard', 'session')),
  run_as TEXT NOT NULL CHECK (run_as IN ('system', 'user')),
  timeout_seconds INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending'
    CHECK (status IN ('pending', 'completed', 'failed', 'timed_out')),
  ran_as TEXT,
  exit_code INTEGER,
  stdout TEXT NOT NULL DEFAULT '',
  stderr TEXT NOT NULL DEFAULT '',
  output_truncated INTEGER NOT NULL DEFAULT 0 CHECK (output_truncated IN (0, 1)),
  error TEXT,
  created_at INTEGER NOT NULL,
  completed_at INTEGER,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE,
  FOREIGN KEY (device_id) REFERENCES agents(id) ON DELETE CASCADE
) STRICT;
INSERT INTO script_runs_new SELECT * FROM script_runs;
DROP TABLE script_runs;
ALTER TABLE script_runs_new RENAME TO script_runs;
CREATE INDEX idx_script_runs_device
  ON script_runs(company_id, device_id, created_at DESC);
CREATE INDEX idx_script_runs_requester
  ON script_runs(company_id, requested_by_user_id, created_at DESC);
CREATE INDEX idx_script_runs_created
  ON script_runs(created_at);
