//! What a peer may make the viewer store through clipboard copies the
//! technician did not ask for. The device may be the machine under
//! investigation, so whatever runs there decides when its side copies files;
//! the viewer takes a few copies a minute and a few GiB every ten minutes,
//! and refuses the rest before any of their data is accepted.
use std::{
    collections::VecDeque,
    fmt,
    time::{Duration, Instant},
};

use crate::mebibytes;

#[derive(Clone, Copy, Debug)]
pub(super) struct ClipboardLimits {
    /// Copies admitted within `transfer_window`.
    pub(super) transfers: usize,
    pub(super) transfer_window: Duration,
    /// Bytes the copies admitted within `byte_window` announced.
    pub(super) bytes: u64,
    pub(super) byte_window: Duration,
    /// Free space a copy must leave on the cache's volume.
    pub(super) free_floor: u64,
}

/// A person copying files on the device copies a few times a minute at
/// most; six in any minute leaves room for copying again after a mistake.
/// Four copies of the largest size, [`crate::CLIPBOARD_LIMIT_BYTES`], fit
/// in any ten minutes. An unsolicited copy also leaves 1 GiB free, more
/// than the 256 MiB a transfer the technician asked for must leave, so it
/// cannot take the space the technician's own programs need.
pub(super) const CLIPBOARD_LIMITS: ClipboardLimits = ClipboardLimits {
    transfers: 6,
    transfer_window: Duration::from_secs(60),
    bytes: 2 * 1024 * 1024 * 1024,
    byte_window: Duration::from_secs(10 * 60),
    free_floor: 1024 * 1024 * 1024,
};

/// A transfer this side refused.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Refusal {
    /// Sent to the peer.
    pub(super) reason: String,
    /// Shown in this side's status line, when it differs from the reason.
    pub(super) notice: Option<String>,
}
impl From<&'static str> for Refusal {
    fn from(reason: &'static str) -> Self {
        Self {
            reason: reason.into(),
            notice: None,
        }
    }
}
impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}
impl std::error::Error for Refusal {}

fn span(window: Duration) -> String {
    match window.as_secs() {
        60 => "a minute".into(),
        secs if secs % 60 == 0 => format!("{} minutes", secs / 60),
        secs => format!("{secs} seconds"),
    }
}

const NOT_FETCHED: &str = "Files copied on the remote device were not fetched";

/// The clipboard copies a viewer admitted recently.
pub(super) struct ClipboardBudget {
    limits: ClipboardLimits,
    /// When each copy was admitted, oldest first, and the bytes it announced
    /// once its totals arrived.
    admitted: VecDeque<(Instant, Option<u64>)>,
}
impl ClipboardBudget {
    pub(super) fn new(limits: ClipboardLimits) -> Self {
        Self {
            limits,
            admitted: VecDeque::new(),
        }
    }

    fn recent(&self, now: Instant, window: Duration) -> impl Iterator<Item = Option<u64>> + '_ {
        self.admitted
            .iter()
            .filter(move |(at, _)| now.saturating_duration_since(*at) < window)
            .map(|(_, bytes)| *bytes)
    }

    fn spent(&self, now: Instant) -> u64 {
        self.recent(now, self.limits.byte_window)
            .flatten()
            .fold(0, u64::saturating_add)
    }

    fn too_often(&self) -> Refusal {
        let (count, window) = (self.limits.transfers, span(self.limits.transfer_window));
        Refusal {
            reason: format!(
                "The technician's viewer takes files copied here at most {count} times in {window}; \
                 copy them again in a moment, or the technician can use Receive files"
            ),
            notice: Some(format!(
                "{NOT_FETCHED}: copies from it are limited to {count} in {window}. \
                 Use Receive files to get them"
            )),
        }
    }

    fn too_much(&self) -> Refusal {
        let (bytes, window) = (mebibytes(self.limits.bytes), span(self.limits.byte_window));
        Refusal {
            reason: format!(
                "The technician's viewer takes at most {bytes} of files copied here in {window}; \
                 copy them again later, or the technician can use Receive files"
            ),
            notice: Some(format!(
                "{NOT_FETCHED}: copies from it are limited to {bytes} in {window}. \
                 Use Receive files to get them"
            )),
        }
    }

    /// A new copy begins: refused while the copies of the last
    /// `transfer_window` reached their count, or those of the last
    /// `byte_window` their bytes.
    pub(super) fn begin(&mut self, now: Instant) -> Result<(), Refusal> {
        let keep = self.limits.transfer_window.max(self.limits.byte_window);
        while self
            .admitted
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= keep)
        {
            self.admitted.pop_front();
        }
        if self.recent(now, self.limits.transfer_window).count() >= self.limits.transfers {
            return Err(self.too_often());
        }
        if self.spent(now) >= self.limits.bytes {
            return Err(self.too_much());
        }
        self.admitted.push_back((now, None));
        Ok(())
    }

    /// The copy that began last announced `bytes`: refused if they would
    /// leave less than the floor free (when `free` can tell) or take the
    /// recent copies past their bytes. Announced bytes count whether or not
    /// they then arrive, so failing a copy halfway does not make room for
    /// another. Only a copy's first totals count; the receiver fails a
    /// transfer that announces them twice.
    pub(super) fn totals(
        &mut self,
        bytes: u64,
        free: impl FnOnce() -> Option<u64>,
        now: Instant,
    ) -> Result<(), Refusal> {
        if !matches!(self.admitted.back(), Some((_, None))) {
            return Ok(());
        }
        if let Some(free) = free()
            && free < bytes.saturating_add(self.limits.free_floor)
        {
            return Err(Refusal {
                reason: format!(
                    "The technician's computer is low on disk space ({} free), \
                     so the copied files were not sent",
                    mebibytes(free)
                ),
                notice: Some(format!(
                    "{NOT_FETCHED}: they need {} and {} is free, which would leave less than {}",
                    mebibytes(bytes),
                    mebibytes(free),
                    mebibytes(self.limits.free_floor)
                )),
            });
        }
        if self.spent(now).saturating_add(bytes) > self.limits.bytes {
            return Err(self.too_much());
        }
        if let Some((_, announced)) = self.admitted.back_mut() {
            *announced = Some(bytes);
        }
        Ok(())
    }
}
