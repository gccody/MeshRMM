-- The company that owns each Durable Object not named after its company, so
-- the platform cost report can attribute the object's Cloudflare usage.
-- Agent coordinators are named after their Agent and recorded when the Agent
-- is created; remote sessions are named after the session and recorded by
-- the Worker when the session is created.
CREATE TABLE usage_object_owners (
    object_name TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('agent', 'remote_session')),
    created_at INTEGER NOT NULL,
    FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT;
CREATE INDEX idx_usage_object_owners_company ON usage_object_owners(company_id);
CREATE INDEX idx_usage_object_owners_expiry ON usage_object_owners(kind, created_at);

INSERT OR IGNORE INTO usage_object_owners (object_name, company_id, kind, created_at)
SELECT id, company_id, 'agent', created_at FROM agents;

CREATE TRIGGER usage_object_owner_agent_insert AFTER INSERT ON agents BEGIN
    INSERT OR IGNORE INTO usage_object_owners (object_name, company_id, kind, created_at)
    VALUES (NEW.id, NEW.company_id, 'agent', NEW.created_at);
END;

-- Users who used a company's dashboard in a UTC month (YYYY-MM), which is how
-- WorkOS counts AuthKit monthly active users.
CREATE TABLE company_active_users (
    company_id TEXT NOT NULL,
    month TEXT NOT NULL CHECK (length(month) = 7),
    user_id TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    PRIMARY KEY (company_id, month, user_id),
    FOREIGN KEY (company_id) REFERENCES companies(id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX idx_company_active_users_month ON company_active_users(month);
