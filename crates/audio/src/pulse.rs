//! The PulseAudio protocol's simple client, which both PulseAudio and
//! PipeWire's `pipewire-pulse` serve. `libpulse-simple` is loaded when a
//! stream opens, so the Agent and the viewer run on computers without it and
//! only audio is unavailable there.
use std::ffi::{CStr, c_char, c_int, c_void};

use anyhow::{Context, bail};

/// `PA_SAMPLE_S16LE`.
const SAMPLE_S16LE: c_int = 3;

/// Which way a stream's audio goes.
#[derive(Clone, Copy)]
pub(crate) enum Direction {
    /// `PA_STREAM_PLAYBACK`.
    Playback = 1,
    /// `PA_STREAM_RECORD`.
    Record = 2,
}

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

/// `pa_buffer_attr`. `u32::MAX` leaves a value to the server.
#[repr(C)]
pub(crate) struct BufferAttributes {
    pub(crate) maxlength: u32,
    pub(crate) tlength: u32,
    pub(crate) prebuf: u32,
    pub(crate) minreq: u32,
    pub(crate) fragsize: u32,
}

impl Default for BufferAttributes {
    fn default() -> Self {
        Self {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: u32::MAX,
        }
    }
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
type Write = unsafe extern "C" fn(
    stream: *mut c_void,
    data: *const c_void,
    bytes: usize,
    error: *mut c_int,
) -> c_int;
type Free = unsafe extern "C" fn(stream: *mut c_void);
type StrError = unsafe extern "C" fn(error: c_int) -> *const c_char;

/// What a stream is called and carries.
pub(crate) struct StreamOptions<'a> {
    pub(crate) direction: Direction,
    /// The application's name in the sound server's mixer.
    pub(crate) application: &'a CStr,
    pub(crate) stream: &'a CStr,
    /// The source or sink, or `None` for the server's default.
    pub(crate) device: Option<&'a CStr>,
    pub(crate) rate: u32,
    pub(crate) channels: u8,
    pub(crate) attributes: BufferAttributes,
}

/// An open stream of interleaved 16-bit samples, freed when dropped.
pub(crate) struct Stream {
    _library: libloading::Library,
    read: Read,
    write: Write,
    free: Free,
    strerror: StrError,
    stream: *mut c_void,
}

// SAFETY: a pa_simple stream may be used from any thread, one at a time;
// only the thread that owns the `Stream` uses it.
unsafe impl Send for Stream {}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the stream came from pa_simple_new and is freed once.
        unsafe { (self.free)(self.stream) };
    }
}

impl Stream {
    pub(crate) fn open(options: StreamOptions<'_>) -> anyhow::Result<Self> {
        // SAFETY: loading libpulse-simple runs its initializers, which have
        // no preconditions.
        let library = unsafe { libloading::Library::new("libpulse-simple.so.0") }
            .context("the PulseAudio client library (libpulse-simple) is not installed")?;
        // SAFETY: the symbols have the declared C signatures.
        let (new, read, write, free, strerror) = unsafe {
            let new = *library.get::<New>(b"pa_simple_new\0")?;
            let read = *library.get::<Read>(b"pa_simple_read\0")?;
            let write = *library.get::<Write>(b"pa_simple_write\0")?;
            let free = *library.get::<Free>(b"pa_simple_free\0")?;
            // pa_strerror is libpulse's, which libpulse-simple links.
            let strerror = *library.get::<StrError>(b"pa_strerror\0")?;
            (new, read, write, free, strerror)
        };
        let spec = SampleSpec {
            format: SAMPLE_S16LE,
            rate: options.rate,
            channels: options.channels,
        };
        let mut error = 0;
        // SAFETY: every pointer is either null where allowed or points to a
        // NUL-terminated string or structure that outlives the call.
        let stream = unsafe {
            new(
                std::ptr::null(),
                options.application.as_ptr(),
                options.direction as c_int,
                options.device.map_or(std::ptr::null(), CStr::as_ptr),
                options.stream.as_ptr(),
                &spec,
                std::ptr::null(),
                &options.attributes,
                &mut error,
            )
        };
        if stream.is_null() {
            bail!("{}", describe(strerror, error));
        }
        Ok(Self {
            _library: library,
            read,
            write,
            free,
            strerror,
            stream,
        })
    }

    /// Fills `samples` from a recording stream, waiting for them.
    pub(crate) fn read(&self, samples: &mut [u8]) -> Result<(), String> {
        let mut error = 0;
        // SAFETY: the stream is open and `samples` is writable for its length.
        let read = unsafe {
            (self.read)(
                self.stream,
                samples.as_mut_ptr().cast(),
                samples.len(),
                &mut error,
            )
        };
        if read < 0 {
            return Err(describe(self.strerror, error));
        }
        Ok(())
    }

    /// Plays `samples` on a playback stream, waiting while the server's
    /// buffer is full.
    pub(crate) fn write(&self, samples: &[u8]) -> Result<(), String> {
        let mut error = 0;
        // SAFETY: the stream is open and `samples` is readable for its length.
        let written = unsafe {
            (self.write)(
                self.stream,
                samples.as_ptr().cast(),
                samples.len(),
                &mut error,
            )
        };
        if written < 0 {
            return Err(describe(self.strerror, error));
        }
        Ok(())
    }
}

fn describe(strerror: StrError, error: c_int) -> String {
    // SAFETY: pa_strerror returns a static string for any error code.
    let text = unsafe { strerror(error) };
    if text.is_null() {
        return format!("PulseAudio error {error}");
    }
    // SAFETY: the string is NUL-terminated and static.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}
