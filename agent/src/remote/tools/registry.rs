//! The registry tool on Windows: keys and values browsed and changed as
//! SYSTEM, in the 64-bit view or the one 32-bit programs see.
//!
//! The Agent's own service key, and every key at the top of a root, can't
//! be deleted, and the service key's values can't be changed: that would
//! cut the device off, or leave Windows unable to start.
use meshrmm_protocol::{
    MAX_LISTED_VALUE_BYTES, MAX_REGISTRY_VALUE_BYTES, RegistryData, RegistryKey, RegistryListing,
    RegistryRoot, RegistrySubkey, RegistryValue, RegistryView, ToolError, ToolErrorCode,
    decode_hex, encode_hex,
};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS,
    FILETIME, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::DELETE;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_CONFIG, HKEY_LOCAL_MACHINE, HKEY_USERS,
    KEY_CREATE_SUB_KEY, KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, KEY_READ, KEY_SET_VALUE,
    KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_BINARY, REG_CREATE_KEY_DISPOSITION, REG_DWORD,
    REG_EXPAND_SZ, REG_MULTI_SZ, REG_OPENED_EXISTING_KEY, REG_OPTION_NON_VOLATILE, REG_QWORD,
    REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteKeyExW,
    RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryInfoKeyW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::win32::wide;

/// The longest key name, in UTF-16 units with its NUL.
const KEY_NAME_UNITS: usize = 256;
/// The longest value name, in UTF-16 units with its NUL.
const VALUE_NAME_UNITS: usize = 16_384;
/// The most data read for a value in a listing, in bytes; past it the
/// listing says only that it's too large.
const MAX_READ_BYTES: u32 = 16 * 1024 * 1024;

/// An open key, closed when dropped.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened here and is closed once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn root(root: RegistryRoot) -> HKEY {
    match root {
        RegistryRoot::LocalMachine => HKEY_LOCAL_MACHINE,
        RegistryRoot::Users => HKEY_USERS,
        RegistryRoot::ClassesRoot => HKEY_CLASSES_ROOT,
        RegistryRoot::CurrentConfig => HKEY_CURRENT_CONFIG,
    }
}

fn view(view: RegistryView) -> REG_SAM_FLAGS {
    match view {
        RegistryView::Bits64 => KEY_WOW64_64KEY,
        RegistryView::Bits32 => KEY_WOW64_32KEY,
    }
}

/// `error` about `what` as the website should see it.
fn registry_error(error: WIN32_ERROR, what: &str) -> ToolError {
    if error == ERROR_FILE_NOT_FOUND {
        ToolError::new(ToolErrorCode::NotFound, format!("{what} doesn't exist."))
    } else if error == ERROR_ACCESS_DENIED {
        ToolError::new(
            ToolErrorCode::AccessDenied,
            format!("Access to {what} is denied."),
        )
    } else {
        ToolError::new(
            ToolErrorCode::Failed,
            format!(
                "{what}: {}",
                windows::core::Error::from(error.to_hresult()).message()
            ),
        )
    }
}

fn check(error: WIN32_ERROR, what: &str) -> Result<(), ToolError> {
    if error == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(registry_error(error, what))
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorCode::Invalid, message)
}

fn protected(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorCode::Protected, message)
}

fn checked(key: &RegistryKey) -> Result<(), ToolError> {
    if key.valid() {
        Ok(())
    } else {
        Err(invalid(format!(
            "“{}” isn't a registry key's path.",
            key.display()
        )))
    }
}

impl Key {
    /// Opens `key` with `access`.
    fn open(key: &RegistryKey, access: REG_SAM_FLAGS) -> Result<Self, ToolError> {
        Self::open_path(root(key.root), &key.path, access | view(key.view))
            .map_err(|error| registry_error(error, &key.display()))
    }

    fn open_path(parent: HKEY, path: &str, access: REG_SAM_FLAGS) -> Result<Self, WIN32_ERROR> {
        let path = wide(path);
        let mut key = HKEY::default();
        // SAFETY: the path is NUL-terminated and `key` is writable.
        let error = unsafe { RegOpenKeyExW(parent, PCWSTR(path.as_ptr()), None, access, &mut key) };
        if error == ERROR_SUCCESS {
            Ok(Self(key))
        } else {
            Err(error)
        }
    }

    /// Its subkeys and values, and when it last changed.
    fn info(&self) -> Result<(u32, u32, FILETIME), WIN32_ERROR> {
        let (mut subkeys, mut values, mut written) = (0u32, 0u32, FILETIME::default());
        // SAFETY: every pointer passed is writable or null.
        let error = unsafe {
            RegQueryInfoKeyW(
                self.0,
                None,
                None,
                None,
                Some(&mut subkeys),
                None,
                None,
                Some(&mut values),
                None,
                None,
                None,
                Some(&mut written),
            )
        };
        if error == ERROR_SUCCESS {
            Ok((subkeys, values, written))
        } else {
            Err(error)
        }
    }

    /// The name of subkey `index`, or `None` past the last.
    fn subkey(&self, index: u32) -> Result<Option<String>, WIN32_ERROR> {
        let mut name = [0u16; KEY_NAME_UNITS];
        let mut length = name.len() as u32;
        // SAFETY: the buffer holds `length` units.
        let error = unsafe {
            RegEnumKeyExW(
                self.0,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut length,
                None,
                None,
                None,
                None,
            )
        };
        match error {
            ERROR_SUCCESS => Ok(Some(String::from_utf16_lossy(&name[..length as usize]))),
            ERROR_NO_MORE_ITEMS => Ok(None),
            error => Err(error),
        }
    }

    /// The name, type and size of value `index`, or `None` past the last.
    fn value_at(
        &self,
        index: u32,
        name: &mut [u16],
    ) -> Result<Option<(String, u32, u32)>, WIN32_ERROR> {
        let mut length = name.len() as u32;
        let (mut kind, mut size) = (0u32, 0u32);
        // SAFETY: the buffer holds `length` units; a null data pointer asks
        // only for the size.
        let error = unsafe {
            RegEnumValueW(
                self.0,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut length,
                None,
                Some(&mut kind),
                None,
                Some(&mut size),
            )
        };
        match error {
            ERROR_SUCCESS => Ok(Some((
                String::from_utf16_lossy(&name[..length as usize]),
                kind,
                size,
            ))),
            ERROR_NO_MORE_ITEMS => Ok(None),
            error => Err(error),
        }
    }

    /// The type and bytes of value `name`, if it has at most `limit` bytes;
    /// otherwise its type and size alone.
    fn read(&self, name: &str, limit: u32) -> Result<(u32, u32, Option<Vec<u8>>), WIN32_ERROR> {
        let name = wide(name);
        loop {
            let (mut kind, mut size) = (REG_VALUE_TYPE::default(), 0u32);
            // SAFETY: a null data pointer asks for the type and size.
            let error = unsafe {
                RegQueryValueExW(
                    self.0,
                    PCWSTR(name.as_ptr()),
                    None,
                    Some(&mut kind),
                    None,
                    Some(&mut size),
                )
            };
            if error != ERROR_SUCCESS {
                return Err(error);
            }
            if size > limit {
                return Ok((kind.0, size, None));
            }
            let mut data = vec![0u8; size as usize];
            let mut read = size;
            // SAFETY: the buffer holds `read` bytes.
            let error = unsafe {
                RegQueryValueExW(
                    self.0,
                    PCWSTR(name.as_ptr()),
                    None,
                    Some(&mut kind),
                    Some(data.as_mut_ptr()),
                    Some(&mut read),
                )
            };
            match error {
                ERROR_SUCCESS => {
                    data.truncate(read as usize);
                    return Ok((kind.0, read, Some(data)));
                }
                // It grew in between: ask again.
                ERROR_MORE_DATA => continue,
                error => return Err(error),
            }
        }
    }

    fn exists(&self, name: &str) -> bool {
        let name = wide(name);
        // SAFETY: null pointers ask for nothing.
        unsafe {
            RegQueryValueExW(self.0, PCWSTR(name.as_ptr()), None, None, None, None) == ERROR_SUCCESS
        }
    }

    fn write(&self, name: &str, kind: u32, data: &[u8]) -> Result<(), WIN32_ERROR> {
        let name = wide(name);
        // SAFETY: the name is NUL-terminated and the data a valid slice.
        let error = unsafe {
            RegSetValueExW(
                self.0,
                PCWSTR(name.as_ptr()),
                None,
                REG_VALUE_TYPE(kind),
                Some(data),
            )
        };
        if error == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn remove(&self, name: &str) -> Result<(), WIN32_ERROR> {
        let name = wide(name);
        // SAFETY: the name is NUL-terminated.
        let error = unsafe { RegDeleteValueW(self.0, PCWSTR(name.as_ptr())) };
        if error == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(error)
        }
    }
}

fn filetime_ms(time: FILETIME) -> Option<i64> {
    let ticks = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    (ticks > 0).then(|| (ticks / 10_000) as i64 - 11_644_473_600_000)
}

/// UTF-16 from the registry's bytes; an odd last byte is dropped.
fn units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// A string as Regedit shows it: up to its first NUL.
fn string(bytes: &[u8]) -> String {
    let units = units(bytes);
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// A multi-string's strings, up to the empty one that ends it.
fn strings(bytes: &[u8]) -> Vec<String> {
    units(bytes)
        .split(|unit| *unit == 0)
        .take_while(|part| !part.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// `bytes` of type `kind` as the wire carries them. A `REG_DWORD` or
/// `REG_QWORD` of the wrong size is shown as other data.
pub(super) fn decode(kind: u32, bytes: &[u8]) -> RegistryData {
    match REG_VALUE_TYPE(kind) {
        REG_SZ => RegistryData::String(string(bytes)),
        REG_EXPAND_SZ => RegistryData::ExpandString(string(bytes)),
        REG_MULTI_SZ => RegistryData::MultiString(strings(bytes)),
        REG_DWORD if bytes.len() == 4 => {
            RegistryData::Dword(u32::from_le_bytes(bytes.try_into().unwrap_or_default()))
        }
        REG_QWORD if bytes.len() == 8 => {
            RegistryData::Qword(u64::from_le_bytes(bytes.try_into().unwrap_or_default()))
        }
        REG_BINARY => RegistryData::Binary(encode_hex(bytes)),
        _ => RegistryData::Other {
            kind,
            hex: encode_hex(bytes),
        },
    }
}

/// `data`'s type and the bytes the registry stores, strings as UTF-16 with
/// their NULs; `None` for data the tool doesn't write.
pub(super) fn encode(data: &RegistryData) -> Option<(u32, Vec<u8>)> {
    if !data.writable() {
        return None;
    }
    let text = |text: &str| -> Vec<u8> {
        text.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    };
    Some(match data {
        RegistryData::String(value) => (REG_SZ.0, text(value)),
        RegistryData::ExpandString(value) => (REG_EXPAND_SZ.0, text(value)),
        RegistryData::MultiString(values) => {
            let mut bytes: Vec<u8> = values.iter().flat_map(|value| text(value)).collect();
            bytes.extend([0, 0]);
            (REG_MULTI_SZ.0, bytes)
        }
        RegistryData::Dword(value) => (REG_DWORD.0, value.to_le_bytes().to_vec()),
        RegistryData::Qword(value) => (REG_QWORD.0, value.to_le_bytes().to_vec()),
        RegistryData::Binary(hex) => (REG_BINARY.0, decode_hex(hex)?),
        RegistryData::Other { .. } => return None,
    })
}

/// A value with its data, cut to `limit` bytes as stored when longer.
fn value(
    name: String,
    kind: u32,
    size: u32,
    bytes: Option<Vec<u8>>,
    limit: usize,
) -> RegistryValue {
    let Some(mut bytes) = bytes else {
        return RegistryValue {
            name,
            data: decode(kind, &[]),
            size: u64::from(size),
            truncated: true,
        };
    };
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    RegistryValue {
        name,
        data: decode(kind, &bytes),
        size: u64::from(size),
        truncated,
    }
}

/// Whether `key` is the Agent's own service key, or holds it.
fn holds_agent_service(key: &RegistryKey) -> bool {
    if key.root != RegistryRoot::LocalMachine {
        return false;
    }
    let names: Vec<&str> = key.names().collect();
    let control_set = |name: &str| {
        name.eq_ignore_ascii_case("CurrentControlSet")
            || name
                .get(..10)
                .is_some_and(|start| start.eq_ignore_ascii_case("ControlSet"))
    };
    let pattern: [&dyn Fn(&str) -> bool; 4] = [
        &|name| name.eq_ignore_ascii_case("SYSTEM"),
        &control_set,
        &|name| name.eq_ignore_ascii_case("Services"),
        &|name| crate::service::is_agent_service(name),
    ];
    names.len() <= pattern.len() && names.iter().zip(pattern).all(|(name, test)| test(name))
}

/// Whether `key` is, or is under, the Agent's own service key.
fn in_agent_service(key: &RegistryKey) -> bool {
    let mut names = key.names();
    let top = RegistryKey {
        root: key.root,
        path: names.by_ref().take(4).collect::<Vec<_>>().join("\\"),
        view: key.view,
    };
    top.depth() == 4 && holds_agent_service(&top)
}

fn agent_key() -> ToolError {
    protected(
        "That is the MeshRMM Agent's own service key, which the registry tool doesn't change: it would cut this device off.",
    )
}

/// `key`'s parent and its own name.
fn split(key: &RegistryKey) -> Option<(RegistryKey, String)> {
    let (parent, name) = match key.path.rsplit_once('\\') {
        Some((parent, name)) => (parent.to_owned(), name.to_owned()),
        None if !key.path.is_empty() => (String::new(), key.path.clone()),
        None => return None,
    };
    Some((
        RegistryKey {
            root: key.root,
            path: parent,
            view: key.view,
        },
        name,
    ))
}

pub(super) fn list_key(key: &RegistryKey) -> Result<RegistryListing, ToolError> {
    checked(key)?;
    let display = key.display();
    let open = Key::open(key, KEY_READ)?;
    let (_, _, written) = open
        .info()
        .map_err(|error| registry_error(error, &display))?;
    let mut subkeys = Vec::new();
    for index in 0.. {
        let Some(name) = open
            .subkey(index)
            .map_err(|error| registry_error(error, &display))?
        else {
            break;
        };
        let has_subkeys = Key::open_path(
            open.0,
            &name,
            KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS | view(key.view),
        )
        .ok()
        .and_then(|child| child.info().ok())
        .is_some_and(|(count, _, _)| count > 0);
        subkeys.push(RegistrySubkey { name, has_subkeys });
    }
    subkeys.sort_by_cached_key(|subkey| subkey.name.to_lowercase());
    let mut values = Vec::new();
    let mut buffer = vec![0u16; VALUE_NAME_UNITS];
    for index in 0.. {
        let Some((name, kind, size)) = open
            .value_at(index, &mut buffer)
            .map_err(|error| registry_error(error, &display))?
        else {
            break;
        };
        let read = open
            .read(&name, MAX_READ_BYTES)
            .unwrap_or((kind, size, None));
        values.push(value(name, read.0, read.1, read.2, MAX_LISTED_VALUE_BYTES));
    }
    values.sort_by_cached_key(|value| value.name.to_lowercase());
    let mut listing = RegistryListing {
        key: key.clone(),
        subkeys,
        values,
        modified_at: filetime_ms(written),
        truncated: false,
    };
    listing.bound();
    Ok(listing)
}

pub(super) fn get_value(key: &RegistryKey, name: &str) -> Result<RegistryValue, ToolError> {
    checked(key)?;
    let open = Key::open(key, KEY_QUERY_VALUE)?;
    let what = value_label(key, name);
    let (kind, size, bytes) = open
        .read(name, MAX_REGISTRY_VALUE_BYTES as u32)
        .map_err(|error| registry_error(error, &what))?;
    Ok(value(
        name.to_owned(),
        kind,
        size,
        bytes,
        MAX_REGISTRY_VALUE_BYTES,
    ))
}

fn value_label(key: &RegistryKey, name: &str) -> String {
    if name.is_empty() {
        format!("The default value of {}", key.display())
    } else {
        format!("The value “{name}” of {}", key.display())
    }
}

pub(super) fn create_key(key: &RegistryKey) -> Result<(), ToolError> {
    checked(key)?;
    let (parent, name) = split(key).ok_or_else(|| invalid("A root key can't be created."))?;
    let parent = Key::open(&parent, KEY_CREATE_SUB_KEY | KEY_ENUMERATE_SUB_KEYS)?;
    if Key::open_path(parent.0, &name, KEY_QUERY_VALUE | view(key.view)).is_ok() {
        return Err(ToolError::new(
            ToolErrorCode::Exists,
            format!("{} already exists.", key.display()),
        ));
    }
    let wide_name = wide(&name);
    let mut created = HKEY::default();
    let mut disposition = REG_CREATE_KEY_DISPOSITION::default();
    // SAFETY: the name is NUL-terminated and the outputs writable.
    let error = unsafe {
        RegCreateKeyExW(
            parent.0,
            PCWSTR(wide_name.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | view(key.view),
            None,
            &mut created,
            Some(&mut disposition),
        )
    };
    check(error, &key.display())?;
    drop(Key(created));
    if disposition == REG_OPENED_EXISTING_KEY {
        return Err(ToolError::new(
            ToolErrorCode::Exists,
            format!("{} already exists.", key.display()),
        ));
    }
    Ok(())
}

pub(super) fn delete_key(key: &RegistryKey, recursive: bool) -> Result<(), ToolError> {
    checked(key)?;
    if key.depth() <= 1 {
        return Err(protected(format!(
            "{} is at the top of its root, which the registry tool doesn't delete: Windows needs it.",
            key.display()
        )));
    }
    if holds_agent_service(key) {
        return Err(agent_key());
    }
    let (parent, name) = split(key).ok_or_else(|| invalid("A root key can't be deleted."))?;
    let display = key.display();
    let target = Key::open(key, KEY_READ | KEY_SET_VALUE | REG_SAM_FLAGS(DELETE.0))?;
    let (subkeys, _, _) = target
        .info()
        .map_err(|error| registry_error(error, &display))?;
    if subkeys > 0 {
        if !recursive {
            return Err(ToolError::new(
                ToolErrorCode::NotEmpty,
                format!("{display} has subkeys; delete it with them, or delete them first."),
            ));
        }
        // SAFETY: a null subkey deletes everything under the open key.
        check(
            unsafe { RegDeleteTreeW(target.0, PCWSTR::null()) },
            &display,
        )?;
    }
    drop(target);
    let parent = Key::open(&parent, KEY_ENUMERATE_SUB_KEYS)?;
    let wide_name = wide(&name);
    // SAFETY: the name is NUL-terminated.
    let error =
        unsafe { RegDeleteKeyExW(parent.0, PCWSTR(wide_name.as_ptr()), view(key.view).0, None) };
    check(error, &display)
}

/// Opens `key` to change its values, unless it's the Agent's service key.
fn open_for_values(key: &RegistryKey) -> Result<Key, ToolError> {
    checked(key)?;
    if in_agent_service(key) {
        return Err(agent_key());
    }
    Key::open(key, KEY_QUERY_VALUE | KEY_SET_VALUE)
}

pub(super) fn set_value(
    key: &RegistryKey,
    name: &str,
    data: &RegistryData,
    create: bool,
) -> Result<RegistryValue, ToolError> {
    let (kind, bytes) = encode(data)
        .ok_or_else(|| invalid(format!("{} data can't be written here.", data.type_name())))?;
    if bytes.len() > MAX_REGISTRY_VALUE_BYTES {
        return Err(invalid(format!(
            "That's {} bytes; the registry tool writes at most {MAX_REGISTRY_VALUE_BYTES}.",
            bytes.len()
        )));
    }
    let open = open_for_values(key)?;
    let what = value_label(key, name);
    if create && open.exists(name) {
        return Err(ToolError::new(
            ToolErrorCode::Exists,
            format!("{what} already exists."),
        ));
    }
    open.write(name, kind, &bytes)
        .map_err(|error| registry_error(error, &what))?;
    let (kind, size, bytes) = open
        .read(name, MAX_REGISTRY_VALUE_BYTES as u32)
        .map_err(|error| registry_error(error, &what))?;
    Ok(value(
        name.to_owned(),
        kind,
        size,
        bytes,
        MAX_REGISTRY_VALUE_BYTES,
    ))
}

pub(super) fn rename_value(key: &RegistryKey, from: &str, to: &str) -> Result<(), ToolError> {
    if from == to {
        return Err(invalid("The new name is the same as the old."));
    }
    let open = open_for_values(key)?;
    let old = value_label(key, from);
    if open.exists(to) {
        return Err(ToolError::new(
            ToolErrorCode::Exists,
            format!("{} already exists.", value_label(key, to)),
        ));
    }
    let (kind, _, bytes) = open
        .read(from, u32::MAX)
        .map_err(|error| registry_error(error, &old))?;
    let bytes = bytes.unwrap_or_default();
    open.write(to, kind, &bytes)
        .map_err(|error| registry_error(error, &value_label(key, to)))?;
    if let Err(error) = open.remove(from) {
        // Leave it as it was.
        let _ = open.remove(to);
        return Err(registry_error(error, &old));
    }
    Ok(())
}

pub(super) fn delete_value(key: &RegistryKey, name: &str) -> Result<(), ToolError> {
    let open = open_for_values(key)?;
    open.remove(name)
        .map_err(|error| registry_error(error, &value_label(key, name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway key under the current user's own key, deleted when the
    /// test ends.
    struct Scratch(RegistryKey);

    impl Scratch {
        fn new(name: &str) -> Self {
            let sid = current_user_sid();
            let key = RegistryKey {
                root: RegistryRoot::Users,
                path: format!(r"{sid}\Software\MeshRMM-test\{name}-{}", std::process::id()),
                view: RegistryView::Bits64,
            };
            let mut parent = String::new();
            for name in key.names() {
                parent = if parent.is_empty() {
                    name.to_owned()
                } else {
                    format!("{parent}\\{name}")
                };
                let _ = create_key(&RegistryKey {
                    root: key.root,
                    path: parent.clone(),
                    view: key.view,
                });
            }
            Self(key)
        }

        fn child(&self, name: &str) -> RegistryKey {
            RegistryKey {
                root: self.0.root,
                path: format!("{}\\{name}", self.0.path),
                view: self.0.view,
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = delete_key(&self.0, true);
            let test = split(&self.0).unwrap().0;
            let _ = delete_key(&test, false);
        }
    }

    fn current_user_sid() -> String {
        let output = std::process::Command::new("whoami")
            .arg("/user")
            .arg("/fo")
            .arg("csv")
            .arg("/nh")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        text.trim()
            .rsplit(',')
            .next()
            .unwrap()
            .trim_matches('"')
            .to_owned()
    }

    #[test]
    fn data_round_trips_through_the_registrys_bytes() {
        for data in [
            RegistryData::String("héllo".into()),
            RegistryData::ExpandString("%SystemRoot%\\x".into()),
            RegistryData::MultiString(vec!["a".into(), "bc".into()]),
            RegistryData::MultiString(vec![]),
            RegistryData::Dword(0xdead_beef),
            RegistryData::Qword(u64::MAX),
            RegistryData::Binary("00ff10".into()),
        ] {
            let (kind, bytes) = encode(&data).unwrap();
            assert_eq!(decode(kind, &bytes), data);
            assert_eq!(bytes.len(), data.stored_bytes(), "{data:?}");
        }
        assert_eq!(
            decode(REG_SZ.0, &[b'a', 0, 0, 0, b'b', 0]),
            RegistryData::String("a".into())
        );
        assert_eq!(
            decode(REG_DWORD.0, &[1, 2]),
            RegistryData::Other {
                kind: REG_DWORD.0,
                hex: "0102".into()
            }
        );
        assert!(
            encode(&RegistryData::Other {
                kind: 0,
                hex: String::new()
            })
            .is_none()
        );
    }

    #[test]
    fn the_agents_service_key_and_the_tops_of_roots_are_protected() {
        let key = |path: &str| RegistryKey {
            root: RegistryRoot::LocalMachine,
            path: path.into(),
            view: RegistryView::Bits64,
        };
        assert!(holds_agent_service(&key(
            r"SYSTEM\CurrentControlSet\Services\MeshRMMAgent"
        )));
        assert!(holds_agent_service(&key(r"system\ControlSet001\services")));
        assert!(!holds_agent_service(&key(
            r"SYSTEM\CurrentControlSet\Services\Spooler"
        )));
        assert!(!holds_agent_service(&key(
            r"SYSTEM\CurrentControlSet\Services\MeshRMMAgent\Parameters"
        )));
        assert!(in_agent_service(&key(
            r"SYSTEM\CurrentControlSet\Services\MeshRMMAgent\Parameters"
        )));
        assert!(!in_agent_service(&key(
            r"SYSTEM\CurrentControlSet\Services"
        )));
        let refused = delete_key(&key("SOFTWARE"), true).unwrap_err();
        assert_eq!(refused.code, ToolErrorCode::Protected);
        let refused = delete_key(&key(r"SYSTEM\CurrentControlSet\Services"), true).unwrap_err();
        assert_eq!(refused.code, ToolErrorCode::Protected);
        let refused = delete_value(
            &key(r"SYSTEM\CurrentControlSet\Services\MeshRMMAgent"),
            "ImagePath",
        )
        .unwrap_err();
        assert_eq!(refused.code, ToolErrorCode::Protected);
    }

    #[test]
    fn keys_and_values_are_created_changed_and_deleted() {
        let scratch = Scratch::new("values");
        let key = &scratch.0;
        for (name, data) in [
            ("", RegistryData::String("default".into())),
            ("Text", RegistryData::String("hello".into())),
            ("Path", RegistryData::ExpandString("%TEMP%".into())),
            (
                "List",
                RegistryData::MultiString(vec!["one".into(), "two".into()]),
            ),
            ("Small", RegistryData::Dword(7)),
            ("Large", RegistryData::Qword(1 << 40)),
            ("Bytes", RegistryData::Binary("0a0b".into())),
        ] {
            let written = set_value(key, name, &data, true).unwrap();
            assert_eq!(written.data, data, "{name}");
            assert_eq!(get_value(key, name).unwrap().data, data, "{name}");
        }
        let again = set_value(key, "Text", &RegistryData::Dword(1), true).unwrap_err();
        assert_eq!(again.code, ToolErrorCode::Exists);
        // Editing may change its type.
        set_value(key, "Text", &RegistryData::Dword(1), false).unwrap();

        let long = RegistryData::Binary("ab".repeat(MAX_LISTED_VALUE_BYTES + 1));
        set_value(key, "Long", &long, true).unwrap();
        create_key(&scratch.child("Sub")).unwrap();
        create_key(&scratch.child(r"Sub\Deeper")).unwrap();
        let exists = create_key(&scratch.child("Sub")).unwrap_err();
        assert_eq!(exists.code, ToolErrorCode::Exists);
        let missing = create_key(&scratch.child(r"Nope\Child")).unwrap_err();
        assert_eq!(missing.code, ToolErrorCode::NotFound);

        let listing = list_key(key).unwrap();
        assert_eq!(
            listing.subkeys,
            vec![RegistrySubkey {
                name: "Sub".into(),
                has_subkeys: true
            }]
        );
        let names: Vec<&str> = listing
            .values
            .iter()
            .map(|value| value.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "", "Bytes", "Large", "List", "Long", "Path", "Small", "Text"
            ]
        );
        let listed = listing
            .values
            .iter()
            .find(|value| value.name == "Long")
            .unwrap();
        assert!(listed.truncated);
        assert_eq!(listed.size, MAX_LISTED_VALUE_BYTES as u64 + 1);
        assert_eq!(get_value(key, "Long").unwrap().data, long);
        assert!(listing.modified_at.is_some());

        rename_value(key, "Small", "Renamed").unwrap();
        assert_eq!(
            get_value(key, "Renamed").unwrap().data,
            RegistryData::Dword(7)
        );
        assert_eq!(
            get_value(key, "Small").unwrap_err().code,
            ToolErrorCode::NotFound
        );
        assert_eq!(
            rename_value(key, "Renamed", "Text").unwrap_err().code,
            ToolErrorCode::Exists
        );
        delete_value(key, "Renamed").unwrap();
        assert_eq!(
            delete_value(key, "Renamed").unwrap_err().code,
            ToolErrorCode::NotFound
        );

        let not_empty = delete_key(&scratch.child("Sub"), false).unwrap_err();
        assert_eq!(not_empty.code, ToolErrorCode::NotEmpty);
        delete_key(&scratch.child(r"Sub\Deeper"), false).unwrap();
        create_key(&scratch.child(r"Sub\Again")).unwrap();
        delete_key(&scratch.child("Sub"), true).unwrap();
        assert!(list_key(key).unwrap().subkeys.is_empty());
        assert_eq!(
            list_key(&scratch.child("Sub")).unwrap_err().code,
            ToolErrorCode::NotFound
        );
    }

    #[test]
    fn the_machines_keys_are_listed_in_both_views() {
        let software = |view| RegistryKey {
            root: RegistryRoot::LocalMachine,
            path: "SOFTWARE".into(),
            view,
        };
        let native = list_key(&software(RegistryView::Bits64)).unwrap();
        assert!(
            native
                .subkeys
                .iter()
                .any(|key| key.name == "Microsoft" && key.has_subkeys)
        );
        // Whether the 64-bit view shows WOW6432Node depends on the machine
        // (CI's Windows Server images don't), but the 32-bit view never does.
        let wow = list_key(&software(RegistryView::Bits32)).unwrap();
        assert!(wow.subkeys.iter().any(|key| key.name == "Microsoft"));
        assert!(wow.subkeys.iter().all(|key| key.name != "WOW6432Node"));
        let roots = list_key(&RegistryKey {
            root: RegistryRoot::LocalMachine,
            path: String::new(),
            view: RegistryView::Bits64,
        })
        .unwrap();
        assert!(roots.subkeys.iter().any(|key| key.name == "SOFTWARE"));
    }
}
