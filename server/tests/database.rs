//! The storage layer on every backend: migrations, row decoding, transactions
//! and maintenance.
mod common;

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use meshrmm_server::{
    db::{Database, Limits, tables::Companies},
    maintenance,
    time::{DAY_MS, MINUTE_MS},
};
use sea_query::{Alias, Expr, ExprTrait, Func, Query};
use sqlx::Connection;

const NOW: i64 = 1_800_000_000_000;

#[derive(Debug, sqlx::FromRow)]
struct SettingsRow {
    instance_name: String,
    dashboard_idle_timeout_minutes: i64,
    display_border: bool,
    connection_approval: bool,
    idle_disconnect_minutes: Option<i64>,
    smtp_password_encrypted: Option<Vec<u8>>,
}

async fn count(database: &Database, table: &str) -> i64 {
    let (count,): (i64,) = database
        .fetch_one(
            &Query::select()
                .expr(Func::count(Expr::col(sea_query::Asterisk)))
                .from(Alias::new(table))
                .to_owned(),
        )
        .await
        .unwrap();
    count
}

#[tokio::test]
async fn migrations_are_idempotent_and_report_the_schema_version() {
    for test in common::databases().await {
        let database = &test.database;
        database.migrate().await.unwrap();
        assert_eq!(
            database.schema_version().await.unwrap(),
            Some(database.backend().expected_schema_version()),
            "{}",
            test.name
        );
        test.drop_database().await;
    }
}

#[tokio::test]
async fn an_unmigrated_database_has_no_schema_version() {
    let mut databases = vec![common::sqlite_with(false).await];
    if let Some(admin_url) = common::postgres_admin_url() {
        databases.push(common::postgres_with(&admin_url, false).await);
    }
    for test in databases {
        assert_eq!(
            test.database.schema_version().await.unwrap(),
            None,
            "{}",
            test.name
        );
        test.drop_database().await;
    }
}

#[tokio::test]
async fn settings_start_with_one_row_of_defaults_that_decode_on_every_backend() {
    for test in common::databases().await {
        let settings: SettingsRow = test
            .database
            .fetch_one(
                &Query::select()
                    .columns([
                        "instance_name",
                        "dashboard_idle_timeout_minutes",
                        "display_border",
                        "connection_approval",
                        "idle_disconnect_minutes",
                        "smtp_password_encrypted",
                    ])
                    .from("settings")
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(settings.instance_name, "MeshRMM", "{}", test.name);
        assert_eq!(settings.dashboard_idle_timeout_minutes, 240);
        assert!(settings.display_border);
        assert!(!settings.connection_approval);
        assert_eq!(settings.idle_disconnect_minutes, None);
        assert_eq!(settings.smtp_password_encrypted, None);

        // The single-row constraint holds on both backends.
        let second = test
            .database
            .execute(
                &Query::insert()
                    .into_table("settings")
                    .columns(["id", "updated_at"])
                    .values_panic([2.into(), 0.into()])
                    .to_owned(),
            )
            .await;
        assert!(
            second.is_err(),
            "{}: a second settings row was accepted",
            test.name
        );
        test.drop_database().await;
    }
}

#[tokio::test]
async fn transactions_commit_or_roll_back() {
    for test in common::databases().await {
        let insert = |id: &str| {
            Query::insert()
                .into_table("agents")
                .columns([
                    "id",
                    "name",
                    "platform",
                    "auth_token_hash",
                    "created_at",
                    "updated_at",
                ])
                .values_panic([
                    id.into(),
                    "Desk".into(),
                    "windows".into(),
                    "a".repeat(64).into(),
                    NOW.into(),
                    NOW.into(),
                ])
                .to_owned()
        };
        let mut transaction = test.database.begin().await.unwrap();
        transaction.execute(&insert("rolled-back")).await.unwrap();
        transaction.rollback().await.unwrap();

        let mut transaction = test.database.begin().await.unwrap();
        transaction.execute(&insert("committed")).await.unwrap();
        let (inside,): (i64,) = transaction
            .fetch_one(
                &Query::select()
                    .expr(Func::count(Expr::col(sea_query::Asterisk)))
                    .from("agents")
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(inside, 1, "{}", test.name);
        transaction.commit().await.unwrap();

        let ids: Vec<(String,)> = test
            .database
            .fetch_all(&Query::select().column("id").from("agents").to_owned())
            .await
            .unwrap();
        assert_eq!(ids, [("committed".to_owned(),)], "{}", test.name);
        test.drop_database().await;
    }
}

#[tokio::test]
async fn maintenance_purges_only_expired_rows_and_old_history() {
    for test in common::databases().await {
        let database = &test.database;
        let expired = NOW - 11 * MINUTE_MS;
        let within_grace = NOW - 5 * MINUTE_MS;
        for (token, expires_at) in [("1", expired), ("2", within_grace), ("3", NOW + MINUTE_MS)] {
            database
                .execute(
                    &Query::insert()
                        .into_table("remote_handoffs")
                        .columns([
                            "token_hash",
                            "device_id",
                            "user_id",
                            "created_at",
                            "expires_at",
                        ])
                        .values_panic([
                            token.repeat(64).into(),
                            "device".into(),
                            "user".into(),
                            (expires_at - MINUTE_MS).into(),
                            expires_at.into(),
                        ])
                        .to_owned(),
                )
                .await
                .unwrap();
        }
        for (id, created_at) in [("old", NOW - 31 * DAY_MS), ("recent", NOW - DAY_MS)] {
            database
                .execute(
                    &Query::insert()
                        .into_table("script_runs")
                        .columns([
                            "id",
                            "device_id",
                            "script_id",
                            "script_name",
                            "language",
                            "requested_by_user_id",
                            "source",
                            "run_as",
                            "timeout_seconds",
                            "output_truncated",
                            "created_at",
                        ])
                        .values_panic([
                            id.into(),
                            "device".into(),
                            "script".into(),
                            "Script".into(),
                            "powershell".into(),
                            "user".into(),
                            "dashboard".into(),
                            "system".into(),
                            300.into(),
                            false.into(),
                            created_at.into(),
                        ])
                        .to_owned(),
                )
                .await
                .unwrap();
        }

        for (minute, device) in [(NOW - 8 * DAY_MS, "old"), (NOW - 6 * DAY_MS, "recent")] {
            database
                .execute(
                    &Query::insert()
                        .into_table("device_metrics")
                        .columns([
                            "device_id",
                            "minute",
                            "samples",
                            "cpu_percent",
                            "cpu_percent_max",
                            "memory_used_bytes",
                            "memory_total_bytes",
                            "network_received_bytes_per_second",
                            "network_sent_bytes_per_second",
                            "storage_used_bytes",
                            "storage_total_bytes",
                        ])
                        .values_panic([
                            device.into(),
                            minute.into(),
                            12.into(),
                            1.5.into(),
                            3.0.into(),
                            1.into(),
                            2.into(),
                            3.into(),
                            4.into(),
                            5.into(),
                            6.into(),
                        ])
                        .to_owned(),
                )
                .await
                .unwrap();
        }

        for (id, observed_at) in [("old", NOW - 91 * DAY_MS), ("recent", NOW - 89 * DAY_MS)] {
            database
                .execute(
                    &Query::insert()
                        .into_table("device_application_changes")
                        .columns(["id", "device_id", "observed_at", "change", "name", "source"])
                        .values_panic([
                            id.into(),
                            "device".into(),
                            observed_at.into(),
                            "installed".into(),
                            "7-Zip".into(),
                            "windows".into(),
                        ])
                        .to_owned(),
                )
                .await
                .unwrap();
        }

        database
            .execute(
                &Query::insert()
                    .into_table("alert_rules")
                    .columns([
                        "id",
                        "name",
                        "severity",
                        "condition",
                        "created_at",
                        "updated_at",
                    ])
                    .values_panic([
                        "rule".into(),
                        "Rule".into(),
                        "info".into(),
                        "script_failed".into(),
                        0.into(),
                        0.into(),
                    ])
                    .to_owned(),
            )
            .await
            .unwrap();
        for (id, state, resolved_at) in [
            ("old", "resolved", Some(NOW - 91 * DAY_MS)),
            ("recent", "resolved", Some(NOW - 89 * DAY_MS)),
            ("open", "open", None),
        ] {
            database
                .execute(
                    &Query::insert()
                        .into_table("alerts")
                        .columns([
                            "id",
                            "rule_id",
                            "device_id",
                            "subject",
                            "state",
                            "detail",
                            "raised_at",
                            "last_occurred_at",
                            "resolved_at",
                        ])
                        .values_panic([
                            id.into(),
                            "rule".into(),
                            "device".into(),
                            id.into(),
                            state.into(),
                            "Failed".into(),
                            (NOW - 100 * DAY_MS).into(),
                            (NOW - 100 * DAY_MS).into(),
                            resolved_at.into(),
                        ])
                        .to_owned(),
                )
                .await
                .unwrap();
        }

        for (id, created_at) in [("old-wake", NOW - 31 * DAY_MS), ("new-wake", NOW - DAY_MS)] {
            database
                .execute(
                    &Query::insert()
                        .into_table("wake_requests")
                        .columns([
                            "id",
                            "device_id",
                            "requested_by_user_id",
                            "status",
                            "mac_addresses",
                            "created_at",
                        ])
                        .values_panic([
                            id.into(),
                            "device".into(),
                            "user".into(),
                            "sent".into(),
                            "AA:00:00:00:00:01".into(),
                            created_at.into(),
                        ])
                        .to_owned(),
                )
                .await
                .unwrap();
            database
                .execute(
                    &Query::insert()
                        .into_table("wake_relays")
                        .columns([
                            "wake_id",
                            "relay_device_id",
                            "relay_name",
                            "broadcast",
                            "prefix_length",
                            "status",
                        ])
                        .values_panic([
                            id.into(),
                            "relay".into(),
                            "Relay".into(),
                            "192.168.1.255".into(),
                            24.into(),
                            "sent".into(),
                        ])
                        .to_owned(),
                )
                .await
                .unwrap();
        }

        database
            .execute(
                &Query::insert()
                    .into_table("users")
                    .columns(["id", "email", "display_name", "created_at", "updated_at"])
                    .values_panic([
                        "token-user".into(),
                        "tokens@example.com".into(),
                        "Tokens".into(),
                        0.into(),
                        0.into(),
                    ])
                    .to_owned(),
            )
            .await
            .unwrap();
        for (id, expires_at) in [
            ("long-expired", Some(NOW - 31 * DAY_MS)),
            ("expired", Some(NOW - DAY_MS)),
            ("never", None),
        ] {
            database
                .execute(
                    &Query::insert()
                        .into_table("api_tokens")
                        .columns([
                            "id",
                            "user_id",
                            "name",
                            "token_hash",
                            "created_at",
                            "expires_at",
                        ])
                        .values_panic([
                            id.into(),
                            "token-user".into(),
                            id.into(),
                            format!("{id:0>64}").into(),
                            0.into(),
                            expires_at.into(),
                        ])
                        .to_owned(),
                )
                .await
                .unwrap();
        }

        let purged = maintenance::purge(database, NOW).await;
        assert_eq!(purged.failed, Vec::<String>::new(), "{}", test.name);
        let rows: BTreeMap<&str, u64> = purged
            .deleted
            .iter()
            .map(|(table, deleted)| (table.as_str(), deleted.rows))
            .collect();
        assert_eq!(
            rows,
            BTreeMap::from([
                ("alerts", 1),
                ("api_tokens", 1),
                ("device_application_changes", 1),
                ("device_metrics", 1),
                ("remote_handoffs", 1),
                ("script_runs", 1),
                ("wake_requests", 1),
            ]),
            "{}",
            test.name
        );
        assert_eq!(count(database, "remote_handoffs").await, 2);
        let remaining: Vec<(String,)> = database
            .fetch_all(
                &Query::select()
                    .column("id")
                    .from("script_runs")
                    .and_where(Expr::col("output_truncated").eq(false))
                    .to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(remaining, [("recent".to_owned(),)], "{}", test.name);
        assert_eq!(count(database, "alerts").await, 2, "{}", test.name);
        assert_eq!(
            count(database, "api_tokens").await,
            2,
            "expired API tokens stay listed for a while: {}",
            test.name
        );
        assert_eq!(
            count(database, "wake_relays").await,
            1,
            "a purged wake request's relays go with it: {}",
            test.name
        );
        assert_eq!(
            count(database, "device_application_changes").await,
            1,
            "{}",
            test.name
        );
        test.drop_database().await;
    }
}

async fn insert_metrics(database: &Database, rows: impl IntoIterator<Item = (String, i64)>) {
    let mut insert = Query::insert();
    insert.into_table("device_metrics").columns([
        "device_id",
        "minute",
        "samples",
        "cpu_percent",
        "cpu_percent_max",
        "memory_used_bytes",
        "memory_total_bytes",
        "network_received_bytes_per_second",
        "network_sent_bytes_per_second",
        "storage_used_bytes",
        "storage_total_bytes",
    ]);
    for (device, minute) in rows {
        insert.values_panic([
            device.into(),
            minute.into(),
            1.into(),
            1.0.into(),
            1.0.into(),
            1.into(),
            2.into(),
            0.into(),
            0.into(),
            1.into(),
            2.into(),
        ]);
    }
    database.execute(&insert).await.unwrap();
}

async fn insert_job(database: &Database, id: &str, created_at: i64, devices: usize) {
    database
        .execute(
            &Query::insert()
                .into_table("jobs")
                .columns([
                    "id",
                    "kind",
                    "item_id",
                    "item_name",
                    "target",
                    "target_label",
                    "offline",
                    "expires_at",
                    "requested_by_user_id",
                    "created_at",
                ])
                .values_panic([
                    id.into(),
                    "script".into(),
                    "script".into(),
                    "Script".into(),
                    "all".into(),
                    "All devices".into(),
                    "skip".into(),
                    created_at.into(),
                    "user".into(),
                    created_at.into(),
                ])
                .to_owned(),
        )
        .await
        .unwrap();
    let mut insert = Query::insert();
    insert
        .into_table("job_devices")
        .columns(["job_id", "device_id", "state", "updated_at"]);
    for device in 0..devices {
        insert.values_panic([
            id.into(),
            format!("device-{device}").into(),
            "started".into(),
            created_at.into(),
        ]);
    }
    database.execute(&insert).await.unwrap();
}

#[tokio::test]
async fn maintenance_purges_a_batch_at_a_time_until_nothing_is_due() {
    for test in common::databases().await {
        let database = &test.database;
        insert_metrics(
            database,
            (0..25).map(|n| {
                (
                    format!("device-{}", n % 4),
                    NOW - 8 * DAY_MS - n * MINUTE_MS,
                )
            }),
        )
        .await;
        insert_metrics(
            database,
            (0..3).map(|n| (format!("device-{n}"), NOW - 6 * DAY_MS)),
        )
        .await;
        // A job on more devices than a batch holds, and a recent one.
        insert_job(database, "old", NOW - 31 * DAY_MS, 25).await;
        insert_job(database, "recent", NOW - DAY_MS, 2).await;

        let purged = maintenance::purge_in_batches(database, NOW, 10).await;
        assert_eq!(purged.failed, Vec::<String>::new(), "{}", test.name);
        let deleted = |table: &str| {
            let deleted = purged.deleted[table];
            (deleted.rows, deleted.batches)
        };
        assert_eq!(deleted("device_metrics"), (25, 3), "{}", test.name);
        assert_eq!(deleted("job_devices"), (25, 3), "{}", test.name);
        assert_eq!(deleted("jobs"), (1, 1), "{}", test.name);
        assert_eq!(count(database, "device_metrics").await, 3, "{}", test.name);
        assert_eq!(count(database, "job_devices").await, 2, "{}", test.name);
        assert_eq!(count(database, "jobs").await, 1, "{}", test.name);

        let again = maintenance::purge_in_batches(database, NOW, 10).await;
        assert_eq!(again, maintenance::Purged::default(), "{}", test.name);
        test.drop_database().await;
    }
}

#[tokio::test]
async fn a_table_the_purge_fails_on_does_not_stop_the_rest() {
    for test in common::databases().await {
        let database = &test.database;
        let drop = "DROP TABLE remote_sessions";
        match test.name {
            "sqlite" => {
                let mut connection = sqlx::SqliteConnection::connect(&test.url).await.unwrap();
                sqlx::query(drop).execute(&mut connection).await.unwrap();
            }
            _ => {
                let mut connection = sqlx::PgConnection::connect(&test.url).await.unwrap();
                sqlx::query(drop).execute(&mut connection).await.unwrap();
            }
        }
        insert_metrics(database, [("device".to_owned(), NOW - 8 * DAY_MS)]).await;

        let purged = maintenance::purge(database, NOW).await;
        assert_eq!(purged.failed, ["remote_sessions"], "{}", test.name);
        assert_eq!(purged.rows("device_metrics"), 1, "{}", test.name);
        assert_eq!(count(database, "device_metrics").await, 0, "{}", test.name);
        test.drop_database().await;
    }
}

fn company(id: &str) -> sea_query::InsertStatement {
    Query::insert()
        .into_table(Companies::Table)
        .columns([
            Companies::Id,
            Companies::Name,
            Companies::CreatedAt,
            Companies::UpdatedAt,
        ])
        .values_panic([id.into(), id.into(), NOW.into(), NOW.into()])
        .to_owned()
}

#[tokio::test]
async fn concurrent_writes_wait_their_turn_instead_of_failing_as_locked() {
    for test in common::databases().await {
        let database = Database::connect_with(
            &test.url,
            Limits {
                connections: 4,
                acquire_timeout: Duration::from_secs(30),
                write_timeout: Duration::from_secs(30),
            },
        )
        .await
        .unwrap();
        // More writers than connections, and transactions that between them
        // hold the write lock for longer than SQLite's busy timeout (5 s).
        let writes = (0..60).map(|i| {
            let database = database.clone();
            tokio::spawn(async move {
                if i % 6 == 0 {
                    let mut transaction = database.begin().await?;
                    transaction.execute(&company(&format!("t{i}"))).await?;
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    transaction.commit().await
                } else {
                    database.execute(&company(&format!("s{i}"))).await.map(drop)
                }
            })
        });
        for (i, write) in futures_util::future::join_all(writes)
            .await
            .into_iter()
            .enumerate()
        {
            write
                .unwrap()
                .unwrap_or_else(|error| panic!("{}: write {i} failed: {error}", test.name));
        }
        assert_eq!(count(&database, "companies").await, 60, "{}", test.name);
        database.close().await;
        test.drop_database().await;
    }
}

#[tokio::test]
async fn reads_proceed_while_a_transaction_holds_the_writer() {
    for test in common::databases().await {
        let database = &test.database;
        let mut transaction = database.begin().await.unwrap();
        transaction.execute(&company("pending")).await.unwrap();
        let during = tokio::time::timeout(Duration::from_secs(2), count(database, "companies"))
            .await
            .unwrap_or_else(|_| panic!("{}: a read waited for the transaction", test.name));
        assert_eq!(during, 0, "{}: saw an uncommitted write", test.name);
        transaction.commit().await.unwrap();
        // Committed, it's visible to the next read at once.
        assert_eq!(count(database, "companies").await, 1, "{}", test.name);
        test.drop_database().await;
    }
}

#[tokio::test]
async fn a_write_with_no_free_connection_fails_at_its_timeout() {
    for test in common::databases().await {
        let timeout = Duration::from_millis(200);
        let database = Database::connect_with(
            &test.url,
            Limits {
                connections: 1,
                acquire_timeout: timeout,
                write_timeout: timeout,
            },
        )
        .await
        .unwrap();
        let transaction = database.begin().await.unwrap();
        let started = Instant::now();
        let error = database.execute(&company("late")).await.unwrap_err();
        assert!(
            matches!(error, sqlx::Error::PoolTimedOut),
            "{}: {error}",
            test.name
        );
        assert!(started.elapsed() < Duration::from_secs(2), "{}", test.name);
        transaction.rollback().await.unwrap();
        database.execute(&company("in time")).await.unwrap();
        database.close().await;
        test.drop_database().await;
    }
}
