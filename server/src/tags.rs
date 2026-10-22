//! Tags: free-form labels for grouping devices besides their company. A
//! device can carry many. See `api/tags.rs` for managing them and
//! [`crate::targets`] for using them as targets.
use std::collections::HashMap;

use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Serialize};

use crate::db::{
    self, Executor,
    tables::{Agents, DeviceTags, Tags},
};

pub const MAX_NAME_LENGTH: usize = 60;
/// The most tags one device can carry.
pub const MAX_TAGS_PER_DEVICE: usize = 50;

/// The colors a tag can have. The website has a light and a dark shade of
/// each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Color {
    Gray,
    Red,
    Orange,
    Amber,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
}

impl Color {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gray => "gray",
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Amber => "amber",
            Self::Green => "green",
            Self::Teal => "teal",
            Self::Blue => "blue",
            Self::Purple => "purple",
            Self::Pink => "pink",
        }
    }
}

/// A tag as the website and the event socket see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Tag {
    pub id: String,
    pub name: String,
    /// One of [`Color`]'s names.
    pub color: String,
}

pub fn select() -> sea_query::SelectStatement {
    Query::select()
        .columns([Tags::Id, Tags::Name, Tags::Color])
        .from(Tags::Table)
        .to_owned()
}

/// Every tag, or only `id`.
pub async fn load(executor: &mut impl Executor, id: Option<&str>) -> db::Result<Vec<Tag>> {
    let mut select = select();
    if let Some(id) = id {
        select.and_where(Expr::col(Tags::Id).eq(id));
    }
    let mut tags: Vec<Tag> = executor.fetch_all(&select).await?;
    sort(&mut tags);
    Ok(tags)
}

/// By name ignoring case, then by ID.
pub fn sort(tags: &mut [Tag]) {
    tags.sort_by_cached_key(|tag| (tag.name.to_lowercase(), tag.id.clone()));
}

/// The tags of every device that isn't deleted, or of `only`, by device.
/// Each device's tag IDs are sorted.
pub async fn device_tag_ids(
    executor: &mut impl Executor,
    only: Option<&str>,
) -> db::Result<HashMap<String, Vec<String>>> {
    let mut select = Query::select();
    select
        .columns([
            (DeviceTags::Table, DeviceTags::DeviceId),
            (DeviceTags::Table, DeviceTags::TagId),
        ])
        .from(DeviceTags::Table)
        .inner_join(
            Agents::Table,
            Expr::col((Agents::Table, Agents::Id))
                .equals((DeviceTags::Table, DeviceTags::DeviceId)),
        )
        .and_where(Expr::col((Agents::Table, Agents::DeletionRequestedAt)).is_null())
        .order_by((DeviceTags::Table, DeviceTags::TagId), Order::Asc);
    if let Some(id) = only {
        select.and_where(Expr::col((DeviceTags::Table, DeviceTags::DeviceId)).eq(id));
    }
    let rows: Vec<(String, String)> = executor.fetch_all(&select).await?;
    let mut tags: HashMap<String, Vec<String>> = HashMap::new();
    for (device_id, tag_id) in rows {
        tags.entry(device_id).or_default().push(tag_id);
    }
    Ok(tags)
}
