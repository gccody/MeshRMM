//! Storage on SQLite or PostgreSQL, chosen by the database URL.
//!
//! Queries are built once with `sea-query` and rendered in the connected
//! backend's dialect, so every query runs on both. Row types decode from
//! either backend when their integers are `i64` (PostgreSQL columns are all
//! `BIGINT`) and their booleans are `bool`.
pub mod tables;

use std::{str::FromStr, time::Duration};

use anyhow::{Context, bail};
use sea_query::{
    Expr, ExprTrait, Func, PostgresQueryBuilder, Query, QueryStatementWriter, SqliteQueryBuilder,
};
use sea_query_sqlx::SqlxBinder;
use sqlx::{
    FromRow, PgPool, SqlitePool,
    migrate::Migrator,
    postgres::{PgPoolOptions, PgRow},
    sqlite::{
        SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
    },
};

use self::tables::SqlxMigrations;

static SQLITE_MIGRATIONS: Migrator = sqlx::migrate!("./migrations/sqlite");
static POSTGRES_MIGRATIONS: Migrator = sqlx::migrate!("./migrations/postgres");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

impl Backend {
    fn migrator(self) -> &'static Migrator {
        match self {
            Self::Sqlite => &SQLITE_MIGRATIONS,
            Self::Postgres => &POSTGRES_MIGRATIONS,
        }
    }

    /// The newest migration this build ships.
    pub fn expected_schema_version(self) -> i64 {
        self.migrator()
            .iter()
            .map(|migration| migration.version)
            .max()
            .unwrap_or_default()
    }
}

/// A row type that decodes from both backends.
pub trait Row: for<'r> FromRow<'r, SqliteRow> + for<'r> FromRow<'r, PgRow> + Send + Unpin {}

impl<T> Row for T where T: for<'r> FromRow<'r, SqliteRow> + for<'r> FromRow<'r, PgRow> + Send + Unpin
{}

pub type Result<T> = std::result::Result<T, sqlx::Error>;

#[derive(Debug, Clone)]
enum Pool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

/// A connection pool for one database.
#[derive(Debug, Clone)]
pub struct Database {
    pool: Pool,
}

/// Runs the same body against whichever pool or transaction is connected,
/// with `$builder` bound to that backend's `sea-query` builder.
macro_rules! dispatch {
    ($target:expr, $conn:ident, $builder:ident => $body:expr) => {
        match $target {
            Pool::Sqlite($conn) => {
                let $builder = SqliteQueryBuilder;
                $body
            }
            Pool::Postgres($conn) => {
                let $builder = PostgresQueryBuilder;
                $body
            }
        }
    };
}

impl Database {
    /// Connects to `url`: `sqlite://<path>` (created if missing) or
    /// `postgres://...`/`postgresql://...`.
    pub async fn connect(url: &str, max_connections: u32) -> anyhow::Result<Self> {
        let pool = if url.starts_with("sqlite:") {
            let options = SqliteConnectOptions::from_str(url)
                .context("invalid SQLite database URL")?
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .foreign_keys(true)
                .busy_timeout(Duration::from_secs(5));
            Pool::Sqlite(
                SqlitePoolOptions::new()
                    .max_connections(max_connections)
                    .connect_with(options)
                    .await
                    .context("could not open the SQLite database")?,
            )
        } else if url.starts_with("postgres:") || url.starts_with("postgresql:") {
            Pool::Postgres(
                PgPoolOptions::new()
                    .max_connections(max_connections)
                    .acquire_timeout(Duration::from_secs(10))
                    .connect(url)
                    .await
                    .context("could not connect to PostgreSQL")?,
            )
        } else {
            bail!("database.url must start with sqlite:// or postgres://");
        };
        Ok(Self { pool })
    }

    pub fn backend(&self) -> Backend {
        match self.pool {
            Pool::Sqlite(_) => Backend::Sqlite,
            Pool::Postgres(_) => Backend::Postgres,
        }
    }

    /// Applies every migration this build ships that the database lacks.
    pub async fn migrate(&self) -> anyhow::Result<()> {
        let migrator = self.backend().migrator();
        match &self.pool {
            Pool::Sqlite(pool) => migrator.run(pool).await,
            Pool::Postgres(pool) => migrator.run(pool).await,
        }
        .context("database migration failed")
    }

    /// The newest migration applied to the database, if any.
    pub async fn schema_version(&self) -> Result<Option<i64>> {
        let statement = Query::select()
            .expr(Func::max(Expr::col(SqlxMigrations::Version)))
            .from(SqlxMigrations::Table)
            .and_where(Expr::col(SqlxMigrations::Success).eq(true))
            .to_owned();
        match self.fetch_one::<(Option<i64>,), _>(&statement).await {
            Ok((version,)) => Ok(version),
            // A database the server has never migrated has no history table.
            Err(sqlx::Error::Database(error)) if is_missing_table(error.as_ref()) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Executes a statement and returns the number of rows it affected.
    pub async fn execute<S: QueryStatementWriter + SqlxBinder>(
        &self,
        statement: &S,
    ) -> Result<u64> {
        dispatch!(&self.pool, pool, builder => {
            let (sql, values) = render(statement, builder);
            Ok(sqlx::query_with(sql, values).execute(pool).await?.rows_affected())
        })
    }

    pub async fn fetch_all<T: Row, S: QueryStatementWriter + SqlxBinder>(
        &self,
        statement: &S,
    ) -> Result<Vec<T>> {
        dispatch!(&self.pool, pool, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_all(pool).await
        })
    }

    pub async fn fetch_optional<T: Row, S: QueryStatementWriter + SqlxBinder>(
        &self,
        statement: &S,
    ) -> Result<Option<T>> {
        dispatch!(&self.pool, pool, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_optional(pool).await
        })
    }

    pub async fn fetch_one<T: Row, S: QueryStatementWriter + SqlxBinder>(
        &self,
        statement: &S,
    ) -> Result<T> {
        dispatch!(&self.pool, pool, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_one(pool).await
        })
    }

    /// Starts a transaction. Dropping it without [`Transaction::commit`]
    /// rolls it back.
    ///
    /// On SQLite it takes the write lock up front (`BEGIN IMMEDIATE`): a
    /// transaction that reads and then writes would otherwise fail at once,
    /// without waiting, when another connection wrote in between.
    pub async fn begin(&self) -> Result<Transaction> {
        Ok(Transaction {
            inner: match &self.pool {
                Pool::Sqlite(pool) => {
                    TransactionInner::Sqlite(pool.begin_with("BEGIN IMMEDIATE").await?)
                }
                Pool::Postgres(pool) => TransactionInner::Postgres(pool.begin().await?),
            },
        })
    }

    /// Waits for in-flight queries and closes every connection.
    pub async fn close(&self) {
        match &self.pool {
            Pool::Sqlite(pool) => pool.close().await,
            Pool::Postgres(pool) => pool.close().await,
        }
    }
}

fn is_missing_table(error: &dyn sqlx::error::DatabaseError) -> bool {
    // PostgreSQL's undefined_table, or SQLite's generic error with its message.
    error.code().as_deref() == Some("42P01") || error.message().starts_with("no such table")
}

/// The file a `sqlite://` URL names, or `None` for another backend.
pub fn sqlite_path(url: &str) -> anyhow::Result<Option<std::path::PathBuf>> {
    if !url.starts_with("sqlite:") {
        return Ok(None);
    }
    let options = SqliteConnectOptions::from_str(url).context("invalid SQLite database URL")?;
    Ok(Some(options.get_filename().to_path_buf()))
}

/// Renders a statement for one backend. The SQL text comes only from
/// `sea-query` and identifiers in this crate; every value is a bind
/// parameter, so the text is safe to run.
fn render<S: SqlxBinder>(
    statement: &S,
    builder: impl sea_query::QueryBuilder,
) -> (sqlx::AssertSqlSafe<String>, sea_query_sqlx::SqlxValues) {
    let (sql, values) = statement.build_sqlx(builder);
    (sqlx::AssertSqlSafe(sql), values)
}

enum TransactionInner {
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
}

/// A transaction on either backend, with the same query methods as
/// [`Database`].
pub struct Transaction {
    inner: TransactionInner,
}

macro_rules! dispatch_transaction {
    ($target:expr, $conn:ident, $builder:ident => $body:expr) => {
        match $target {
            TransactionInner::Sqlite($conn) => {
                let $builder = SqliteQueryBuilder;
                $body
            }
            TransactionInner::Postgres($conn) => {
                let $builder = PostgresQueryBuilder;
                $body
            }
        }
    };
}

impl Transaction {
    pub async fn execute<S: QueryStatementWriter + SqlxBinder>(
        &mut self,
        statement: &S,
    ) -> Result<u64> {
        dispatch_transaction!(&mut self.inner, connection, builder => {
            let (sql, values) = render(statement, builder);
            Ok(sqlx::query_with(sql, values).execute(&mut **connection).await?.rows_affected())
        })
    }

    pub async fn fetch_all<T: Row, S: QueryStatementWriter + SqlxBinder>(
        &mut self,
        statement: &S,
    ) -> Result<Vec<T>> {
        dispatch_transaction!(&mut self.inner, connection, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_all(&mut **connection).await
        })
    }

    pub async fn fetch_optional<T: Row, S: QueryStatementWriter + SqlxBinder>(
        &mut self,
        statement: &S,
    ) -> Result<Option<T>> {
        dispatch_transaction!(&mut self.inner, connection, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_optional(&mut **connection).await
        })
    }

    pub async fn fetch_one<T: Row, S: QueryStatementWriter + SqlxBinder>(
        &mut self,
        statement: &S,
    ) -> Result<T> {
        dispatch_transaction!(&mut self.inner, connection, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_one(&mut **connection).await
        })
    }

    pub async fn commit(self) -> Result<()> {
        match self.inner {
            TransactionInner::Sqlite(transaction) => transaction.commit().await,
            TransactionInner::Postgres(transaction) => transaction.commit().await,
        }
    }

    pub async fn rollback(self) -> Result<()> {
        match self.inner {
            TransactionInner::Sqlite(transaction) => transaction.rollback().await,
            TransactionInner::Postgres(transaction) => transaction.rollback().await,
        }
    }
}

/// Query methods shared by [`Database`] and [`Transaction`], for helpers that
/// run either on their own or as part of a larger change.
pub trait Executor: Send {
    fn execute<S: QueryStatementWriter + SqlxBinder + Sync>(
        &mut self,
        statement: &S,
    ) -> impl Future<Output = Result<u64>> + Send;

    fn fetch_all<T: Row, S: QueryStatementWriter + SqlxBinder + Sync>(
        &mut self,
        statement: &S,
    ) -> impl Future<Output = Result<Vec<T>>> + Send;

    fn fetch_optional<T: Row, S: QueryStatementWriter + SqlxBinder + Sync>(
        &mut self,
        statement: &S,
    ) -> impl Future<Output = Result<Option<T>>> + Send;

    fn fetch_one<T: Row, S: QueryStatementWriter + SqlxBinder + Sync>(
        &mut self,
        statement: &S,
    ) -> impl Future<Output = Result<T>> + Send;
}

/// Implements [`Executor`] by calling the type's inherent methods, named in
/// full so the call can't resolve back to the trait.
macro_rules! impl_executor {
    ($type:ty, $self:ident => $inherent:ty, $receiver:expr) => {
        impl Executor for $type {
            async fn execute<S: QueryStatementWriter + SqlxBinder + Sync>(
                &mut $self,
                statement: &S,
            ) -> Result<u64> {
                <$inherent>::execute($receiver, statement).await
            }

            async fn fetch_all<T: Row, S: QueryStatementWriter + SqlxBinder + Sync>(
                &mut $self,
                statement: &S,
            ) -> Result<Vec<T>> {
                <$inherent>::fetch_all($receiver, statement).await
            }

            async fn fetch_optional<T: Row, S: QueryStatementWriter + SqlxBinder + Sync>(
                &mut $self,
                statement: &S,
            ) -> Result<Option<T>> {
                <$inherent>::fetch_optional($receiver, statement).await
            }

            async fn fetch_one<T: Row, S: QueryStatementWriter + SqlxBinder + Sync>(
                &mut $self,
                statement: &S,
            ) -> Result<T> {
                <$inherent>::fetch_one($receiver, statement).await
            }
        }
    };
}

impl_executor!(&Database, self => Database, *self);
impl_executor!(Transaction, self => Transaction, self);
