//! The storage layer on every backend: migrations, row decoding, transactions
//! and maintenance.
mod common;

use meshrmm_server::{
    db::Database,
    maintenance::{self, Purged},
    time::{DAY_MS, MINUTE_MS},
};
use sea_query::{Alias, Expr, ExprTrait, Func, Query};

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
                .columns(["id", "name", "auth_token_hash", "created_at", "updated_at"])
                .values_panic([
                    id.into(),
                    "Desk".into(),
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

        let purged = maintenance::purge(database, NOW).await.unwrap();
        assert_eq!(
            purged,
            Purged {
                handoffs: 1,
                script_runs: 1,
                ..Purged::default()
            },
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
        test.drop_database().await;
    }
}
