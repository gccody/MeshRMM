//! Hides the desktop wallpaper behind a plain color while a technician works,
//! which keeps the video small, and puts it back afterwards. GNOME and Xfce
//! keep the setting in their configuration stores; the settings it replaced
//! are saved in the user's cache first, so a helper that stopped mid-session
//! restores them when it starts again.
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// The settings the wallpaper had, by desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Saved {
    /// GNOME's `org.gnome.desktop.background picture-options`.
    Gnome(String),
    /// Xfce's image style properties and their values, `None` for those the
    /// Agent created.
    Xfce(Vec<(String, Option<String>)>),
}

#[derive(Default)]
pub(crate) struct Wallpaper {
    saved: Option<Saved>,
}

impl Wallpaper {
    pub(crate) fn set_hidden(&mut self, hidden: bool) -> anyhow::Result<()> {
        if hidden && self.saved.is_none() {
            let saved = hide()?;
            if let Some(path) = state_file() {
                let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
                let _ = std::fs::write(&path, serde_json::to_vec(&saved)?);
            }
            self.saved = Some(saved);
        } else if !hidden && let Some(saved) = self.saved.take() {
            restore(&saved)?;
            if let Some(path) = state_file() {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }
}

impl Drop for Wallpaper {
    fn drop(&mut self) {
        let _ = self.set_hidden(false);
    }
}

/// Puts back a wallpaper an earlier helper hid and did not restore.
pub(crate) fn restore_interrupted() {
    let Some(path) = state_file() else { return };
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    if let Ok(saved) = serde_json::from_slice::<Saved>(&bytes)
        && let Err(error) = restore(&saved)
    {
        tracing::warn!(error = ?error, "could not restore the wallpaper");
        return;
    }
    let _ = std::fs::remove_file(path);
}

fn state_file() -> Option<PathBuf> {
    super::cache_directory().map(|cache| cache.join("hidden-wallpaper.json"))
}

fn output(program: &str, arguments: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .with_context(|| format!("could not run {program}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

const GNOME_SCHEMA: &str = "org.gnome.desktop.background";

fn desktop() -> String {
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()
}

fn hide() -> anyhow::Result<Saved> {
    let desktop = desktop();
    if desktop.contains("GNOME") || desktop.contains("Unity") {
        let current = output("gsettings", &["get", GNOME_SCHEMA, "picture-options"])?;
        output(
            "gsettings",
            &["set", GNOME_SCHEMA, "picture-options", "none"],
        )?;
        return Ok(Saved::Gnome(current));
    }
    if desktop.contains("XFCE") {
        // xfdesktop reads a style per monitor connector and workspace, and
        // keeps only the ones that were ever changed.
        let mut properties: Vec<String> = output("xfconf-query", &["-c", "xfce4-desktop", "-l"])?
            .lines()
            .filter(|property| property.ends_with("/image-style"))
            .map(str::to_owned)
            .collect();
        for monitor in x11_monitors() {
            for workspace in 0..4 {
                properties.push(format!(
                    "/backdrop/screen0/monitor{monitor}/workspace{workspace}/image-style"
                ));
            }
        }
        properties.sort();
        properties.dedup();
        let mut saved = Vec::new();
        for property in properties {
            let current = output("xfconf-query", &["-c", "xfce4-desktop", "-p", &property]).ok();
            output(
                "xfconf-query",
                &[
                    "-c",
                    "xfce4-desktop",
                    "-p",
                    &property,
                    "-n",
                    "-t",
                    "int",
                    "-s",
                    "0",
                ],
            )?;
            saved.push((property, current));
        }
        return Ok(Saved::Xfce(saved));
    }
    bail!("hiding the wallpaper is available on GNOME and Xfce desktops")
}

/// The X server's monitor names, such as `Virtual-1`.
fn x11_monitors() -> Vec<String> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let Ok((connection, screen)) = x11rb::rust_connection::RustConnection::connect(None) else {
        return Vec::new();
    };
    let root = connection.setup().roots[screen].root;
    let Some(monitors) = connection
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
    else {
        return Vec::new();
    };
    monitors
        .monitors
        .iter()
        .filter_map(|monitor| {
            let name = connection.get_atom_name(monitor.name).ok()?.reply().ok()?;
            Some(String::from_utf8_lossy(&name.name).into_owned())
        })
        .collect()
}

fn restore(saved: &Saved) -> anyhow::Result<()> {
    match saved {
        Saved::Gnome(options) => {
            output(
                "gsettings",
                &["set", GNOME_SCHEMA, "picture-options", options],
            )?;
        }
        Saved::Xfce(properties) => {
            for (property, value) in properties {
                match value {
                    Some(value) => output(
                        "xfconf-query",
                        &["-c", "xfce4-desktop", "-p", property, "-s", value],
                    )?,
                    None => output(
                        "xfconf-query",
                        &["-c", "xfce4-desktop", "-p", property, "-r"],
                    )?,
                };
            }
        }
    }
    Ok(())
}
