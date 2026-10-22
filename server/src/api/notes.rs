//! Devices' notes. Each save names the revision it edited; a save over a
//! revision that is no longer the current one is refused, so two people
//! editing at once can't silently overwrite each other. The last
//! [`KEPT_REVISIONS`] revisions are kept as history.
use axum::{
    Json,
    extract::{Path, State},
};
use sea_query::{Expr, ExprTrait, OnConflict, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    agents::parse_id,
    audit::{self, Target},
    auth::Authorized,
    db::{
        Executor,
        tables::{Agents, DeviceNotes, Users},
    },
    http::{ApiError, AppState, JsonBody},
    notes::{KEPT_REVISIONS, MAX_LENGTH},
    rbac::Permission,
    time::now_ms,
};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Revision {
    revision: i64,
    updated_at: i64,
    updated_by_user_id: Option<String>,
    /// The editor's name, or "A removed user".
    updated_by: Option<String>,
    /// The notes' length in characters.
    #[sqlx(skip)]
    length: usize,
    #[serde(skip)]
    body: String,
}

#[derive(Debug, Serialize)]
pub struct Notes {
    /// The current text; empty without notes.
    body: String,
    /// The current revision; 0 without notes.
    revision: i64,
    updated_at: Option<i64>,
    updated_by_user_id: Option<String>,
    updated_by: Option<String>,
    /// The kept revisions, newest first, the current one included.
    history: Vec<Revision>,
}

/// The device's kept revisions, newest first, or only `revision`.
async fn revisions(
    executor: &mut impl Executor,
    device_id: &str,
    revision: Option<i64>,
) -> Result<Vec<Revision>, ApiError> {
    let note = |column| (DeviceNotes::Table, column);
    let mut select = Query::select();
    select
        .columns([
            note(DeviceNotes::Revision),
            note(DeviceNotes::UpdatedAt),
            note(DeviceNotes::UpdatedByUserId),
            note(DeviceNotes::Body),
        ])
        .expr_as(
            Expr::cust("COALESCE(NULLIF(users.display_name, ''), users.email)"),
            "updated_by",
        )
        .from(DeviceNotes::Table)
        .left_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id)).equals(note(DeviceNotes::UpdatedByUserId)),
        )
        .and_where(Expr::col(note(DeviceNotes::DeviceId)).eq(device_id))
        .order_by(note(DeviceNotes::Revision), Order::Desc);
    if let Some(revision) = revision {
        select.and_where(Expr::col(note(DeviceNotes::Revision)).eq(revision));
    }
    let mut rows: Vec<Revision> = executor.fetch_all(&select).await?;
    for row in &mut rows {
        row.length = row.body.chars().count();
        if row.updated_by.is_none() && row.updated_by_user_id.is_some() {
            row.updated_by = Some("A removed user".to_owned());
        }
    }
    Ok(rows)
}

async fn notes(executor: &mut impl Executor, device_id: &str) -> Result<Notes, ApiError> {
    let history = revisions(executor, device_id, None).await?;
    Ok(match history.first().cloned() {
        Some(current) => Notes {
            body: current.body,
            revision: current.revision,
            updated_at: Some(current.updated_at),
            updated_by_user_id: current.updated_by_user_id,
            updated_by: current.updated_by,
            history,
        },
        None => Notes {
            body: String::new(),
            revision: 0,
            updated_at: None,
            updated_by_user_id: None,
            updated_by: None,
            history,
        },
    })
}

/// The device's name, if it is enrolled and not deleted.
async fn device_name(executor: &mut impl Executor, id: &str) -> Result<String, ApiError> {
    let row: Option<(String,)> = executor
        .fetch_optional(
            &Query::select()
                .column(Agents::Name)
                .from(Agents::Table)
                .and_where(Expr::col(Agents::Id).eq(id))
                .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
                .to_owned(),
        )
        .await?;
    row.map(|(name,)| name)
        .ok_or_else(|| ApiError::not_found("Device not found"))
}

/// `GET /v1/agents/{id}/notes`: the device's notes, who last changed them
/// and when, and the kept revisions without their text.
pub async fn get(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
) -> Result<Json<Notes>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let id = parse_id(&id, "device")?;
    let mut database = &state.database;
    device_name(&mut database, &id).await?;
    Ok(Json(notes(&mut database, &id).await?))
}

#[derive(Debug, Serialize)]
pub struct RevisionText {
    #[serde(flatten)]
    revision: Revision,
    body: String,
}

/// `GET /v1/agents/{id}/notes/{revision}`: a kept revision's text.
pub async fn revision(
    State(state): State<AppState>,
    actor: Authorized,
    Path((id, revision)): Path<(String, i64)>,
) -> Result<Json<RevisionText>, ApiError> {
    actor.require(Permission::DevicesView)?;
    let id = parse_id(&id, "device")?;
    let mut database = &state.database;
    device_name(&mut database, &id).await?;
    let found = revisions(&mut database, &id, Some(revision))
        .await?
        .pop()
        .ok_or_else(|| ApiError::not_found("That revision of the notes isn't kept"))?;
    Ok(Json(RevisionText {
        body: found.body.clone(),
        revision: found,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotesEdit {
    body: String,
    /// The revision the edit started from; 0 for a device without notes.
    revision: i64,
}

fn changed_meanwhile() -> ApiError {
    ApiError::conflict("someone else changed these notes since you started editing")
        .with_code("notes_changed")
}

/// `PUT /v1/agents/{id}/notes`: saves the device's notes as a new revision,
/// if `revision` is still the current one. The audit log records the edit
/// and the notes' length, not their text.
pub async fn save(
    State(state): State<AppState>,
    actor: Authorized,
    Path(id): Path<String>,
    JsonBody(edit): JsonBody<NotesEdit>,
) -> Result<Json<Notes>, ApiError> {
    actor.require(Permission::DevicesEdit)?;
    let id = parse_id(&id, "device")?;
    let body = edit.body.replace("\r\n", "\n");
    let length = body.chars().count();
    if length > MAX_LENGTH || body.contains('\0') {
        return Err(
            ApiError::bad_request(format!("notes can be at most {MAX_LENGTH} characters"))
                .with_code("invalid_notes"),
        );
    }
    let mut transaction = state.database.begin().await?;
    let name = device_name(&mut transaction, &id).await?;
    let current = notes(&mut transaction, &id).await?;
    if current.revision != edit.revision {
        return Err(changed_meanwhile());
    }
    if current.body == body {
        return Ok(Json(current));
    }
    let revision = current.revision + 1;
    let now = now_ms();
    // Of two saves of the same revision, only the first inserts.
    let inserted = transaction
        .execute(
            &Query::insert()
                .into_table(DeviceNotes::Table)
                .columns([
                    DeviceNotes::DeviceId,
                    DeviceNotes::Revision,
                    DeviceNotes::Body,
                    DeviceNotes::UpdatedAt,
                    DeviceNotes::UpdatedByUserId,
                ])
                .values_panic([
                    id.as_str().into(),
                    revision.into(),
                    body.as_str().into(),
                    now.into(),
                    actor.user.id.as_str().into(),
                ])
                .on_conflict(
                    OnConflict::columns([DeviceNotes::DeviceId, DeviceNotes::Revision])
                        .do_nothing()
                        .to_owned(),
                )
                .to_owned(),
        )
        .await?;
    if inserted != 1 {
        return Err(changed_meanwhile());
    }
    transaction
        .execute(
            &Query::delete()
                .from_table(DeviceNotes::Table)
                .and_where(Expr::col(DeviceNotes::DeviceId).eq(id.as_str()))
                .and_where(Expr::col(DeviceNotes::Revision).lte(revision - KEPT_REVISIONS))
                .to_owned(),
        )
        .await?;
    audit::record(
        &mut transaction,
        &actor.actor(),
        "agent.notes",
        Target::device(&id),
        json!({
            "name": name,
            "revision": revision,
            "length": length,
            "previous_length": current.body.chars().count(),
        }),
    )
    .await?;
    let saved = notes(&mut transaction, &id).await?;
    transaction.commit().await?;
    state.presence.refresh(&id).await;
    Ok(Json(saved))
}
