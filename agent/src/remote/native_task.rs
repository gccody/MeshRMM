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
