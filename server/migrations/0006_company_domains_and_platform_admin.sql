PRAGMA foreign_keys = ON;

-- Existing company IDs are WorkOS organization IDs. Keep those primary keys stable so existing
-- Agent, handoff, event, and Durable Object ownership remains intact. New companies receive an
-- internal UUID and store WorkOS as an external identity provider reference.
ALTER TABLE companies ADD COLUMN slug TEXT;
ALTER TABLE companies ADD COLUMN workos_organization_id TEXT;
ALTER TABLE companies ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
  CHECK (status IN ('provisioning', 'awaiting_admin', 'active', 'suspended', 'failed'));
ALTER TABLE companies ADD COLUMN initial_admin_email TEXT;
ALTER TABLE companies ADD COLUMN created_by_platform_user_id TEXT;
ALTER TABLE companies ADD COLUMN updated_at INTEGER;
ALTER TABLE companies ADD COLUMN provisioning_error TEXT;

UPDATE companies
SET workos_organization_id = id,
    updated_at = created_at
WHERE workos_organization_id IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_companies_slug
  ON companies(slug COLLATE NOCASE)
  WHERE slug IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_companies_workos_organization
  ON companies(workos_organization_id)
  WHERE workos_organization_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_companies_status
  ON companies(status, created_at DESC);

CREATE TABLE IF NOT EXISTS company_domains (
  hostname TEXT PRIMARY KEY COLLATE NOCASE,
  company_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('primary', 'legacy_alias')),
  created_at INTEGER NOT NULL,
  retired_at INTEGER,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_company_domains_primary
  ON company_domains(company_id)
  WHERE kind = 'primary' AND retired_at IS NULL;

CREATE TABLE IF NOT EXISTS company_provisioning_operations (
  id TEXT PRIMARY KEY,
  company_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending', 'creating_workos_organization', 'configuring_workos_authorization', 'configuring_workos_origin', 'inviting_admin', 'complete', 'failed')),
  attempt_count INTEGER NOT NULL DEFAULT 0,
  workos_invitation_id TEXT,
  last_error TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_company_provisioning_state
  ON company_provisioning_operations(state, updated_at);

CREATE TABLE IF NOT EXISTS platform_audit_events (
  id TEXT PRIMARY KEY,
  actor_user_id TEXT NOT NULL,
  action TEXT NOT NULL,
  company_id TEXT,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  created_at INTEGER NOT NULL,
  FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE SET NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_platform_audit_created
  ON platform_audit_events(created_at DESC);
