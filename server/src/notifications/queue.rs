//! Each channel's waiting notifications, and the one worker per channel that
//! delivers them.
//!
//! [`Queues::push`] only adds to a channel's queue; the first notification
//! for an idle channel starts its worker, which runs until nothing is
//! waiting or being delivered. A channel holds at most [`MAX_WAITING`]
//! notifications; beyond that new ones are dropped, and the channel's
//! `last_delivery_error` says how many. The worker reads the channel as it
//! goes, so a channel that is turned off or deleted loses what was waiting.
//!
//! An email worker waits [`SETTLE`] before its first batch, then takes
//! everything waiting at once (see [`email::send`]). A webhook worker keeps
//! up to [`WEBHOOK_IN_FLIGHT`] requests open, starting them in dispatch
//! order; a delivery waiting for a retry gives its place to the next one.
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::{sync::Notify, time::Instant};

use super::{
    Channel, Failure, Kind, Notification, attempt_delivery, by_id, email, record, webhook,
};
use crate::http::AppState;

/// The most notifications a channel holds, waiting or being delivered.
pub const MAX_WAITING: usize = 10_000;
/// How long an email worker waits before its first batch, so a burst of
/// notifications arriving over a moment becomes one digest.
pub const SETTLE: Duration = Duration::from_secs(1);
/// The most webhook requests a channel has open at once.
pub const WEBHOOK_IN_FLIGHT: usize = 4;
/// How often a worker writes how its deliveries went to the channel, at
/// most, and reads the channel again while it delivers webhooks.
const RECORD_EVERY: Duration = Duration::from_secs(1);

/// Every channel's queue. A channel has one while its worker runs.
#[derive(Debug, Clone, Default)]
pub struct Queues {
    inner: Arc<Mutex<HashMap<String, Waiting>>>,
}

#[derive(Debug, Default)]
struct Waiting {
    notifications: VecDeque<Arc<Notification>>,
    /// Taken by the worker and not finished: being sent or waiting for a
    /// retry.
    taken: usize,
    /// Dropped because the queue was full, not yet reported by the worker.
    dropped: u64,
    /// Whether this worker's drops have been logged.
    warned: bool,
    /// Woken when something is added.
    added: Arc<Notify>,
}

impl Queues {
    /// Adds `notification` to each channel's queue and returns the channels
    /// whose worker must be started.
    pub fn push(&self, channel_ids: &[String], notification: &Arc<Notification>) -> Vec<String> {
        let mut queues = self.inner.lock().unwrap();
        let mut start = Vec::new();
        for channel_id in channel_ids {
            let waiting = queues.entry(channel_id.clone()).or_insert_with(|| {
                start.push(channel_id.clone());
                Waiting::default()
            });
            if waiting.notifications.len() + waiting.taken >= MAX_WAITING {
                waiting.dropped += 1;
                if !waiting.warned {
                    waiting.warned = true;
                    tracing::warn!(
                        channel_id,
                        waiting = MAX_WAITING,
                        "a notification channel is full; dropping new notifications until it catches up"
                    );
                }
                continue;
            }
            waiting.notifications.push_back(notification.clone());
            waiting.added.notify_one();
        }
        start
    }

    /// Takes up to `most` of the channel's notifications, oldest first.
    fn take(&self, channel_id: &str, most: usize) -> Vec<Arc<Notification>> {
        let mut queues = self.inner.lock().unwrap();
        let Some(waiting) = queues.get_mut(channel_id) else {
            return Vec::new();
        };
        let count = most.min(waiting.notifications.len());
        waiting.taken += count;
        waiting.notifications.drain(..count).collect()
    }

    /// Marks `count` taken notifications as done with.
    fn finished(&self, channel_id: &str, count: usize) {
        if let Some(waiting) = self.inner.lock().unwrap().get_mut(channel_id) {
            waiting.taken -= count;
        }
    }

    /// Forgets what is waiting, for a channel that is off or gone.
    fn discard(&self, channel_id: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .get_mut(channel_id)
            .map(|waiting| std::mem::take(&mut waiting.notifications).len())
            .unwrap_or_default()
    }

    /// How many were dropped since the last call.
    fn take_dropped(&self, channel_id: &str) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .get_mut(channel_id)
            .map(|waiting| std::mem::take(&mut waiting.dropped))
            .unwrap_or_default()
    }

    fn added(&self, channel_id: &str) -> Arc<Notify> {
        self.inner
            .lock()
            .unwrap()
            .get(channel_id)
            .map(|waiting| waiting.added.clone())
            .unwrap_or_default()
    }

    /// Removes the channel's queue if nothing is left in it, so the next
    /// notification starts a new worker. Returns whether it did.
    fn stop_if_idle(&self, channel_id: &str) -> bool {
        let mut queues = self.inner.lock().unwrap();
        let idle = queues.get(channel_id).is_none_or(|waiting| {
            waiting.notifications.is_empty() && waiting.taken == 0 && waiting.dropped == 0
        });
        if idle {
            queues.remove(channel_id);
        }
        idle
    }
}

/// Delivers the channel's notifications until none are left.
pub(super) async fn run(state: AppState, channel_id: String) {
    let mut recorder = Recorder::default();
    let mut settled = false;
    loop {
        let channel = match by_id(&mut &state.database, &channel_id).await {
            Ok(channel) => channel.filter(|channel| channel.enabled),
            Err(error) => {
                tracing::warn!(channel_id, %error, "could not read a notification channel");
                None
            }
        };
        match channel {
            None => {
                let discarded = state.notifications.discard(&channel_id);
                tracing::debug!(
                    channel_id,
                    discarded,
                    "dropped the notifications waiting for a channel that is off or gone"
                );
            }
            Some(channel) => match channel.kind() {
                Some(Kind::Email) if !settled => {
                    settled = true;
                    tokio::time::sleep(SETTLE).await;
                    // Read the channel again: it may have changed meanwhile.
                    continue;
                }
                Some(Kind::Email) => deliver_emails(&state, &channel, &mut recorder).await,
                Some(Kind::Webhook) => deliver_webhooks(&state, channel, &mut recorder).await,
                None => {
                    let unknown = state.notifications.take(&channel_id, usize::MAX);
                    state.notifications.finished(&channel_id, unknown.len());
                    recorder.note(Err("the channel's kind is unknown".to_owned()));
                }
            },
        }
        recorder.flush(&state, &channel_id).await;
        if state.notifications.stop_if_idle(&channel_id) {
            if recorder.dropped > 0 {
                tracing::warn!(
                    channel_id,
                    dropped = recorder.dropped,
                    "a notification channel dropped notifications because too many were waiting"
                );
            }
            return;
        }
    }
}

/// Emails everything waiting for the channel as one batch.
async fn deliver_emails(state: &AppState, channel: &Channel, recorder: &mut Recorder) {
    let batch = state.notifications.take(&channel.id, usize::MAX);
    if batch.is_empty() {
        return;
    }
    let notifications: Vec<&Notification> = batch.iter().map(Arc::as_ref).collect();
    let outcome = email::send(state, &channel.id, &notifications).await;
    if let Err(failure) = &outcome {
        tracing::warn!(
            channel_id = channel.id,
            count = batch.len(),
            error = failure.message,
            "could not deliver notifications"
        );
    }
    state.notifications.finished(&channel.id, batch.len());
    recorder.note(outcome.map_err(|failure| failure.message));
}

/// A webhook delivery waiting for its retry.
struct Retry {
    due: Instant,
    /// How many attempts failed.
    attempts: usize,
    notification: Arc<Notification>,
}

/// Posts what is waiting for the channel, [`WEBHOOK_IN_FLIGHT`] at a time,
/// until nothing is waiting, being sent or due for a retry, or the channel
/// is turned off or deleted, which drops the retries.
async fn deliver_webhooks(state: &AppState, channel: Channel, recorder: &mut Recorder) {
    let queues = &state.notifications;
    let added = queues.added(&channel.id);
    let mut channel = Arc::new(channel);
    let mut read_at = Instant::now();
    let mut in_flight = FuturesUnordered::new();
    let mut retries: Vec<Retry> = Vec::new();
    let mut open = true;
    loop {
        if open && read_at.elapsed() >= RECORD_EVERY {
            read_at = Instant::now();
            match by_id(&mut &state.database, &channel.id).await {
                Ok(Some(current)) if current.enabled => channel = Arc::new(current),
                Ok(_) => {
                    open = false;
                    queues.finished(&channel.id, retries.len());
                    retries.clear();
                }
                Err(error) => {
                    tracing::warn!(channel_id = channel.id, %error, "could not read a notification channel");
                }
            }
        }
        // Due retries first, then the oldest waiting.
        while open && in_flight.len() < WEBHOOK_IN_FLIGHT {
            let now = Instant::now();
            let next = match retries.iter().position(|retry| retry.due <= now) {
                Some(index) => {
                    let retry = retries.remove(index);
                    Some((retry.attempts, retry.notification))
                }
                None => queues
                    .take(&channel.id, 1)
                    .pop()
                    .map(|notification| (0, notification)),
            };
            let Some((attempts, notification)) = next else {
                break;
            };
            in_flight.push(post(state, channel.clone(), attempts, notification));
        }
        if in_flight.is_empty() && retries.is_empty() {
            return;
        }
        let room = open && in_flight.len() < WEBHOOK_IN_FLIGHT;
        let next_retry = retries.iter().map(|retry| retry.due).min();
        tokio::select! {
            Some((attempts, notification, outcome)) = in_flight.next(), if !in_flight.is_empty() => {
                let retry = match &outcome {
                    Err(failure) if failure.retry && open => webhook::RETRY_DELAYS.get(attempts),
                    _ => None,
                };
                if let Some(delay) = retry {
                    tracing::info!(
                        channel_id = channel.id,
                        error = outcome.err().map(|failure| failure.message),
                        retry_in_seconds = delay.as_secs(),
                        "a notification delivery failed; retrying"
                    );
                    retries.push(Retry { due: Instant::now() + *delay, attempts: attempts + 1, notification });
                } else {
                    if let Err(failure) = &outcome {
                        tracing::warn!(
                            channel_id = channel.id,
                            event = notification.event,
                            error = failure.message,
                            "could not deliver a notification"
                        );
                    }
                    queues.finished(&channel.id, 1);
                    recorder.note(outcome.map_err(|failure| failure.message));
                }
            }
            () = tokio::time::sleep_until(next_retry.unwrap_or_else(Instant::now)), if room && next_retry.is_some() => {}
            () = added.notified(), if room => {}
            () = tokio::time::sleep_until(recorder.due(Instant::now())), if recorder.pending() => {}
        }
        recorder.flush_if_due(state, &channel.id).await;
    }
}

/// One attempt at posting `notification`, after `attempts` failed ones.
async fn post(
    state: &AppState,
    channel: Arc<Channel>,
    attempts: usize,
    notification: Arc<Notification>,
) -> (usize, Arc<Notification>, Result<(), Failure>) {
    let outcome = attempt_delivery(state, &channel, &notification).await;
    (attempts, notification, outcome)
}

/// Collects how a worker's deliveries went and writes it to the channel at
/// most every [`RECORD_EVERY`].
#[derive(Debug, Default)]
struct Recorder {
    /// The latest failure since the last write, or success if none failed.
    outcome: Option<Result<(), String>>,
    /// Every notification this worker's channel dropped. Each write says
    /// so, so the website shows it until the next burst.
    dropped: u64,
    /// Whether `dropped` grew since the last write.
    more_dropped: bool,
    written_at: Option<Instant>,
}

impl Recorder {
    fn note(&mut self, outcome: Result<(), String>) {
        if outcome.is_err() || self.outcome.is_none() {
            self.outcome = Some(outcome);
        }
    }

    fn pending(&self) -> bool {
        self.outcome.is_some() || self.more_dropped
    }

    /// When the next write may happen: `now` if none happened yet.
    fn due(&self, now: Instant) -> Instant {
        self.written_at.map_or(now, |at| at + RECORD_EVERY)
    }

    fn count_dropped(&mut self, dropped: u64) {
        if dropped > 0 {
            self.dropped += dropped;
            self.more_dropped = true;
        }
    }

    /// What to write now, if anything: the latest outcome, with how many
    /// notifications were dropped.
    fn take(&mut self, now: Instant) -> Option<Result<(), String>> {
        if !self.pending() {
            return None;
        }
        self.written_at = Some(now);
        self.more_dropped = false;
        let outcome = self.outcome.take().unwrap_or(Ok(()));
        if self.dropped == 0 {
            return Some(outcome);
        }
        let dropped = format!(
            "{} {} dropped because {MAX_WAITING} were already waiting",
            self.dropped,
            if self.dropped == 1 {
                "notification was"
            } else {
                "notifications were"
            }
        );
        Some(Err(match outcome {
            Ok(()) => dropped,
            Err(error) => format!("{error}; {dropped}"),
        }))
    }

    async fn flush_if_due(&mut self, state: &AppState, channel_id: &str) {
        let now = Instant::now();
        if now >= self.due(now) {
            self.flush(state, channel_id).await;
        }
    }

    async fn flush(&mut self, state: &AppState, channel_id: &str) {
        self.count_dropped(state.notifications.take_dropped(channel_id));
        if let Some(outcome) = self.take(Instant::now()) {
            record(&state.database, channel_id, outcome).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    fn notification(n: usize) -> Arc<Notification> {
        Arc::new(Notification {
            event: "test",
            summary: format!("Notification {n}"),
            body: String::new(),
            data: Map::new(),
        })
    }

    #[test]
    fn queues_keep_order_and_drop_beyond_their_bound() {
        let queues = Queues::default();
        let channels = ["a".to_owned(), "b".to_owned()];
        assert_eq!(queues.push(&channels, &notification(0)), channels);
        assert!(
            queues.push(&channels, &notification(1)).is_empty(),
            "the workers run"
        );
        let taken = queues.take("a", 1);
        assert_eq!(taken[0].summary, "Notification 0");
        assert!(
            Arc::ptr_eq(&taken[0], &queues.take("b", 1)[0]),
            "shared between channels"
        );
        for n in 2..MAX_WAITING + 5 {
            queues.push(&channels[..1], &notification(n));
        }
        // One taken and MAX_WAITING - 1 waiting: the rest are dropped.
        assert_eq!(queues.take_dropped("a"), 5);
        assert_eq!(queues.take_dropped("a"), 0);
        let rest = queues.take("a", usize::MAX);
        assert_eq!(rest.len(), MAX_WAITING - 1);
        assert_eq!(rest[0].summary, "Notification 1");
        assert_eq!(
            rest[MAX_WAITING - 2].summary,
            format!("Notification {}", MAX_WAITING - 1)
        );
        assert!(
            !queues.stop_if_idle("a"),
            "it still has notifications taken"
        );
        queues.finished("a", MAX_WAITING);
        assert!(queues.stop_if_idle("a"));
        assert_eq!(
            queues.push(&channels[..1], &notification(0)),
            ["a"],
            "a new worker"
        );
        assert_eq!(queues.discard("b"), 1);
    }

    #[test]
    fn recorders_write_at_most_every_second_and_keep_the_drops() {
        let mut recorder = Recorder::default();
        let start = Instant::now();
        assert_eq!(recorder.take(start), None, "nothing happened");
        let mut writes = 0;
        for n in 0..1000 {
            recorder.note(if n == 500 {
                Err("refused".into())
            } else {
                Ok(())
            });
            if start >= recorder.due(start) && recorder.take(start).is_some() {
                writes += 1;
            }
        }
        assert_eq!(
            writes, 1,
            "the first outcome, then nothing within the second"
        );
        let later = start + RECORD_EVERY;
        assert!(later >= recorder.due(later));
        assert_eq!(
            recorder.take(later),
            Some(Err("refused".into())),
            "failures win"
        );

        recorder.count_dropped(25);
        assert_eq!(
            recorder.take(later + RECORD_EVERY),
            Some(Err(
                "25 notifications were dropped because 10000 were already waiting".into()
            ))
        );
        recorder.note(Err("timed out".into()));
        assert_eq!(
            recorder.take(later + 2 * RECORD_EVERY),
            Some(Err(
                "timed out; 25 notifications were dropped because 10000 were already waiting"
                    .into()
            ))
        );
    }
}
