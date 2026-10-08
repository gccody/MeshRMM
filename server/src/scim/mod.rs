//! SCIM 2.0 (RFC 7643, 7644): an identity provider creates, updates,
//! disables and deletes users, and manages groups whose members hold a role
//! an administrator maps to the group.
//!
//! The provider authenticates with a bearer token an administrator creates.
//! A user's `userName` is their email address, which is how MeshRMM
//! identifies people.
pub mod filter;
mod groups;
pub mod patch;
mod users;

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
    db::tables::ScimTokens,
    http::{ApiError, AppState, client_ip::ClientIp},
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
            .map_err(|error| ScimError::bad_request("invalidSyntax", error.to_string()))
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

/// Drops top-level attributes the client didn't ask for. `id` and
/// `schemas` are always returned.
fn project(mut resource: Value, attributes: Option<&str>, excluded: Option<&str>) -> Value {
    let wanted = names(attributes);
    let excluded = names(excluded);
    if let Some(object) = resource.as_object_mut() {
        object.retain(|key, _| {
            let key = key.to_ascii_lowercase();
            if key == "id" || key == "schemas" {
                return true;
            }
            (wanted.is_empty() || wanted.contains(&key)) && !excluded.contains(&key)
        });
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
}

/// Filters, pages and projects `resources` into a list response.
fn list_response(resources: Vec<Value>, query: &ListQuery) -> Result<Response, ScimError> {
    let filter = query
        .filter
        .as_deref()
        .map(str::trim)
        .filter(|filter| !filter.is_empty())
        .map(filter::parse)
        .transpose()
        .map_err(|error| ScimError::bad_request("invalidFilter", error))?;
    let matching = resources
        .into_iter()
        .filter(|resource| {
            filter
                .as_ref()
                .is_none_or(|filter| filter::matches(resource, filter))
        })
        .collect::<Vec<_>>();
    let total = matching.len();
    let start_index = Ord::max(query.start_index.unwrap_or(1), 1);
    let count = query
        .count
        .map_or(DEFAULT_PAGE, |count| usize::try_from(count).unwrap_or(0))
        .min(MAX_PAGE);
    let page = matching
        .into_iter()
        .skip(usize::try_from(start_index - 1).unwrap_or(usize::MAX))
        .take(count)
        .map(|resource| {
            project(
                resource,
                query.attributes.as_deref(),
                query.excluded_attributes.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    Ok(scim_json(
        StatusCode::OK,
        json!({
            "schemas": [LIST_SCHEMA],
            "totalResults": total,
            "startIndex": start_index,
            "itemsPerPage": page.len(),
            "Resources": page,
        }),
    ))
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
