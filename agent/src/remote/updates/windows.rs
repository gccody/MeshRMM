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
//!
//! The search, the download and each install use Windows Update's
//! asynchronous calls, checked on every second, so none outlasts the
//! deadline the coordinator sets for the scan or install
//! ([`crate::remote::process::by`]). Past it, Windows Update is asked to stop
//! the job and given a minute to; one it doesn't stop is left to it, and
//! Windows Update refuses to start another install while one runs.
mod describe;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
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
    IDownloadCompletedCallback, IDownloadCompletedCallback_Impl, IDownloadCompletedCallbackArgs,
    IDownloadJob, IDownloadProgressChangedCallback, IDownloadProgressChangedCallback_Impl,
    IDownloadProgressChangedCallbackArgs, IInstallationCompletedCallback,
    IInstallationCompletedCallback_Impl, IInstallationCompletedCallbackArgs, IInstallationJob,
    IInstallationProgressChangedCallback, IInstallationProgressChangedCallback_Impl,
    IInstallationProgressChangedCallbackArgs, ISearchCompletedCallback,
    ISearchCompletedCallback_Impl, ISearchCompletedCallbackArgs, ISearchJob, ISystemInformation,
    IUpdate, IUpdateCollection, IUpdateInstaller, IUpdateInstaller2, IUpdateSession,
    SystemInformation, UpdateCollection, UpdateSession, irbNeverReboots, orcAborted, orcFailed,
    orcSucceeded, orcSucceededWithErrors, utDriver,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BSTR, IUnknown, Interface, PCWSTR, Ref, implement};

use self::describe::{category_of_classification, error_message, knowledge_base, ole_date_unix_ms};
use super::{Progress, span};
use crate::win32::wide;

/// What scans and installs, for messages.
pub(super) const SERVICE: &str = "Windows Update";
/// What a search asks for: what Windows Update would offer.
const CRITERIA: &str = "IsInstalled=0 and IsHidden=0";
const CLIENT: &str = "MeshRMM";
/// How often a search, download or install is checked on.
const POLL: Duration = Duration::from_secs(1);
/// How long Windows Update has to stop a job it was asked to stop.
const ABORT_WAIT: Duration = Duration::from_secs(60);

/// A search, download or install Windows Update works on for the Agent.
trait Job {
    fn completed(&self) -> windows::core::Result<bool>;
    fn abort(&self) -> windows::core::Result<()>;
    /// Lets go of its callbacks; this waits for the job to end.
    fn clean_up(&self) -> windows::core::Result<()>;
}

macro_rules! job {
    ($interface:ty) => {
        impl Job for $interface {
            fn completed(&self) -> windows::core::Result<bool> {
                // SAFETY: a valid job object.
                unsafe { self.IsCompleted() }.map(|completed| completed.as_bool())
            }

            fn abort(&self) -> windows::core::Result<()> {
                // SAFETY: as above.
                unsafe { self.RequestAbort() }
            }

            fn clean_up(&self) -> windows::core::Result<()> {
                // SAFETY: as above.
                unsafe { self.CleanUp() }
            }
        }
    };
}

job!(ISearchJob);
job!(IDownloadJob);
job!(IInstallationJob);

/// A job that didn't end in time.
#[derive(Debug)]
struct Stalled {
    message: String,
    /// Windows Update stopped it when asked.
    stopped: bool,
}

/// Waits for `job` until `deadline`, checking every `poll`. Past it,
/// Windows Update is asked to stop the job and given `abort_wait` to; `what`
/// names the job for the message, as in "did not finish the search".
fn wait(
    job: &impl Job,
    what: &str,
    deadline: Option<Instant>,
    poll: Duration,
    abort_wait: Duration,
) -> Result<(), Stalled> {
    let started = Instant::now();
    loop {
        match job.completed() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => {
                return Err(Stalled {
                    message: format!("Windows Update lost {what}: {}", com_error(&error)),
                    stopped: false,
                });
            }
        }
        let now = Instant::now();
        let Some(deadline) = deadline.filter(|deadline| now >= *deadline) else {
            std::thread::sleep(deadline.map_or(poll, |deadline| poll.min(deadline - now)));
            continue;
        };
        let allowed = span(deadline.saturating_duration_since(started));
        tracing::warn!(
            what,
            "asking Windows Update to stop work that ran out of time"
        );
        if let Err(error) = job.abort() {
            tracing::warn!(error = %error, "Windows Update couldn't be asked to stop");
        }
        let asked = Instant::now();
        let stopped = loop {
            if job.completed().unwrap_or(false) {
                break true;
            }
            if asked.elapsed() >= abort_wait {
                break false;
            }
            std::thread::sleep(poll.min(abort_wait));
        };
        return Err(Stalled {
            message: if stopped {
                format!("Windows Update did not finish {what} within {allowed}, so it was stopped")
            } else {
                format!(
                    "Windows Update did not finish {what} within {allowed}, and did not stop when asked"
                )
            },
            stopped,
        });
    }
}

/// Waits for `job` (see [`wait`]) by the thread's deadline, then returns
/// what `end` makes of it and lets the job go. A job Windows Update didn't
/// stop is leaked rather than cleaned up, since cleaning up waits for it.
fn finish<J: Job, T>(
    job: J,
    what: &str,
    end: impl FnOnce(&J) -> windows::core::Result<T>,
) -> anyhow::Result<windows::core::Result<T>> {
    if let Err(stalled) = wait(
        &job,
        what,
        crate::remote::process::deadline(),
        POLL,
        ABORT_WAIT,
    ) {
        if stalled.stopped {
            let _ = job.clean_up();
        } else {
            std::mem::forget(job);
        }
        bail!(stalled.message);
    }
    let ended = end(&job);
    let _ = job.clean_up();
    Ok(ended)
}

/// Errs when the thread's deadline has passed, before more is asked of
/// Windows Update.
fn time_left(what: &str) -> anyhow::Result<()> {
    if crate::remote::process::deadline().is_some_and(|deadline| Instant::now() >= deadline) {
        bail!("There was no time left for Windows Update to finish {what}");
    }
    Ok(())
}

/// The callbacks Windows Update's asynchronous calls take. The Agent checks
/// on the jobs itself, so they do nothing.
#[implement(
    ISearchCompletedCallback,
    IDownloadProgressChangedCallback,
    IDownloadCompletedCallback,
    IInstallationProgressChangedCallback,
    IInstallationCompletedCallback
)]
struct Unheeded;

impl ISearchCompletedCallback_Impl for Unheeded_Impl {
    fn Invoke(
        &self,
        _: Ref<ISearchJob>,
        _: Ref<ISearchCompletedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

impl IDownloadProgressChangedCallback_Impl for Unheeded_Impl {
    fn Invoke(
        &self,
        _: Ref<IDownloadJob>,
        _: Ref<IDownloadProgressChangedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

impl IDownloadCompletedCallback_Impl for Unheeded_Impl {
    fn Invoke(
        &self,
        _: Ref<IDownloadJob>,
        _: Ref<IDownloadCompletedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

impl IInstallationProgressChangedCallback_Impl for Unheeded_Impl {
    fn Invoke(
        &self,
        _: Ref<IInstallationJob>,
        _: Ref<IInstallationProgressChangedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

impl IInstallationCompletedCallback_Impl for Unheeded_Impl {
    fn Invoke(
        &self,
        _: Ref<IInstallationJob>,
        _: Ref<IInstallationCompletedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

fn unheeded() -> IUnknown {
    Unheeded.into()
}

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
fn search(session: &IUpdateSession) -> anyhow::Result<Vec<IUpdate>> {
    let couldnt = |error: windows::core::Error| {
        anyhow::anyhow!("Windows Update couldn't search: {}", com_error(&error))
    };
    time_left("the search")?;
    // SAFETY: calls on valid COM objects with owned BSTRs.
    unsafe {
        let searcher = session.CreateUpdateSearcher().map_err(couldnt)?;
        searcher
            .SetClientApplicationID(&BSTR::from(CLIENT))
            .map_err(couldnt)?;
        let job = searcher
            .BeginSearch(&BSTR::from(CRITERIA), &unheeded(), &VARIANT::default())
            .map_err(couldnt)?;
        let result = finish(job, "the search", |job| searcher.EndSearch(job))?.map_err(couldnt)?;
        let code = result.ResultCode().map_err(couldnt)?;
        if code == orcFailed || code == orcAborted {
            bail!("Windows Update couldn't search: the search for updates did not complete");
        }
        let updates = result.Updates().map_err(couldnt)?;
        (0..updates.Count().map_err(couldnt)?)
            .map(|index| updates.get_Item(index).map_err(couldnt))
            .collect()
    }
}

pub(super) fn scan() -> UpdateScan {
    let _com = Com::new();
    let reboot_pending = reboot_pending();
    let found = session()
        .map_err(|error| anyhow::anyhow!("Windows Update couldn't start: {}", com_error(&error)))
        .and_then(|session| search(&session));
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
            error: Some(format!("{error:#}")),
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
    let offered = search(&session)?;
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
    let couldnt = |error: windows::core::Error| {
        anyhow::anyhow!("Windows Update couldn't download: {}", com_error(&error))
    };
    time_left("the download")?;
    // SAFETY: calls on valid COM objects.
    let downloaded = unsafe {
        let downloader = session.CreateUpdateDownloader().map_err(couldnt)?;
        downloader
            .SetClientApplicationID(&BSTR::from(CLIENT))
            .map_err(couldnt)?;
        downloader.SetUpdates(&downloads).map_err(couldnt)?;
        let callbacks = unheeded();
        let job = downloader
            .BeginDownload(&callbacks, &callbacks, &VARIANT::default())
            .map_err(couldnt)?;
        finish(job, "the download", |job| downloader.EndDownload(job))?.map_err(couldnt)?
    };
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
    let mut out_of_time = false;
    for (position, (update, object)) in installable.into_iter().enumerate() {
        if waiting_for_restart {
            items.push(item(
                update,
                Some("The computer must restart before it can install".into()),
                false,
            ));
            continue;
        }
        if out_of_time {
            items.push(item(
                update,
                Some("The install ran out of time before it got to this update".into()),
                false,
            ));
            continue;
        }
        progress.report(
            format!("Installing {} of {total}: {}", position + 1, update.title),
            Some(u8::try_from(position * 100 / total).unwrap_or(100)),
        );
        let installed = install_one(&installer, object);
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
                items.push(item(update, Some(format!("{error:#}")), false));
                // What's left would only find no time left either.
                if crate::remote::process::deadline()
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    out_of_time = true;
                    continue;
                }
            }
        }
        // SAFETY: a valid installer.
        waiting_for_restart = unsafe { installer.RebootRequiredBeforeInstallation() }
            .is_ok_and(|required| required.as_bool());
    }
    Ok(items)
}

/// Installs one update: its result code, `HRESULT` and whether the computer
/// must restart.
fn install_one(
    installer: &IUpdateInstaller,
    update: &IUpdate,
) -> anyhow::Result<(
    windows::Win32::System::UpdateAgent::OperationResultCode,
    i32,
    bool,
)> {
    let failed = |error: windows::core::Error| anyhow::anyhow!(com_error(&error));
    time_left("installing it")?;
    // SAFETY: calls on valid COM objects.
    unsafe {
        let single = collection([update]).map_err(failed)?;
        installer.SetUpdates(&single).map_err(failed)?;
        let callbacks = unheeded();
        let job = installer
            .BeginInstall(&callbacks, &callbacks, &VARIANT::default())
            .map_err(failed)?;
        let result =
            finish(job, "installing it", |job| installer.EndInstall(job))?.map_err(failed)?;
        let update_result = result.GetUpdateResult(0).map_err(failed)?;
        Ok((
            update_result.ResultCode().map_err(failed)?,
            update_result.HResult().map_err(failed)?,
            update_result.RebootRequired().map_err(failed)?.as_bool()
                || result.RebootRequired().map_err(failed)?.as_bool(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// A search, download or install that ends after a number of checks,
    /// when asked to stop, or never.
    #[derive(Default)]
    struct Fake {
        checks: Cell<u32>,
        ends_after: Option<u32>,
        stops_when_asked: bool,
        asked: Cell<u32>,
        cleaned_up: Cell<bool>,
    }

    impl Job for Fake {
        fn completed(&self) -> windows::core::Result<bool> {
            self.checks.set(self.checks.get() + 1);
            let ended = self
                .ends_after
                .is_some_and(|after| self.checks.get() > after);
            Ok(ended || (self.stops_when_asked && self.asked.get() > 0))
        }

        fn abort(&self) -> windows::core::Result<()> {
            self.asked.set(self.asked.get() + 1);
            Ok(())
        }

        fn clean_up(&self) -> windows::core::Result<()> {
            self.cleaned_up.set(true);
            Ok(())
        }
    }

    const POLL: Duration = Duration::from_millis(10);
    const ABORT_WAIT: Duration = Duration::from_millis(200);

    #[test]
    fn jobs_that_end_in_time_are_waited_for() {
        let job = Fake {
            ends_after: Some(3),
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        assert!(wait(&job, "the search", Some(deadline), POLL, ABORT_WAIT).is_ok());
        assert_eq!(job.asked.get(), 0);
        // With no deadline, a job is waited for however long it takes.
        let job = Fake {
            ends_after: Some(20),
            ..Default::default()
        };
        assert!(wait(&job, "the search", None, POLL, ABORT_WAIT).is_ok());
    }

    #[test]
    fn jobs_past_the_deadline_are_asked_to_stop() {
        let job = Fake {
            stops_when_asked: true,
            ..Default::default()
        };
        let started = Instant::now();
        let stalled = wait(
            &job,
            "the download",
            Some(started + Duration::from_millis(300)),
            POLL,
            ABORT_WAIT,
        )
        .unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(job.asked.get(), 1);
        assert!(stalled.stopped);
        assert_eq!(
            stalled.message,
            "Windows Update did not finish the download within 1 minute, so it was stopped"
        );
    }

    #[test]
    fn jobs_that_wont_stop_are_left_after_a_while() {
        let job = Fake::default();
        let started = Instant::now();
        let stalled = wait(
            &job,
            "the search",
            Some(started + Duration::from_millis(300)),
            POLL,
            ABORT_WAIT,
        )
        .unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(300) + ABORT_WAIT);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(job.asked.get(), 1);
        assert!(!stalled.stopped);
        assert!(
            stalled.message.contains("did not stop when asked"),
            "{}",
            stalled.message
        );
        // A deadline already gone asks at once.
        let job = Fake::default();
        assert!(wait(&job, "the search", Some(Instant::now()), POLL, ABORT_WAIT).is_err());
        assert_eq!(job.asked.get(), 1);
    }

    #[test]
    fn nothing_more_is_asked_of_windows_update_after_the_deadline() {
        let message = crate::remote::process::by(Instant::now(), || {
            time_left("the search").unwrap_err().to_string()
        });
        assert_eq!(
            message,
            "There was no time left for Windows Update to finish the search"
        );
        assert!(time_left("the search").is_ok());
    }

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

    /// Searches this computer's real Windows Update with too little time,
    /// and checks the search is stopped rather than waited for: `cargo test
    /// -p meshrmm-agent real_scan_stops_at_its_deadline -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "searches Windows Update over the network"]
    fn real_scan_stops_at_its_deadline() {
        let started = Instant::now();
        let scan = crate::remote::process::by(Instant::now() + Duration::from_millis(500), scan);
        println!("{:?} after {:?}", scan.error, started.elapsed());
        assert!(
            started.elapsed() < Duration::from_millis(500) + super::ABORT_WAIT + super::POLL * 5
        );
        let error = scan.error.expect("a search this short fails");
        assert!(
            error.starts_with("Windows Update did not finish the search"),
            "{error}"
        );
    }
}
