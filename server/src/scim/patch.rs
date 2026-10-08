//! SCIM PATCH (RFC 7644 section 3.5.2), applied to a resource's JSON. The
//! patched JSON is then read like a PUT, so both share one set of rules.
//!
//! Identity providers differ in how they patch: Okta sends `replace` with a
//! value object, Microsoft Entra ID sends capitalized operations, paths such
//! as `emails[type eq "work"].value`, booleans as strings, and `remove` on
//! `members` with the members to remove as the value. All of these work.
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{
    filter::{self, Filter},
    strip_urn,
};

#[derive(Debug, Deserialize)]
pub struct PatchRequest {
    #[serde(rename = "Operations", alias = "operations")]
    pub operations: Vec<Operation>,
}

#[derive(Debug, Deserialize)]
pub struct Operation {
    pub op: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Add,
    Replace,
    Remove,
}

/// Why a patch can't be applied, with its SCIM error type.
#[derive(Debug, PartialEq, Eq)]
pub struct PatchError {
    pub scim_type: &'static str,
    pub detail: String,
}

fn invalid(scim_type: &'static str, detail: impl Into<String>) -> PatchError {
    PatchError {
        scim_type,
        detail: detail.into(),
    }
}

enum Path {
    /// `name.givenName`
    Attribute(Vec<String>),
    /// `emails[type eq "work"].value`
    Value {
        attribute: String,
        filter: Filter,
        sub_attribute: Option<String>,
    },
}

fn parse_path(raw: &str) -> Result<Path, PatchError> {
    let raw = raw.trim();
    let Some(open) = raw.find('[') else {
        if raw.is_empty() {
            return Err(invalid("invalidPath", "the path is empty"));
        }
        return Ok(Path::Attribute(
            strip_urn(raw).split('.').map(str::to_owned).collect(),
        ));
    };
    let close = raw
        .rfind(']')
        .filter(|close| *close > open)
        .ok_or_else(|| invalid("invalidPath", format!("unbalanced brackets in {raw}")))?;
    let filter = filter::parse(&raw[open + 1..close])
        .map_err(|error| invalid("invalidPath", format!("{raw}: {error}")))?;
    let rest = &raw[close + 1..];
    let sub_attribute = match rest.strip_prefix('.') {
        Some(sub) if !sub.is_empty() => Some(sub.to_owned()),
        None if rest.is_empty() => None,
        _ => return Err(invalid("invalidPath", format!("invalid path {raw}"))),
    };
    Ok(Path::Value {
        attribute: strip_urn(&raw[..open]).to_owned(),
        filter,
        sub_attribute,
    })
}

/// The key in `object` that matches `key` without case, or `key` itself.
fn key_in(object: &Map<String, Value>, key: &str) -> String {
    object
        .keys()
        .find(|name| name.eq_ignore_ascii_case(key))
        .cloned()
        .unwrap_or_else(|| key.to_owned())
}

/// Applies every operation, in order, to `resource` (a JSON object).
pub fn apply(resource: &mut Value, operations: &[Operation]) -> Result<(), PatchError> {
    let object = resource
        .as_object_mut()
        .ok_or_else(|| invalid("invalidValue", "the resource isn't an object"))?;
    for operation in operations {
        let kind = match operation.op.to_ascii_lowercase().as_str() {
            "add" => Kind::Add,
            "replace" => Kind::Replace,
            "remove" => Kind::Remove,
            other => {
                return Err(invalid(
                    "invalidSyntax",
                    format!("unknown operation {other}"),
                ));
            }
        };
        match operation.path.as_deref().map(str::trim) {
            Some(path) if !path.is_empty() => {
                apply_path(object, kind, &parse_path(path)?, operation.value.clone())?;
            }
            _ => {
                if kind == Kind::Remove {
                    return Err(invalid("noTarget", "remove needs a path"));
                }
                let Some(Value::Object(values)) = &operation.value else {
                    return Err(invalid(
                        "invalidValue",
                        "an operation without a path needs an object value",
                    ));
                };
                for (key, value) in values {
                    apply_path(object, kind, &parse_path(key)?, Some(value.clone()))?;
                }
            }
        }
    }
    Ok(())
}

fn apply_path(
    object: &mut Map<String, Value>,
    kind: Kind,
    path: &Path,
    value: Option<Value>,
) -> Result<(), PatchError> {
    match path {
        Path::Attribute(parts) => apply_attribute(object, kind, parts, value),
        Path::Value {
            attribute,
            filter,
            sub_attribute,
        } => apply_value_path(
            object,
            kind,
            attribute,
            filter,
            sub_attribute.as_deref(),
            value,
        ),
    }
}

fn apply_attribute(
    object: &mut Map<String, Value>,
    kind: Kind,
    parts: &[String],
    value: Option<Value>,
) -> Result<(), PatchError> {
    let (last, parents) = parts
        .split_last()
        .ok_or_else(|| invalid("invalidPath", "the path is empty"))?;
    if let Some(parent) = parents.first() {
        let key = key_in(object, parent);
        if kind == Kind::Remove && !object.contains_key(&key) {
            return Ok(());
        }
        let entry = object
            .entry(key)
            .or_insert_with(|| Value::Object(Map::new()));
        return match entry {
            // `emails.value` names that attribute of every element.
            Value::Array(items) => {
                for item in items.iter_mut().filter_map(Value::as_object_mut) {
                    apply_attribute(item, kind, &parts[1..], value.clone())?;
                }
                Ok(())
            }
            Value::Object(child) => apply_attribute(child, kind, &parts[1..], value),
            other => {
                *other = Value::Object(Map::new());
                let child = other.as_object_mut().expect("just made an object");
                apply_attribute(child, kind, &parts[1..], value)
            }
        };
    }
    let target = object;
    let key = key_in(target, last);
    match kind {
        Kind::Remove => {
            match (target.get_mut(&key), value) {
                // Microsoft Entra ID removes group members this way.
                (Some(Value::Array(items)), Some(Value::Array(removed))) => {
                    items.retain(|item| !removed.iter().any(|gone| same_element(item, gone)));
                }
                _ => {
                    target.remove(&key);
                }
            }
            Ok(())
        }
        Kind::Replace => {
            target.insert(key, value.unwrap_or(Value::Null));
            Ok(())
        }
        Kind::Add => {
            let value = value.ok_or_else(|| invalid("invalidValue", "add needs a value"))?;
            match (target.get_mut(&key), value) {
                (Some(Value::Array(items)), Value::Array(added)) => {
                    for item in added {
                        if !items.iter().any(|existing| same_element(existing, &item)) {
                            items.push(item);
                        }
                    }
                }
                (Some(Value::Array(items)), item) => {
                    if !items.iter().any(|existing| same_element(existing, &item)) {
                        items.push(item);
                    }
                }
                (Some(Value::Object(existing)), Value::Object(added)) => {
                    for (name, item) in added {
                        let name = key_in(existing, &name);
                        existing.insert(name, item);
                    }
                }
                (_, value) => {
                    target.insert(key, value);
                }
            }
            Ok(())
        }
    }
}

/// Elements of a multi-valued attribute are the same when their `value`s
/// are, or when they are equal.
fn same_element(a: &Value, b: &Value) -> bool {
    match (filter::member(a, "value"), filter::member(b, "value")) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

fn apply_value_path(
    object: &mut Map<String, Value>,
    kind: Kind,
    attribute: &str,
    element_filter: &Filter,
    sub_attribute: Option<&str>,
    value: Option<Value>,
) -> Result<(), PatchError> {
    let key = key_in(object, attribute);
    let items = match object.get_mut(&key) {
        Some(Value::Array(items)) => items,
        _ if kind == Kind::Remove => return Ok(()),
        _ => {
            object.insert(key.clone(), Value::Array(Vec::new()));
            object
                .get_mut(&key)
                .and_then(Value::as_array_mut)
                .expect("just inserted an array")
        }
    };
    let matching = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            filter::element_matches(item, &attribute.to_ascii_lowercase(), element_filter)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match (kind, sub_attribute) {
        (Kind::Remove, None) => {
            let mut index = 0;
            items.retain(|_| {
                let keep = !matching.contains(&index);
                index += 1;
                keep
            });
        }
        (Kind::Remove, Some(sub)) => {
            for index in matching {
                if let Some(element) = items[index].as_object_mut() {
                    let sub = key_in(element, sub);
                    element.remove(&sub);
                }
            }
        }
        (_, sub) => {
            let value =
                value.ok_or_else(|| invalid("invalidValue", "the operation needs a value"))?;
            if matching.is_empty() {
                // Entra ID sets `emails[type eq "work"].value` on a user
                // with no work email: create the element the filter names.
                let mut element = seed(element_filter);
                match sub {
                    Some(sub) => {
                        element.insert(sub.to_owned(), value);
                    }
                    None => {
                        if let Value::Object(fields) = value {
                            element.extend(fields);
                        }
                    }
                }
                items.push(Value::Object(element));
                return Ok(());
            }
            for index in matching {
                match sub {
                    Some(sub) => {
                        if let Some(element) = items[index].as_object_mut() {
                            let sub = key_in(element, sub);
                            element.insert(sub, value.clone());
                        }
                    }
                    None if kind == Kind::Replace => items[index] = value.clone(),
                    None => {
                        if let (Some(element), Value::Object(fields)) =
                            (items[index].as_object_mut(), &value)
                        {
                            for (name, field) in fields {
                                let name = key_in(element, name);
                                element.insert(name, field.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// The fields a value-path filter pins down with `eq`.
fn seed(element_filter: &Filter) -> Map<String, Value> {
    let mut element = Map::new();
    let mut pending = vec![element_filter];
    while let Some(next) = pending.pop() {
        match next {
            Filter::Compare {
                path,
                op: filter::Op::Eq,
                value,
            } if path.len() == 1 => {
                element.insert(path[0].clone(), value.clone());
            }
            Filter::And(left, right) => {
                pending.push(left);
                pending.push(right);
            }
            _ => {}
        }
    }
    element
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn patch(mut resource: Value, operations: Value) -> Value {
        let request: PatchRequest =
            serde_json::from_value(json!({ "Operations": operations })).unwrap();
        apply(&mut resource, &request.operations).unwrap();
        resource
    }

    #[test]
    fn okta_style_value_objects() {
        let user = json!({ "userName": "a@x.com", "active": true, "name": { "givenName": "A" } });
        let patched = patch(
            user,
            json!([{ "op": "replace", "value": { "active": false, "name.familyName": "B" } }]),
        );
        assert_eq!(patched["active"], false);
        assert_eq!(
            patched["name"],
            json!({ "givenName": "A", "familyName": "B" })
        );
    }

    #[test]
    fn entra_style_paths() {
        let user = json!({ "userName": "a@x.com", "active": true });
        let patched = patch(
            user,
            json!([
                { "op": "Replace", "path": "active", "value": "False" },
                { "op": "Add", "path": "emails[type eq \"work\"].value", "value": "a@y.com" },
                { "op": "Replace", "path": "emails[type eq \"work\"].value", "value": "b@y.com" },
                { "op": "Add", "path": "externalId", "value": "ext" },
            ]),
        );
        assert_eq!(patched["active"], "False");
        assert_eq!(
            patched["emails"],
            json!([{ "type": "work", "value": "b@y.com" }])
        );
        assert_eq!(patched["externalId"], "ext");
    }

    #[test]
    fn group_members_are_added_and_removed() {
        let group = json!({ "displayName": "Techs", "members": [{ "value": "u1" }] });
        let patched = patch(
            group,
            json!([
                { "op": "add", "path": "members", "value": [{ "value": "u2" }, { "value": "u1" }] },
                { "op": "remove", "path": "members[value eq \"u1\"]" },
                { "op": "add", "path": "members", "value": [{ "value": "u3" }, { "value": "u4" }] },
                { "op": "Remove", "path": "members", "value": [{ "value": "u3" }] },
                { "op": "replace", "path": "displayName", "value": "Technicians" },
            ]),
        );
        assert_eq!(
            patched["members"],
            json!([{ "value": "u2" }, { "value": "u4" }])
        );
        assert_eq!(patched["displayName"], "Technicians");
        let cleared = patch(patched, json!([{ "op": "remove", "path": "members" }]));
        assert!(cleared.get("members").is_none());
        let replaced = patch(
            json!({ "members": [{ "value": "u1" }] }),
            json!([{ "op": "replace", "path": "members", "value": [{ "value": "u9" }] }]),
        );
        assert_eq!(replaced["members"], json!([{ "value": "u9" }]));
    }

    #[test]
    fn a_sub_attribute_of_a_list_applies_to_each_element() {
        let user = json!({ "emails": [{ "value": "a@x.com", "type": "work" }] });
        let patched = patch(
            user,
            json!([{ "op": "replace", "path": "emails.value", "value": "b@x.com" }]),
        );
        assert_eq!(
            patched["emails"],
            json!([{ "value": "b@x.com", "type": "work" }])
        );
    }

    #[test]
    fn bad_operations_are_refused() {
        let mut resource = json!({});
        for operation in [
            json!({ "op": "move", "path": "a", "value": 1 }),
            json!({ "op": "remove" }),
            json!({ "op": "replace", "value": "not an object" }),
            json!({ "op": "add", "path": "emails[type eq \"work\"", "value": 1 }),
        ] {
            let operation: Operation = serde_json::from_value(operation).unwrap();
            assert!(apply(&mut resource, &[operation]).is_err());
        }
    }
}
