//! Windows updates through the Windows Update Agent's COM API, as Windows
//! Update itself finds and installs them: from Microsoft Update or the WSUS
//! server Group Policy names, with the device's own settings.
//!
//! A scan searches for updates that are neither installed nor hidden. An
//! install searches again for the chosen updates' COM objects, accepts their
//! licence terms, downloads them together, and then installs them one at a
//! time. One at a time gives each update its own result and lets the install
//! report which one it is on, and an update that must be installed alone
//! (such as a servicing stack update) always is; once one says the computer
//! must restart before anything else installs, the rest wait for the next
//! install. Restarts are left to the caller.
mod describe;

use std::collections::HashMap;

use anyhow::Context;
use meshrmm_protocol::{
    PendingUpdate, UpdateCategory, UpdateInstallItem, UpdateItemOutcome, UpdateManager, UpdateScan,
    UpdateSeverity,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RegCloseKey, RegOpenKeyExW,
};
use windows::Win32::System::UpdateAgent::{
    ISystemInformation, IUpdate, IUpdateCollection, IUpdateInstaller2, IUpdateSession,
    SystemInformation, UpdateCollection, UpdateSession, irbNeverReboots, orcAborted, orcFailed,
    orcSucceeded, orcSucceededWithErrors, utDriver,
};
use windows::core::{BSTR, Interface, PCWSTR};

use self::describe::{category_of_classification, error_message, knowledge_base, ole_date_unix_ms};
use super::Progress;
use crate::win32::wide;

/// What a search asks for: what Windows Update would offer.
const CRITERIA: &str = "IsInstalled=0 and IsHidden=0";
const CLIENT: &str = "MeshRMM";

/// COM, initialized on this thread until dropped.
struct Com {
    initialized: bool,
}

impl Com {
    fn new() -> Self {
        // SAFETY: called on the thread that uses COM, and undone on drop.
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        Self { initialized }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances the successful CoInitializeEx in `new`.
            unsafe { CoUninitialize() };
        }
    }
}

/// A COM error as a readable message with its code.
fn com_error(error: &windows::core::Error) -> String {
    error_message(error.code().0, &error.message())
}

fn session() -> windows::core::Result<IUpdateSession> {
    // SAFETY: COM is initialized on this thread.
    let session: IUpdateSession = unsafe { CoCreateInstance(&UpdateSession, None, CLSCTX_ALL) }?;
    // SAFETY: a valid session and a BSTR it copies.
    unsafe { session.SetClientApplicationID(&BSTR::from(CLIENT)) }?;
    Ok(session)
}

/// The updates Windows Update offers, as COM objects.
fn search(session: &IUpdateSession) -> windows::core::Result<Vec<IUpdate>> {
    // SAFETY: calls on valid COM objects with owned BSTRs.
    unsafe {
        let searcher = session.CreateUpdateSearcher()?;
        searcher.SetClientApplicationID(&BSTR::from(CLIENT))?;
        let result = searcher.Search(&BSTR::from(CRITERIA))?;
        let code = result.ResultCode()?;
        if code == orcFailed || code == orcAborted {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "the search for updates did not complete",
            ));
        }
        let updates = result.Updates()?;
        (0..updates.Count()?)
            .map(|index| updates.get_Item(index))
            .collect()
    }
}

pub(super) fn scan() -> UpdateScan {
    let _com = Com::new();
    let reboot_pending = reboot_pending();
    let found = session().and_then(|session| search(&session));
    match found {
        Ok(updates) => UpdateScan {
            manager: Some(UpdateManager::WindowsUpdate),
            updates: updates.iter().filter_map(pending).collect(),
            reboot_pending,
            ..Default::default()
        },
        Err(error) => UpdateScan {
            manager: Some(UpdateManager::WindowsUpdate),
            reboot_pending,
            error: Some(format!(
                "Windows Update couldn't search: {}",
                com_error(&error)
            )),
            ..Default::default()
        },
    }
}

/// An offered update as the protocol describes it, or `None` (logged) if
/// it couldn't be read.
fn pending(update: &IUpdate) -> Option<PendingUpdate> {
    match read(update) {
        Ok(pending) => Some(pending),
        Err(error) => {
            tracing::warn!(error = ?error, "could not read an update Windows Update offered");
            None
        }
    }
}

fn read(update: &IUpdate) -> anyhow::Result<PendingUpdate> {
    // SAFETY: calls on a valid COM object; collections are indexed within
    // their counts.
    unsafe {
        let id = update.Identity()?.UpdateID()?.to_string();
        let title = update.Title()?.to_string();
        let mut category = None;
        let mut products = Vec::new();
        let categories = update.Categories()?;
        for index in 0..categories.Count()? {
            let entry = categories.get_Item(index)?;
            match entry.Type()?.to_string().as_str() {
                "UpdateClassification" if category.is_none() => {
                    category = category_of_classification(&entry.CategoryID()?.to_string());
                }
                "Product" => products.push(entry.Name()?.to_string()),
                _ => {}
            }
        }
        if update.Type().is_ok_and(|kind| kind == utDriver) {
            category = Some(UpdateCategory::Driver);
        }
        let articles = update.KBArticleIDs()?;
        let articles = (0..articles.Count()?)
            .map(|index| articles.get_Item(index).map(|article| article.to_string()))
            .collect::<windows::core::Result<Vec<_>>>()?;
        let reboot_required = update
            .InstallationBehavior()
            .and_then(|behavior| behavior.RebootBehavior())
            .is_ok_and(|behavior| behavior != irbNeverReboots);
        Ok(PendingUpdate {
            id,
            title,
            kb: knowledge_base(&articles),
            version: None,
            current_version: None,
            category: category.unwrap_or(UpdateCategory::Other),
            severity: update
                .MsrcSeverity()
                .ok()
                .and_then(|severity| UpdateSeverity::from_vendor(&severity.to_string())),
            // MaxDownloadSize is the largest of every package variant (97 GB
            // for a cumulative update), not what this computer downloads, and
            // MinDownloadSize is 0, so Windows updates have no size.
            size_bytes: None,
            reboot_required,
            released_at_unix_ms: update
                .LastDeploymentChangeTime()
                .ok()
                .and_then(ole_date_unix_ms),
            // An update for many products, such as the Malicious Software
            // Removal Tool, names none of them in particular.
            source: (products.len() == 1).then(|| products.remove(0)),
        })
    }
}

/// Whether Windows waits for a restart to finish installing updates: Windows
/// Update says so, or component servicing does.
fn reboot_pending() -> bool {
    // SAFETY: COM is initialized on this thread.
    let update_agent = unsafe {
        CoCreateInstance::<_, ISystemInformation>(&SystemInformation, None, CLSCTX_ALL)
            .and_then(|information| information.RebootRequired())
    }
    .is_ok_and(|required| required.as_bool());
    update_agent
        || key_exists(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired",
        )
        || key_exists(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending",
        )
}

fn key_exists(path: &str) -> bool {
    let path = wide(path);
    let mut key = HKEY::default();
    // SAFETY: the path is NUL-terminated and `key` is writable.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            None,
            KEY_READ | KEY_WOW64_64KEY,
            &mut key,
        )
    }
    .is_ok();
    if opened {
        // SAFETY: the key was opened above.
        let _ = unsafe { RegCloseKey(key) };
    }
    opened
}

/// A collection holding `updates`, for a downloader or installer.
fn collection<'a>(
    updates: impl IntoIterator<Item = &'a IUpdate>,
) -> windows::core::Result<IUpdateCollection> {
    // SAFETY: COM is initialized on this thread; the updates are valid.
    unsafe {
        let collection: IUpdateCollection = CoCreateInstance(&UpdateCollection, None, CLSCTX_ALL)?;
        for update in updates {
            collection.Add(update)?;
        }
        Ok(collection)
    }
}

/// How one update's download or install went: `None` if it succeeded, or
/// why it failed.
fn outcome(
    code: windows::Win32::System::UpdateAgent::OperationResultCode,
    hresult: i32,
) -> Option<String> {
    if code == orcSucceeded || code == orcSucceededWithErrors {
        None
    } else if hresult < 0 {
        Some(error_message(hresult, ""))
    } else if code == orcAborted {
        Some("Windows Update stopped it".to_owned())
    } else {
        Some("Windows Update couldn't install it".to_owned())
    }
}

pub(super) fn install(
    _manager: UpdateManager,
    chosen: &[&PendingUpdate],
    _everything: bool,
    progress: &Progress,
) -> anyhow::Result<Vec<UpdateInstallItem>> {
    let _com = Com::new();
    let session = session()
        .map_err(|error| anyhow::anyhow!("Windows Update couldn't start: {}", com_error(&error)))?;
    let offered = search(&session).map_err(|error| {
        anyhow::anyhow!("Windows Update couldn't search: {}", com_error(&error))
    })?;
    let mut by_id = HashMap::new();
    for update in offered {
        // SAFETY: a valid update object.
        if let Ok(id) = unsafe { update.Identity().and_then(|identity| identity.UpdateID()) } {
            by_id.insert(id.to_string(), update);
        }
    }
    let item =
        |update: &PendingUpdate, error: Option<String>, reboot_required: bool| UpdateInstallItem {
            id: update.id.clone(),
            title: update.title.clone(),
            kb: update.kb.clone(),
            category: update.category,
            outcome: if error.is_some() {
                UpdateItemOutcome::Failed
            } else {
                UpdateItemOutcome::Installed
            },
            error,
            reboot_required,
        };
    let mut items = Vec::new();
    let mut ready = Vec::new();
    for update in chosen {
        match by_id.get(&update.id) {
            Some(object) => ready.push((*update, object)),
            None => items.push(item(
                update,
                Some("Windows Update no longer offers it".into()),
                false,
            )),
        }
    }
    if ready.is_empty() {
        return Ok(items);
    }
    for (update, object) in &ready {
        // SAFETY: a valid update object.
        let accepted = unsafe { object.EulaAccepted() }.is_ok_and(|accepted| accepted.as_bool());
        if !accepted && let Err(error) = unsafe { object.AcceptEula() } {
            tracing::warn!(id = update.id, error = %error, "could not accept an update's licence terms");
        }
    }

    progress.report(
        format!(
            "Downloading {} update{}",
            ready.len(),
            if ready.len() == 1 { "" } else { "s" }
        ),
        Some(0),
    );
    let downloads = collection(ready.iter().map(|(_, object)| *object))
        .context("could not list the updates to download")?;
    // SAFETY: calls on valid COM objects.
    let downloaded = unsafe {
        let downloader = session.CreateUpdateDownloader()?;
        downloader.SetClientApplicationID(&BSTR::from(CLIENT))?;
        downloader.SetUpdates(&downloads)?;
        downloader.Download()
    }
    .map_err(|error| anyhow::anyhow!("Windows Update couldn't download: {}", com_error(&error)))?;
    let mut installable = Vec::new();
    for (index, (update, object)) in ready.iter().enumerate() {
        // SAFETY: the index is within the downloaded collection.
        let result = unsafe {
            downloaded
                .GetUpdateResult(index as i32)
                .and_then(|result| Ok((result.ResultCode()?, result.HResult()?)))
        };
        // An update already downloaded may report nothing new; installing
        // it says whether it's usable.
        match result {
            Ok((code, hresult)) if code == orcFailed || code == orcAborted => items.push(item(
                update,
                Some(format!(
                    "Couldn't download it: {}",
                    outcome(code, hresult).unwrap_or_default()
                )),
                false,
            )),
            _ => installable.push((*update, *object)),
        }
    }

    // SAFETY: calls on valid COM objects.
    let installer = unsafe {
        let installer = session.CreateUpdateInstaller()?;
        installer.SetClientApplicationID(&BSTR::from(CLIENT))?;
        if let Ok(quiet) = installer.cast::<IUpdateInstaller2>() {
            let _ = quiet.SetForceQuiet(true.into());
        }
        Ok::<_, windows::core::Error>(installer)
    }
    .map_err(|error| {
        anyhow::anyhow!(
            "Windows Update couldn't start installing: {}",
            com_error(&error)
        )
    })?;
    let total = installable.len();
    let mut waiting_for_restart = false;
    for (position, (update, object)) in installable.into_iter().enumerate() {
        if waiting_for_restart {
            items.push(item(
                update,
                Some("The computer must restart before it can install".into()),
                false,
            ));
            continue;
        }
        progress.report(
            format!("Installing {} of {total}: {}", position + 1, update.title),
            Some(u8::try_from(position * 100 / total).unwrap_or(100)),
        );
        // SAFETY: calls on valid COM objects.
        let installed = unsafe {
            collection([object]).and_then(|single| {
                installer.SetUpdates(&single)?;
                let result = installer.Install()?;
                let update_result = result.GetUpdateResult(0)?;
                Ok((
                    update_result.ResultCode()?,
                    update_result.HResult()?,
                    update_result.RebootRequired()?.as_bool() || result.RebootRequired()?.as_bool(),
                ))
            })
        };
        match installed {
            Ok((code, hresult, reboot_required)) => {
                let error = outcome(code, hresult);
                tracing::info!(
                    id = update.id,
                    title = update.title,
                    code = code.0,
                    hresult = format!("0x{:08X}", hresult as u32),
                    reboot_required,
                    "Windows Update installed an update"
                );
                items.push(item(update, error, reboot_required));
            }
            Err(error) => {
                tracing::warn!(id = update.id, error = %error, "Windows Update couldn't install an update");
                items.push(item(update, Some(com_error(&error)), false));
            }
        }
        // SAFETY: a valid installer.
        waiting_for_restart = unsafe { installer.RebootRequiredBeforeInstallation() }
            .is_ok_and(|required| required.as_bool());
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Searches this computer's real Windows Update and prints what it
    /// offers. Run it as an administrator: `cargo test -p meshrmm-agent
    /// real_scan_is_plausible -- --ignored --nocapture`. It never installs.
    #[test]
    #[ignore = "searches Windows Update over the network"]
    fn real_scan_is_plausible() {
        let scan = scan();
        println!("{}", serde_json::to_string_pretty(&scan).unwrap());
        assert_eq!(scan.manager, Some(UpdateManager::WindowsUpdate));
        assert_eq!(scan.error, None);
        for update in &scan.updates {
            assert_eq!(update.id.len(), 36, "{update:?}");
            assert!(!update.title.is_empty());
        }
    }
}
