use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;

use crate::http::AppState;

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Health {
    Ok { schema_version: i64 },
    Error { reason: &'static str },
}

/// `GET /healthz`: whether the database answers and has this build's schema.
pub async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    let health = assess(
        state.database.schema_version().await.map_err(|error| {
            tracing::warn!(%error, "health check could not read the schema version");
        }),
        state.database.backend().expected_schema_version(),
    );
    let status = match health {
        Health::Ok { .. } => StatusCode::OK,
        Health::Error { .. } => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(health))
}

fn assess(applied: Result<Option<i64>, ()>, expected: i64) -> Health {
    match applied {
        Err(()) => Health::Error {
            reason: "database_unavailable",
        },
        Ok(Some(version)) if version == expected => Health::Ok {
            schema_version: version,
        },
        Ok(_) => Health::Error {
            reason: "schema_mismatch",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_only_with_the_expected_schema() {
        assert_eq!(assess(Ok(Some(3)), 3), Health::Ok { schema_version: 3 });
        assert_eq!(
            assess(Ok(Some(2)), 3),
            Health::Error {
                reason: "schema_mismatch"
            }
        );
        assert_eq!(
            assess(Ok(None), 3),
            Health::Error {
                reason: "schema_mismatch"
            }
        );
        assert_eq!(
            assess(Err(()), 3),
            Health::Error {
                reason: "database_unavailable"
            }
        );
    }
}
