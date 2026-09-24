//! `/healthz`: whether the Worker runs and D1 has the migrations it needs.
use serde::{Deserialize, Serialize};
use worker::{query, *};

/// The newest D1 migration the Worker's queries rely on. A test keeps it in
/// step with `server/migrations`.
pub(crate) const SCHEMA_MIGRATION: &str = "0012_background_handoff.sql";

#[derive(Debug, PartialEq, Serialize)]
struct Health {
    status: &'static str,
    schema: Schema,
}

#[derive(Debug, PartialEq, Serialize)]
struct Schema {
    expected: &'static str,
    applied: Option<String>,
}

/// D1 ahead of the Worker is healthy: migrations are applied before the Worker
/// that needs them is deployed. Migration names start with a zero-padded
/// number, so they order by name.
fn assess(applied: std::result::Result<Option<String>, ()>) -> (Health, u16) {
    let (status, code) = match &applied {
        Ok(Some(name)) if name.as_str() >= SCHEMA_MIGRATION => ("ok", 200),
        Ok(_) => ("schema_behind", 503),
        Err(()) => ("database_unavailable", 503),
    };
    let health = Health {
        status,
        schema: Schema {
            expected: SCHEMA_MIGRATION,
            applied: applied.ok().flatten(),
        },
    };
    (health, code)
}

pub(crate) async fn health(environment: &Env) -> Result<Response> {
    #[derive(Deserialize)]
    struct Applied {
        name: Option<String>,
    }
    let applied = async {
        let db = environment.d1("DB")?;
        // wrangler records each applied migration file here.
        query!(&db, "SELECT MAX(name) AS name FROM d1_migrations")
            .first::<Applied>(None)
            .await
    }
    .await;
    let applied = match applied {
        Ok(row) => Ok(row.and_then(|row| row.name)),
        Err(error) => {
            console_error!("health check could not read D1 migrations: {error}");
            Err(())
        }
    };
    let (health, code) = assess(applied);
    let response = Response::from_json(&health)?.with_status(code);
    response.headers().set("Cache-Control", "no-store")?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_migration_is_the_newest_file() {
        let newest = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".sql"))
            .max()
            .unwrap();
        assert_eq!(SCHEMA_MIGRATION, newest);
    }

    #[test]
    fn behind_or_unreadable_schemas_are_unhealthy() {
        let status = |applied| {
            let (health, code) = assess(applied);
            (health.status, code)
        };
        assert_eq!(status(Ok(Some(SCHEMA_MIGRATION.into()))), ("ok", 200));
        assert_eq!(status(Ok(Some("0013_next.sql".into()))), ("ok", 200));
        assert_eq!(
            status(Ok(Some("0011_presence_catalog_outbox.sql".into()))),
            ("schema_behind", 503)
        );
        assert_eq!(status(Ok(None)), ("schema_behind", 503));
        assert_eq!(status(Err(())), ("database_unavailable", 503));
        let (health, _) = assess(Ok(Some("0011_x.sql".into())));
        assert_eq!(
            serde_json::to_value(&health).unwrap(),
            serde_json::json!({"status": "schema_behind", "schema": {"expected": SCHEMA_MIGRATION, "applied": "0011_x.sql"}})
        );
    }
}
