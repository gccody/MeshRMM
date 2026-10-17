//! Linux system audio: the monitor of the default output, through the
//! PulseAudio protocol that both PulseAudio and PipeWire's `pipewire-pulse`
//! serve. `libpulse-simple` is loaded when capture starts, so the Agent runs
//! on computers without it and only audio is unavailable there. It must run
//! as the user whose sound server it captures, with their environment.
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail};

use super::{HEADER, MAX_PACKET};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
/// 10 ms, like the other platforms' packets.
const PACKET_BYTES: usize = SAMPLE_RATE as usize / 100 * CHANNELS as usize * 2;
/// `PA_SAMPLE_S16LE`.
const SAMPLE_S16LE: c_int = 3;
/// `PA_STREAM_RECORD`.
const STREAM_RECORD: c_int = 2;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttributes {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type New = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    direction: c_int,
    device: *const c_char,
    stream_name: *const c_char,
    spec: *const SampleSpec,
    map: *const c_void,
    attributes: *const BufferAttributes,
    error: *mut c_int,
) -> *mut c_void;
type Read = unsafe extern "C" fn(
    stream: *mut c_void,
    data: *mut c_void,
    bytes: usize,
    error: *mut c_int,
) -> c_int;
type Free = unsafe extern "C" fn(stream: *mut c_void);
type StrError = unsafe extern "C" fn(error: c_int) -> *const c_char;

struct Library {
    _library: libloading::Library,
    read: Read,
    free: Free,
    strerror: StrError,
}

impl Library {
    fn describe(&self, error: c_int) -> String {
        // SAFETY: pa_strerror returns a static string for any error code.
        let text = unsafe { (self.strerror)(error) };
        if text.is_null() {
            return format!("PulseAudio error {error}");
        }
        // SAFETY: the string is NUL-terminated and static.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    }
}

/// The open recording stream, freed when dropped.
struct Stream {
    library: Library,
    stream: *mut c_void,
}

// SAFETY: a pa_simple stream may be used from any thread, one at a time;
// only the capture thread uses it after it is opened.
unsafe impl Send for Stream {}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the stream came from pa_simple_new and is freed once.
        unsafe { (self.library.free)(self.stream) };
    }
}

fn open() -> anyhow::Result<Stream> {
    // SAFETY: loading libpulse-simple runs its initializers, which have no
    // preconditions.
    let library = unsafe { libloading::Library::new("libpulse-simple.so.0") }
        .context("the PulseAudio client library (libpulse-simple) is not installed")?;
    // SAFETY: the symbols have the declared C signatures.
    let (new, read, free, strerror) = unsafe {
        let new = *library.get::<New>(b"pa_simple_new\0")?;
        let read = *library.get::<Read>(b"pa_simple_read\0")?;
        let free = *library.get::<Free>(b"pa_simple_free\0")?;
        // pa_strerror is libpulse's, which libpulse-simple links.
        let strerror = *library.get::<StrError>(b"pa_strerror\0")?;
        (new, read, free, strerror)
    };
    let library = Library {
        _library: library,
        read,
        free,
        strerror,
    };
    let spec = SampleSpec {
        format: SAMPLE_S16LE,
        rate: SAMPLE_RATE,
        channels: CHANNELS,
    };
    // Small fragments keep the latency near one packet; the rest are the
    // server's defaults.
    let attributes = BufferAttributes {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: PACKET_BYTES as u32,
    };
    let mut error = 0;
    // SAFETY: every pointer is either null where allowed or points to a
    // NUL-terminated string or structure that outlives the call.
    let stream = unsafe {
        new(
            std::ptr::null(),
            c"MeshRMM Agent".as_ptr(),
            STREAM_RECORD,
            c"@DEFAULT_MONITOR@".as_ptr(),
            c"System audio".as_ptr(),
            &spec,
            std::ptr::null(),
            &attributes,
            &mut error,
        )
    };
    if stream.is_null() {
        bail!(
            "could not record the sound server's output: {}",
            library.describe(error)
        );
    }
    Ok(Stream { library, stream })
}

/// System audio being captured; capture stops when it is dropped.
pub struct Capture {
    stopped: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

impl Capture {
    pub fn healthy(&self) -> bool {
        !self.failed.load(Ordering::SeqCst)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // A read in progress returns within a packet; the thread then stops.
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// Starts capturing the default output's monitor, sending 10 ms PCM16
/// packets to `send`.
pub fn capture(send: impl Fn(Vec<u8>) + Send + 'static) -> anyhow::Result<Capture> {
    let stream = open()?;
    let stopped = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));
    let (thread_stopped, thread_failed) = (Arc::clone(&stopped), Arc::clone(&failed));
    std::thread::Builder::new()
        .name("meshrmm-audio-capture".into())
        .spawn(move || {
            let mut samples = vec![0_u8; PACKET_BYTES];
            while !thread_stopped.load(Ordering::SeqCst) {
                let mut error = 0;
                // SAFETY: the stream is open and `samples` is writable for
                // its length.
                let read = unsafe {
                    (stream.library.read)(
                        stream.stream,
                        samples.as_mut_ptr().cast(),
                        samples.len(),
                        &mut error,
                    )
                };
                if read < 0 {
                    tracing::warn!(
                        error = %stream.library.describe(error),
                        "system audio capture stopped"
                    );
                    thread_failed.store(true, Ordering::SeqCst);
                    break;
                }
                let mut packet = Vec::with_capacity(HEADER + samples.len());
                packet.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
                packet.extend_from_slice(&u16::from(CHANNELS).to_le_bytes());
                packet.extend_from_slice(&samples);
                debug_assert!(packet.len() <= MAX_PACKET);
                send(packet);
            }
        })
        .context("could not start the audio capture thread")?;
    Ok(Capture { stopped, failed })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a desktop session with a PulseAudio or PipeWire sound server.
    #[test]
    #[ignore = "records the sound server's output"]
    fn captures_ten_millisecond_packets() {
        let (sender, packets) = std::sync::mpsc::channel();
        let capture = capture(move |packet| {
            let _ = sender.send(packet);
        })
        .unwrap();
        let packet = packets
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(packet.len(), HEADER + PACKET_BYTES);
        assert_eq!(super::super::pcm_bits_per_second(&packet), Some(1_536_000));
        assert!(capture.healthy());
    }
}
