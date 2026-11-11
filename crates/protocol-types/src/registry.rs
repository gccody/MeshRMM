//! The registry tool's wire types: a Windows device's registry, browsed and
//! changed from the website as SYSTEM. See docs/remote-tools.md#registry.
//!
//! Requests and answers are [`crate::ToolOperation`]s and
//! [`crate::ToolReply`]s like the other tools'. A key's listing carries its
//! subkeys' names and its values, each value's data cut to
//! [`MAX_LISTED_VALUE_BYTES`]; a value too long for the listing is read
//! whole with [`crate::ToolOperation::GetRegistryValue`] before it is
//! edited.
use serde::{Deserialize, Serialize};

/// The longest key path either side accepts, in bytes of UTF-8.
pub const MAX_REGISTRY_PATH_BYTES: usize = 4096;
/// The longest key name Windows allows, in characters.
pub const MAX_REGISTRY_KEY_NAME_CHARS: usize = 255;
/// The longest value name either side accepts, in characters: Windows'
/// own limit.
pub const MAX_REGISTRY_VALUE_NAME_CHARS: usize = 16_383;
/// The most data a value read whole or written carries, in bytes as the
/// registry stores it. Microsoft advises keeping values far smaller; a
/// larger one is listed, cut short, and can't be edited here.
pub const MAX_REGISTRY_VALUE_BYTES: usize = 256 * 1024;
/// The most data a listing carries for one value, in bytes as the registry
/// stores it.
pub const MAX_LISTED_VALUE_BYTES: usize = 2048;
/// The most subkeys one listing carries.
pub const MAX_LISTED_SUBKEYS: usize = 50_000;
/// The most values one listing carries.
pub const MAX_LISTED_VALUES: usize = 5000;
/// The most bytes of JSON one listing's subkeys and values take in all.
pub const MAX_REGISTRY_LISTING_BYTES: usize = 2 * 1024 * 1024;

/// The root keys the tool browses. `HKEY_CURRENT_USER` is each signed-in
/// user's key under [`RegistryRoot::Users`]; SYSTEM has no user of its own
/// worth browsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RegistryRoot {
    #[serde(rename = "HKLM")]
    LocalMachine,
    #[serde(rename = "HKU")]
    Users,
    #[serde(rename = "HKCR")]
    ClassesRoot,
    #[serde(rename = "HKCC")]
    CurrentConfig,
}

impl RegistryRoot {
    pub const ALL: [Self; 4] = [
        Self::LocalMachine,
        Self::Users,
        Self::ClassesRoot,
        Self::CurrentConfig,
    ];

    /// Its short name, such as `HKLM`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalMachine => "HKLM",
            Self::Users => "HKU",
            Self::ClassesRoot => "HKCR",
            Self::CurrentConfig => "HKCC",
        }
    }
}

/// Which registry view a 64-bit Windows shows: its own, or the one 32-bit
/// programs see, where `SOFTWARE` is `SOFTWARE\WOW6432Node`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RegistryView {
    #[default]
    #[serde(rename = "64")]
    Bits64,
    #[serde(rename = "32")]
    Bits32,
}

impl RegistryView {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bits64 => "64",
            Self::Bits32 => "32",
        }
    }
}

/// A key: its root, its path below it (names joined with `\`, empty for the
/// root itself) and the view it's seen in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RegistryKey {
    pub root: RegistryRoot,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub view: RegistryView,
}

impl RegistryKey {
    /// The key's names below its root.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.path.split('\\').filter(|name| !name.is_empty())
    }

    /// How deep it is below its root: 0 for the root itself.
    pub fn depth(&self) -> usize {
        self.names().count()
    }

    /// Its full name, such as `HKLM\SOFTWARE\Contoso`.
    pub fn display(&self) -> String {
        if self.path.is_empty() {
            self.root.as_str().to_owned()
        } else {
            format!("{}\\{}", self.root.as_str(), self.path)
        }
    }

    /// Whether its path is one Windows could have: names of 1 to 255
    /// characters without control characters, joined with single
    /// backslashes, with none leading or trailing.
    pub fn valid(&self) -> bool {
        self.path.len() <= MAX_REGISTRY_PATH_BYTES
            && (self.path.is_empty()
                || self.path.split('\\').all(|name| {
                    let length = name.chars().count();
                    (1..=MAX_REGISTRY_KEY_NAME_CHARS).contains(&length)
                        && !name.chars().any(char::is_control)
                }))
    }
}

/// Whether `name` is a value name Windows allows. The empty name is the
/// key's default value.
pub fn valid_registry_value_name(name: &str) -> bool {
    name.chars().count() <= MAX_REGISTRY_VALUE_NAME_CHARS && !name.contains('\0')
}

/// A value's data, by its type. Strings are as stored, without their
/// terminating NULs; `REG_EXPAND_SZ` isn't expanded. A `REG_QWORD` travels
/// as a decimal string, since JavaScript's numbers can't hold all of them,
/// and binary data as lowercase hexadecimal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RegistryData {
    /// `REG_SZ`.
    String(String),
    /// `REG_EXPAND_SZ`.
    ExpandString(String),
    /// `REG_MULTI_SZ`: each string, without the empty one that ends it.
    MultiString(Vec<String>),
    /// `REG_DWORD`.
    Dword(u32),
    /// `REG_QWORD`.
    Qword(#[serde(with = "decimal")] u64),
    /// `REG_BINARY`.
    Binary(String),
    /// Any other type, such as `REG_NONE` or `REG_RESOURCE_LIST`: its type
    /// number and data, shown but not edited here.
    Other { kind: u32, hex: String },
}

impl RegistryData {
    /// The type's name, such as `REG_SZ`.
    pub fn type_name(&self) -> String {
        match self {
            Self::String(_) => "REG_SZ".into(),
            Self::ExpandString(_) => "REG_EXPAND_SZ".into(),
            Self::MultiString(_) => "REG_MULTI_SZ".into(),
            Self::Dword(_) => "REG_DWORD".into(),
            Self::Qword(_) => "REG_QWORD".into(),
            Self::Binary(_) => "REG_BINARY".into(),
            Self::Other { kind, .. } => match kind {
                0 => "REG_NONE".into(),
                5 => "REG_DWORD_BIG_ENDIAN".into(),
                6 => "REG_LINK".into(),
                8 => "REG_RESOURCE_LIST".into(),
                9 => "REG_FULL_RESOURCE_DESCRIPTOR".into(),
                10 => "REG_RESOURCE_REQUIREMENTS_LIST".into(),
                other => format!("type {other}"),
            },
        }
    }

    /// Whether the tool may write it: every type but [`Self::Other`], with
    /// hexadecimal that decodes and a multi-string without empty strings,
    /// which would end it early.
    pub fn writable(&self) -> bool {
        match self {
            Self::Other { .. } => false,
            Self::Binary(hex) => decode_hex(hex).is_some(),
            Self::String(text) | Self::ExpandString(text) => !text.contains('\0'),
            Self::MultiString(strings) => strings
                .iter()
                .all(|text| !text.is_empty() && !text.contains('\0')),
            Self::Dword(_) | Self::Qword(_) => true,
        }
    }

    /// How many bytes the registry stores it in, for strings as UTF-16 with
    /// their terminating NULs.
    pub fn stored_bytes(&self) -> usize {
        let units = |text: &str| text.encode_utf16().count();
        match self {
            Self::String(text) | Self::ExpandString(text) => (units(text) + 1) * 2,
            Self::MultiString(strings) => {
                (strings.iter().map(|text| units(text) + 1).sum::<usize>() + 1) * 2
            }
            Self::Dword(_) => 4,
            Self::Qword(_) => 8,
            Self::Binary(hex) | Self::Other { hex, .. } => hex.len() / 2,
        }
    }
}

/// `bytes` as lowercase hexadecimal.
pub fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The bytes `hex` spells, ignoring whitespace; `None` if it isn't
/// hexadecimal pairs.
pub fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = hex
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
}

mod decimal {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Text(String),
            Number(u64),
        }
        match Either::deserialize(deserializer)? {
            Either::Number(value) => Ok(value),
            Either::Text(text) => text.trim().parse().map_err(D::Error::custom),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryValue {
    /// Its name; empty for the key's default value.
    pub name: String,
    #[serde(flatten)]
    pub data: RegistryData,
    /// How many bytes the registry stores it in.
    pub size: u64,
    /// The data was cut short for a listing, or is too large to read whole.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrySubkey {
    pub name: String,
    /// It has subkeys of its own, for the website to offer to open it and
    /// to warn that deleting it deletes them too.
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_subkeys: bool,
}

/// A key's subkeys and values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryListing {
    pub key: RegistryKey,
    pub subkeys: Vec<RegistrySubkey>,
    pub values: Vec<RegistryValue>,
    /// When the key last changed, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<i64>,
    /// More subkeys or values than a listing carries.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

impl RegistryListing {
    /// Keeps the listing within one control message: at most
    /// [`MAX_LISTED_SUBKEYS`] and [`MAX_LISTED_VALUES`], taking together
    /// about [`MAX_REGISTRY_LISTING_BYTES`] of JSON at most, subkeys first.
    pub fn bound(&mut self) {
        let mut budget = MAX_REGISTRY_LISTING_BYTES;
        let mut take = |bytes: usize| {
            let fits = bytes <= budget;
            if fits {
                budget -= bytes;
            }
            fits
        };
        let subkeys = self.subkeys.len();
        let mut kept = 0;
        while kept < subkeys.min(MAX_LISTED_SUBKEYS) && take(subkey_bytes(&self.subkeys[kept])) {
            kept += 1;
        }
        self.truncated |= kept < subkeys;
        self.subkeys.truncate(kept);
        let values = self.values.len();
        let mut kept = 0;
        while kept < values.min(MAX_LISTED_VALUES) && take(value_bytes(&self.values[kept])) {
            kept += 1;
        }
        self.truncated |= kept < values;
        self.values.truncate(kept);
    }
}

/// At least how many bytes `text` takes as a JSON string, escapes and
/// quotes included.
fn json_text(text: &str) -> usize {
    2 + text
        .chars()
        .map(|character| match character {
            '"' | '\\' => 2,
            character if character.is_control() => 6,
            character => character.len_utf8(),
        })
        .sum::<usize>()
}

/// At least how many bytes a subkey takes in a listing's JSON.
fn subkey_bytes(subkey: &RegistrySubkey) -> usize {
    json_text(&subkey.name) + r#"{"name":,"has_subkeys":true},"#.len()
}

/// At least how many bytes a value takes in a listing's JSON.
fn value_bytes(value: &RegistryValue) -> usize {
    let data = match &value.data {
        RegistryData::String(text) | RegistryData::ExpandString(text) => json_text(text),
        RegistryData::MultiString(strings) => {
            2 + strings
                .iter()
                .map(|text| json_text(text) + 1)
                .sum::<usize>()
        }
        RegistryData::Dword(_) | RegistryData::Qword(_) => 22,
        RegistryData::Binary(hex) => json_text(hex),
        RegistryData::Other { hex, .. } => json_text(hex) + 24,
    };
    json_text(&value.name)
        + data
        + r#"{"name":,"type":"expand_string","data":,"size":18446744073709551615,"truncated":true},"#.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_checked_and_named() {
        let key = |path: &str| RegistryKey {
            root: RegistryRoot::LocalMachine,
            path: path.into(),
            view: RegistryView::Bits64,
        };
        assert!(key("").valid());
        assert!(key("SOFTWARE\\Contoso App").valid());
        assert!(!key("SOFTWARE\\").valid());
        assert!(!key("\\SOFTWARE").valid());
        assert!(!key("SOFTWARE\\\\Contoso").valid());
        assert!(!key("a\nb").valid());
        assert!(!key(&"x".repeat(256)).valid());
        assert_eq!(key("SOFTWARE\\Contoso").depth(), 2);
        assert_eq!(key("").display(), "HKLM");
        assert_eq!(key("SOFTWARE").display(), "HKLM\\SOFTWARE");
        assert!(valid_registry_value_name(""));
        assert!(!valid_registry_value_name("a\0"));
    }

    #[test]
    fn data_keeps_its_shape() {
        let value = RegistryValue {
            name: "Big".into(),
            data: RegistryData::Qword(u64::MAX),
            size: 8,
            truncated: false,
        };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "name": "Big", "type": "qword", "data": "18446744073709551615", "size": 8 })
        );
        assert_eq!(
            serde_json::from_value::<RegistryValue>(json).unwrap(),
            value
        );
        let data: RegistryData =
            serde_json::from_value(serde_json::json!({ "type": "qword", "data": 5 })).unwrap();
        assert_eq!(data, RegistryData::Qword(5));
        let key: RegistryKey =
            serde_json::from_value(serde_json::json!({ "root": "HKU" })).unwrap();
        assert_eq!(key.view, RegistryView::Bits64);
        assert_eq!(serde_json::to_value(RegistryView::Bits32).unwrap(), "32");
    }

    #[test]
    fn data_says_whether_it_may_be_written() {
        assert!(RegistryData::Binary("00 ff A0".into()).writable());
        assert!(!RegistryData::Binary("0".into()).writable());
        assert!(!RegistryData::Binary("zz".into()).writable());
        assert!(!RegistryData::MultiString(vec!["a".into(), String::new()]).writable());
        assert!(
            !RegistryData::Other {
                kind: 0,
                hex: String::new()
            }
            .writable()
        );
        assert_eq!(decode_hex("00ffA0"), Some(vec![0, 255, 160]));
        assert_eq!(encode_hex(&[0, 255, 160]), "00ffa0");
        assert_eq!(RegistryData::String("ab".into()).stored_bytes(), 6);
        assert_eq!(
            RegistryData::MultiString(vec!["a".into(), "b".into()]).stored_bytes(),
            10
        );
        assert_eq!(
            RegistryData::Other {
                kind: 8,
                hex: String::new()
            }
            .type_name(),
            "REG_RESOURCE_LIST"
        );
    }

    #[test]
    fn listings_stay_within_a_control_message() {
        let mut listing = RegistryListing {
            key: RegistryKey {
                root: RegistryRoot::ClassesRoot,
                path: String::new(),
                view: RegistryView::Bits64,
            },
            subkeys: (0..MAX_LISTED_SUBKEYS + 10)
                .map(|index| RegistrySubkey {
                    name: format!("{index:0>8}"),
                    has_subkeys: true,
                })
                .collect(),
            values: (0..MAX_LISTED_VALUES)
                .map(|index| RegistryValue {
                    name: format!("v{index}"),
                    data: RegistryData::Binary("ab".repeat(MAX_LISTED_VALUE_BYTES)),
                    size: MAX_LISTED_VALUE_BYTES as u64,
                    truncated: false,
                })
                .collect(),
            modified_at: None,
            truncated: false,
        };
        listing.bound();
        assert!(listing.truncated);
        assert_eq!(listing.subkeys.len(), MAX_LISTED_SUBKEYS);
        assert!(listing.values.len() < MAX_LISTED_VALUES);
        let json = serde_json::to_vec(&crate::ToolReply::RegistryKey(listing)).unwrap();
        assert!(
            json.len() < MAX_REGISTRY_LISTING_BYTES + 1024,
            "{}",
            json.len()
        );
    }
}
