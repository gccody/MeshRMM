//! Code signature checks for macOS app updates.
use std::{path::Path, process::Command};

use anyhow::{Context, bail};

const CODESIGN: &str = "/usr/bin/codesign";

/// The developer team of `app`'s Developer ID signature, or `None` when it's
/// ad-hoc signed, unsigned or missing.
pub fn team_identifier(app: &Path) -> Option<String> {
    let output = Command::new(CODESIGN)
        .args(["--display", "--verbose=2"])
        .arg(app)
        .output()
        .ok()?;
    // codesign describes the signature on standard error.
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|team| !team.is_empty() && *team != "not set")
        .map(str::to_owned)
}

/// Checks that `app`'s code signature is valid and seals every file.
pub fn verify_signature(app: &Path) -> anyhow::Result<()> {
    codesign_verify(app, None).context("the update's code signature is invalid")
}

/// Checks that `app` is `identifier`, signed with a Developer ID issued to
/// `team`, and that its bundle version is `version`. Only `team` can sign
/// such a build, so a server can't swap in someone else's code, and the
/// version check keeps it from passing an old build off as a new one.
pub fn verify_developer_id(
    app: &Path,
    identifier: &str,
    team: &str,
    version: &str,
) -> anyhow::Result<()> {
    codesign_verify(app, Some(&developer_id_requirement(identifier, team)?)).with_context(
        || format!("the update is not {identifier} signed by Developer ID team {team}"),
    )?;
    let actual = bundle_version(app)?;
    if actual != version {
        bail!("the update is version {actual}, not the {version} the server offered");
    }
    Ok(())
}

/// The code requirement Apple gives apps signed with a Developer ID
/// Application certificate, for `identifier` and `team`.
fn developer_id_requirement(identifier: &str, team: &str) -> anyhow::Result<String> {
    let plain = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    };
    if !plain(identifier) || !plain(team) {
        bail!("invalid code signing identifier {identifier:?} or team {team:?}");
    }
    Ok(format!(
        "identifier \"{identifier}\" and anchor apple generic \
         and certificate 1[field.1.2.840.113635.100.6.2.6] \
         and certificate leaf[field.1.2.840.113635.100.6.1.13] \
         and certificate leaf[subject.OU] = \"{team}\""
    ))
}

fn codesign_verify(app: &Path, requirement: Option<&str>) -> anyhow::Result<()> {
    let mut command = Command::new(CODESIGN);
    command.args(["--verify", "--strict", "--deep"]);
    if let Some(requirement) = requirement {
        // A leading "=" makes the argument the requirement's text.
        command.arg("-R").arg(format!("={requirement}"));
    }
    let output = command
        .arg(app)
        .output()
        .context("could not run codesign")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

fn bundle_version(app: &Path) -> anyhow::Result<String> {
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .output()
        .context("could not run plutil")?;
    if !output.status.success() {
        bail!("the update has no bundle version");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, version: &str) -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("meshrmm-self-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let app = directory.join("Test.app");
        let executable = app.join("Contents/MacOS/test");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::copy("/usr/bin/true", &executable).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>test</string>
<key>CFBundleIdentifier</key><string>com.meshrmm.test</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>
"#
            ),
        )
        .unwrap();
        let signed = Command::new(CODESIGN)
            .args(["--force", "--sign", "-"])
            .arg(&app)
            .output()
            .unwrap();
        assert!(signed.status.success());
        app
    }

    #[test]
    fn verifies_ad_hoc_signatures_and_refuses_altered_apps() {
        let app = app("ad-hoc", "1.2.0");
        verify_signature(&app).unwrap();
        assert_eq!(team_identifier(&app), None);
        assert_eq!(bundle_version(&app).unwrap(), "1.2.0");
        std::fs::OpenOptions::new()
            .append(true)
            .open(app.join("Contents/MacOS/test"))
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"tampered"))
            .unwrap();
        assert!(verify_signature(&app).is_err());
        let _ = std::fs::remove_dir_all(app.parent().unwrap());
    }

    #[test]
    fn an_ad_hoc_app_is_not_a_developer_id_build() {
        let app = app("not-developer-id", "1.2.0");
        let error = verify_developer_id(&app, "com.meshrmm.test", "ABCDE12345", "1.2.0")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ABCDE12345"), "{error}");
        let _ = std::fs::remove_dir_all(app.parent().unwrap());
    }

    #[test]
    fn requirements_name_only_plain_identifiers_and_teams() {
        assert!(developer_id_requirement("com.meshrmm.agent", "ABCDE12345").is_ok());
        assert!(developer_id_requirement("com.meshrmm.agent\" or true", "ABCDE12345").is_err());
        assert!(developer_id_requirement("com.meshrmm.agent", "").is_err());
    }

    #[test]
    fn finds_no_team_for_a_missing_app() {
        assert_eq!(team_identifier(Path::new("/nonexistent/Test.app")), None);
    }
}
