//! The users signed in to the computer, at its console or over Remote
//! Desktop, from the Terminal Services session list.
use anyhow::Context;
use meshrmm_protocol::SignedInUser;
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_INFO_CLASS, WTSActive, WTSClientName, WTSClientProtocolType,
    WTSDisconnected, WTSDomainName, WTSEnumerateSessionsW, WTSFreeMemory, WTSINFOW,
    WTSQuerySessionInformationW, WTSSessionInfo, WTSUserName,
};
use windows::core::PWSTR;

/// FILETIME of 1970-01-01, in 100-nanosecond units since 1601.
const UNIX_EPOCH_FILETIME: i64 = 116_444_736_000_000_000;
/// `WTSClientProtocolType` of a session at the computer itself.
const PROTOCOL_CONSOLE: u16 = 0;

pub(super) fn list() -> anyhow::Result<Vec<SignedInUser>> {
    let mut sessions = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: the list is freed below.
    unsafe {
        WTSEnumerateSessionsW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            0,
            1,
            &mut sessions,
            &mut count,
        )
    }
    .context("WTSEnumerateSessionsW")?;
    if sessions.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: the API returned `count` sessions.
    let listed = unsafe { std::slice::from_raw_parts(sessions, count as usize) };
    let mut users = Vec::new();
    for session in listed {
        if session.State != WTSActive && session.State != WTSDisconnected {
            continue;
        }
        let id = session.SessionId;
        let Some(name) = query(id, WTSUserName, text).filter(|name| !name.is_empty()) else {
            continue;
        };
        let name = match query(id, WTSDomainName, text) {
            Some(domain) if !domain.is_empty() => format!(r"{domain}\{name}"),
            _ => name,
        };
        let remote = query(id, WTSClientProtocolType, |bytes| {
            Some(u16::from_ne_bytes(bytes.get(..2)?.try_into().ok()?))
        })
        .is_some_and(|protocol| protocol != PROTOCOL_CONSOLE);
        let since_unix_ms = query(id, WTSSessionInfo, |bytes| {
            (bytes.len() >= std::mem::size_of::<WTSINFOW>()).then(|| {
                // SAFETY: the bytes hold a WTSINFOW.
                unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<WTSINFOW>()) }.LogonTime
            })
        })
        .and_then(filetime_to_unix_ms);
        users.push(SignedInUser {
            name,
            // SAFETY: the station name is NUL-terminated, or null.
            session: unsafe { session.pWinStationName.to_string() }
                .ok()
                .filter(|station| !station.is_empty()),
            remote_host: remote
                .then(|| query(id, WTSClientName, text))
                .flatten()
                .filter(|host| !host.is_empty()),
            since_unix_ms,
        });
    }
    // SAFETY: the list came from WTSEnumerateSessionsW.
    unsafe { WTSFreeMemory(sessions.cast()) };
    Ok(users)
}

/// Queries one piece of a session's information and reads its bytes.
fn query<T>(
    session: u32,
    class: WTS_INFO_CLASS,
    read: impl FnOnce(&[u8]) -> Option<T>,
) -> Option<T> {
    let mut buffer = PWSTR::null();
    let mut length = 0u32;
    // SAFETY: the buffer is freed below.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            class,
            &mut buffer,
            &mut length,
        )
    }
    .ok()?;
    if buffer.is_null() {
        return None;
    }
    // SAFETY: the API returned `length` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buffer.0.cast::<u8>(), length as usize) };
    let value = read(bytes);
    // SAFETY: the buffer came from WTSQuerySessionInformationW.
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    value
}

/// A NUL-terminated UTF-16 string in `bytes`.
fn text(bytes: &[u8]) -> Option<String> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

fn filetime_to_unix_ms(filetime: i64) -> Option<i64> {
    (filetime > UNIX_EPOCH_FILETIME).then(|| (filetime - UNIX_EPOCH_FILETIME) / 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logon_times_convert_to_unix_milliseconds() {
        assert_eq!(filetime_to_unix_ms(UNIX_EPOCH_FILETIME + 10_000), Some(1));
        assert_eq!(
            filetime_to_unix_ms(133_000_000_000_000_000),
            Some(1_655_526_400_000)
        );
        assert_eq!(filetime_to_unix_ms(0), None);
    }

    #[test]
    fn session_strings_stop_at_nul() {
        let bytes: Vec<u8> = "gccody\0junk"
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        assert_eq!(text(&bytes).as_deref(), Some("gccody"));
        assert_eq!(text(&[]).as_deref(), Some(""));
    }
}
