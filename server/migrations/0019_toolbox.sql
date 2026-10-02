-- The toolbox: scripts and library files a company keeps in the dashboard,
-- private to their owner or shared with the company. See docs/toolbox.md.
CREATE TABLE IF NOT EXISTS toolbox_scripts (
  id TEXT PRIMARY KEY,
  company_id TEXT NOT NULL,
  owner_user_id TEXT NOT NULL,
  shared INTEGER NOT NULL DEFAULT 0 CHECK (shared IN (0, 1)),
  folder TEXT NOT NULL DEFAULT '' CHECK (length(CAST(folder AS BLOB)) <= 255),
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
  description TEXT NOT NULL DEFAULT '' CHECK (length(CAST(description AS BLOB)) <= 1000),
  language TEXT NOT NULL CHECK (language IN ('powershell', 'cmd')),
  body TEXT NOT NULL CHECK (length(CAST(body AS BLOB)) BETWEEN 1 AND 131072),
  timeout_seconds INTEGER NOT NULL DEFAULT 300 CHECK (timeout_seconds BETWEEN 10 AND 3600),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  updated_by_user_id TEXT NOT NULL,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_toolbox_scripts_company
  ON toolbox_scripts(company_id, owner_user_id);

-- The content lives in R2 under toolbox/<company_id>/<id>.
CREATE TABLE IF NOT EXISTS toolbox_files (
  id TEXT PRIMARY KEY,
  company_id TEXT NOT NULL,
  owner_user_id TEXT NOT NULL,
  shared INTEGER NOT NULL DEFAULT 0 CHECK (shared IN (0, 1)),
  folder TEXT NOT NULL DEFAULT '' CHECK (length(CAST(folder AS BLOB)) <= 255),
  name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
  size_bytes INTEGER NOT NULL CHECK (size_bytes BETWEEN 0 AND 99614720),
  sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  updated_by_user_id TEXT NOT NULL,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_toolbox_files_company
  ON toolbox_files(company_id, owner_user_id);

-- Runs keep the script's name and language, so they outlive edits and
-- deletion of the script.
CREATE TABLE IF NOT EXISTS script_runs (
  id TEXT PRIMARY KEY,
  company_id TEXT NOT NULL,
  device_id TEXT NOT NULL,
  script_id TEXT NOT NULL,
  script_name TEXT NOT NULL,
  language TEXT NOT NULL CHECK (language IN ('powershell', 'cmd')),
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

CREATE INDEX IF NOT EXISTS idx_script_runs_device
  ON script_runs(company_id, device_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_script_runs_requester
  ON script_runs(company_id, requested_by_user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_script_runs_created
  ON script_runs(created_at);

CREATE TABLE IF NOT EXISTS file_deliveries (
  id TEXT PRIMARY KEY,
  company_id TEXT NOT NULL,
  device_id TEXT NOT NULL,
  file_id TEXT NOT NULL,
  file_name TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  requested_by_user_id TEXT NOT NULL,
  destination TEXT NOT NULL CHECK (destination IN ('user', 'public')),
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'delivered', 'failed')),
  path TEXT,
  error TEXT,
  created_at INTEGER NOT NULL,
  completed_at INTEGER,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE,
  FOREIGN KEY (device_id) REFERENCES agents(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_file_deliveries_device
  ON file_deliveries(company_id, device_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_file_deliveries_created
  ON file_deliveries(created_at);
