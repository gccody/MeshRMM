//! SCIM 2.0 (RFC 7643, 7644): an identity provider creates, updates,
//! disables and deletes users, and manages groups whose members hold a role
//! an administrator maps to the group.
//!
//! The provider authenticates with a bearer token an administrator creates.
//! A user's `userName` is their email address, which is how MeshRMM
//! identifies people.
pub mod filter;
pub mod groups;
pub mod patch;
pub mod users;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{FromRequest, FromRequestParts, Path, Request, State},
    http::{HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
    routing::get,
};
use sea_query::{Expr, ExprTrait, Query as Sql};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::{
    audit::Actor,
    auth::limits::ip_key,
    db::{Executor, tables::ScimTokens},
    http::{ApiError, AppState, client_ip::ClientIp},
    rbac,
    secrets::token_hash,
    time::{MINUTE_MS, now_ms},
};

pub const BASE_PATH: &str = "/scim/v2";
const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const CONTENT_TYPE: &str = "application/scim+json";
const DEFAULT_PAGE: usize = 100;
const MAX_PAGE: usize = 1000;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ServiceProviderConfig", get(service_provider_config))
        .route("/ResourceTypes", get(resource_types))
        .route("/ResourceTypes/{name}", get(resource_type))
        .route("/Schemas", get(schemas))
        .route("/Schemas/{id}", get(schema))
        .route("/Users", get(users::list).post(users::create))
        .route(
            "/Users/{id}",
            get(users::get)
                .put(users::replace)
                .patch(users::patch)
                .delete(users::delete),
        )
        .route("/Groups", get(groups::list).post(groups::create))
        .route(
            "/Groups/{id}",
            get(groups::get)
                .put(groups::replace)
                .patch(groups::patch)
                .delete(groups::delete),
        )
        .fallback(|| async { ScimError::not_found("no such SCIM endpoint") })
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
}

/// The public URL of the SCIM endpoint, for the identity provider.
pub fn base_url(state: &AppState) -> String {
    format!("{}{BASE_PATH}", state.config.public_origin())
}

/// An attribute name without its schema URN.
pub fn strip_urn(raw: &str) -> &str {
    let raw = raw.trim();
    if raw.len() > 4
        && raw
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("urn:"))
    {
        raw.rsplit_once(':').map_or(raw, |(_, name)| name)
    } else {
        raw
    }
}

/// An attribute name without its schema URN, lowercased, for comparing.
pub fn attribute_key(raw: &str) -> String {
    strip_urn(raw).to_ascii_lowercase()
}

/// The most characters of the client's own text an error quotes. One path,
/// filter, name or value in a request can be megabytes long, and an error
/// that quoted all of it would send it back and write it to any log the
/// error reaches.
const MAX_QUOTED: usize = 64;

/// `text` as an error quotes it: whole when it is at most [`MAX_QUOTED`]
/// characters, otherwise its first [`MAX_QUOTED`] and `…` to show it was cut.
/// Every error that quotes the client's text quotes it through this.
pub fn excerpt(text: &str) -> String {
    match text.char_indices().nth(MAX_QUOTED) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// Why a body isn't the JSON a request takes. serde's message can quote a
/// whole string from the body, so only an excerpt of it is kept.
fn body_error(error: &serde_json::Error) -> String {
    let message = error.to_string();
    if error.line() == 0 {
        return excerpt(&message);
    }
    let position = format!(" at line {} column {}", error.line(), error.column());
    let message = message.strip_suffix(&position).unwrap_or(&message);
    format!("{}{position}", excerpt(message))
}

/// A SCIM error response.
#[derive(Debug)]
pub struct ScimError {
    status: StatusCode,
    scim_type: Option<&'static str>,
    detail: String,
}

impl ScimError {
    pub fn new(
        status: StatusCode,
        scim_type: Option<&'static str>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status,
            scim_type,
            detail: detail.into(),
        }
    }

    pub fn bad_request(scim_type: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some(scim_type), detail)
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, None, detail)
    }

    pub fn uniqueness(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, Some("uniqueness"), detail)
    }
}

/// Fails unless an enabled administrator would remain; see
/// [`rbac::administrator_remains`]. Call inside the transaction that may
/// remove one, after the change and before committing.
async fn ensure_administrator_remains(executor: &mut impl Executor) -> Result<(), ScimError> {
    if rbac::administrator_remains(executor).await? {
        return Ok(());
    }
    // The request is valid but MeshRMM's state refuses it, which RFC 7644
    // calls `mutability` ("not compatible with ... current state"). The
    // provider's administrator reads the detail in their sync log.
    Err(ScimError::bad_request(
        "mutability",
        "this would leave MeshRMM without an enabled administrator; \
         give another user the Administrator role in MeshRMM first",
    ))
}

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "schemas": [ERROR_SCHEMA],
            "status": self.status.as_u16().to_string(),
            "detail": self.detail,
        });
        if let Some(scim_type) = self.scim_type {
            body["scimType"] = json!(scim_type);
        }
        scim_json(self.status, body)
    }
}

impl From<sqlx::Error> for ScimError {
    fn from(error: sqlx::Error) -> Self {
        ApiError::from(error).into()
    }
}

impl From<anyhow::Error> for ScimError {
    fn from(error: anyhow::Error) -> Self {
        ApiError::from(error).into()
    }
}

impl From<ApiError> for ScimError {
    fn from(error: ApiError) -> Self {
        let status = error.status();
        let scim_type = (status == StatusCode::BAD_REQUEST).then_some("invalidValue");
        Self::new(status, scim_type, error.message())
    }
}

impl From<patch::PatchError> for ScimError {
    fn from(error: patch::PatchError) -> Self {
        Self::bad_request(error.scim_type, error.detail)
    }
}

pub fn scim_json(status: StatusCode, body: Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
    response
}

/// A JSON body, sent as `application/scim+json` or `application/json`.
pub struct ScimBody<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ScimBody<T> {
    type Rejection = ScimError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ScimError> {
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(|error| ScimError::new(error.status(), None, error.body_text()))?;
        serde_json::from_slice(&bytes)
            .map(Self)
            .map_err(|error| ScimError::bad_request("invalidSyntax", body_error(&error)))
    }
}

/// A request authenticated with a SCIM token.
#[derive(Debug, Clone)]
pub struct ScimClient {
    pub actor: Actor,
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    id: String,
    name: String,
    last_used_at: Option<i64>,
}

impl FromRequestParts<AppState> for ScimClient {
    type Rejection = ScimError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ScimError> {
        let ClientIp(ip) = ClientIp::from_request_parts(parts, state)
            .await
            .unwrap_or_else(|never| match never {});
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .split_once(' ')
                    .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                    .map(|(_, token)| token.trim())
            })
            .filter(|token| !token.is_empty());
        let unauthorized = || {
            ScimError::new(
                StatusCode::UNAUTHORIZED,
                None,
                "a valid SCIM bearer token is required",
            )
        };
        let database = &state.database;
        let row: Option<TokenRow> = match token {
            Some(token) => {
                database
                    .fetch_optional(
                        &Sql::select()
                            .columns([ScimTokens::Id, ScimTokens::Name, ScimTokens::LastUsedAt])
                            .from(ScimTokens::Table)
                            .and_where(Expr::col(ScimTokens::TokenHash).eq(token_hash(token)))
                            .and_where(Expr::col(ScimTokens::RevokedAt).is_null())
                            .to_owned(),
                    )
                    .await?
            }
            None => None,
        };
        let Some(row) = row else {
            state
                .auth
                .token_attempts
                .hit(&ip_key(ip))
                .map_err(|seconds| ScimError::from(ApiError::rate_limited(seconds)))?;
            return Err(unauthorized());
        };
        let now = now_ms();
        if row.last_used_at.is_none_or(|used| now - used >= MINUTE_MS) {
            database
                .execute(
                    &Sql::update()
                        .table(ScimTokens::Table)
                        .value(ScimTokens::LastUsedAt, now)
                        .and_where(Expr::col(ScimTokens::Id).eq(row.id.as_str()))
                        .to_owned(),
                )
                .await?;
        }
        Ok(Self {
            actor: Actor {
                user_id: None,
                label: format!("SCIM ({})", row.name),
                ip: Some(ip),
                api_token: None,
            },
        })
    }
}

/// The query of a list request.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    filter: Option<String>,
    #[serde(default, rename = "startIndex")]
    start_index: Option<i64>,
    #[serde(default)]
    count: Option<i64>,
    #[serde(default)]
    attributes: Option<String>,
    #[serde(default, rename = "excludedAttributes")]
    excluded_attributes: Option<String>,
}

/// Which attributes to return, from `attributes` and `excludedAttributes`.
#[derive(Debug, Default, Deserialize)]
pub struct Projection {
    #[serde(default)]
    attributes: Option<String>,
    #[serde(default, rename = "excludedAttributes")]
    excluded_attributes: Option<String>,
}

fn names(list: Option<&str>) -> Vec<String> {
    list.unwrap_or_default()
        .split(',')
        .map(attribute_key)
        .filter(|name| !name.is_empty())
        .map(|name| name.split('.').next().unwrap_or_default().to_owned())
        .collect()
}

/// Whether the top-level attribute `key` (lowercase) is returned. `id` and
/// `schemas` always are.
fn returned(key: &str, wanted: &[String], excluded: &[String]) -> bool {
    if key == "id" || key == "schemas" {
        return true;
    }
    (wanted.is_empty() || wanted.iter().any(|name| name == key))
        && !excluded.iter().any(|name| name == key)
}

/// Drops top-level attributes the client didn't ask for.
fn project(mut resource: Value, attributes: Option<&str>, excluded: Option<&str>) -> Value {
    let wanted = names(attributes);
    let excluded = names(excluded);
    if let Some(object) = resource.as_object_mut() {
        object.retain(|key, _| returned(&key.to_ascii_lowercase(), &wanted, &excluded));
    }
    resource
}

impl Projection {
    fn apply(&self, resource: Value) -> Value {
        project(
            resource,
            self.attributes.as_deref(),
            self.excluded_attributes.as_deref(),
        )
    }

    /// Whether the response has the top-level attribute `key` (lowercase),
    /// so whether it must be read.
    fn returns(&self, key: &str) -> bool {
        returned(
            key,
            &names(self.attributes.as_deref()),
            &names(self.excluded_attributes.as_deref()),
        )
    }
}

/// The matching resources a list returns: from `startIndex`, which counts
/// from 1, at most `count` of them (RFC 7644 section 3.4.2.4).
#[derive(Debug, Clone, Copy)]
struct Page {
    start_index: i64,
    count: usize,
}

impl Page {
    /// How many matching resources come before the page.
    fn offset(self) -> u64 {
        u64::try_from(self.start_index - 1).unwrap_or(u64::MAX)
    }

    fn limit(self) -> u64 {
        u64::try_from(self.count).unwrap_or(u64::MAX)
    }

    /// `totalResults`, when the page read shows it: a page with fewer than
    /// `count` resources holds the last of them, unless it starts past the
    /// end. Otherwise it takes a count.
    fn total(self, read: usize) -> Option<usize> {
        let before = usize::try_from(self.offset()).ok()?;
        (read < self.count && (read > 0 || before == 0)).then(|| before + read)
    }

    /// Cuts the page from every matching resource.
    fn cut<T>(self, matching: impl IntoIterator<Item = T>) -> Vec<T> {
        matching
            .into_iter()
            .skip(usize::try_from(self.offset()).unwrap_or(usize::MAX))
            .take(self.count)
            .collect()
    }
}

impl ListQuery {
    fn filter(&self) -> Result<Option<filter::Filter>, ScimError> {
        self.filter
            .as_deref()
            .map(str::trim)
            .filter(|filter| !filter.is_empty())
            .map(filter::parse)
            .transpose()
            .map_err(|error| ScimError::bad_request("invalidFilter", error))
    }

    /// Without `count`, a page of [`DEFAULT_PAGE`]; never more than
    /// [`MAX_PAGE`]. A negative `count` is 0, which returns only
    /// `totalResults`.
    fn page(&self) -> Page {
        Page {
            start_index: Ord::max(self.start_index.unwrap_or(1), 1),
            count: self
                .count
                .map_or(DEFAULT_PAGE, |count| usize::try_from(count).unwrap_or(0))
                .min(MAX_PAGE),
        }
    }

    fn projection(&self) -> Projection {
        Projection {
            attributes: self.attributes.clone(),
            excluded_attributes: self.excluded_attributes.clone(),
        }
    }
}

/// Whose memberships to read: a user's groups, or a group's members.
#[derive(Debug, Clone, Copy)]
pub enum Whose<'a, K> {
    /// These resources', by ID.
    Ids(&'a [String]),
    /// Those of every resource the conditions select.
    Matching(&'a [K]),
}

/// A list response of `total` matching resources, of which `page` are
/// returned, projected as the query asks.
fn list_page(total: usize, page: Page, resources: Vec<Value>, query: &ListQuery) -> Response {
    let projection = query.projection();
    let resources = resources
        .into_iter()
        .map(|resource| projection.apply(resource))
        .collect::<Vec<_>>();
    scim_json(
        StatusCode::OK,
        json!({
            "schemas": [LIST_SCHEMA],
            "totalResults": total,
            "startIndex": page.start_index,
            "itemsPerPage": resources.len(),
            "Resources": resources,
        }),
    )
}

/// Filters, pages and projects `resources` into a list response.
fn list_response(resources: Vec<Value>, query: &ListQuery) -> Result<Response, ScimError> {
    let filter = query.filter()?;
    let matching = resources
        .into_iter()
        .filter(|resource| {
            filter
                .as_ref()
                .is_none_or(|filter| filter::matches(resource, filter))
        })
        .collect::<Vec<_>>();
    let page = query.page();
    Ok(list_page(matching.len(), page, page.cut(matching), query))
}

/// Unix milliseconds as an RFC 3339 time.
fn timestamp(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn meta(
    state: &AppState,
    kind: &str,
    collection: &str,
    id: &str,
    created: i64,
    modified: i64,
) -> Value {
    json!({
        "resourceType": kind,
        "created": timestamp(created),
        "lastModified": timestamp(modified),
        "location": format!("{}/{collection}/{id}", base_url(state)),
    })
}

/// A string attribute, ignoring the case of its name.
fn text(resource: &Value, key: &str) -> Option<String> {
    filter::member(resource, key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// A boolean attribute; Microsoft Entra ID sends `"True"` and `"False"`.
fn boolean(resource: &Value, key: &str) -> Result<Option<bool>, ScimError> {
    match filter::member(resource, key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(Value::String(value)) if value.eq_ignore_ascii_case("true") => Ok(Some(true)),
        Some(Value::String(value)) if value.eq_ignore_ascii_case("false") => Ok(Some(false)),
        Some(_) => Err(ScimError::bad_request(
            "invalidValue",
            format!("{key} must be true or false"),
        )),
    }
}

async fn service_provider_config(State(state): State<AppState>) -> Response {
    scim_json(
        StatusCode::OK,
        json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
            "patch": { "supported": true },
            "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
            "filter": { "supported": true, "maxResults": MAX_PAGE },
            "changePassword": { "supported": false },
            "sort": { "supported": false },
            "etag": { "supported": false },
            "authenticationSchemes": [{
                "type": "oauthbearertoken",
                "name": "Bearer token",
                "description": "A SCIM token an administrator creates on the Authentication page.",
                "primary": true,
            }],
            "meta": {
                "resourceType": "ServiceProviderConfig",
                "location": format!("{}/ServiceProviderConfig", base_url(&state)),
            },
        }),
    )
}

fn resource_type_entries(state: &AppState) -> Vec<Value> {
    [
        ("User", "Users", USER_SCHEMA),
        ("Group", "Groups", GROUP_SCHEMA),
    ]
    .into_iter()
    .map(|(name, endpoint, schema)| {
        json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],
            "id": name,
            "name": name,
            "endpoint": format!("/{endpoint}"),
            "schema": schema,
            "meta": {
                "resourceType": "ResourceType",
                "location": format!("{}/ResourceTypes/{name}", base_url(state)),
            },
        })
    })
    .collect()
}

async fn resource_types(State(state): State<AppState>) -> Result<Response, ScimError> {
    list_response(resource_type_entries(&state), &ListQuery::default())
}

async fn resource_type(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ScimError> {
    resource_type_entries(&state)
        .into_iter()
        .find(|entry| {
            entry["id"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(&name))
        })
        .map(|entry| scim_json(StatusCode::OK, entry))
        .ok_or_else(|| ScimError::not_found("no such resource type"))
}

fn attribute(
    name: &str,
    kind: &str,
    multi_valued: bool,
    required: bool,
    uniqueness: &str,
) -> Value {
    json!({
        "name": name,
        "type": kind,
        "multiValued": multi_valued,
        "required": required,
        "caseExact": false,
        "mutability": "readWrite",
        "returned": "default",
        "uniqueness": uniqueness,
    })
}

fn schema_entries(state: &AppState) -> Vec<Value> {
    let entry = |id: &str, name: &str, attributes: Vec<Value>| {
        json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Schema"],
            "id": id,
            "name": name,
            "attributes": attributes,
            "meta": {
                "resourceType": "Schema",
                "location": format!("{}/Schemas/{id}", base_url(state)),
            },
        })
    };
    vec![
        entry(
            USER_SCHEMA,
            "User",
            vec![
                attribute("userName", "string", false, true, "server"),
                attribute("displayName", "string", false, false, "none"),
                attribute("name", "complex", false, false, "none"),
                attribute("emails", "complex", true, false, "none"),
                attribute("active", "boolean", false, false, "none"),
                attribute("externalId", "string", false, false, "server"),
            ],
        ),
        entry(
            GROUP_SCHEMA,
            "Group",
            vec![
                attribute("displayName", "string", false, true, "server"),
                attribute("members", "complex", true, false, "none"),
                attribute("externalId", "string", false, false, "server"),
            ],
        ),
    ]
}

async fn schemas(State(state): State<AppState>) -> Result<Response, ScimError> {
    list_response(schema_entries(&state), &ListQuery::default())
}

async fn schema(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    schema_entries(&state)
        .into_iter()
        .find(|entry| entry["id"] == id)
        .map(|entry| scim_json(StatusCode::OK, entry))
        .ok_or_else(|| ScimError::not_found("no such schema"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_names_lose_their_urn_and_case() {
        assert_eq!(attribute_key("userName"), "username");
        assert_eq!(attribute_key("abcé"), "abcé");
        assert_eq!(
            attribute_key("urn:ietf:params:scim:schemas:core:2.0:User:name.givenName"),
            "name.givenname"
        );
    }

    #[test]
    fn projection_keeps_id_and_schemas() {
        let resource = json!({ "id": "1", "schemas": [], "displayName": "A", "members": [] });
        assert_eq!(
            project(resource.clone(), None, Some("members")),
            json!({ "id": "1", "schemas": [], "displayName": "A" })
        );
        assert_eq!(
            project(resource, Some("displayName"), None),
            json!({ "id": "1", "schemas": [], "displayName": "A" })
        );
    }

    #[test]
    fn times_are_rfc_3339() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
    }
}
