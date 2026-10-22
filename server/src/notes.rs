//! Devices' notes: free text the website shows on each device's page. Every
//! save is a new revision; see `api/notes.rs`.
use std::collections::HashMap;

use sea_query::{Expr, ExprTrait, Func, Query};

use crate::db::{self, Executor, tables::DeviceNotes};

/// The longest notes, in characters.
pub const MAX_LENGTH: usize = 20_000;
/// How many revisions of a device's notes are kept, the current one
/// included.
pub const KEPT_REVISIONS: i64 = 20;

/// The current revision of each device's notes, or only `only`'s. Devices
/// without notes aren't listed.
pub async fn revisions(
    executor: &mut impl Executor,
    only: Option<&str>,
) -> db::Result<HashMap<String, i64>> {
    let mut select = Query::select();
    select
        .column(DeviceNotes::DeviceId)
        .expr(Func::max(Expr::col(DeviceNotes::Revision)))
        .from(DeviceNotes::Table)
        .group_by_col(DeviceNotes::DeviceId);
    if let Some(id) = only {
        select.and_where(Expr::col(DeviceNotes::DeviceId).eq(id));
    }
    let rows: Vec<(String, i64)> = executor.fetch_all(&select).await?;
    Ok(rows.into_iter().collect())
}
