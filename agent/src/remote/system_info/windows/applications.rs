//! Installed programs, as Programs and Features lists them: the Uninstall
//! keys of both registry views, and of each signed-in user's hive. Microsoft
//! Store apps register elsewhere and are not listed.
use meshrmm_protocol::{Application, ApplicationSource};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_SAM_FLAGS,
};

use super::super::Problems;
use super::registry::Key;

const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
/// Entries of these release types are updates to another program.
const UPDATE_TYPES: &[&str] = &["update", "hotfix", "security update", "update rollup"];

pub(super) fn list(problems: &mut Problems, architecture: &str) -> Vec<Application> {
    let mut applications = Vec::new();
    for (view, architecture) in [(KEY_WOW64_64KEY, architecture), (KEY_WOW64_32KEY, "x86")] {
        if let Some(key) = problems.take(
            "applications",
            Key::open(HKEY_LOCAL_MACHINE, UNINSTALL, view),
        ) {
            read(
                &key,
                view,
                ApplicationSource::Windows,
                Some(architecture),
                &mut applications,
            );
        }
    }
    // Only the hives of users who are signed in, or whose programs run, are
    // loaded; the rest would have to be loaded from their profiles.
    if let Ok(users) = Key::open(HKEY_USERS, "", REG_SAM_FLAGS(0)) {
        for sid in users.subkeys() {
            // Local and domain accounts, and Microsoft Entra ID accounts.
            if !(sid.starts_with("S-1-5-21-") || sid.starts_with("S-1-12-1-"))
                || sid.ends_with("_Classes")
            {
                continue;
            }
            if let Ok(key) = Key::open(HKEY_USERS, &format!(r"{sid}\{UNINSTALL}"), REG_SAM_FLAGS(0))
            {
                read(
                    &key,
                    REG_SAM_FLAGS(0),
                    ApplicationSource::WindowsUser,
                    None,
                    &mut applications,
                );
            }
        }
    }
    applications
}

fn read(
    uninstall: &Key,
    view: REG_SAM_FLAGS,
    source: ApplicationSource,
    architecture: Option<&str>,
    applications: &mut Vec<Application>,
) {
    for name in uninstall.subkeys() {
        let Ok(entry) = uninstall.child(&name, view) else {
            continue;
        };
        let Some(display_name) = entry
            .string("DisplayName")
            .filter(|name| !name.trim().is_empty())
        else {
            continue;
        };
        let release_type = entry.string("ReleaseType").unwrap_or_default();
        if entry.dword("SystemComponent") == Some(1)
            || entry
                .string("ParentKeyName")
                .is_some_and(|parent| !parent.is_empty())
            || UPDATE_TYPES.contains(&release_type.trim().to_lowercase().as_str())
        {
            continue;
        }
        applications.push(Application {
            name: display_name,
            version: entry.string("DisplayVersion"),
            publisher: entry.string("Publisher"),
            architecture: architecture.map(str::to_owned),
            installed_at_unix_ms: entry
                .string("InstallDate")
                .as_deref()
                .and_then(install_date),
            source,
        });
    }
}

/// An `InstallDate` of the form `YYYYMMDD`, as Unix milliseconds of that
/// day's start in UTC.
fn install_date(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.len() != 8 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let year: i64 = text[..4].parse().ok()?;
    let month: u32 = text[4..6].parse().ok()?;
    let day: u32 = text[6..].parse().ok()?;
    if !(1980..=2200).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=days_in_month).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400_000)
}

/// Days since 1970-01-01 of a Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_dates_are_days_in_utc() {
        assert_eq!(install_date("19700101"), None);
        assert_eq!(install_date("20240229"), Some(1_709_164_800_000));
        assert_eq!(install_date(" 20231005 "), Some(1_696_464_000_000));
        assert_eq!(install_date("20230229"), None);
        assert_eq!(install_date("20231301"), None);
        assert_eq!(install_date("2023-10-05"), None);
        assert_eq!(install_date("10/05/2023"), None);
        assert_eq!(install_date(""), None);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }
}
