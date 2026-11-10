//! Storage on SQLite or PostgreSQL, chosen by the database URL.
//!
//! Queries are built once with `sea-query` and rendered in the connected
//! backend's dialect, so every query runs on both. Row types decode from
//! either backend when their integers are `i64` (PostgreSQL columns are all
//! `BIGINT`) and their booleans are `bool`.
//!
//! SQLite lets one connection write at a time, so it gets one writer and a
//! pool of read-only readers. Every transaction and every statement that
//! isn't a `SELECT` waits its turn for the writer in the pool's queue, instead
//! of polling for SQLite's lock and failing with "database is locked".
//! Readers see each committed write at once (WAL mode).
mod counts;
pub mod tables;

use std::{str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, bail};
use sea_query::{
    DeleteStatement, Expr, ExprTrait, Func, InsertStatement, PostgresQueryBuilder, Query,
    QueryStatementWriter, SelectStatement, SqliteQueryBuilder, UpdateStatement,
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

pub use self::counts::Counts;
use self::{counts::Counters, tables::SqlxMigrations};

static SQLITE_MIGRATIONS: Migrator = sqlx::migrate!("./migrations/sqlite");
static POSTGRES_MIGRATIONS: Migrator = sqlx::migrate!("./migrations/postgres");

/// By default, how long a read, or any query on PostgreSQL, waits for a
/// connection before failing, which a request answers as busy. Opening a
/// connection counts too.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
/// By default, how long a write waits its turn for SQLite's one writer.
/// Waiting in line is normal there: this covers a burst of thousands of
/// one-row writes, and is shorter than an Agent waits for an answer (15 s).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Only another process holding the file's write lock makes SQLite wait on
/// this, since the server writes through one connection.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// SQLite page caches in KiB, the negative `cache_size`. The writer's holds
/// the pages its transactions change; readers read mostly through the memory
/// map, so theirs stay small: 64 MiB plus 8 MiB per reader at most.
const WRITER_CACHE_KIB: i64 = 64 * 1024;
const READER_CACHE_KIB: i64 = 8 * 1024;
/// How much of the file each SQLite connection maps. Mapped pages are the
/// operating system's page cache, shared by every connection, so this is
/// address space rather than memory of each connection's own.
const MMAP_BYTES: i64 = 256 * 1024 * 1024;

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

/// How many connections to open for `url` when the configuration names no
/// number: SQLite's readers, besides its one writer, are twice the CPUs from
/// 4 to 32; PostgreSQL's pool is four times the CPUs from 10 to 50.
pub fn default_connections(url: &str) -> u32 {
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let cpus = u32::try_from(cpus).unwrap_or(u32::MAX);
    if url.starts_with("sqlite:") {
        cpus.saturating_mul(2).clamp(4, 32)
    } else {
        cpus.saturating_mul(4).clamp(10, 50)
    }
}

/// How many connections a [`Database`] opens and how long a query waits for
/// one before failing with [`sqlx::Error::PoolTimedOut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// SQLite's readers, besides its one writer; every connection on
    /// PostgreSQL.
    pub connections: u32,
    /// For a reader, or any connection on PostgreSQL.
    pub acquire_timeout: Duration,
    /// For SQLite's writer.
    pub write_timeout: Duration,
}

impl Limits {
    pub fn new(connections: u32) -> Self {
        Self {
            connections,
            acquire_timeout: ACQUIRE_TIMEOUT,
            write_timeout: WRITE_TIMEOUT,
        }
    }
}

/// A statement [`Database`] runs. Whether it writes picks SQLite's writer or
/// its readers, so only these four kinds exist.
pub trait Statement: QueryStatementWriter + SqlxBinder {
    const WRITES: bool;
}

impl Statement for SelectStatement {
    const WRITES: bool = false;
}

// With or without `RETURNING`.
impl Statement for InsertStatement {
    const WRITES: bool = true;
}

impl Statement for UpdateStatement {
    const WRITES: bool = true;
}

impl Statement for DeleteStatement {
    const WRITES: bool = true;
}

/// A row type that decodes from both backends.
pub trait Row: for<'r> FromRow<'r, SqliteRow> + for<'r> FromRow<'r, PgRow> + Send + Unpin {}

impl<T> Row for T where T: for<'r> FromRow<'r, SqliteRow> + for<'r> FromRow<'r, PgRow> + Send + Unpin
{}

pub type Result<T> = std::result::Result<T, sqlx::Error>;

#[derive(Debug, Clone)]
enum Pool {
    Sqlite {
        /// One connection, so writes queue here rather than in SQLite.
        writer: SqlitePool,
        /// Opened with `query_only`, so a write sent here is an error.
        readers: SqlitePool,
    },
    Postgres(PgPool),
}

/// The pool one statement runs on.
enum Route<'a> {
    Sqlite(&'a SqlitePool),
    Postgres(&'a PgPool),
}

impl Pool {
    fn route<S: Statement>(&self) -> Route<'_> {
        match self {
            Self::Sqlite { writer, .. } if S::WRITES => Route::Sqlite(writer),
            Self::Sqlite { readers, .. } => Route::Sqlite(readers),
            Self::Postgres(pool) => Route::Postgres(pool),
        }
    }
}

/// The connection pools for one database.
#[derive(Debug, Clone)]
pub struct Database {
    pool: Pool,
    counters: Arc<Counters>,
}

/// Runs the same body against the pool a statement routes to, with
/// `$builder` bound to that backend's `sea-query` builder.
macro_rules! dispatch {
    ($target:expr, $conn:ident, $builder:ident => $body:expr) => {
        match $target {
            Route::Sqlite($conn) => {
                let $builder = SqliteQueryBuilder;
                $body
            }
            Route::Postgres($conn) => {
                let $builder = PostgresQueryBuilder;
                $body
            }
        }
    };
}

impl Database {
    /// Connects to `url`: `sqlite://<path>` (created if missing) or
    /// `postgres://...`/`postgresql://...`, with `connections` as
    /// [`Limits::connections`] and the default timeouts.
    pub async fn connect(url: &str, connections: u32) -> anyhow::Result<Self> {
        Self::connect_with(url, Limits::new(connections)).await
    }

    pub async fn connect_with(url: &str, limits: Limits) -> anyhow::Result<Self> {
        let pool = if url.starts_with("sqlite:") {
            // Both pools come from these options, so for `sqlite::memory:`
            // they name the same shared in-memory database.
            let options = SqliteConnectOptions::from_str(url)
                .context("invalid SQLite database URL")?
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .foreign_keys(true)
                .busy_timeout(BUSY_TIMEOUT)
                .pragma("mmap_size", MMAP_BYTES.to_string());
            // A local connection doesn't drop, so testing it on each acquire
            // would only cost a round trip per statement. The writer stays
            // open: an in-memory database lasts while a connection does.
            let writer = SqlitePoolOptions::new()
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
                .test_before_acquire(false)
                .acquire_timeout(limits.write_timeout)
                .connect_with(
                    options
                        .clone()
                        .pragma("cache_size", (-WRITER_CACHE_KIB).to_string()),
                )
                .await
                .context("could not open the SQLite database")?;
            // After the writer, which creates the file and turns on WAL.
            let readers = SqlitePoolOptions::new()
                .max_connections(limits.connections)
                .test_before_acquire(false)
                .acquire_timeout(limits.acquire_timeout)
                .connect_with(
                    options
                        .pragma("cache_size", (-READER_CACHE_KIB).to_string())
                        .pragma("query_only", "ON"),
                )
                .await
                .context("could not open the SQLite database for reading")?;
            Pool::Sqlite { writer, readers }
        } else if url.starts_with("postgres:") || url.starts_with("postgresql:") {
            Pool::Postgres(
                PgPoolOptions::new()
                    .max_connections(limits.connections)
                    .acquire_timeout(limits.acquire_timeout)
                    .connect(url)
                    .await
                    .context("could not connect to PostgreSQL")?,
            )
        } else {
            bail!("database.url must start with sqlite:// or postgres://");
        };
        Ok(Self {
            pool,
            counters: Arc::default(),
        })
    }

    /// What the pool has run since it opened.
    pub fn counts(&self) -> Counts {
        self.counters.counts()
    }

    pub fn backend(&self) -> Backend {
        match self.pool {
            Pool::Sqlite { .. } => Backend::Sqlite,
            Pool::Postgres(_) => Backend::Postgres,
        }
    }

    /// Applies every migration this build ships that the database lacks.
    pub async fn migrate(&self) -> anyhow::Result<()> {
        let migrator = self.backend().migrator();
        match &self.pool {
            Pool::Sqlite { writer, .. } => migrator.run(writer).await,
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
    pub async fn execute<S: Statement>(&self, statement: &S) -> Result<u64> {
        self.counters.write();
        dispatch!(self.pool.route::<S>(), pool, builder => {
            let (sql, values) = render(statement, builder);
            Ok(sqlx::query_with(sql, values).execute(pool).await?.rows_affected())
        })
    }

    pub async fn fetch_all<T: Row, S: Statement>(&self, statement: &S) -> Result<Vec<T>> {
        self.counters.read();
        dispatch!(self.pool.route::<S>(), pool, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_all(pool).await
        })
    }

    pub async fn fetch_optional<T: Row, S: Statement>(&self, statement: &S) -> Result<Option<T>> {
        self.counters.read();
        dispatch!(self.pool.route::<S>(), pool, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_optional(pool).await
        })
    }

    pub async fn fetch_one<T: Row, S: Statement>(&self, statement: &S) -> Result<T> {
        self.counters.read();
        dispatch!(self.pool.route::<S>(), pool, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_one(pool).await
        })
    }

    /// Starts a transaction. Dropping it without [`Transaction::commit`]
    /// rolls it back.
    ///
    /// On SQLite it holds the one writer until it ends, so it must not wait
    /// on a write through `self` meanwhile, and it takes the write lock up
    /// front (`BEGIN IMMEDIATE`) in case another process has the file open.
    pub async fn begin(&self) -> Result<Transaction> {
        self.counters.transaction();
        Ok(Transaction {
            counters: Arc::clone(&self.counters),
            inner: match &self.pool {
                Pool::Sqlite { writer, .. } => {
                    TransactionInner::Sqlite(writer.begin_with("BEGIN IMMEDIATE").await?)
                }
                Pool::Postgres(pool) => TransactionInner::Postgres(pool.begin().await?),
            },
        })
    }

    /// Waits for in-flight queries and closes every connection.
    pub async fn close(&self) {
        match &self.pool {
            Pool::Sqlite { writer, readers } => {
                tokio::join!(writer.close(), readers.close());
            }
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
    counters: Arc<Counters>,
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
    pub async fn execute<S: Statement>(&mut self, statement: &S) -> Result<u64> {
        self.counters.write();
        dispatch_transaction!(&mut self.inner, connection, builder => {
            let (sql, values) = render(statement, builder);
            Ok(sqlx::query_with(sql, values).execute(&mut **connection).await?.rows_affected())
        })
    }

    pub async fn fetch_all<T: Row, S: Statement>(&mut self, statement: &S) -> Result<Vec<T>> {
        self.counters.read();
        dispatch_transaction!(&mut self.inner, connection, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_all(&mut **connection).await
        })
    }

    pub async fn fetch_optional<T: Row, S: Statement>(
        &mut self,
        statement: &S,
    ) -> Result<Option<T>> {
        self.counters.read();
        dispatch_transaction!(&mut self.inner, connection, builder => {
            let (sql, values) = render(statement, builder);
            sqlx::query_as_with(sql, values).fetch_optional(&mut **connection).await
        })
    }

    pub async fn fetch_one<T: Row, S: Statement>(&mut self, statement: &S) -> Result<T> {
        self.counters.read();
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
    fn execute<S: Statement + Sync>(
        &mut self,
        statement: &S,
    ) -> impl Future<Output = Result<u64>> + Send;

    fn fetch_all<T: Row, S: Statement + Sync>(
        &mut self,
        statement: &S,
    ) -> impl Future<Output = Result<Vec<T>>> + Send;

    fn fetch_optional<T: Row, S: Statement + Sync>(
        &mut self,
        statement: &S,
    ) -> impl Future<Output = Result<Option<T>>> + Send;

    fn fetch_one<T: Row, S: Statement + Sync>(
        &mut self,
        statement: &S,
    ) -> impl Future<Output = Result<T>> + Send;
}

/// Implements [`Executor`] by calling the type's inherent methods, named in
/// full so the call can't resolve back to the trait.
macro_rules! impl_executor {
    ($type:ty, $self:ident => $inherent:ty, $receiver:expr) => {
        impl Executor for $type {
            async fn execute<S: Statement + Sync>(
                &mut $self,
                statement: &S,
            ) -> Result<u64> {
                <$inherent>::execute($receiver, statement).await
            }

            async fn fetch_all<T: Row, S: Statement + Sync>(
                &mut $self,
                statement: &S,
            ) -> Result<Vec<T>> {
                <$inherent>::fetch_all($receiver, statement).await
            }

            async fn fetch_optional<T: Row, S: Statement + Sync>(
                &mut $self,
                statement: &S,
            ) -> Result<Option<T>> {
                <$inherent>::fetch_optional($receiver, statement).await
            }

            async fn fetch_one<T: Row, S: Statement + Sync>(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tables::Companies;

    /// Long waits: a busy test machine can take seconds to open a connection.
    fn patient(connections: u32) -> Limits {
        Limits {
            connections,
            acquire_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
        }
    }

    fn insert_company(id: &str) -> InsertStatement {
        Query::insert()
            .into_table(Companies::Table)
            .columns([
                Companies::Id,
                Companies::Name,
                Companies::CreatedAt,
                Companies::UpdatedAt,
            ])
            .values_panic([id.into(), id.into(), 0.into(), 0.into()])
            .to_owned()
    }

    async fn companies(database: &Database) -> i64 {
        let (count,): (i64,) = database
            .fetch_one(
                &Query::select()
                    .expr(Func::count(Expr::col(Companies::Id)))
                    .from(Companies::Table)
                    .to_owned(),
            )
            .await
            .unwrap();
        count
    }

    #[tokio::test]
    async fn a_write_sent_to_a_reader_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("test.db").display());
        let database = Database::connect_with(&url, patient(2)).await.unwrap();
        database.migrate().await.unwrap();
        let Pool::Sqlite { readers, .. } = &database.pool else {
            unreachable!("a SQLite URL opens SQLite");
        };
        let (sql, values) = render(&insert_company("acme"), SqliteQueryBuilder);
        let error = sqlx::query_with(sql, values)
            .execute(readers)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("readonly"),
            "unexpected error: {error}"
        );
        // Through the database it reaches the writer.
        database.execute(&insert_company("acme")).await.unwrap();
        assert_eq!(companies(&database).await, 1);
        database.close().await;
    }

    #[tokio::test]
    async fn readers_and_the_writer_share_an_in_memory_database() {
        let database = Database::connect_with("sqlite::memory:", patient(3))
            .await
            .unwrap();
        database.migrate().await.unwrap();
        database.execute(&insert_company("acme")).await.unwrap();
        // Concurrent reads take several readers, and each sees the write.
        let counts = futures_util::future::join_all((0..6).map(|_| companies(&database))).await;
        assert_eq!(counts, [1; 6]);
        database.close().await;
    }

    #[test]
    fn default_connections_follow_the_cpus_within_bounds() {
        for url in ["sqlite://meshrmm.db", "postgres://db.internal/meshrmm"] {
            let connections = default_connections(url);
            let (floor, cap) = if url.starts_with("sqlite:") {
                (4, 32)
            } else {
                (10, 50)
            };
            assert!((floor..=cap).contains(&connections), "{url}: {connections}");
        }
    }
}
