"""Exercise the SQL shipped by the Worker against SQLite, including fault paths."""
import pathlib
import re
import sqlite3
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


def sql(file, prefix):
    source = (ROOT / file).read_text()
    return next(query for query in re.findall(r'"([^"\n]+)"', source) if query.startswith(prefix))


class CompanyProvisioningTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:", isolation_level=None)
        for migration in sorted((ROOT / "server/migrations").glob("*.sql")):
            self.db.executescript(migration.read_text())
        platform = "server/src/routes/platform.rs"
        self.retry = sql(platform, "UPDATE companies SET status = 'provisioning'")
        self.complete = sql(platform, "UPDATE companies SET status = ?1, provisioning_error = NULL")
        self.fail = sql(platform, "UPDATE companies SET status = 'failed'")

    def company(self, status):
        self.db.execute("DELETE FROM companies")
        self.db.execute("INSERT INTO companies (id,name,created_at,slug,status) VALUES ('co','Company',0,'acme',?)", (status,))

    def status(self):
        return self.db.execute("SELECT status FROM companies WHERE id='co'").fetchone()[0]

    def test_retry_is_limited_to_unfinished_provisioning(self):
        for status, claimed in [("provisioning", 1), ("failed", 1), ("active", 0), ("awaiting_admin", 0), ("suspended", 0)]:
            self.company(status)
            self.assertEqual(self.db.execute(self.retry, (5, "co")).rowcount, claimed, status)
            self.assertEqual(self.status(), "provisioning" if claimed else status)

    def test_provisioning_outcomes_keep_a_status_set_meanwhile(self):
        for status in ["suspended", "active", "awaiting_admin"]:
            self.company(status)
            self.db.execute(self.complete, ("active" if status != "active" else "awaiting_admin", 5, "co"))
            self.assertEqual(self.status(), status)
            self.db.execute(self.fail, ("WorkOS error", 5, "co"))
            self.assertEqual(self.status(), status)

    def test_provisioning_outcomes_apply_to_unfinished_companies(self):
        for status in ["provisioning", "failed"]:
            self.company(status)
            self.db.execute(self.complete, ("awaiting_admin", 5, "co"))
            self.assertEqual(self.status(), "awaiting_admin")
            self.company(status)
            self.db.execute(self.fail, ("WorkOS error", 5, "co"))
            self.assertEqual(self.db.execute("SELECT status, provisioning_error FROM companies").fetchone(), ("failed", "WorkOS error"))


class EnrollmentTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:", isolation_level=None)
        for migration in sorted((ROOT / "server/migrations").glob("*.sql")):
            self.db.executescript(migration.read_text())
        self.db.execute("INSERT INTO companies (id,name,created_at,slug,status) VALUES ('co','Company',0,'acme','active')")
        self.db.execute("INSERT INTO agent_install_tokens (id,token_hash,company_id,created_by_user_id,platform,created_at,expires_at) VALUES ('ticket',?,'co','user','windows-x64',0,1000)", ("a" * 64,))
        self.claim = sql("server/src/routes/agents.rs", "UPDATE agent_install_tokens SET")
        self.insert = sql("server/src/routes/agents.rs", "INSERT OR IGNORE INTO agents")

    def redeem(self, key="key-hash", name="PC", tenant="co", now=1):
        self.db.execute("BEGIN")
        try:
            self.db.execute(self.claim, (now, "device", name, key, "a" * 64, tenant))
            self.db.execute(self.insert, ("b" * 64, now, "a" * 64, key, name, tenant))
            self.db.execute("COMMIT")
        except Exception:
            self.db.execute("ROLLBACK")
            raise

    def test_catalog_outbox_commits_with_changes_and_coalesces_without_losing_newer_edits(self):
        self.redeem()
        event = self.db.execute("SELECT event_id FROM presence_catalog_outbox WHERE agent_id='device'").fetchone()[0]
        # Auth/lease metadata must not cause inventory broadcasts.
        self.db.execute("UPDATE agents SET updated_at=50 WHERE id='device'")
        self.assertEqual(self.db.execute("SELECT event_id FROM presence_catalog_outbox").fetchone()[0], event)
        self.db.execute("UPDATE agents SET name='Renamed' WHERE id='device'")
        newer = self.db.execute("SELECT event_id FROM presence_catalog_outbox").fetchone()[0]
        self.assertNotEqual(event, newer)
        acknowledge = sql("server/src/company_presence.rs", "DELETE FROM presence_catalog_outbox")
        self.db.execute(acknowledge, ('device', 'co', event))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM presence_catalog_outbox").fetchone()[0], 1)
        self.db.execute(acknowledge, ('device', 'another-company', newer))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM presence_catalog_outbox").fetchone()[0], 1)
        self.db.execute(acknowledge, ('device', 'co', newer))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM presence_catalog_outbox").fetchone()[0], 0)

    def test_catalog_outbox_rolls_back_and_recovers_soft_and_hard_deletion(self):
        self.redeem()
        self.db.execute("DELETE FROM presence_catalog_outbox")
        self.db.execute("BEGIN")
        self.db.execute("UPDATE agents SET deletion_requested_at=100 WHERE id='device'")
        self.db.execute("ROLLBACK")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM presence_catalog_outbox").fetchone()[0], 0)
        self.db.execute("UPDATE agents SET deletion_requested_at=100 WHERE id='device'")
        pending = sql("server/src/company_presence.rs", "SELECT o.agent_id, o.event_id, a.name")
        row = self.db.execute(pending, ('co',)).fetchone()
        self.assertEqual(row[0], 'device')
        self.assertIsNone(row[2])
        self.assertEqual(self.db.execute(pending, ('other',)).fetchall(), [])
        self.db.execute("DELETE FROM presence_catalog_outbox")
        self.db.execute("DELETE FROM agents WHERE id='device'")
        self.assertIsNone(self.db.execute(pending, ('co',)).fetchone()[2])

    def test_blackout_policy_is_company_scoped_and_preserved_by_legacy_updates(self):
        self.redeem()
        default = "This machine is under maintenance by {user_name}."
        self.assertEqual(self.db.execute("SELECT blackout_message FROM companies WHERE id='co'").fetchone(), (default,))
        update = sql("server/src/routes/account.rs", "UPDATE companies SET dashboard_idle_timeout_minutes")
        self.db.execute("INSERT INTO companies (id,name,created_at,slug,status) VALUES ('other','Other',0,'other','active')")
        self.db.execute(update, (60, "Maintenance by {user_name}\nPlease wait", "co", 0, 0, 0))
        self.db.execute(update, (120, None, "co", None, None, None))
        self.assertEqual(self.db.execute("SELECT blackout_message FROM companies WHERE id='other'").fetchone(), (default,))
        policy = sql("server/src/routes/handoffs.rs", "SELECT c.blackout_message")
        self.assertEqual(self.db.execute(policy, ("device",)).fetchone(), ("Maintenance by {user_name}\nPlease wait", 0, 0, 0))
        self.db.execute("UPDATE agents SET deletion_requested_at=1")
        self.assertIsNone(self.db.execute(policy, ("device",)).fetchone())

    def test_lost_response_retry_keeps_one_identity(self):
        self.redeem()
        self.redeem(now=2)
        self.assertEqual(self.db.execute("SELECT id FROM agents").fetchall(), [("device",)])
        self.assertEqual(self.db.execute("SELECT used_at FROM agent_install_tokens").fetchone(), (1,))

    def test_session_cleanup_only_targets_own_non_deleted_agent(self):
        self.redeem()
        query = sql("server/src/routes/handoffs.rs", "SELECT 1 AS permitted FROM agents")
        self.assertEqual(self.db.execute(query, ("device", "co")).fetchone(), (1,))
        self.assertIsNone(self.db.execute(query, ("device", "another-company")).fetchone())
        self.assertIsNone(self.db.execute(query, ("missing", "co")).fetchone())
        self.db.execute("UPDATE agents SET deletion_requested_at=1")
        self.assertIsNone(self.db.execute(query, ("device", "co")).fetchone())

    def test_another_endpoint_cannot_reclaim_ticket(self):
        self.redeem()
        self.redeem(key="attacker", name="other")
        self.assertEqual(self.db.execute("SELECT computer_name,redemption_key_hash FROM agent_install_tokens").fetchone(), ("PC", "key-hash"))

    def test_wrong_tenant_and_expiry_do_not_consume_ticket(self):
        self.redeem(tenant="another")
        self.redeem(now=1000)
        self.assertEqual(self.db.execute("SELECT used_at FROM agent_install_tokens").fetchone(), (None,))
        self.assertEqual(self.db.execute("SELECT count(*) FROM agents").fetchone(), (0,))

    def test_suspension_blocks_legacy_and_tenant_enrollment(self):
        self.db.execute("UPDATE companies SET status='suspended'")
        self.redeem(tenant="")
        self.redeem()
        self.assertEqual(self.db.execute("SELECT used_at FROM agent_install_tokens").fetchone(), (None,))

    def test_insertion_failure_rolls_back_claim(self):
        self.db.execute("CREATE TRIGGER fail_insert BEFORE INSERT ON agents BEGIN SELECT RAISE(ABORT,'injected failure'); END")
        with self.assertRaises(sqlite3.IntegrityError):
            self.redeem()
        self.assertEqual(self.db.execute("SELECT used_at,device_id FROM agent_install_tokens").fetchone(), (None, None))
        self.db.execute("DROP TRIGGER fail_insert")
        self.redeem()
        self.assertEqual(self.db.execute("SELECT count(*) FROM agents").fetchone(), (1,))

    def test_suspension_blocks_recovery_of_an_existing_claim(self):
        self.redeem()
        self.db.execute("UPDATE companies SET status='suspended'")
        query = sql("server/src/routes/agents.rs", "SELECT t.id, t.company_id")
        self.assertIsNone(self.db.execute(query, ("a" * 64, "key-hash", "PC", 2, "b" * 64, "co")).fetchone())

    def test_suspended_handoff_cannot_be_redeemed(self):
        self.redeem()
        self.db.execute("INSERT INTO remote_handoffs (token_hash,company_id,device_id,user_id,created_at,expires_at) VALUES (?,'co','device','user',0,1000)", ("c" * 64,))
        self.db.execute("UPDATE companies SET status='suspended'")
        query = sql("server/src/routes/handoffs.rs", "UPDATE remote_handoffs SET")
        self.assertIsNone(self.db.execute(query, (1, "c" * 64, "co")).fetchone())

    def test_background_handoff_mode_survives_redemption(self):
        self.redeem()
        insert = sql("server/src/routes/handoffs.rs", "INSERT INTO remote_handoffs")
        redeem = sql("server/src/routes/handoffs.rs", "UPDATE remote_handoffs SET")
        for mode, token in ((False, "c" * 64), (True, "d" * 64)):
            self.db.execute(insert, (token, "co", "device", "user", 0, 1000, mode))
            row = self.db.execute(redeem, (1, token, "co")).fetchone()
            self.assertEqual(row[3], int(mode))

    def test_rotation_stages_without_disabling_current_credential(self):
        self.redeem()
        query = sql("server/src/routes/agents.rs", "UPDATE agents SET pending_auth_token_hash")
        self.db.execute(query, ("c" * 64, 2, "device", "co"))
        self.assertEqual(self.db.execute("SELECT auth_token_hash,pending_auth_token_hash FROM agents").fetchone(), ("b" * 64, "c" * 64))
        self.db.execute(query, ("d" * 64, 3, "device", "co"))
        self.assertEqual(self.db.execute("SELECT pending_auth_token_hash FROM agents").fetchone(), ("c" * 64,))


if __name__ == "__main__":
    unittest.main()
