//! Custom fields: the fields administrators define for devices, companies
//! or both, and the values devices and companies hold. See
//! docs/custom-fields.md.
//!
//! A value is stored as text in its type's canonical form (see
//! [`FieldType`]), checked here whoever sets it: a user in the website or
//! through the API, or a toolbox script through its Agent. A secret field's
//! value is sealed with the instance key instead and leaves the server only
//! when someone allowed to reveals it, or to a script allowed to read it.
//!
//! A device field that is also defined for companies takes its company's
//! value while the device has none of its own.
use std::collections::HashMap;

use meshrmm_protocol_types::{ScriptField, valid_field_key};
use sea_query::{Expr, ExprTrait, OnConflict, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    db::{
        self, Executor,
        tables::{Agents, Companies, CustomFields, FieldValues, Users},
    },
    http::ApiError,
    secrets::InstanceKey,
};

/// The most fields an install defines.
pub const MAX_FIELDS: usize = 200;
pub const MAX_LABEL_CHARS: usize = 80;
pub const MAX_DESCRIPTION_CHARS: usize = 500;
pub const MAX_OPTIONS: usize = 100;
pub const MAX_OPTION_CHARS: usize = 100;
pub const MAX_TEXT_CHARS: usize = 1000;
pub const MAX_MULTILINE_CHARS: usize = 10_000;
pub const MAX_URL_CHARS: usize = 2048;
pub const MAX_SECRET_CHARS: usize = 4096;
/// The most devices one bulk change sets a field on.
pub const MAX_DEVICES_PER_CHANGE: usize = 1000;
/// How much of a value the audit log keeps.
const AUDITED_VALUE_CHARS: usize = 200;

/// What a field holds. Each type's canonical text, as it is stored and as
/// scripts read it:
///
/// - text, multi-line text, URL and dropdown: the text itself;
/// - number: decimal, as Rust formats an `f64` (`42`, `-1.5`);
/// - checkbox: `true` or `false`;
/// - date: `YYYY-MM-DD`;
/// - multi-select: the chosen options in the field's order, one per line;
/// - secret: the text itself, sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Text,
    Multiline,
    Number,
    Checkbox,
    Date,
    Dropdown,
    MultiSelect,
    Url,
    Secret,
}

impl FieldType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Multiline => "multiline",
            Self::Number => "number",
            Self::Checkbox => "checkbox",
            Self::Date => "date",
            Self::Dropdown => "dropdown",
            Self::MultiSelect => "multi_select",
            Self::Url => "url",
            Self::Secret => "secret",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "text" => Self::Text,
            "multiline" => Self::Multiline,
            "number" => Self::Number,
            "checkbox" => Self::Checkbox,
            "date" => Self::Date,
            "dropdown" => Self::Dropdown,
            "multi_select" => Self::MultiSelect,
            "url" => Self::Url,
            "secret" => Self::Secret,
            _ => return None,
        })
    }

    pub fn has_options(self) -> bool {
        matches!(self, Self::Dropdown | Self::MultiSelect)
    }

    /// Whether the device filter can compare values of this type.
    pub fn takes(self, operator: Operator) -> bool {
        match operator {
            Operator::Empty | Operator::NotEmpty => true,
            Operator::Equals => self != Self::Secret && self != Self::MultiSelect,
            Operator::Contains => matches!(
                self,
                Self::Text | Self::Multiline | Self::Url | Self::MultiSelect
            ),
        }
    }
}

/// What toolbox scripts may do with a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptAccess {
    None,
    Read,
    ReadWrite,
}

impl ScriptAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Read => "read",
            Self::ReadWrite => "read_write",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "none" => Self::None,
            "read" => Self::Read,
            "read_write" => Self::ReadWrite,
            _ => return None,
        })
    }
}

/// Whose values a field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliesTo {
    Devices,
    Companies,
    Both,
}

impl AppliesTo {
    pub fn devices(self) -> bool {
        self != Self::Companies
    }

    pub fn companies(self) -> bool {
        self != Self::Devices
    }

    fn of(devices: bool, companies: bool) -> Option<Self> {
        match (devices, companies) {
            (true, true) => Some(Self::Both),
            (true, false) => Some(Self::Devices),
            (false, true) => Some(Self::Companies),
            (false, false) => None,
        }
    }
}

/// A device filter's comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    /// The value is the one given (a dropdown's option, a date, a number,
    /// `true` or `false`; text ignoring case).
    Equals,
    /// Text holds the one given, ignoring case; a multi-select has the
    /// option given.
    Contains,
    Empty,
    NotEmpty,
}

/// A field definition as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Definition {
    pub id: String,
    pub key: String,
    pub label: String,
    pub description: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    /// A dropdown's or multi-select's options, in order; empty otherwise.
    pub options: Vec<String>,
    pub applies_to: AppliesTo,
    pub position: i64,
    pub script_access: ScriptAccess,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct DefinitionRow {
    id: String,
    field_key: String,
    label: String,
    description: String,
    field_type: String,
    options: String,
    applies_to_devices: bool,
    applies_to_companies: bool,
    position: i64,
    script_access: String,
    created_at: i64,
    updated_at: i64,
}

impl DefinitionRow {
    fn into_definition(self) -> Result<Definition, ApiError> {
        let corrupt = || {
            tracing::error!(field_id = self.id, "stored custom field is invalid");
            ApiError::internal()
        };
        Ok(Definition {
            field_type: FieldType::parse(&self.field_type).ok_or_else(corrupt)?,
            options: serde_json::from_str(&self.options).map_err(|_| corrupt())?,
            applies_to: AppliesTo::of(self.applies_to_devices, self.applies_to_companies)
                .ok_or_else(corrupt)?,
            script_access: ScriptAccess::parse(&self.script_access).ok_or_else(corrupt)?,
            id: self.id,
            key: self.field_key,
            label: self.label,
            description: self.description,
            position: self.position,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

fn definition_select() -> sea_query::SelectStatement {
    Query::select()
        .columns([
            CustomFields::Id,
            CustomFields::FieldKey,
            CustomFields::Label,
            CustomFields::Description,
            CustomFields::FieldType,
            CustomFields::Options,
            CustomFields::AppliesToDevices,
            CustomFields::AppliesToCompanies,
            CustomFields::Position,
            CustomFields::ScriptAccess,
            CustomFields::CreatedAt,
            CustomFields::UpdatedAt,
        ])
        .from(CustomFields::Table)
        .order_by(CustomFields::Position, Order::Asc)
        .order_by(CustomFields::Label, Order::Asc)
        .to_owned()
}

/// Every field, in display order.
pub async fn definitions(executor: &mut impl Executor) -> Result<Vec<Definition>, ApiError> {
    let rows: Vec<DefinitionRow> = executor.fetch_all(&definition_select()).await?;
    rows.into_iter()
        .map(DefinitionRow::into_definition)
        .collect()
}

/// The field with this ID or key.
pub async fn definition(
    executor: &mut impl Executor,
    id_or_key: &str,
) -> Result<Option<Definition>, ApiError> {
    let row: Option<DefinitionRow> = executor
        .fetch_optional(
            &definition_select()
                .cond_where(
                    sea_query::Cond::any()
                        .add(Expr::col(CustomFields::Id).eq(id_or_key))
                        .add(Expr::col(CustomFields::FieldKey).eq(id_or_key)),
                )
                .to_owned(),
        )
        .await?;
    row.map(DefinitionRow::into_definition).transpose()
}

/// A definition as it is created or changed, already checked.
#[derive(Debug, Clone)]
pub struct DefinitionInput {
    pub label: String,
    pub description: String,
    pub options: Vec<String>,
    pub applies_to: AppliesTo,
    pub script_access: ScriptAccess,
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::bad_request(message.into()).with_code("invalid_custom_field")
}

/// Checks a new field's key.
pub fn check_key(key: &str) -> Result<(), ApiError> {
    if valid_field_key(key) {
        Ok(())
    } else {
        Err(invalid(
            "a key starts with a lowercase letter and has up to 64 lowercase letters, digits and underscores",
        ))
    }
}

/// Text on one line: trimmed, without control characters.
fn one_line(text: &str, what: &str, max_chars: usize) -> Result<String, ApiError> {
    let text = text.trim();
    if text.chars().count() > max_chars || text.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{what} must be one line of at most {max_chars} characters"
        )));
    }
    Ok(text.to_owned())
}

/// Checks and tidies a definition's label, description and options.
pub fn check_definition(
    field_type: FieldType,
    label: &str,
    description: &str,
    options: Vec<String>,
    applies_to: AppliesTo,
    script_access: ScriptAccess,
) -> Result<DefinitionInput, ApiError> {
    let label = one_line(label, "a label", MAX_LABEL_CHARS)?;
    if label.is_empty() {
        return Err(invalid("a field needs a label"));
    }
    let description = description.trim().replace("\r\n", "\n");
    if description.chars().count() > MAX_DESCRIPTION_CHARS
        || description
            .chars()
            .any(|character| character.is_control() && character != '\n')
    {
        return Err(invalid(format!(
            "a description has at most {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    let mut tidy = Vec::with_capacity(options.len());
    for option in options {
        let option = one_line(&option, "an option", MAX_OPTION_CHARS)?;
        if option.is_empty() {
            return Err(invalid("an option can't be empty"));
        }
        if tidy
            .iter()
            .any(|known: &String| known.to_lowercase() == option.to_lowercase())
        {
            return Err(invalid(format!("the option “{option}” is there twice")));
        }
        tidy.push(option);
    }
    if field_type.has_options() {
        if tidy.is_empty() || tidy.len() > MAX_OPTIONS {
            return Err(invalid(format!(
                "a {} field has 1 to {MAX_OPTIONS} options",
                if field_type == FieldType::Dropdown {
                    "dropdown"
                } else {
                    "multi-select"
                }
            )));
        }
    } else if !tidy.is_empty() {
        return Err(invalid(
            "only dropdown and multi-select fields have options",
        ));
    }
    Ok(DefinitionInput {
        label,
        description,
        options: tidy,
        applies_to,
        script_access,
    })
}

/// Whose value: a device's or a company's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner<'a> {
    Device(&'a str),
    Company(&'a str),
}

impl<'a> Owner<'a> {
    fn table(self) -> FieldValues {
        match self {
            Self::Device(_) => FieldValues::DeviceTable,
            Self::Company(_) => FieldValues::CompanyTable,
        }
    }

    fn column(self) -> FieldValues {
        match self {
            Self::Device(_) => FieldValues::DeviceId,
            Self::Company(_) => FieldValues::CompanyId,
        }
    }

    pub fn id(self) -> &'a str {
        match self {
            Self::Device(id) | Self::Company(id) => id,
        }
    }

    pub fn kind(self) -> &'static str {
        match self {
            Self::Device(_) => "device",
            Self::Company(_) => "company",
        }
    }
}

/// The encryption context of a secret value: which field's, and whose, so
/// a sealed value copied to another row no longer opens.
fn secret_context(field_id: &str, owner: Owner<'_>) -> String {
    format!("custom_field:{field_id}:{}:{}", owner.kind(), owner.id())
}

/// Who changed a value.
#[derive(Debug, Clone, Copy)]
pub enum ChangedBy<'a> {
    User(&'a str),
    /// A toolbox script, through its run.
    Run {
        run_id: &'a str,
        script_name: &'a str,
    },
}

/// Sets `owner`'s value of `field` to `value`, in canonical form, or clears
/// it when `None`. Returns how many rows changed: 0 for clearing a value
/// that wasn't set.
pub async fn store(
    executor: &mut impl Executor,
    key: &InstanceKey,
    field: &Definition,
    owner: Owner<'_>,
    value: Option<&str>,
    by: ChangedBy<'_>,
    now: i64,
) -> db::Result<u64> {
    let Some(value) = value else {
        return executor
            .execute(
                &Query::delete()
                    .from_table(owner.table())
                    .and_where(Expr::col(owner.column()).eq(owner.id()))
                    .and_where(Expr::col(FieldValues::FieldId).eq(field.id.as_str()))
                    .to_owned(),
            )
            .await;
    };
    let (plain, sealed): (Option<&str>, Option<Vec<u8>>) = if field.field_type == FieldType::Secret
    {
        (
            None,
            Some(key.encrypt(&secret_context(&field.id, owner), value.as_bytes())),
        )
    } else {
        (Some(value), None)
    };
    let (user_id, run_id, script) = match by {
        ChangedBy::User(user_id) => (Some(user_id), None, None),
        ChangedBy::Run {
            run_id,
            script_name,
        } => (None, Some(run_id), Some(script_name)),
    };
    executor
        .execute(
            &Query::insert()
                .into_table(owner.table())
                .columns([
                    owner.column(),
                    FieldValues::FieldId,
                    FieldValues::Value,
                    FieldValues::ValueEncrypted,
                    FieldValues::UpdatedAt,
                    FieldValues::UpdatedByUserId,
                    FieldValues::UpdatedByRunId,
                    FieldValues::UpdatedByScript,
                ])
                .values_panic([
                    owner.id().into(),
                    field.id.as_str().into(),
                    plain.into(),
                    sealed.into(),
                    now.into(),
                    user_id.into(),
                    run_id.into(),
                    script.into(),
                ])
                .on_conflict(
                    OnConflict::columns([owner.column(), FieldValues::FieldId])
                        .update_columns([
                            FieldValues::Value,
                            FieldValues::ValueEncrypted,
                            FieldValues::UpdatedAt,
                            FieldValues::UpdatedByUserId,
                            FieldValues::UpdatedByRunId,
                            FieldValues::UpdatedByScript,
                        ])
                        .to_owned(),
                )
                .to_owned(),
        )
        .await
}

/// A stored value, sealed if the field is secret, with who last changed it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Stored {
    pub owner_id: String,
    pub field_id: String,
    pub value: Option<String>,
    pub value_encrypted: Option<Vec<u8>>,
    pub updated_at: i64,
    pub updated_by_user_id: Option<String>,
    pub updated_by_run_id: Option<String>,
    pub updated_by_script: Option<String>,
    /// The user's name, when a user changed it and still exists.
    pub updated_by_name: Option<String>,
}

impl Stored {
    /// The plain value: as stored, or unsealed for a secret field.
    pub fn open(
        &self,
        key: &InstanceKey,
        field: &Definition,
        owner: Owner<'_>,
    ) -> Result<String, ApiError> {
        if let Some(value) = &self.value {
            return Ok(value.clone());
        }
        let sealed = self.value_encrypted.as_deref().unwrap_or_default();
        let plain = key
            .decrypt(&secret_context(&field.id, owner), sealed)
            .and_then(|plain| Ok(String::from_utf8(plain)?))
            .map_err(|error| {
                tracing::error!(field_id = field.id, owner_id = owner.id(), error = ?error, "could not open a secret custom field value");
                ApiError::internal()
            })?;
        Ok(plain)
    }

    pub fn author(&self) -> Author {
        match (&self.updated_by_run_id, &self.updated_by_user_id) {
            (Some(run_id), _) => Author::Script {
                run_id: run_id.clone(),
                script_name: self.updated_by_script.clone().unwrap_or_default(),
            },
            (None, Some(user_id)) => Author::User {
                user_id: user_id.clone(),
                name: self
                    .updated_by_name
                    .clone()
                    .unwrap_or_else(|| "A removed user".to_owned()),
            },
            (None, None) => Author::User {
                user_id: String::new(),
                name: "A removed user".to_owned(),
            },
        }
    }
}

/// Who last changed a value.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Author {
    User { user_id: String, name: String },
    Script { run_id: String, script_name: String },
}

fn stored_select(owner_column: FieldValues, table: FieldValues) -> sea_query::SelectStatement {
    let value = |column| (table, column);
    Query::select()
        .expr_as(Expr::col(value(owner_column)), "owner_id")
        .columns([
            value(FieldValues::FieldId),
            value(FieldValues::Value),
            value(FieldValues::ValueEncrypted),
            value(FieldValues::UpdatedAt),
            value(FieldValues::UpdatedByUserId),
            value(FieldValues::UpdatedByRunId),
            value(FieldValues::UpdatedByScript),
        ])
        .expr_as(
            Expr::cust("COALESCE(NULLIF(users.display_name, ''), users.email)"),
            "updated_by_name",
        )
        .from(table)
        .left_join(
            Users::Table,
            Expr::col((Users::Table, Users::Id)).equals(value(FieldValues::UpdatedByUserId)),
        )
        .to_owned()
}

/// The values `owner` holds, by field ID.
pub async fn stored(
    executor: &mut impl Executor,
    owner: Owner<'_>,
) -> db::Result<HashMap<String, Stored>> {
    let rows: Vec<Stored> = executor
        .fetch_all(
            &stored_select(owner.column(), owner.table())
                .and_where(Expr::col((owner.table(), owner.column())).eq(owner.id()))
                .to_owned(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.field_id.clone(), row))
        .collect())
}

/// Every value of one field, devices' or companies'. A filter reads these
/// with every device: a field has at most one value per device and company.
pub async fn stored_of_field(
    executor: &mut impl Executor,
    field_id: &str,
    companies: bool,
) -> db::Result<Vec<Stored>> {
    let (table, column) = if companies {
        (FieldValues::CompanyTable, FieldValues::CompanyId)
    } else {
        (FieldValues::DeviceTable, FieldValues::DeviceId)
    };
    executor
        .fetch_all(
            &stored_select(column, table)
                .and_where(Expr::col((table, FieldValues::FieldId)).eq(field_id))
                .to_owned(),
        )
        .await
}

fn type_error(field: &Definition, expected: &str) -> String {
    format!("“{}” takes {expected}", field.label)
}

fn checked_text(field: &Definition, text: &str) -> Result<Option<String>, String> {
    let limit = |max: usize| {
        format!(
            "“{}” takes at most {max} characters, on one line",
            field.label
        )
    };
    match field.field_type {
        FieldType::Text | FieldType::Url => {
            let max = if field.field_type == FieldType::Url {
                MAX_URL_CHARS
            } else {
                MAX_TEXT_CHARS
            };
            let text = text.trim();
            if text.chars().count() > max || text.chars().any(char::is_control) {
                return Err(limit(max));
            }
            if text.is_empty() {
                return Ok(None);
            }
            if field.field_type == FieldType::Url {
                let url = url::Url::parse(text).map_err(|_| {
                    type_error(field, "a web address starting with https:// or http://")
                })?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return Err(type_error(
                        field,
                        "a web address starting with https:// or http://",
                    ));
                }
            }
            Ok(Some(text.to_owned()))
        }
        FieldType::Multiline => {
            let text = text.replace("\r\n", "\n");
            let text = text.trim_end();
            if text.chars().count() > MAX_MULTILINE_CHARS
                || text
                    .chars()
                    .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
            {
                return Err(format!(
                    "“{}” takes at most {MAX_MULTILINE_CHARS} characters",
                    field.label
                ));
            }
            Ok((!text.trim().is_empty()).then(|| text.to_owned()))
        }
        FieldType::Secret => {
            if text.chars().count() > MAX_SECRET_CHARS || text.contains('\0') {
                return Err(format!(
                    "“{}” takes at most {MAX_SECRET_CHARS} characters",
                    field.label
                ));
            }
            Ok((!text.is_empty()).then(|| text.to_owned()))
        }
        FieldType::Date => {
            let text = text.trim();
            if text.is_empty() {
                return Ok(None);
            }
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .ok()
                .filter(|date| date.format("%Y-%m-%d").to_string() == text)
                .map(|_| Some(text.to_owned()))
                .ok_or_else(|| type_error(field, "a date as YYYY-MM-DD"))
        }
        FieldType::Dropdown => {
            let text = text.trim();
            if text.is_empty() {
                return Ok(None);
            }
            field
                .options
                .iter()
                .find(|option| option.to_lowercase() == text.to_lowercase())
                .map(|option| Some(option.clone()))
                .ok_or_else(|| {
                    format!(
                        "“{text}” isn't one of the options of “{}”: {}",
                        field.label,
                        field.options.join(", ")
                    )
                })
        }
        FieldType::Number => {
            let text = text.trim();
            if text.is_empty() {
                return Ok(None);
            }
            text.parse::<f64>()
                .ok()
                .filter(|number| number.is_finite())
                .map(|number| Some(canonical_number(number)))
                .ok_or_else(|| type_error(field, "a number"))
        }
        FieldType::Checkbox => match text.trim().to_ascii_lowercase().as_str() {
            "" => Ok(None),
            "true" | "yes" | "1" | "on" => Ok(Some("true".to_owned())),
            "false" | "no" | "0" | "off" => Ok(Some("false".to_owned())),
            _ => Err(type_error(field, "true or false")),
        },
        FieldType::MultiSelect => {
            let chosen = text
                .split('\n')
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            options_in_order(field, &chosen)
        }
    }
}

/// The chosen options in the field's order, each once; `None` for none.
fn options_in_order(field: &Definition, chosen: &[&str]) -> Result<Option<String>, String> {
    if let Some(unknown) = chosen.iter().find(|choice| {
        !field
            .options
            .iter()
            .any(|option| option.to_lowercase() == choice.to_lowercase())
    }) {
        return Err(format!(
            "“{unknown}” isn't one of the options of “{}”: {}",
            field.label,
            field.options.join(", ")
        ));
    }
    let picked = field
        .options
        .iter()
        .filter(|option| {
            chosen
                .iter()
                .any(|choice| choice.to_lowercase() == option.to_lowercase())
        })
        .cloned()
        .collect::<Vec<_>>();
    Ok((!picked.is_empty()).then(|| picked.join("\n")))
}

fn canonical_number(number: f64) -> String {
    // Display never uses an exponent, and writes integers without ".0".
    if number == 0.0 {
        "0".to_owned()
    } else {
        number.to_string()
    }
}

/// Checks a value a script wrote, as text, and returns its canonical form;
/// `None` clears the value.
pub fn from_text(field: &Definition, text: &str) -> Result<Option<String>, ApiError> {
    checked_text(field, text)
        .map_err(|message| ApiError::bad_request(message).with_code("invalid_value"))
}

/// Checks a value from the API, as JSON of the field's type, and returns
/// its canonical form; `null` (and empty text) clears the value.
pub fn from_json(field: &Definition, value: &Value) -> Result<Option<String>, ApiError> {
    let wrong = |expected: &str| {
        ApiError::bad_request(type_error(field, expected)).with_code("invalid_value")
    };
    match (field.field_type, value) {
        (_, Value::Null) => Ok(None),
        (FieldType::Number, Value::Number(number)) => {
            let number = number.as_f64().ok_or_else(|| wrong("a number"))?;
            from_text(field, &canonical_number(number))
        }
        (FieldType::Number | FieldType::Checkbox, Value::String(text)) => from_text(field, text),
        (FieldType::Number, _) => Err(wrong("a number")),
        (FieldType::Checkbox, Value::Bool(checked)) => Ok(Some(checked.to_string())),
        (FieldType::Checkbox, _) => Err(wrong("true or false")),
        (FieldType::MultiSelect, Value::Array(items)) => {
            let chosen = items
                .iter()
                .map(|item| item.as_str().ok_or_else(|| wrong("a list of its options")))
                .collect::<Result<Vec<_>, _>>()?;
            if chosen.iter().any(|choice| choice.contains('\n')) {
                return Err(wrong("a list of its options"));
            }
            options_in_order(field, &chosen)
                .map_err(|message| ApiError::bad_request(message).with_code("invalid_value"))
        }
        (FieldType::MultiSelect, _) => Err(wrong("a list of its options")),
        (_, Value::String(text)) => from_text(field, text),
        (FieldType::Date, _) => Err(wrong("a date as YYYY-MM-DD")),
        _ => Err(wrong("text")),
    }
}

/// A canonical value as the API shows it.
pub fn to_json(field: &Definition, value: &str) -> Value {
    match field.field_type {
        FieldType::Number => value
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| Value::String(value.to_owned()), Value::Number),
        FieldType::Checkbox => Value::Bool(value == "true"),
        FieldType::MultiSelect => Value::Array(
            value
                .split('\n')
                .map(|option| Value::String(option.to_owned()))
                .collect(),
        ),
        _ => Value::String(value.to_owned()),
    }
}

/// What the audit log records of a value: its text, cut short, or for a
/// secret only that it was set.
pub fn audited(field: &Definition, value: Option<&str>) -> Value {
    match value {
        None => Value::Null,
        Some(_) if field.field_type == FieldType::Secret => Value::String("(secret)".to_owned()),
        Some(value) => Value::String(value.chars().take(AUDITED_VALUE_CHARS).collect()),
    }
}

/// A device, as field values need it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeviceRef {
    pub id: String,
    pub name: String,
    pub company_id: Option<String>,
}

/// The device, if it is enrolled and not deleted.
pub async fn device(
    executor: &mut impl Executor,
    device_id: &str,
) -> db::Result<Option<DeviceRef>> {
    executor
        .fetch_optional(
            &Query::select()
                .columns([Agents::Id, Agents::Name, Agents::CompanyId])
                .from(Agents::Table)
                .and_where(Expr::col(Agents::Id).eq(device_id))
                .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
                .to_owned(),
        )
        .await
}

/// The company's name, if it exists.
pub async fn company_name(
    executor: &mut impl Executor,
    company_id: &str,
) -> db::Result<Option<String>> {
    let row: Option<(String,)> = executor
        .fetch_optional(
            &Query::select()
                .column(Companies::Name)
                .from(Companies::Table)
                .and_where(Expr::col(Companies::Id).eq(company_id))
                .to_owned(),
        )
        .await?;
    Ok(row.map(|(name,)| name))
}

/// Where an effective value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Device,
    Company,
}

/// A device's value of each field that applies to devices: its own, or
/// else its company's for a field companies have too.
pub struct Effective<'a> {
    pub field: &'a Definition,
    pub stored: Option<&'a Stored>,
    pub source: Option<Source>,
}

/// The device's own values and its company's, read once.
pub struct DeviceValues {
    pub own: HashMap<String, Stored>,
    pub company: HashMap<String, Stored>,
}

impl DeviceValues {
    pub async fn load(executor: &mut impl Executor, device: &DeviceRef) -> db::Result<Self> {
        let own = stored(executor, Owner::Device(&device.id)).await?;
        let company = match &device.company_id {
            Some(company_id) => stored(executor, Owner::Company(company_id)).await?,
            None => HashMap::new(),
        };
        Ok(Self { own, company })
    }

    pub fn effective<'a>(&'a self, field: &'a Definition) -> Effective<'a> {
        if let Some(stored) = self.own.get(&field.id) {
            return Effective {
                field,
                stored: Some(stored),
                source: Some(Source::Device),
            };
        }
        match self
            .company
            .get(&field.id)
            .filter(|_| field.applies_to.companies())
        {
            Some(stored) => Effective {
                field,
                stored: Some(stored),
                source: Some(Source::Company),
            },
            None => Effective {
                field,
                stored: None,
                source: None,
            },
        }
    }
}

/// The fields a run's script gets on `device_id`: each the script may read,
/// with the device's value (or its company's), and whether it may write it.
/// A device that no longer exists gets none.
pub async fn script_fields(
    executor: &mut impl Executor,
    key: &InstanceKey,
    device_id: &str,
) -> Result<Vec<ScriptField>, ApiError> {
    let fields = definitions(executor)
        .await?
        .into_iter()
        .filter(|field| field.applies_to.devices() && field.script_access != ScriptAccess::None)
        .collect::<Vec<_>>();
    if fields.is_empty() {
        return Ok(Vec::new());
    }
    let Some(device) = device(executor, device_id).await? else {
        return Ok(Vec::new());
    };
    let values = DeviceValues::load(executor, &device).await?;
    let mut granted = Vec::with_capacity(fields.len());
    for field in &fields {
        let effective = values.effective(field);
        let value = match (effective.stored, effective.source) {
            (Some(stored), Some(Source::Device)) => {
                Some(stored.open(key, field, Owner::Device(&device.id))?)
            }
            (Some(stored), Some(Source::Company)) => Some(stored.open(
                key,
                field,
                Owner::Company(device.company_id.as_deref().unwrap_or_default()),
            )?),
            _ => None,
        };
        granted.push(ScriptField {
            key: field.key.clone(),
            value,
            writable: field.script_access == ScriptAccess::ReadWrite,
        });
    }
    Ok(granted)
}

/// Whether a value matches a filter, as canonical text; `None` has no
/// value.
pub fn matches(field: &Definition, value: Option<&str>, operator: Operator, wanted: &str) -> bool {
    match (operator, value) {
        (Operator::Empty, value) => value.is_none(),
        (Operator::NotEmpty, value) => value.is_some(),
        (_, None) => false,
        (Operator::Equals, Some(value)) => match field.field_type {
            FieldType::Number => {
                matches!((value.parse::<f64>(), wanted.trim().parse::<f64>()), (Ok(a), Ok(b)) if a == b)
            }
            FieldType::Checkbox => {
                checked_text(field, wanted).ok().flatten().as_deref() == Some(value)
            }
            _ => value.to_lowercase() == wanted.trim().to_lowercase(),
        },
        (Operator::Contains, Some(value)) => match field.field_type {
            FieldType::MultiSelect => value
                .split('\n')
                .any(|option| option.to_lowercase() == wanted.trim().to_lowercase()),
            _ => value.to_lowercase().contains(&wanted.trim().to_lowercase()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(field_type: FieldType, options: &[&str]) -> Definition {
        Definition {
            id: "f".into(),
            key: "f".into(),
            label: "Field".into(),
            description: String::new(),
            field_type,
            options: options.iter().map(|option| (*option).to_owned()).collect(),
            applies_to: AppliesTo::Both,
            position: 0,
            script_access: ScriptAccess::ReadWrite,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn values_take_their_types_canonical_form() {
        let number = field(FieldType::Number, &[]);
        assert_eq!(from_text(&number, " 42.0 ").unwrap().as_deref(), Some("42"));
        assert_eq!(from_text(&number, "-1.5").unwrap().as_deref(), Some("-1.5"));
        assert_eq!(from_text(&number, "-0").unwrap().as_deref(), Some("0"));
        assert!(from_text(&number, "NaN").is_err());
        assert!(from_text(&number, "12 apples").is_err());
        assert_eq!(
            from_json(&number, &serde_json::json!(7))
                .unwrap()
                .as_deref(),
            Some("7")
        );
        assert!(from_json(&number, &serde_json::json!("7")).is_ok());
        assert!(from_json(&number, &serde_json::json!(true)).is_err());

        let checkbox = field(FieldType::Checkbox, &[]);
        assert_eq!(
            from_text(&checkbox, "Yes").unwrap().as_deref(),
            Some("true")
        );
        assert_eq!(from_text(&checkbox, "0").unwrap().as_deref(), Some("false"));
        assert!(from_text(&checkbox, "maybe").is_err());
        assert_eq!(
            from_json(&checkbox, &serde_json::json!(false))
                .unwrap()
                .as_deref(),
            Some("false")
        );

        let date = field(FieldType::Date, &[]);
        assert_eq!(
            from_text(&date, "2024-02-29").unwrap().as_deref(),
            Some("2024-02-29")
        );
        for wrong in ["2023-02-29", "2024-2-9", "29/02/2024", "2024-13-01"] {
            assert!(from_text(&date, wrong).is_err(), "{wrong}");
        }

        let dropdown = field(FieldType::Dropdown, &["Gold", "Silver"]);
        assert_eq!(
            from_text(&dropdown, "gold").unwrap().as_deref(),
            Some("Gold")
        );
        assert!(from_text(&dropdown, "Bronze").is_err());

        let multi = field(FieldType::MultiSelect, &["A", "B", "C"]);
        assert_eq!(
            from_text(&multi, "c\nA\n\na").unwrap().as_deref(),
            Some("A\nC")
        );
        assert_eq!(
            from_json(&multi, &serde_json::json!(["B", "A"]))
                .unwrap()
                .as_deref(),
            Some("A\nB")
        );
        assert_eq!(from_json(&multi, &serde_json::json!([])).unwrap(), None);
        assert!(from_json(&multi, &serde_json::json!(["D"])).is_err());
        assert_eq!(to_json(&multi, "A\nB"), serde_json::json!(["A", "B"]));

        let url = field(FieldType::Url, &[]);
        assert!(from_text(&url, "https://example.com/x").is_ok());
        for wrong in [
            "example.com",
            "javascript:alert(1)",
            "ftp://example.com",
            "https://",
        ] {
            assert!(from_text(&url, wrong).is_err(), "{wrong}");
        }

        let text = field(FieldType::Text, &[]);
        assert_eq!(from_text(&text, "  hi  ").unwrap().as_deref(), Some("hi"));
        assert_eq!(from_text(&text, "   ").unwrap(), None);
        assert!(from_text(&text, "two\nlines").is_err());
        assert!(from_text(&text, &"x".repeat(MAX_TEXT_CHARS + 1)).is_err());

        let multiline = field(FieldType::Multiline, &[]);
        assert_eq!(
            from_text(&multiline, "one\r\ntwo\n").unwrap().as_deref(),
            Some("one\ntwo")
        );
        assert!(from_text(&multiline, "nul\0").is_err());

        let secret = field(FieldType::Secret, &[]);
        assert_eq!(
            from_text(&secret, " p@ss ").unwrap().as_deref(),
            Some(" p@ss ")
        );
        assert_eq!(from_text(&secret, "").unwrap(), None);
    }

    #[test]
    fn filters_compare_by_type() {
        let text = field(FieldType::Text, &[]);
        assert!(matches(
            &text,
            Some("Veeam Backup"),
            Operator::Contains,
            "veeam"
        ));
        assert!(matches(&text, Some("Veeam"), Operator::Equals, " VEEAM "));
        assert!(!matches(&text, None, Operator::Equals, "x"));
        assert!(matches(&text, None, Operator::Empty, ""));
        assert!(matches(&text, Some("x"), Operator::NotEmpty, ""));
        let number = field(FieldType::Number, &[]);
        assert!(matches(&number, Some("42"), Operator::Equals, "42.0"));
        let checkbox = field(FieldType::Checkbox, &[]);
        assert!(matches(&checkbox, Some("true"), Operator::Equals, "yes"));
        assert!(!matches(&checkbox, Some("false"), Operator::Equals, "true"));
        let multi = field(FieldType::MultiSelect, &["A", "AB"]);
        assert!(matches(&multi, Some("AB"), Operator::Contains, "ab"));
        assert!(!matches(&multi, Some("AB"), Operator::Contains, "a"));
        assert!(!FieldType::Secret.takes(Operator::Equals));
        assert!(FieldType::Secret.takes(Operator::Empty));
        assert!(!FieldType::Number.takes(Operator::Contains));
    }

    #[test]
    fn definitions_need_options_only_where_they_choose() {
        let check = |field_type, options: &[&str]| {
            check_definition(
                field_type,
                "Tier",
                "",
                options.iter().map(|option| (*option).to_owned()).collect(),
                AppliesTo::Devices,
                ScriptAccess::None,
            )
        };
        assert!(check(FieldType::Dropdown, &[]).is_err());
        assert!(
            check(FieldType::Dropdown, &["A", "a"]).is_err(),
            "duplicates"
        );
        assert_eq!(
            check(FieldType::Dropdown, &[" A ", "B"]).unwrap().options,
            ["A", "B"]
        );
        assert!(check(FieldType::Text, &["A"]).is_err());
        assert!(check_key("contract_tier").is_ok());
        assert!(check_key("Contract").is_err());
    }
}
