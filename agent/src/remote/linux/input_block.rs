//! Blocks the local keyboard and pointer by grabbing their evdev devices,
//! which keeps their events from the display server while the viewer's
//! virtual devices still reach it. Grabbing needs root, so the installed
//! coordinator does it for the session its helper serves. Devices plugged in
//! while input is blocked are grabbed too.
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;

/// `EVIOCGRAB`, `_IOW('E', 0x90, int)`.
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
const EV_KEY: usize = 0x01;
const EV_REL: usize = 0x02;
const EV_ABS: usize = 0x03;
const RESCAN: Duration = Duration::from_secs(2);

struct Grabs {
    devices: Arc<Mutex<HashMap<PathBuf, File>>>,
    stopped: Arc<AtomicBool>,
}

impl Drop for Grabs {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        // Closing a grabbed device releases it.
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

static GRABS: Mutex<Option<Grabs>> = Mutex::new(None);

pub(crate) fn blocked() -> bool {
    GRABS.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

pub(crate) fn set_blocked(blocked: bool) -> anyhow::Result<()> {
    let mut grabs = GRABS.lock().unwrap_or_else(|e| e.into_inner());
    if !blocked {
        if grabs.take().is_some() {
            tracing::info!("unblocked the local keyboard and pointer");
        }
        return Ok(());
    }
    if grabs.is_some() {
        return Ok(());
    }
    let devices = Arc::new(Mutex::new(HashMap::new()));
    let grabbed = grab_new(&devices)?;
    anyhow::ensure!(
        grabbed > 0,
        "blocking local input needs root, which this Agent does not have"
    );
    let stopped = Arc::new(AtomicBool::new(false));
    let (watched, stop) = (Arc::clone(&devices), Arc::clone(&stopped));
    std::thread::Builder::new()
        .name("meshrmm-input-block".into())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(RESCAN);
                if !stop.load(Ordering::SeqCst) {
                    let _ = grab_new(&watched);
                }
            }
        })
        .context("could not start watching for new input devices")?;
    tracing::info!(devices = grabbed, "blocked the local keyboard and pointer");
    *grabs = Some(Grabs { devices, stopped });
    Ok(())
}

/// Grabs the keyboards and pointers not grabbed yet, returning how many
/// devices are grabbed in all.
fn grab_new(devices: &Mutex<HashMap<PathBuf, File>>) -> anyhow::Result<usize> {
    let mut devices = devices.lock().unwrap_or_else(|e| e.into_inner());
    // Devices that went away.
    devices.retain(|path, _| path.exists());
    for entry in std::fs::read_dir("/dev/input").context("could not list input devices")? {
        let path = entry?.path();
        let is_event = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"));
        if !is_event || devices.contains_key(&path) {
            continue;
        }
        let Ok(file) = File::open(&path) else {
            continue;
        };
        if !local_input(&file) {
            continue;
        }
        // SAFETY: EVIOCGRAB takes an int; 1 grabs the device.
        if unsafe { libc::ioctl(file.as_raw_fd(), EVIOCGRAB, 1) } == 0 {
            devices.insert(path, file);
        }
    }
    Ok(devices.len())
}

/// Whether a device is a keyboard or pointer the user handles: it sends
/// keys or motion, and it is not one of the Agent's virtual devices.
fn local_input(file: &File) -> bool {
    let mut name = [0_u8; 256];
    // SAFETY: EVIOCGNAME writes at most the buffer's length.
    let length = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            ioc_read(0x06, name.len()),
            name.as_mut_ptr(),
        )
    };
    if length < 0 || name.starts_with(b"MeshRMM virtual") {
        return false;
    }
    let mut types = [0_u8; 4];
    // SAFETY: EVIOCGBIT(0) writes the event type bits, at most the buffer's length.
    if unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            ioc_read(0x20, types.len()),
            types.as_mut_ptr(),
        )
    } < 0
    {
        return false;
    }
    let has = |bit: usize| types[bit / 8] & (1 << (bit % 8)) != 0;
    has(EV_KEY) || has(EV_REL) || has(EV_ABS)
}

/// `_IOR('E', number, length)` for the variable-length evdev queries.
const fn ioc_read(number: libc::c_ulong, length: usize) -> libc::c_ulong {
    (2 << 30) | ((length as libc::c_ulong) << 16) | (0x45 << 8) | number
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_numbers_match_the_kernel_macros() {
        // EVIOCGNAME(256) and EVIOCGBIT(0, 4).
        assert_eq!(ioc_read(0x06, 256), 0x8100_4506);
        assert_eq!(ioc_read(0x20, 4), 0x8004_4520);
    }
}
