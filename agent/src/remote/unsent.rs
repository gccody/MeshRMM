//! The update or application report a control connection failed to send,
//! which the next connection sends first, before anything newer from the
//! same source.
//!
//! A report the connection can never get through, such as one the server
//! drops the connection over, would then take down every connection in turn
//! and hold up everything behind it, and the device would flap between
//! online and offline for good. So once [`ATTEMPTS`] connections in a row
//! have failed to send it, it is given up on. The server treats a report
//! given up on as one that never came: an update run it asked for is lost
//! after its report window, an application run as soon as the next
//! connection's activity leaves it out, and scheduled scans come again.

use meshrmm_protocol::AgentStatusMessage;

use crate::remote::service_link::ActivityGuard;

/// How many connections in a row may fail to send a report. More than one,
/// so a connection that drops for some other reason while the report goes
/// out doesn't cost it; few, because each failure takes the device offline
/// until it reconnects.
pub(crate) const ATTEMPTS: u32 = 3;

/// A report for the server, and how many connections in a row have failed
/// to send it.
#[derive(Debug)]
pub(crate) struct Report {
    pub(crate) message: AgentStatusMessage,
    failures: u32,
    /// Keeps automatic updates waiting until the report is written or given
    /// up on, as the job it answers did while it ran.
    _activity: Option<ActivityGuard>,
}

impl Report {
    pub(crate) fn new(message: AgentStatusMessage) -> Self {
        Self::answering(message, None)
    }

    /// The result of the job `activity` was held for since it was accepted.
    pub(crate) fn answering(message: AgentStatusMessage, activity: Option<ActivityGuard>) -> Self {
        if let Some(activity) = &activity {
            activity.finished();
        }
        Self {
            message,
            failures: 0,
            _activity: activity,
        }
    }
}

/// The report the last connection failed to send, if it is still kept.
#[derive(Debug, Default)]
pub(crate) struct Unsent(Option<Report>);

impl Unsent {
    /// The report to send first.
    pub(crate) fn take(&mut self) -> Option<Report> {
        self.0.take()
    }

    /// A connection failed to send `report`, or ended before it was
    /// written. It is kept to go first on the next connection, unless this
    /// was its last attempt: then it is returned, given up on.
    pub(crate) fn failed(&mut self, mut report: Report) -> Option<AgentStatusMessage> {
        report.failures += 1;
        if report.failures >= ATTEMPTS {
            return Some(report.message);
        }
        self.0 = Some(report);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(run: &str) -> AgentStatusMessage {
        AgentStatusMessage::ApplicationActivity {
            run_ids: vec![run.into()],
        }
    }

    #[test]
    fn a_report_goes_first_until_it_has_failed_on_every_attempt() {
        let mut unsent = Unsent::default();
        let mut report = Report::new(message("a"));
        for _ in 1..ATTEMPTS {
            assert_eq!(unsent.failed(report), None);
            report = unsent.take().expect("kept to go first");
            assert_eq!(report.message, message("a"));
        }
        assert_eq!(unsent.failed(report), Some(message("a")));
        assert!(unsent.take().is_none(), "given up on, not sent first again");
    }

    #[test]
    fn a_new_report_gets_every_attempt() {
        let mut unsent = Unsent::default();
        let report = Report::new(message("a"));
        assert_eq!(unsent.failed(report), None);
        drop(unsent.take());
        // The next report from the same source counts from nothing.
        let mut report = Report::new(message("b"));
        for _ in 1..ATTEMPTS {
            assert_eq!(unsent.failed(report), None);
            report = unsent.take().unwrap();
        }
        assert_eq!(unsent.failed(report), Some(message("b")));
    }
}
