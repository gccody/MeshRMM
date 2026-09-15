//! Secure attention originates in the Session 0 coordinator, never the desktop
//! helper. Temporarily allow service SAS generation and restore the local policy.

use anyhow::Context;

trait Policy {
    fn read(&self) -> anyhow::Result<Option<u32>>;
    fn write(&self, value: Option<u32>) -> anyhow::Result<()>;
}

struct PolicyOverride<'a, P: Policy> {
    policy: &'a P,
    original: Option<u32>,
    temporary: u32,
    pending: bool,
}

impl<P: Policy> PolicyOverride<'_, P> {
    fn restore(&mut self) -> anyhow::Result<()> {
        if self.pending {
            // Avoid overwriting a different value installed by a concurrent GPO
            // refresh. Registry values have no compare-and-swap operation.
            if self.policy.read()? == Some(self.temporary) {
                self.policy.write(self.original)?;
            }
            self.pending = false;
        }
        Ok(())
    }
}

impl<P: Policy> Drop for PolicyOverride<'_, P> {
    fn drop(&mut self) {
        // Also restore during unwinding, and retry once if explicit cleanup failed.
        if let Err(error) = self.restore() {
            tracing::error!(error = %error, "failed to restore secure attention policy");
        }
    }
}

fn with_service_policy<P: Policy>(policy: &P, send: impl FnOnce()) -> anyhow::Result<()> {
    let original = policy
        .read()
        .context("could not read Windows secure attention policy")?;
    if matches!(original, Some(1 | 3)) {
        send();
        return Ok(());
    }

    // Preserve the accessibility permission (2 -> 3); otherwise allow services only.
    let temporary = if original == Some(2) { 3 } else { 1 };
    policy
        .write(Some(temporary))
        .context("could not temporarily allow Ctrl+Alt+Del in Windows policy")?;
    let mut guard = PolicyOverride {
        policy,
        original,
        temporary,
        pending: true,
    };
    send();
    guard
        .restore()
        .context("Ctrl+Alt+Del was requested, but restoring Windows secure attention policy failed")
}

#[cfg(windows)]
pub(super) fn send() -> anyhow::Result<()> {
    use windows::Win32::Security::Authentication::Identity::SendSAS;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;

    // Concurrent button presses must not save another request's temporary value.
    static REQUEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _request = REQUEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut session_id = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) }
        .context("could not determine the Agent session")?;
    anyhow::ensure!(
        session_id == 0,
        "Ctrl+Alt+Del requires the installed Windows Agent service"
    );
    let policy = registry::WindowsPolicy::open()?;
    // FALSE identifies the service caller and targets the active console. SendSAS
    // returns no status, so success means the request was issued, not observed.
    with_service_policy(&policy, || unsafe { SendSAS(false) })
}

#[cfg(windows)]
mod registry {
    use super::Policy;
    use anyhow::Context;
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::System::Registry::*;
    use windows::core::w;

    pub(super) struct WindowsPolicy(HKEY);

    impl WindowsPolicy {
        pub(super) fn open() -> anyhow::Result<Self> {
            let mut key = HKEY::default();
            unsafe {
                RegOpenKeyExW(
                    HKEY_LOCAL_MACHINE,
                    w!("Software\\Microsoft\\Windows\\CurrentVersion\\Policies\\System"),
                    None,
                    KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
                    &mut key,
                )
            }
            .ok()
            .context("could not open Windows secure attention policy")?;
            Ok(Self(key))
        }
    }

    impl Policy for WindowsPolicy {
        fn read(&self) -> anyhow::Result<Option<u32>> {
            let mut value = 0u32;
            let mut size = std::mem::size_of_val(&value) as u32;
            let result = unsafe {
                RegGetValueW(
                    self.0,
                    None,
                    w!("SoftwareSASGeneration"),
                    RRF_RT_REG_DWORD,
                    None,
                    Some((&mut value as *mut u32).cast()),
                    Some(&mut size),
                )
            };
            if result == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            // Reject unexpected registry types without replacing them.
            result.ok()?;
            Ok(Some(value))
        }

        fn write(&self, value: Option<u32>) -> anyhow::Result<()> {
            let result = unsafe {
                match value {
                    Some(value) => RegSetValueExW(
                        self.0,
                        w!("SoftwareSASGeneration"),
                        None,
                        REG_DWORD,
                        Some(&value.to_le_bytes()),
                    ),
                    None => RegDeleteValueW(self.0, w!("SoftwareSASGeneration")),
                }
            };
            if value.is_none() && result == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            result.ok()?;
            Ok(())
        }
    }

    impl Drop for WindowsPolicy {
        fn drop(&mut self) {
            let _ = unsafe { RegCloseKey(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct FakePolicy {
        value: Cell<Option<u32>>,
        writes: RefCell<Vec<Option<u32>>>,
        fail_write: Cell<bool>,
        fail_read: Cell<bool>,
    }
    impl Policy for FakePolicy {
        fn read(&self) -> anyhow::Result<Option<u32>> {
            anyhow::ensure!(!self.fail_read.get(), "read denied");
            Ok(self.value.get())
        }
        fn write(&self, value: Option<u32>) -> anyhow::Result<()> {
            anyhow::ensure!(!self.fail_write.replace(false), "write denied");
            self.writes.borrow_mut().push(value);
            self.value.set(value);
            Ok(())
        }
    }

    #[test]
    fn restores_missing_and_existing_policy_after_sas() {
        for original in [None, Some(0), Some(2), Some(42)] {
            let policy = FakePolicy::default();
            policy.value.set(original);
            let temporary = if original == Some(2) { 3 } else { 1 };
            with_service_policy(&policy, || assert_eq!(policy.value.get(), Some(temporary)))
                .unwrap();
            assert_eq!(policy.value.get(), original);
            assert_eq!(*policy.writes.borrow(), vec![Some(temporary), original]);
        }
    }

    #[test]
    fn permitted_policy_is_never_written() {
        for value in [1, 3] {
            let policy = FakePolicy::default();
            policy.value.set(Some(value));
            let sent = Cell::new(false);
            with_service_policy(&policy, || sent.set(true)).unwrap();
            assert!(sent.get());
            assert!(policy.writes.borrow().is_empty());
        }
    }

    #[test]
    fn read_or_override_failure_does_not_send() {
        for read_failure in [false, true] {
            let policy = FakePolicy::default();
            policy.fail_read.set(read_failure);
            policy.fail_write.set(!read_failure);
            assert!(with_service_policy(&policy, || panic!("must not send")).is_err());
            assert!(policy.writes.borrow().is_empty());
        }
    }

    #[test]
    fn cleanup_failure_is_reported_and_retried() {
        let policy = FakePolicy::default();
        let error = with_service_policy(&policy, || policy.fail_write.set(true)).unwrap_err();
        assert!(error.to_string().contains("restoring"));
        assert_eq!(policy.value.get(), None);
    }

    #[test]
    fn restores_on_unwind() {
        let policy = FakePolicy::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = with_service_policy(&policy, || panic!("send panicked"));
        }));
        assert!(result.is_err());
        assert_eq!(policy.value.get(), None);
    }

    #[test]
    fn preserves_a_concurrent_policy_change() {
        let policy = FakePolicy::default();
        with_service_policy(&policy, || policy.value.set(Some(2))).unwrap();
        assert_eq!(policy.value.get(), Some(2));
        assert_eq!(*policy.writes.borrow(), vec![Some(1)]);
    }
}
