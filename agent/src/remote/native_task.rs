use std::future::Future;

/// A cancellable native worker. Blocking OS calls stay on this thread; Tokio
/// continues driving sockets, input dispatch and timers on its runtime threads.
/// Cancellation is cooperative: an in-flight OS operation must return before
/// cleanup runs. The owner never joins an OS thread from an async task or Drop.
pub struct NativeTask {
    stop: tokio::sync::watch::Sender<bool>,
    done: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl NativeTask {
    pub fn spawn<F, Fut>(name: &str, run: F) -> std::io::Result<Self>
    where
        F: FnOnce(tokio::sync::watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let runtime = tokio::runtime::Handle::current();
        let (stop, cancelled) = tokio::sync::watch::channel(false);
        let (finished, done) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                runtime.block_on(run(cancelled));
                let _ = finished.send(());
            })?;
        Ok(Self {
            stop,
            done: Some(done),
        })
    }

    pub async fn shutdown(&mut self) {
        let _ = self.stop.send(true);
        if let Some(done) = self.done.take() {
            let _ = done.await;
        }
    }
}

impl Drop for NativeTask {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_native_work_does_not_stall_runtime_and_drop_does_not_join() {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let (cleaned, cleanup) = tokio::sync::oneshot::channel();
        let worker = NativeTask::spawn("isolation-test", move |mut stop| async move {
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            if !*stop.borrow() {
                let _ = stop.changed().await;
            }
            cleaned.send(()).unwrap();
        })
        .unwrap();
        started.await.unwrap();
        // The native worker is deliberately blocked until this runtime resumes.
        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::time::sleep(Duration::from_millis(20)),
        )
        .await
        .unwrap();
        drop(worker);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), cleanup)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_waits_for_cleanup() {
        let (cleaned, cleanup) = tokio::sync::oneshot::channel();
        let mut worker = NativeTask::spawn("shutdown-test", move |mut stop| async move {
            if !*stop.borrow() {
                let _ = stop.changed().await;
            }
            cleaned.send(()).unwrap();
        })
        .unwrap();
        worker.shutdown().await;
        cleanup.await.unwrap();
    }
}

/// Serializes commands for one native service without blocking its caller.
/// Queues are bounded; callers must handle overload explicitly. Cancellation
/// discards pending commands before running cleanup (especially key releases).
pub fn command_worker<T, F, C>(
    name: &str,
    capacity: usize,
    interval: std::time::Duration,
    mut handle: F,
    cleanup: C,
) -> std::io::Result<(tokio::sync::mpsc::Sender<T>, NativeTask)>
where
    T: Send + 'static,
    F: FnMut(Option<T>) + Send + 'static,
    C: FnOnce() + Send + 'static,
{
    let (sender, mut receiver) = tokio::sync::mpsc::channel(capacity);
    let task = NativeTask::spawn(name, move |mut stop| async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                _ = stop.changed() => break,
                command = receiver.recv() => {
                    let Some(command) = command else { break; };
                    handle(Some(command));
                }
                _ = tick.tick() => handle(None),
            }
        }
        cleanup();
    })?;
    Ok((sender, task))
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn slow_service_does_not_block_another_and_overload_is_explicit() {
        let (entered, mut started) = tokio::sync::mpsc::channel(1);
        let (release, wait) = std::sync::mpsc::channel();
        let (slow, mut slow_task) = command_worker(
            "slow-service",
            1,
            Duration::from_secs(60),
            move |command: Option<u8>| {
                if command.is_some() {
                    entered.try_send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                }
            },
            || {},
        )
        .unwrap();
        slow.try_send(1).unwrap();
        started.recv().await.unwrap();
        slow.try_send(2).unwrap();
        assert!(matches!(
            slow.try_send(3),
            Err(tokio::sync::mpsc::error::TrySendError::Full(3))
        ));
        let (events, mut received) = tokio::sync::mpsc::channel(4);
        let (fast, mut fast_task) = command_worker(
            "input-service",
            4,
            Duration::from_secs(60),
            move |command| {
                if let Some(command) = command {
                    events.try_send(command).unwrap();
                }
            },
            || {},
        )
        .unwrap();
        for key in [1, 2, 3] {
            fast.try_send(key).unwrap();
        }
        for key in [1, 2, 3] {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), received.recv())
                    .await
                    .unwrap(),
                Some(key)
            );
        }
        // Shutdown skips the queued command; only the operation in flight runs.
        let _ = slow_task.stop.send(true);
        release.send(()).unwrap();
        slow_task.shutdown().await;
        fast_task.shutdown().await;
    }
}
