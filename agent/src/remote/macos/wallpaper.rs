//! Hides the desktop picture for cleaner, cheaper video by showing solid black
//! on every screen, and puts the user's pictures back afterwards.
//!
//! macOS has no switch that turns the picture off, so the original picture and
//! its placement are journaled beside the Agent's state. A session the Agent
//! could not finish restores them when the next one starts.
use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Context;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSScreen, NSWorkspace, NSWorkspaceDesktopImageAllowClippingKey,
    NSWorkspaceDesktopImageOptionKey, NSWorkspaceDesktopImageScalingKey,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use serde::{Deserialize, Serialize};

const BLACK: &str = "/System/Library/Desktop Pictures/Solid Colors/Black.png";
const JOURNAL: &str = "hidden-wallpaper.json";

/// A screen's picture and its placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Picture {
    url: String,
    scaling: Option<i64>,
    allow_clipping: Option<bool>,
}

#[derive(Default)]
pub(crate) struct Wallpaper {
    /// Original pictures by display ID while they are hidden.
    hidden: Option<BTreeMap<u32, Picture>>,
}

impl Wallpaper {
    pub(crate) fn set_hidden(&mut self, hidden: bool) -> anyhow::Result<()> {
        if hidden && self.hidden.is_none() {
            let originals = super::on_main(hide)?;
            write_journal(&originals)?;
            tracing::info!(screens = originals.len(), "desktop picture hidden");
            self.hidden = Some(originals);
        } else if !hidden {
            let originals = match self.hidden.take() {
                Some(originals) => originals,
                None => match read_journal() {
                    Some(originals) => originals,
                    None => return Ok(()),
                },
            };
            super::on_main(move |mtm| restore(mtm, &originals))?;
            let _ = std::fs::remove_file(journal_path()?);
            tracing::info!("desktop picture restored");
        }
        Ok(())
    }
}

impl Drop for Wallpaper {
    fn drop(&mut self) {
        if let Err(error) = self.set_hidden(false) {
            tracing::warn!(error = ?error, "could not restore the desktop picture");
        }
    }
}

/// Restores pictures a session left hidden, for example when the Agent stopped.
pub(crate) fn restore_interrupted() {
    if let Err(error) = Wallpaper::default().set_hidden(false) {
        tracing::warn!(error = ?error, "could not restore an interrupted desktop picture");
    }
}

fn hide(mtm: MainThreadMarker) -> anyhow::Result<BTreeMap<u32, Picture>> {
    let workspace = NSWorkspace::sharedWorkspace();
    let black = NSURL::fileURLWithPath(&NSString::from_str(BLACK));
    let mut originals = BTreeMap::new();
    for screen in NSScreen::screens(mtm).iter() {
        let Some(id) = display_id(&screen) else {
            continue;
        };
        let Some(url) = workspace.desktopImageURLForScreen(&screen) else {
            continue;
        };
        let options = workspace.desktopImageOptionsForScreen(&screen);
        let number = |key: &NSWorkspaceDesktopImageOptionKey| {
            options
                .as_ref()
                .and_then(|options| options.objectForKey(key))
                .and_then(|value| value.downcast::<NSNumber>().ok())
        };
        // SAFETY: the keys are valid static AppKit constants.
        let (scaling, allow_clipping) = unsafe {
            (
                number(NSWorkspaceDesktopImageScalingKey).map(|value| value.integerValue() as i64),
                number(NSWorkspaceDesktopImageAllowClippingKey).map(|value| value.boolValue()),
            )
        };
        let picture = Picture {
            url: url
                .absoluteString()
                .context("the desktop picture has no URL")?
                .to_string(),
            scaling,
            allow_clipping,
        };
        set_picture(&workspace, &screen, &black, &NSDictionary::new())?;
        originals.insert(id, picture);
    }
    Ok(originals)
}

fn restore(mtm: MainThreadMarker, originals: &BTreeMap<u32, Picture>) -> anyhow::Result<()> {
    let workspace = NSWorkspace::sharedWorkspace();
    let mut failure = None;
    for screen in NSScreen::screens(mtm).iter() {
        let Some(picture) = display_id(&screen).and_then(|id| originals.get(&id)) else {
            continue;
        };
        let Some(url) = NSURL::URLWithString(&NSString::from_str(&picture.url)) else {
            continue;
        };
        let mut keys = Vec::<&NSWorkspaceDesktopImageOptionKey>::new();
        let mut values = Vec::<Retained<AnyObject>>::new();
        // SAFETY: the keys are valid static AppKit constants.
        unsafe {
            if let Some(scaling) = picture.scaling {
                keys.push(NSWorkspaceDesktopImageScalingKey);
                values.push(NSNumber::new_isize(scaling as isize).into());
            }
            if let Some(allow_clipping) = picture.allow_clipping {
                keys.push(NSWorkspaceDesktopImageAllowClippingKey);
                values.push(NSNumber::new_bool(allow_clipping).into());
            }
        }
        let values = values.iter().map(|value| &**value).collect::<Vec<_>>();
        let options = NSDictionary::from_slices(&keys, &values);
        if let Err(error) = set_picture(&workspace, &screen, &url, &options) {
            failure = Some(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

fn set_picture(
    workspace: &NSWorkspace,
    screen: &NSScreen,
    url: &NSURL,
    options: &NSDictionary<NSWorkspaceDesktopImageOptionKey, AnyObject>,
) -> anyhow::Result<()> {
    // SAFETY: all arguments are valid AppKit objects on the main thread.
    unsafe { workspace.setDesktopImageURL_forScreen_options_error(url, screen, options) }.map_err(
        |error| {
            anyhow::anyhow!(
                "macOS could not change the desktop picture: {}",
                error.localizedDescription()
            )
        },
    )
}

fn display_id(screen: &NSScreen) -> Option<u32> {
    screen
        .deviceDescription()
        .objectForKey(&NSString::from_str("NSScreenNumber"))
        .and_then(|value| value.downcast::<NSNumber>().ok())
        .map(|number| number.unsignedIntValue())
}

fn journal_path() -> anyhow::Result<PathBuf> {
    Ok(crate::installer::config_directory()?.join(JOURNAL))
}

fn write_journal(originals: &BTreeMap<u32, Picture>) -> anyhow::Result<()> {
    crate::installer::replace_file(&journal_path()?, &serde_json::to_vec(originals)?)
}

fn read_journal() -> Option<BTreeMap<u32, Picture>> {
    serde_json::from_slice(&std::fs::read(journal_path().ok()?).ok()?).ok()
}
