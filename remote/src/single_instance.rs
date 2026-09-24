//! One Windows viewer per device. A dashboard link for a device that already
//! has a viewer asks that viewer to end its session and waits for it to exit.
//! Otherwise the new handoff is redeemed while the old lease is still active,
//! and the server refuses it for up to 15 minutes.
use std::time::Duration;

use anyhow::{Context, bail};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, INFINITE, ReleaseMutex, ResetEvent, SetEvent, WaitForSingleObject,
};
use windows::core::HSTRING;

/// Holds this device's viewer mutex until dropped or the process exits.
pub struct InstanceGuard {
    mutex: HANDLE,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.mutex);
            let _ = CloseHandle(self.mutex);
        }
    }
}

struct SendHandle(HANDLE);
// Safety: kernel event handles may be used from any thread.
unsafe impl Send for SendHandle {}

/// Becomes the viewer for `device_id`. An existing viewer for the device is
/// asked to end its session, and this waits up to `timeout` for it to exit.
/// `on_replaced` runs, once, when a later viewer asks this one to end.
///
/// Must be called on the thread that lives as long as the session, since a
/// mutex belongs to the thread that acquired it.
pub fn claim(
    device_id: &str,
    timeout: Duration,
    on_replaced: impl FnOnce() + Send + 'static,
) -> anyhow::Result<InstanceGuard> {
    let name = format!("Local\\MeshRMM-Viewer-{device_id}");
    let mutex = unsafe { CreateMutexW(None, true, &HSTRING::from(&name)) }
        .context("could not create the viewer instance mutex")?;
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let guard = InstanceGuard { mutex };
    let event = unsafe {
        CreateEventW(
            None,
            false,
            false,
            &HSTRING::from(format!("{name}-replace")),
        )
    }
    .context("could not create the viewer replacement event")?;
    let event = SendHandle(event);
    if existed {
        tracing::info!(
            device_id,
            "asking the open viewer for this device to end its session"
        );
        unsafe { SetEvent(event.0) }.context("could not signal the open viewer")?;
        let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        match unsafe { WaitForSingleObject(mutex, milliseconds) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {}
            WAIT_TIMEOUT => {
                unsafe {
                    let _ = CloseHandle(event.0);
                }
                bail!(
                    "The remote viewer that is already open for this device did not close within {} seconds. Close it and try again.",
                    timeout.as_secs()
                );
            }
            other => {
                unsafe {
                    let _ = CloseHandle(event.0);
                }
                bail!("waiting for the open viewer failed: {other:?}");
            }
        }
        // A viewer that exited before it consumed the signal leaves it set.
        let _ = unsafe { ResetEvent(event.0) };
    }
    std::thread::Builder::new()
        .name("viewer-replacement".into())
        .spawn(move || {
            let event = event;
            if unsafe { WaitForSingleObject(event.0, INFINITE) } == WAIT_OBJECT_0 {
                on_replaced();
            }
            unsafe {
                let _ = CloseHandle(event.0);
            }
        })
        .context("could not start the viewer replacement listener")?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(test: &str) -> String {
        format!("test-{test}-{}", std::process::id())
    }

    #[test]
    fn a_second_viewer_ends_the_first_and_takes_over() {
        let device = device("takeover");
        let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
        let (replaced_tx, replaced_rx) = std::sync::mpsc::channel();
        let first_device = device.clone();
        let first = std::thread::spawn(move || {
            let guard = claim(&first_device, Duration::from_secs(5), move || {
                let _ = replaced_tx.send(());
            })
            .unwrap();
            claimed_tx.send(()).unwrap();
            // The first viewer ends its session when asked, then exits.
            replaced_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(guard);
        });
        claimed_rx.recv().unwrap();
        let (second_replaced_tx, second_replaced_rx) = std::sync::mpsc::channel();
        let second = claim(&device, Duration::from_secs(5), move || {
            let _ = second_replaced_tx.send(());
        })
        .expect("second viewer took over");
        first.join().unwrap();
        // Taking over must not trigger the new viewer's own listener.
        assert!(
            second_replaced_rx
                .recv_timeout(Duration::from_millis(300))
                .is_err()
        );
        drop(second);
    }

    #[test]
    fn an_abandoned_mutex_is_taken_over() {
        let device = device("abandoned");
        let first_device = device.clone();
        std::thread::spawn(move || {
            // The thread exits without releasing, like a crashed viewer.
            std::mem::forget(claim(&first_device, Duration::from_secs(5), || {}).unwrap());
        })
        .join()
        .unwrap();
        claim(&device, Duration::from_secs(5), || {}).expect("abandoned viewer replaced");
    }

    #[test]
    fn a_viewer_that_does_not_close_times_out() {
        let device = device("stuck");
        let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let first_device = device.clone();
        let first = std::thread::spawn(move || {
            let guard = claim(&first_device, Duration::from_secs(5), || {}).unwrap();
            claimed_tx.send(()).unwrap();
            let _ = done_rx.recv();
            drop(guard);
        });
        claimed_rx.recv().unwrap();
        let error = match claim(&device, Duration::from_millis(300), || {}) {
            Ok(_) => panic!("claimed a device that another viewer holds"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("did not close"));
        done_tx.send(()).unwrap();
        first.join().unwrap();
    }
}
