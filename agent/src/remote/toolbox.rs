//! Runs toolbox scripts and saves toolbox files when the server asks, then
//! reports how they went. See docs/toolbox.md.
//!
//! Each run or delivery gets its own thread, off the coordinator's signaling
//! loop, and reports over HTTPS with the Agent's credential, so a result
//! survives a signaling reconnect.
//!
//! A script runs as SYSTEM, or as the signed-in user when asked; with nobody
//! signed in it runs as SYSTEM instead. A file goes to the signed-in user's
//! Documents transfer folder, where the viewer's own transfers go, or to
//! Public Documents for the background desktop or when nobody is signed in.
//! While a user is signed in, files are written with their token, so they
//! own them and a folder they control cannot redirect a SYSTEM write.
//! On a Mac, root takes SYSTEM's place and `/Users/Shared` Public Documents'.

use meshrmm_protocol::ScriptLanguage;

/// Runs past this many at once are refused rather than queued.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const MAX_RUNNING_SCRIPTS: usize = 8;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const MAX_RUNNING_DELIVERIES: usize = 4;

/// The script file's name and its bytes as the interpreter reads them.
/// Both interpreters want CRLF lines. Windows PowerShell 5.1 reads a script
/// without a byte-order mark in the ANSI code page, so the file gets one.
/// `cmd` reads a batch file a line at a time in the console code page, so a
/// first line switches it to UTF-8, which also makes the output UTF-8.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn script_file(language: ScriptLanguage, body: &str) -> (&'static str, Vec<u8>) {
    let body = body.replace("\r\n", "\n").replace('\n', "\r\n");
    match language {
        ScriptLanguage::Powershell => {
            let mut bytes = "\u{feff}".as_bytes().to_vec();
            bytes.extend_from_slice(body.as_bytes());
            ("script.ps1", bytes)
        }
        ScriptLanguage::Cmd => {
            let mut bytes = b"@chcp 65001>nul\r\n".to_vec();
            bytes.extend_from_slice(body.as_bytes());
            ("script.cmd", bytes)
        }
        ScriptLanguage::Shell => ("script.sh", body.replace("\r\n", "\n").into_bytes()),
    }
}

/// The arguments that run `script` in `language`'s interpreter. PowerShell
/// is told to write UTF-8, and its exit code is the script's `exit` code.
#[cfg_attr(not(windows), allow(dead_code))]
fn interpreter_arguments(language: ScriptLanguage, script: &str) -> String {
    match language {
        ScriptLanguage::Powershell => format!(
            "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"try {{ [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) }} catch {{}}; & '{}'; exit $LASTEXITCODE\"",
            script.replace('\'', "''")
        ),
        ScriptLanguage::Cmd => format!("/D /S /C \"\"{script}\"\""),
        ScriptLanguage::Shell => format!("\"{script}\""),
    }
}

/// Output a script wrote, up to the limit the server keeps.
#[derive(Debug, Default)]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
struct Output {
    bytes: Vec<u8>,
    truncated: bool,
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
impl Output {
    fn push(&mut self, chunk: &[u8]) {
        let room = meshrmm_protocol::MAX_SCRIPT_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.truncated = true;
        }
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
}

/// Output bytes as text: UTF-8, which both interpreters are asked to write,
/// else `fallback`'s decoding, for programs that write the OEM code page.
/// A character cut at the end by the output limit is dropped.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn decode_output(bytes: &[u8], fallback: impl Fn(&[u8]) -> String) -> String {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(error) if error.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned()
        }
        Err(_) => fallback(bytes),
    }
}

/// `name`, or `name (2)`, `name (3)`… before its extension, whichever
/// `taken` says is free.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    if !taken(name) {
        return Some(name.to_owned());
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    (2..1000)
        .map(|number| format!("{stem} ({number}){extension}"))
        .find(|candidate| !taken(candidate))
}

/// Run IDs name a directory, so they must be plain.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(windows)]
use self::windows as platform;

#[cfg(any(windows, target_os = "macos"))]
mod shared {
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use anyhow::Context;
    use meshrmm_protocol::{
        FileDeliveryReport, FileDeliveryRequest, FileDeliveryStatus, ScriptRunReport,
        ScriptRunRequest, ScriptRunStatus,
    };
    use sha2::{Digest, Sha256};

    use super::{MAX_RUNNING_DELIVERIES, MAX_RUNNING_SCRIPTS, platform};
    use crate::remote::config::{Config, ExecutionMode};

    static RUNNING_SCRIPTS: AtomicUsize = AtomicUsize::new(0);
    static RUNNING_DELIVERIES: AtomicUsize = AtomicUsize::new(0);
    /// Reports are retried this many times, a little longer apart each time.
    const REPORT_ATTEMPTS: u32 = 6;
    const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

    /// A place among a limited number of concurrent jobs, given back on drop.
    struct Slot(&'static AtomicUsize);

    impl Slot {
        fn take(counter: &'static AtomicUsize, limit: usize) -> Option<Self> {
            counter
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |running| {
                    (running < limit).then_some(running + 1)
                })
                .ok()
                .map(|_| Self(counter))
        }
    }

    impl Drop for Slot {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Runs `run` on its own thread and reports how it went.
    pub(crate) fn run_script(config: &Config, mode: ExecutionMode, run: ScriptRunRequest) {
        let config = config.clone();
        let slot = Slot::take(&RUNNING_SCRIPTS, MAX_RUNNING_SCRIPTS);
        let spawned = std::thread::Builder::new()
            .name("meshrmm-toolbox-script".into())
            .spawn(move || {
                let report = match slot {
                    _ if !platform::runs(run.language) => failed(
                        platform::own_account(),
                        format!(
                            "{} scripts do not run on this device's operating system.",
                            run.language.label()
                        ),
                    ),
                    Some(_slot) => platform::execute_script(mode, &run),
                    None => failed(
                        platform::own_account(),
                        format!(
                            "The device is already running {MAX_RUNNING_SCRIPTS} toolbox scripts. Try again when one finishes."
                        ),
                    ),
                };
                tracing::info!(
                    run_id = %run.run_id,
                    status = report.status.as_str(),
                    exit_code = ?report.exit_code,
                    ran_as = %report.ran_as,
                    error = ?report.error,
                    "toolbox script finished"
                );
                send_report(&config, &["script-runs", &run.run_id, "result"], &report);
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for a toolbox script");
        }
    }

    /// Saves `delivery`'s file on its own thread and reports how it went.
    pub(crate) fn deliver_file(
        config: &Config,
        mode: ExecutionMode,
        delivery: FileDeliveryRequest,
    ) {
        let config = config.clone();
        let slot = Slot::take(&RUNNING_DELIVERIES, MAX_RUNNING_DELIVERIES);
        let spawned = std::thread::Builder::new()
            .name("meshrmm-toolbox-file".into())
            .spawn(move || {
                let result = match slot {
                    Some(_slot) => platform::save_delivery(&config, mode, &delivery),
                    None => Err(anyhow::anyhow!(
                        "The device is already receiving {MAX_RUNNING_DELIVERIES} toolbox files. Try again when one finishes."
                    )),
                };
                let report = match result {
                    Ok(path) => {
                        tracing::info!(delivery_id = %delivery.delivery_id, path = %path.display(), "saved a toolbox file");
                        FileDeliveryReport {
                            status: FileDeliveryStatus::Delivered,
                            path: Some(path.display().to_string()),
                            error: None,
                        }
                    }
                    Err(error) => {
                        tracing::warn!(delivery_id = %delivery.delivery_id, error = ?error, "could not save a toolbox file");
                        FileDeliveryReport {
                            status: FileDeliveryStatus::Failed,
                            path: None,
                            error: Some(format!("{error:#}")),
                        }
                    }
                };
                send_report(
                    &config,
                    &["file-deliveries", &delivery.delivery_id, "result"],
                    &report,
                );
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a thread for a toolbox file");
        }
    }

    pub(super) fn failed(ran_as: String, error: impl Into<String>) -> ScriptRunReport {
        ScriptRunReport {
            status: ScriptRunStatus::Failed,
            ran_as,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            output_truncated: false,
            error: Some(error.into()),
        }
    }

    /// A downloaded file waiting to be saved, removed with it.
    pub(super) struct Staged(pub(super) PathBuf);

    impl Drop for Staged {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Downloads the file into an administrator-only folder, checking its
    /// size and SHA-256 on the way.
    pub(super) fn download(
        config: &Config,
        mode: ExecutionMode,
        delivery: &FileDeliveryRequest,
    ) -> anyhow::Result<Staged> {
        let folder = platform::staging_folder(mode)?;
        let staged = Staged(folder.join(format!("{}.partial", delivery.delivery_id)));
        let url = meshrmm_signaling_client::endpoint_url(
            &config.server,
            &[
                "v1",
                "agents",
                &config.device_id,
                "file-deliveries",
                &delivery.delivery_id,
                "content",
            ],
            &[],
            false,
        )?;
        let response = http(DOWNLOAD_TIMEOUT)
            .get(url.as_str())
            .header("Authorization", &format!("Bearer {}", config.agent_token))
            .call()
            .context("the server did not provide the file")?;
        let mut body = response
            .into_body()
            .into_with_config()
            .limit(delivery.size_bytes.saturating_add(1))
            .reader();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged.0)
            .with_context(|| format!("could not create {}", staged.0.display()))?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        let mut chunk = vec![0; 64 * 1024];
        loop {
            let read = body
                .read(&mut chunk)
                .context("the download was interrupted")?;
            if read == 0 {
                break;
            }
            size += read as u64;
            anyhow::ensure!(
                size <= delivery.size_bytes,
                "the server sent more than the file's size"
            );
            digest.update(&chunk[..read]);
            file.write_all(&chunk[..read])
                .with_context(|| format!("could not write {}", staged.0.display()))?;
        }
        file.flush()?;
        anyhow::ensure!(size == delivery.size_bytes, "the download was incomplete");
        let actual: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        anyhow::ensure!(
            actual == delivery.sha256,
            "the download did not match the file's SHA-256"
        );
        Ok(staged)
    }

    fn http(timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(true)
            .tls_config(crate::enrollment::https_tls_config())
            .build()
            .new_agent()
    }

    /// Posts a result to `/v1/agents/{device}/{path}`, retrying while the
    /// server cannot be reached. A refusal is final.
    fn send_report(config: &Config, path: &[&str], report: &impl serde::Serialize) {
        let mut segments = vec!["v1", "agents", config.device_id.as_str()];
        segments.extend_from_slice(path);
        let url =
            match meshrmm_signaling_client::endpoint_url(&config.server, &segments, &[], false) {
                Ok(url) => url,
                Err(error) => {
                    tracing::error!(%error, "could not build the toolbox report URL");
                    return;
                }
            };
        let http = http(Duration::from_secs(60));
        for attempt in 1..=REPORT_ATTEMPTS {
            match http
                .post(url.as_str())
                .header("Authorization", &format!("Bearer {}", config.agent_token))
                .send_json(report)
            {
                Ok(_) => return,
                Err(ureq::Error::StatusCode(status)) => {
                    tracing::warn!(status, url = %url, "the server refused a toolbox report");
                    return;
                }
                Err(error) if attempt < REPORT_ATTEMPTS => {
                    tracing::warn!(%error, attempt, "could not send a toolbox report; retrying");
                    std::thread::sleep(Duration::from_secs(2_u64.pow(attempt)));
                }
                Err(error) => {
                    tracing::error!(%error, url = %url, "could not send a toolbox report");
                }
            }
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) use self::shared::{deliver_file, run_script};
#[cfg(any(windows, target_os = "macos"))]
use self::shared::{download, failed};

#[cfg(windows)]
mod windows;

/// Scripts and files on a Mac. Scripts run with zsh, as root or as the user
/// signed in on the console, in their own process group so a timeout stops
/// everything they started. Files go to the user's Documents transfer folder,
/// written as the user, or to the shared folder when nobody is signed in.
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
use self::macos as platform;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    fn run_here(body: &str, timeout_seconds: u32) -> meshrmm_protocol::ScriptRunReport {
        macos::execute_script(
            crate::remote::config::ExecutionMode::Console,
            &meshrmm_protocol::ScriptRunRequest {
                run_id: format!("test-{}-{timeout_seconds}", std::process::id()),
                language: ScriptLanguage::Shell,
                body: body.into(),
                run_as: meshrmm_protocol::RunAs::User,
                timeout_seconds,
            },
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_scripts_run_in_zsh_and_report_their_output() {
        let report = run_here(
            "print -r -- héllo\r\nread line || echo no input >&2\nexit 3\n",
            30,
        );
        assert_eq!(report.status, meshrmm_protocol::ScriptRunStatus::Completed);
        assert_eq!(report.exit_code, Some(3));
        assert_eq!(report.stdout, "héllo\n");
        assert_eq!(report.stderr, "no input\n", "the script gets no input");
        assert!(!report.ran_as.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_scripts_that_overrun_are_stopped_with_what_they_started() {
        let started = std::time::Instant::now();
        let report = run_here("sleep 60 &\necho started\nsleep 60\n", 1);
        assert_eq!(report.status, meshrmm_protocol::ScriptRunStatus::TimedOut);
        assert_eq!(report.stdout, "started\n");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the background sleep does not hold the output open"
        );
    }

    #[test]
    fn script_files_use_crlf_and_the_interpreters_encoding() {
        let (name, bytes) = script_file(ScriptLanguage::Powershell, "Write-Output 'é'\nexit 3");
        assert_eq!(name, "script.ps1");
        assert_eq!(
            bytes,
            "\u{feff}Write-Output 'é'\r\nexit 3".as_bytes(),
            "PowerShell 5.1 needs the byte-order mark to read UTF-8"
        );
        let (name, bytes) = script_file(ScriptLanguage::Cmd, "@echo off\r\necho hi\n");
        assert_eq!(name, "script.cmd");
        assert_eq!(bytes, b"@chcp 65001>nul\r\n@echo off\r\necho hi\r\n");
    }

    #[test]
    fn interpreters_get_the_script_quoted() {
        let powershell =
            interpreter_arguments(ScriptLanguage::Powershell, r"C:\Users\O'Brien\script.ps1");
        assert!(
            powershell.contains(r"& 'C:\Users\O''Brien\script.ps1'; exit $LASTEXITCODE"),
            "{powershell}"
        );
        assert!(
            powershell.starts_with(
                "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \""
            )
        );
        assert_eq!(
            interpreter_arguments(ScriptLanguage::Cmd, r"C:\Program Data\script.cmd"),
            r#"/D /S /C ""C:\Program Data\script.cmd"""#
        );
    }

    #[test]
    fn output_stops_at_the_limit() {
        let mut output = Output::default();
        output.push(&vec![b'a'; meshrmm_protocol::MAX_SCRIPT_OUTPUT_BYTES - 1]);
        assert!(!output.truncated);
        output.push(b"bc");
        assert!(output.truncated);
        assert_eq!(
            output.bytes.len(),
            meshrmm_protocol::MAX_SCRIPT_OUTPUT_BYTES
        );
        assert_eq!(output.bytes.last(), Some(&b'b'));
        output.push(b"d");
        assert_eq!(
            output.bytes.len(),
            meshrmm_protocol::MAX_SCRIPT_OUTPUT_BYTES
        );
    }

    #[test]
    fn output_is_utf8_or_the_fallback() {
        let fallback = |_: &[u8]| "fallback".to_owned();
        assert_eq!(decode_output("héllo".as_bytes(), fallback), "héllo");
        assert_eq!(decode_output(b"\xef\xbb\xbfbom", fallback), "bom");
        // A character cut by the limit is dropped, not a reason to fall back.
        assert_eq!(decode_output(b"ab\xc3", fallback), "ab");
        assert_eq!(decode_output(b"a\x82b", fallback), "fallback");
    }

    #[test]
    fn saved_files_get_a_free_name() {
        let taken = ["setup.exe", "setup (2).exe", "notes"];
        let taken = |name: &str| taken.contains(&name);
        assert_eq!(
            unique_name("other.exe", taken).as_deref(),
            Some("other.exe")
        );
        assert_eq!(
            unique_name("setup.exe", taken).as_deref(),
            Some("setup (3).exe")
        );
        assert_eq!(unique_name("notes", taken).as_deref(), Some("notes (2)"));
        assert_eq!(
            unique_name(".profile", |_| false).as_deref(),
            Some(".profile")
        );
        assert_eq!(
            unique_name(".profile", |name| name == ".profile").as_deref(),
            Some(".profile (2)")
        );
        assert_eq!(unique_name("full", |_| true), None);
    }

    #[test]
    fn ids_cannot_leave_their_directory() {
        assert!(valid_id("0f8a3c52-6c1e-4f1a-9a9e-2b0c7f1d2e3a"));
        for id in ["", "..", "a/b", r"a\b", "a.b", &"a".repeat(65)] {
            assert!(!valid_id(id), "{id}");
        }
    }
}
