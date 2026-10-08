//! Shared setup for the integration tests.
//!
//! Database tests run against SQLite always, and against PostgreSQL when
//! `MESHRMM_TEST_POSTGRES_URL` names a server the tests may create databases
//! on (e.g. `postgres://postgres@127.0.0.1:5432/postgres`). CI sets it; there,
//! a missing URL fails the tests instead of skipping PostgreSQL silently.
#![allow(dead_code)]

use std::path::Path;

use meshrmm_server::{config::Config, db::Database};
use sqlx::{Connection, PgConnection};
use tempfile::TempDir;

pub const POSTGRES_URL_VARIABLE: &str = "MESHRMM_TEST_POSTGRES_URL";

/// A migrated, empty database that is removed when the test calls
/// [`TestDatabase::drop_database`] (PostgreSQL) or the value is dropped
/// (SQLite).
pub struct TestDatabase {
    pub name: &'static str,
    pub url: String,
    pub database: Database,
    postgres: Option<(String, String)>,
    _dir: Option<TempDir>,
}

impl TestDatabase {
    pub async fn drop_database(self) {
        self.database.close().await;
        if let Some((admin_url, name)) = self.postgres {
            let mut admin = PgConnection::connect(&admin_url).await.unwrap();
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE \"{name}\" WITH (FORCE)"
            )))
            .execute(&mut admin)
            .await
            .unwrap();
        }
    }
}

/// The PostgreSQL admin URL, if PostgreSQL tests should run.
pub fn postgres_admin_url() -> Option<String> {
    match std::env::var(POSTGRES_URL_VARIABLE) {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("{POSTGRES_URL_VARIABLE} must be set in CI so PostgreSQL is tested")
        }
        _ => {
            eprintln!("{POSTGRES_URL_VARIABLE} is not set; skipping PostgreSQL");
            None
        }
    }
}

pub async fn sqlite() -> TestDatabase {
    sqlite_with(true).await
}

pub async fn sqlite_with(migrate: bool) -> TestDatabase {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("test.db").display());
    let database = Database::connect(&url, 4).await.unwrap();
    if migrate {
        database.migrate().await.unwrap();
    }
    TestDatabase {
        name: "sqlite",
        url,
        database,
        postgres: None,
        _dir: Some(dir),
    }
}

pub async fn postgres(admin_url: &str) -> TestDatabase {
    postgres_with(admin_url, true).await
}

pub async fn postgres_with(admin_url: &str, migrate: bool) -> TestDatabase {
    let name = format!("meshrmm_test_{}", random_hex(8));
    let mut admin = PgConnection::connect(admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(&mut admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(admin_url).unwrap();
    url.set_path(&name);
    let url = url.to_string();
    let database = Database::connect(&url, 4).await.unwrap();
    if migrate {
        database.migrate().await.unwrap();
    }
    TestDatabase {
        name: "postgres",
        url,
        database,
        postgres: Some((admin_url.to_owned(), name)),
        _dir: None,
    }
}

/// Every backend under test, each freshly migrated.
pub async fn databases() -> Vec<TestDatabase> {
    let mut databases = vec![sqlite().await];
    if let Some(admin_url) = postgres_admin_url() {
        databases.push(postgres(&admin_url).await);
    }
    databases
}

/// A proxy-mode configuration with its data, database and downloads in `dir`.
pub fn config(dir: &Path, extra: &str) -> Config {
    Config::from_toml(&format!(
        r#"
        public_url = "https://rmm.example.com"
        data_dir = "{data}"
        downloads.dir = "{downloads}"
        {extra}
        "#,
        data = dir.join("data").display(),
        downloads = dir.join("downloads").display(),
    ))
    .unwrap()
}

pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0; bytes];
    getrandom::fill(&mut buffer).unwrap();
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}
