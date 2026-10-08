//! The SQLite and PostgreSQL migrations must define the same tables, columns,
//! nullability and named indexes, since one set of queries runs on both.
mod common;

use std::collections::BTreeSet;

use sqlx::{Connection, PgConnection, SqliteConnection};

type Columns = BTreeSet<(String, String, bool)>;

async fn sqlite_schema(url: &str) -> (Columns, BTreeSet<String>) {
    let mut connection = SqliteConnection::connect(url).await.unwrap();
    let columns: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT m.name, p.name, p.\"notnull\" = 0 \
         FROM sqlite_master AS m JOIN pragma_table_info(m.name) AS p \
         WHERE m.type IN ('table', 'view') AND m.name NOT LIKE 'sqlite_%' AND m.name <> '_sqlx_migrations'",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    let indexes: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%'")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    (
        columns.into_iter().collect(),
        indexes.into_iter().map(|(name,)| name).collect(),
    )
}

async fn postgres_schema(url: &str) -> (Columns, BTreeSet<String>) {
    let mut connection = PgConnection::connect(url).await.unwrap();
    let columns: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text, is_nullable = 'YES' \
         FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name <> '_sqlx_migrations'",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    let indexes: Vec<(String,)> =
        sqlx::query_as("SELECT indexname::text FROM pg_indexes WHERE schemaname = 'public' AND indexname LIKE 'idx_%'")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    (
        columns.into_iter().collect(),
        indexes.into_iter().map(|(name,)| name).collect(),
    )
}

#[tokio::test]
async fn sqlite_and_postgres_schemas_match() {
    let Some(admin_url) = common::postgres_admin_url() else {
        return;
    };
    let sqlite = common::sqlite().await;
    let postgres = common::postgres(&admin_url).await;
    let (mut sqlite_columns, sqlite_indexes) = sqlite_schema(&sqlite.url).await;
    let (postgres_columns, postgres_indexes) = postgres_schema(&postgres.url).await;

    // SQLite reports an INTEGER PRIMARY KEY as nullable; PostgreSQL does not.
    for table in ["settings", "oidc_provider"] {
        if sqlite_columns.remove(&(table.to_owned(), "id".to_owned(), true)) {
            sqlite_columns.insert((table.to_owned(), "id".to_owned(), false));
        }
    }

    let only_sqlite = sqlite_columns
        .difference(&postgres_columns)
        .collect::<Vec<_>>();
    let only_postgres = postgres_columns
        .difference(&sqlite_columns)
        .collect::<Vec<_>>();
    assert!(
        only_sqlite.is_empty() && only_postgres.is_empty(),
        "columns differ (table, column, nullable):\n only in SQLite: {only_sqlite:#?}\n only in PostgreSQL: {only_postgres:#?}"
    );
    assert_eq!(sqlite_indexes, postgres_indexes, "named indexes differ");
    assert!(
        sqlite_columns.len() > 100,
        "the schema query found too few columns"
    );

    sqlite.drop_database().await;
    postgres.drop_database().await;
}
