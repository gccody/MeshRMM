//! The SQLite and PostgreSQL migrations must define the same tables, columns,
//! nullability and named indexes (on the same table and key columns, unique
//! and partial alike), since one set of queries runs on both.
mod common;

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Connection, PgConnection, SqliteConnection};

type Columns = BTreeSet<(String, String, bool)>;

/// Each named index's table, whether it is unique, whether it is partial, and
/// its key columns in order (`None` for an expression).
#[derive(Debug, PartialEq, Eq)]
struct Index {
    table: String,
    unique: bool,
    partial: bool,
    columns: Vec<Option<String>>,
}

type Indexes = BTreeMap<String, Index>;

/// Collects one row per key column, ordered by index and position.
fn indexes(rows: Vec<(String, String, bool, bool, Option<String>)>) -> Indexes {
    let mut indexes = Indexes::new();
    for (name, table, unique, partial, column) in rows {
        let index = indexes.entry(name).or_insert_with(|| Index {
            table,
            unique,
            partial,
            columns: Vec::new(),
        });
        index.columns.push(column);
    }
    indexes
}

async fn sqlite_schema(url: &str) -> (Columns, Indexes) {
    let mut connection = SqliteConnection::connect(url).await.unwrap();
    let columns: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT m.name, p.name, p.\"notnull\" = 0 \
         FROM sqlite_master AS m JOIN pragma_table_info(m.name) AS p \
         WHERE m.type IN ('table', 'view') AND m.name NOT LIKE 'sqlite_%' AND m.name <> '_sqlx_migrations'",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    let index_columns: Vec<(String, String, bool, bool, Option<String>)> = sqlx::query_as(
        "SELECT m.name, m.tbl_name, l.\"unique\" = 1, l.partial = 1, x.name \
         FROM sqlite_master AS m \
         JOIN pragma_index_list(m.tbl_name) AS l ON l.name = m.name \
         JOIN pragma_index_xinfo(m.name) AS x \
         WHERE m.type = 'index' AND m.name LIKE 'idx_%' AND x.key = 1 \
         ORDER BY m.name, x.seqno",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    (columns.into_iter().collect(), indexes(index_columns))
}

async fn postgres_schema(url: &str) -> (Columns, Indexes) {
    let mut connection = PgConnection::connect(url).await.unwrap();
    let columns: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text, is_nullable = 'YES' \
         FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name <> '_sqlx_migrations'",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    let index_columns: Vec<(String, String, bool, bool, Option<String>)> = sqlx::query_as(
        "SELECT c.relname::text, t.relname::text, i.indisunique, i.indpred IS NOT NULL, \
         a.attname::text \
         FROM pg_index AS i \
         JOIN pg_class AS c ON c.oid = i.indexrelid \
         JOIN pg_class AS t ON t.oid = i.indrelid \
         JOIN pg_namespace AS n ON n.oid = c.relnamespace \
         CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, position) \
         LEFT JOIN pg_attribute AS a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
         WHERE n.nspname = 'public' AND c.relname LIKE 'idx_%' AND k.position <= i.indnkeyatts \
         ORDER BY c.relname, k.position",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    (columns.into_iter().collect(), indexes(index_columns))
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
    let differing = sqlite_indexes
        .keys()
        .chain(postgres_indexes.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|name| sqlite_indexes.get(*name) != postgres_indexes.get(*name))
        .map(|name| (name, sqlite_indexes.get(name), postgres_indexes.get(name)))
        .collect::<Vec<_>>();
    assert!(
        differing.is_empty(),
        "named indexes differ (name, SQLite, PostgreSQL):\n{differing:#?}"
    );
    assert!(
        sqlite_indexes.len() > 50,
        "the index query found too few indexes"
    );
    assert!(
        sqlite_columns.len() > 100,
        "the schema query found too few columns"
    );

    sqlite.drop_database().await;
    postgres.drop_database().await;
}
