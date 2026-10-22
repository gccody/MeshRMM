//! The account file operations run as.
//!
//! SYSTEM or root operations run on the calling thread. A user's run on a
//! thread of their own that takes on the user's identity first and ends
//! with the operation, so the identity can't leak into anything else the
//! Agent does: Windows impersonates the user's token; Linux switches the
//! thread's own user, groups and capabilities (the raw system calls, which
//! unlike libc's act on one thread); macOS gives the thread the user's
//! credentials with `pthread_setugid_np`. Files a thread opens stay open
//! with the access it had, so a transfer opens its file as the user and
//! streams it from any thread.
use std::path::PathBuf;

use meshrmm_protocol::{FileError, FileErrorCode, RunAs};

use super::platform;
use crate::remote::config::ExecutionMode;

pub(super) struct Account {
    /// Who operations run as: `NT AUTHORITY\SYSTEM`, `root`, `alice`.
    pub(super) label: String,
    /// The account's home folder, when it has one worth offering.
    pub(super) home: Option<PathBuf>,
    /// The signed-in user to act as; `None` for the Agent's own account.
    pub(super) user: Option<platform::User>,
}

impl Account {
    /// The account for `run_as`. A development Agent has only its own. As
    /// the signed-in user, it fails when nobody is signed in: unlike a
    /// script, it never falls back to SYSTEM or root.
    pub(super) fn resolve(mode: ExecutionMode, run_as: RunAs) -> Result<Self, FileError> {
        platform::account(mode, run_as)
    }

    /// Whether operations act as the signed-in user.
    pub(super) fn is_user(&self) -> bool {
        self.user.is_some()
    }

    /// Runs `work` as the account.
    pub(super) fn run<T: Send>(
        &self,
        work: impl FnOnce() -> Result<T, FileError> + Send,
    ) -> Result<T, FileError> {
        let Some(user) = &self.user else {
            return work();
        };
        let failed = |message: &str| FileError::new(FileErrorCode::Failed, message);
        std::thread::scope(|scope| {
            let thread = std::thread::Builder::new()
                .name("meshrmm-files-as-user".into())
                .spawn_scoped(scope, || {
                    let _acting = platform::act_as(user).map_err(|error| {
                        tracing::warn!(%error, "could not act as the signed-in user");
                        failed("The Agent could not act as the signed-in user.")
                    })?;
                    work()
                })
                .map_err(|_| failed("The Agent could not start the operation."))?;
            thread
                .join()
                .unwrap_or_else(|_| Err(failed("The operation stopped unexpectedly.")))
        })
    }
}

/// The error for a user operation when nobody is signed in.
pub(super) fn nobody_signed_in() -> FileError {
    FileError::new(
        FileErrorCode::NoUser,
        "Nobody is signed in to this computer, so there is no user to act as. Use SYSTEM or root instead.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_development_agent_works_as_itself() {
        for run_as in [RunAs::System, RunAs::User] {
            let account = Account::resolve(ExecutionMode::Console, run_as).unwrap();
            assert!(!account.is_user());
            assert!(!account.label.is_empty());
            assert_eq!(account.run(|| Ok(7)).unwrap(), 7);
        }
    }
}
