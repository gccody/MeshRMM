//! PLACEHOLDER: replaced by the platform implementation.
use meshrmm_protocol::{PendingUpdate, UpdateInstallItem, UpdateManager, UpdateScan};

use super::Progress;

pub(super) fn scan() -> UpdateScan {
    UpdateScan {
        error: Some("Scanning for updates is not available yet.".into()),
        ..Default::default()
    }
}

pub(super) fn install(
    _manager: UpdateManager,
    _chosen: &[&PendingUpdate],
    _everything: bool,
    _progress: &Progress,
) -> anyhow::Result<Vec<UpdateInstallItem>> {
    anyhow::bail!("Installing updates is not available yet.")
}
