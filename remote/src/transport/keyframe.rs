//! The one limiter every keyframe request of the viewer passes through:
//! network loss, the presenter's recovery, a new stream and a recording all
//! ask for the same keyframe. A keyframe is a stream's largest frame, so
//! each needless one adds to the congestion that often caused the loss.

use meshrmm_protocol::VideoStreamId;

/// The least time between two requests for a stream's keyframe.
pub(super) const MIN_INTERVAL_US: u64 = 250_000;
/// How long a request may go unanswered before it is sent again. A
/// keyframe can take a while to arrive on a congested link; asking again
/// sooner would only add another one behind it.
pub(super) const RETRY_AFTER_US: u64 = 1_000_000;

/// Whether a stream needs a keyframe, and when one was last asked for.
#[derive(Debug, Default)]
pub(super) struct KeyframeRequests {
    stream_id: Option<VideoStreamId>,
    last_sent_us: Option<u64>,
    /// A request was sent and no keyframe has arrived since.
    outstanding: bool,
    /// A keyframe is needed and none has arrived since.
    wanted: bool,
}

impl KeyframeRequests {
    /// Notes that `stream_id` needs a keyframe, and returns whether to send
    /// the request now. The first request for a stream always goes; one that
    /// cannot go yet waits for [`Self::poll`].
    pub(super) fn request(&mut self, stream_id: VideoStreamId, now_us: u64) -> bool {
        if self.stream_id != Some(stream_id) {
            *self = Self {
                stream_id: Some(stream_id),
                ..Self::default()
            };
        }
        self.wanted = true;
        self.due(now_us)
    }

    /// The stream to send a waiting or unanswered request for, once one is
    /// due.
    pub(super) fn poll(&mut self, now_us: u64) -> Option<VideoStreamId> {
        if self.wanted && self.due(now_us) {
            self.stream_id
        } else {
            None
        }
    }

    /// A keyframe of `stream_id` arrived: it answers every request so far.
    pub(super) fn keyframe_arrived(&mut self, stream_id: VideoStreamId) {
        if self.stream_id == Some(stream_id) {
            self.wanted = false;
            self.outstanding = false;
        }
    }

    fn due(&mut self, now_us: u64) -> bool {
        let wait_us = if self.outstanding {
            RETRY_AFTER_US
        } else {
            MIN_INTERVAL_US
        };
        if self
            .last_sent_us
            .is_some_and(|last| now_us.saturating_sub(last) < wait_us)
        {
            return false;
        }
        self.last_sent_us = Some(now_us);
        self.outstanding = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: VideoStreamId = VideoStreamId(3);

    #[test]
    fn the_first_request_goes_at_once() {
        let mut requests = KeyframeRequests::default();
        assert!(requests.request(STREAM, 1_000));
    }

    #[test]
    fn a_burst_of_loss_within_the_interval_sends_one_request() {
        let mut requests = KeyframeRequests::default();
        let sent = (0..20)
            .filter(|step| requests.request(STREAM, 1_000 + step * 12_000))
            .count();
        assert_eq!(sent, 1);
        // Nothing more goes until the request has had time to be answered.
        assert_eq!(requests.poll(1_000 + MIN_INTERVAL_US), None);
    }

    #[test]
    fn an_unanswered_request_is_retried() {
        let mut requests = KeyframeRequests::default();
        assert!(requests.request(STREAM, 1_000));
        assert_eq!(requests.poll(1_000 + RETRY_AFTER_US - 1), None);
        assert_eq!(requests.poll(1_000 + RETRY_AFTER_US), Some(STREAM));
        assert_eq!(requests.poll(1_000 + RETRY_AFTER_US + 1), None);
    }

    #[test]
    fn a_keyframe_answers_the_outstanding_request() {
        let mut requests = KeyframeRequests::default();
        assert!(requests.request(STREAM, 1_000));
        requests.keyframe_arrived(STREAM);
        // No retry once it came.
        assert_eq!(requests.poll(1_000 + 2 * RETRY_AFTER_US), None);
        // The next loss may ask again once the interval has passed.
        assert!(!requests.request(STREAM, 1_000 + MIN_INTERVAL_US - 1));
        requests.keyframe_arrived(STREAM);
        assert!(requests.request(STREAM, 1_000 + MIN_INTERVAL_US));
    }

    #[test]
    fn a_request_too_soon_after_a_keyframe_waits_for_the_interval() {
        let mut requests = KeyframeRequests::default();
        assert!(requests.request(STREAM, 1_000));
        requests.keyframe_arrived(STREAM);
        assert!(!requests.request(STREAM, 100_000));
        assert_eq!(requests.poll(1_000 + MIN_INTERVAL_US - 1), None);
        assert_eq!(requests.poll(1_000 + MIN_INTERVAL_US), Some(STREAM));
    }

    #[test]
    fn a_keyframe_of_another_stream_answers_nothing() {
        let mut requests = KeyframeRequests::default();
        assert!(requests.request(STREAM, 1_000));
        requests.keyframe_arrived(VideoStreamId(4));
        assert_eq!(requests.poll(1_000 + RETRY_AFTER_US), Some(STREAM));
    }

    #[test]
    fn a_new_stream_asks_at_once() {
        let mut requests = KeyframeRequests::default();
        assert!(requests.request(STREAM, 1_000));
        assert!(requests.request(VideoStreamId(4), 2_000));
        // The old stream's request is gone.
        requests.keyframe_arrived(VideoStreamId(4));
        assert_eq!(requests.poll(2_000 + 2 * RETRY_AFTER_US), None);
    }
}
