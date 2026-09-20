-- Commit inventory notifications in the same transaction as the catalog write.
-- Coalesce pending changes per Agent; the consumer always reads the current row.
CREATE TABLE presence_catalog_outbox (
    agent_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    event_id TEXT NOT NULL
) STRICT;
CREATE INDEX idx_presence_catalog_company ON presence_catalog_outbox(company_id);

CREATE TRIGGER presence_agent_insert AFTER INSERT ON agents BEGIN
    INSERT INTO presence_catalog_outbox (agent_id, company_id, event_id)
    VALUES (NEW.id, NEW.company_id, lower(hex(randomblob(16))))
    ON CONFLICT(agent_id) DO UPDATE SET company_id = excluded.company_id, event_id = excluded.event_id;
END;
CREATE TRIGGER presence_agent_update AFTER UPDATE OF name, deletion_requested_at ON agents
WHEN NEW.name IS NOT OLD.name OR NEW.deletion_requested_at IS NOT OLD.deletion_requested_at BEGIN
    INSERT INTO presence_catalog_outbox (agent_id, company_id, event_id)
    VALUES (NEW.id, NEW.company_id, lower(hex(randomblob(16))))
    ON CONFLICT(agent_id) DO UPDATE SET company_id = excluded.company_id, event_id = excluded.event_id;
END;
CREATE TRIGGER presence_agent_delete AFTER DELETE ON agents BEGIN
    INSERT INTO presence_catalog_outbox (agent_id, company_id, event_id)
    VALUES (OLD.id, OLD.company_id, lower(hex(randomblob(16))))
    ON CONFLICT(agent_id) DO UPDATE SET company_id = excluded.company_id, event_id = excluded.event_id;
END;
