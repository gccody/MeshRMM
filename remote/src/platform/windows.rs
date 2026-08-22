use std::collections::{HashSet, VecDeque};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, bail};
use meshrmm_protocol::{
    CursorShape, Display, EncodedFrame, PointerButton, RemoteInput, SessionMessage, VideoFormat,
};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, GetStockObject, ScreenToClient, SetBkColor, SetTextColor,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_F8, VK_F12};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, Interface, PCWSTR, w};

use super::ControlSink;
use crate::debug::DebugInfo;

mod pipeline;
mod renderer;
mod window;

use pipeline::WorkerPipeline;
use renderer::D3d11Renderer;
use window::{create_window, pump_window_messages, set_window_cursor};

const MAX_DECODER_PENDING_FRAMES: usize = 4;

struct QueuedFrame {
    frame: EncodedFrame,
    received_at_us: u64,
}

struct Shared {
    latest: Mutex<Option<QueuedFrame>>,
    cursor_shape: Mutex<Option<CursorShape>>,
    ready: Condvar,
    stopping: AtomicBool,
    running: AtomicBool,
    failure: Mutex<Option<String>>,
    replaced_frames: AtomicU64,
    debug: DebugInfo,
}

pub struct Presenter {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Presenter {
    pub fn start(
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
        debug: DebugInfo,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            cursor_shape: Mutex::new(None),
            ready: Condvar::new(),
            stopping: AtomicBool::new(false),
            running: AtomicBool::new(false),
            failure: Mutex::new(None),
            replaced_frames: AtomicU64::new(0),
            debug,
        });
        let worker_shared = Arc::clone(&shared);
        let worker_debug = worker_shared.debug.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("meshrmm-decode-present".into())
            .spawn(move || {
                run_worker(
                    worker_shared,
                    format,
                    active_display,
                    displays,
                    control,
                    worker_debug,
                    started_tx,
                )
            })
            .context("failed to spawn Windows decode/presentation worker")?;
        started_rx
            .recv()
            .context("Windows decode/presentation worker exited during startup")??;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    pub fn publish(&self, frame: EncodedFrame, received_at_us: u64) {
        let Ok(mut latest) = self.shared.latest.lock() else {
            return;
        };
        if latest
            .replace(QueuedFrame {
                frame,
                received_at_us,
            })
            .is_some()
        {
            self.shared.replaced_frames.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.ready.notify_one();
    }

    pub fn set_cursor_shape(&self, shape: CursorShape) {
        if let Ok(mut pending) = self.shared.cursor_shape.lock() {
            *pending = Some(shape);
        }
    }

    pub fn stop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.ready.notify_all();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("Windows decode/presentation worker panicked");
        }
        tracing::info!(
            latest_frames_dropped = self.shared.replaced_frames.load(Ordering::Relaxed),
            "video presenter stopped"
        );
    }

    pub fn poll_ended(&self) -> Option<Result<(), String>> {
        if self.shared.running.load(Ordering::Acquire) {
            return None;
        }
        let failure = self
            .shared
            .failure
            .lock()
            .ok()
            .and_then(|mut failure| failure.take());
        Some(failure.map_or(Ok(()), Err))
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_worker(
    shared: Arc<Shared>,
    format: VideoFormat,
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    debug: DebugInfo,
    started: std::sync::mpsc::SyncSender<anyhow::Result<()>>,
) {
    let initialized =
        unsafe { WorkerPipeline::new(format, active_display, displays, control, debug) };
    let mut pipeline = match initialized {
        Ok(pipeline) => {
            shared.running.store(true, Ordering::Release);
            let _ = started.send(Ok(()));
            pipeline
        }
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };

    while !shared.stopping.load(Ordering::Acquire) {
        if unsafe { pump_window_messages(pipeline.window()) } {
            break;
        }
        let queued = {
            let Ok(latest) = shared.latest.lock() else {
                break;
            };
            let Ok((mut latest, _)) =
                shared
                    .ready
                    .wait_timeout_while(latest, Duration::from_millis(4), |slot| {
                        slot.is_none() && !shared.stopping.load(Ordering::Acquire)
                    })
            else {
                break;
            };
            latest.take()
        };
        if let Some(shape) = shared
            .cursor_shape
            .lock()
            .ok()
            .and_then(|mut pending| pending.take())
        {
            unsafe { pipeline.set_cursor_shape(shape) };
        }
        let Some(queued) = queued else {
            continue;
        };
        let frame_id = queued.frame.frame_id;
        match unsafe { pipeline.process(queued, shared.replaced_frames.load(Ordering::Relaxed)) } {
            Ok(()) => {}
            Err(error) => {
                tracing::error!(error = %error, frame_id, "hardware decode/presentation failed");
                if let Ok(mut failure) = shared.failure.lock() {
                    *failure = Some(error.to_string());
                }
                break;
            }
        }
    }
    shared.running.store(false, Ordering::Release);
}

pub fn monotonic_timestamp_us() -> u64 {
    unsafe {
        let mut counter = 0_i64;
        let mut frequency = 0_i64;
        if QueryPerformanceCounter(&mut counter).is_err()
            || QueryPerformanceFrequency(&mut frequency).is_err()
            || counter < 0
            || frequency <= 0
        {
            return 0;
        }
        (counter as u64).saturating_mul(1_000_000) / frequency as u64
    }
}
