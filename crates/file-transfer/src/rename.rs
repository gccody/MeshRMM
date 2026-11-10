//! Moving a file or folder without replacing what is already there.
use std::{fs, io, path::Path};

use crate::native;

/// Moves `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`]
/// instead of replacing an existing file or folder. Some volumes, such as
/// exFAT and many SMB and NFS shares, have no atomic rename that refuses to
/// replace; there the target is checked first and then renamed over, which
/// leaves a moment in which a file created at `to` meanwhile is replaced.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match native::rename_no_replace(from, to) {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            if to.try_exists()? {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(from, to)
        }
        result => result,
    }
}

/// macOS's values for the `errno` codes below, which differ from Linux's, so
/// what they mean can be tested everywhere.
pub(crate) mod macos_errno {
    pub(crate) const EINVAL: i32 = 22;
    pub(crate) const ENOTSUP: i32 = 45;
}

/// What a failed `renamex_np` with `RENAME_EXCL` on macOS means. A volume
/// that can't rename exclusively answers `ENOTSUP`, and some network file
/// systems `EINVAL`; both are [`io::ErrorKind::Unsupported`], for the
/// fallback in [`rename_no_replace`]. `EINVAL` also means moving a folder
/// into itself, which the plain rename then refuses the same way.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn macos_exclusive_rename_error(code: i32) -> io::Error {
    match code {
        macos_errno::ENOTSUP | macos_errno::EINVAL => io::ErrorKind::Unsupported.into(),
        code => io::Error::from_raw_os_error(code),
    }
}
