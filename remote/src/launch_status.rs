//! What the viewer is waiting on before the remote desktop appears, shown in
//! its connecting window so a slow launch explains itself.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Download progress is shown at most this often, and on completion.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchStatus {
    /// Windows allows one viewer per device and waits for the open one to end.
    #[cfg_attr(not(windows), allow(dead_code))]
    ClosingPreviousViewer,
    RequestingSession,
    CheckingForUpdate,
    DownloadingUpdate {
        version: String,
        downloaded: u64,
        total: Option<u64>,
    },
    InstallingUpdate {
        version: String,
    },
    WaitingForRemoteComputer,
    EstablishingConnection,
    StartingDisplay,
}

impl LaunchStatus {
    pub fn message(&self) -> String {
        match self {
            Self::ClosingPreviousViewer => {
                "Closing the viewer already open for this computer…".to_owned()
            }
            Self::RequestingSession => "Requesting a remote session…".to_owned(),
            Self::CheckingForUpdate => "Checking for a viewer update…".to_owned(),
            Self::DownloadingUpdate {
                version,
                downloaded,
                total: Some(total),
            } if *total > 0 => format!(
                "Downloading viewer update {version}… {}% ({} of {})",
                downloaded.saturating_mul(100) / total,
                megabytes(*downloaded),
                megabytes(*total)
            ),
            Self::DownloadingUpdate {
                version,
                downloaded,
                ..
            } => format!(
                "Downloading viewer update {version}… {}",
                megabytes(*downloaded)
            ),
            Self::InstallingUpdate { version } => {
                format!("Installing viewer update {version}; the viewer restarts when it is done…")
            }
            Self::WaitingForRemoteComputer => {
                "Waiting for the remote computer to respond…".to_owned()
            }
            Self::EstablishingConnection => "Establishing a secure connection…".to_owned(),
            Self::StartingDisplay => "Starting the remote display…".to_owned(),
        }
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

struct Shown {
    status: LaunchStatus,
    at: Instant,
    phase_started: Instant,
}

/// The status last shown, until the remote desktop appears.
enum Launch {
    Starting(Option<Shown>),
    Finished,
}

static LAUNCH: Mutex<Launch> = Mutex::new(Launch::Starting(None));

/// Shows `status` in the connecting window and logs each phase, so a slow
/// launch can be diagnosed afterwards. Does nothing once the remote desktop
/// has appeared: reconnecting shows in the session window instead.
pub fn report(status: LaunchStatus) {
    let now = Instant::now();
    {
        let mut launch = LAUNCH.lock().unwrap_or_else(|error| error.into_inner());
        let Launch::Starting(shown) = &mut *launch else {
            return;
        };
        let phase_started = match shown.as_ref() {
            Some(previous) if previous.status == status => return,
            Some(previous)
                if std::mem::discriminant(&previous.status) == std::mem::discriminant(&status) =>
            {
                if !progress_due(&status, now.duration_since(previous.at)) {
                    return;
                }
                previous.phase_started
            }
            previous => {
                tracing::info!(
                    status = %status.message(),
                    previous_phase_ms = previous.map(|previous| elapsed_ms(previous, now)),
                    "viewer launch status"
                );
                now
            }
        };
        *shown = Some(Shown {
            status: status.clone(),
            at: now,
            phase_started,
        });
    }
    #[cfg(any(windows, target_os = "macos"))]
    crate::platform::show_launch_status(status.message());
}

/// Records that the remote desktop appeared, ending the launch.
pub fn finish() {
    let mut launch = LAUNCH.lock().unwrap_or_else(|error| error.into_inner());
    if let Launch::Starting(shown) = std::mem::replace(&mut *launch, Launch::Finished) {
        tracing::info!(
            previous_phase_ms = shown
                .as_ref()
                .map(|previous| elapsed_ms(previous, Instant::now())),
            "viewer launch finished"
        );
    }
}

fn elapsed_ms(shown: &Shown, now: Instant) -> u64 {
    u64::try_from(now.duration_since(shown.phase_started).as_millis()).unwrap_or(u64::MAX)
}

/// Whether a phase that only changed its details is redrawn: download
/// progress is throttled, except for its final update.
fn progress_due(next: &LaunchStatus, elapsed: Duration) -> bool {
    match next {
        LaunchStatus::DownloadingUpdate {
            downloaded, total, ..
        } => Some(*downloaded) == *total || elapsed >= PROGRESS_INTERVAL,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn downloading(downloaded: u64, total: Option<u64>) -> LaunchStatus {
        LaunchStatus::DownloadingUpdate {
            version: "0.3.1".to_owned(),
            downloaded,
            total,
        }
    }

    #[test]
    fn download_progress_shows_percentage_when_the_size_is_known() {
        assert_eq!(
            downloading(13_500_000, Some(27_000_000)).message(),
            "Downloading viewer update 0.3.1… 50% (13.5 MB of 27.0 MB)"
        );
        assert_eq!(
            downloading(2_000_000, None).message(),
            "Downloading viewer update 0.3.1… 2.0 MB"
        );
        assert_eq!(
            downloading(0, Some(0)).message(),
            "Downloading viewer update 0.3.1… 0.0 MB"
        );
    }

    #[test]
    fn download_progress_is_throttled_except_for_completion() {
        assert!(!progress_due(
            &downloading(2, Some(10)),
            Duration::from_millis(10)
        ));
        assert!(progress_due(&downloading(2, Some(10)), PROGRESS_INTERVAL));
        assert!(progress_due(&downloading(10, Some(10)), Duration::ZERO));
    }
}
