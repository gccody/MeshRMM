//! A PipeWire video stream from a compositor's screencast node. GNOME and
//! KDE hand out their screen as PipeWire nodes, so the helper connects to
//! the session's PipeWire daemon and receives the frames in shared memory.
//!
//! `libpipewire-0.3` is loaded when a capture starts, so the Agent runs on
//! computers without it. The few functions, structures and SPA pods used
//! here are declared by hand from the stable PipeWire 0.3 ABI.
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, bail};
use meshrmm_protocol::CursorShape;

use super::FrameSink;
use crate::remote::linux::cursor::ThemeShapes;
use crate::remote::linux::encoder::{PixelLayout, RawFrame};

// SPA types, from spa/utils/type.h.
const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_INT: u32 = 4;
const SPA_TYPE_RECTANGLE: u32 = 10;
const SPA_TYPE_FRACTION: u32 = 11;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_CHOICE: u32 = 19;
const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
const SPA_TYPE_OBJECT_PARAM_BUFFERS: u32 = 0x40004;
const SPA_TYPE_OBJECT_PARAM_META: u32 = 0x40005;
// spa/pod/pod.h
const SPA_CHOICE_NONE: u32 = 0;
const SPA_CHOICE_RANGE: u32 = 1;
const SPA_CHOICE_ENUM: u32 = 3;
const SPA_CHOICE_FLAGS: u32 = 4;
// spa/param/param.h
const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_PARAM_FORMAT: u32 = 4;
const SPA_PARAM_BUFFERS: u32 = 5;
const SPA_PARAM_META: u32 = 6;
// spa/param/format.h
const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_VIDEO_FORMAT: u32 = 0x20001;
const SPA_FORMAT_VIDEO_SIZE: u32 = 0x20003;
const SPA_FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
// spa/param/video/raw.h
const SPA_VIDEO_FORMAT_RGBX: u32 = 7;
const SPA_VIDEO_FORMAT_BGRX: u32 = 8;
const SPA_VIDEO_FORMAT_XRGB: u32 = 9;
const SPA_VIDEO_FORMAT_XBGR: u32 = 10;
const SPA_VIDEO_FORMAT_RGBA: u32 = 11;
const SPA_VIDEO_FORMAT_BGRA: u32 = 12;
const SPA_VIDEO_FORMAT_ARGB: u32 = 13;
const SPA_VIDEO_FORMAT_ABGR: u32 = 14;
// spa/param/buffers.h
const SPA_PARAM_BUFFERS_DATA_TYPE: u32 = 6;
const SPA_PARAM_META_TYPE: u32 = 1;
const SPA_PARAM_META_SIZE: u32 = 2;
// spa/buffer/meta.h
const SPA_META_CURSOR: u32 = 5;
// spa/buffer/buffer.h
const SPA_DATA_MEM_PTR: u32 = 1;
const SPA_DATA_MEM_FD: u32 = 2;
const SPA_CHUNK_FLAG_CORRUPTED: i32 = 1;
// pipewire/stream.h
const PW_DIRECTION_INPUT: c_int = 0;
const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
const PW_STREAM_STATE_ERROR: c_int = -1;
const PW_STREAM_STATE_UNCONNECTED: c_int = 0;
const PW_VERSION_STREAM_EVENTS: u32 = 2;

#[repr(C)]
struct SpaList {
    next: *mut SpaList,
    prev: *mut SpaList,
}

#[repr(C)]
struct SpaHook {
    link: SpaList,
    funcs: *const c_void,
    data: *mut c_void,
    removed: Option<unsafe extern "C" fn(*mut SpaHook)>,
    private: *mut c_void,
}

#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

#[repr(C)]
struct SpaData {
    kind: u32,
    flags: u32,
    fd: i64,
    map_offset: u32,
    max_size: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

#[repr(C)]
struct SpaBuffer {
    meta_count: u32,
    data_count: u32,
    metas: *mut SpaMeta,
    datas: *mut SpaData,
}

#[repr(C)]
struct SpaMeta {
    kind: u32,
    size: u32,
    data: *mut c_void,
}

/// `struct spa_meta_cursor`.
#[repr(C)]
struct SpaMetaCursor {
    id: u32,
    flags: u32,
    position: [i32; 2],
    hotspot: [i32; 2],
    bitmap_offset: u32,
}

/// `struct spa_meta_bitmap`.
#[repr(C)]
struct SpaMetaBitmap {
    format: u32,
    size: [u32; 2],
    stride: i32,
    offset: u32,
}

#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
    requested: u64,
}

#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    process: Option<unsafe extern "C" fn(*mut c_void)>,
    drained: Option<unsafe extern "C" fn(*mut c_void)>,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Option<unsafe extern "C" fn(*mut c_void)>,
}

type Opaque = c_void;

/// The functions used from libpipewire-0.3.
struct Api {
    _library: libloading::Library,
    thread_loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut Opaque,
    thread_loop_get_loop: unsafe extern "C" fn(*mut Opaque) -> *mut Opaque,
    thread_loop_start: unsafe extern "C" fn(*mut Opaque) -> c_int,
    thread_loop_stop: unsafe extern "C" fn(*mut Opaque),
    thread_loop_lock: unsafe extern "C" fn(*mut Opaque),
    thread_loop_unlock: unsafe extern "C" fn(*mut Opaque),
    thread_loop_destroy: unsafe extern "C" fn(*mut Opaque),
    context_new: unsafe extern "C" fn(*mut Opaque, *mut Opaque, usize) -> *mut Opaque,
    context_connect: unsafe extern "C" fn(*mut Opaque, *mut Opaque, usize) -> *mut Opaque,
    context_connect_fd: unsafe extern "C" fn(*mut Opaque, c_int, *mut Opaque, usize) -> *mut Opaque,
    context_destroy: unsafe extern "C" fn(*mut Opaque),
    core_disconnect: unsafe extern "C" fn(*mut Opaque) -> c_int,
    properties_new_string: unsafe extern "C" fn(*const c_char) -> *mut Opaque,
    stream_new: unsafe extern "C" fn(*mut Opaque, *const c_char, *mut Opaque) -> *mut Opaque,
    stream_add_listener:
        unsafe extern "C" fn(*mut Opaque, *mut SpaHook, *const StreamEvents, *mut c_void),
    stream_connect:
        unsafe extern "C" fn(*mut Opaque, c_int, u32, u32, *mut *const c_void, u32) -> c_int,
    stream_update_params: unsafe extern "C" fn(*mut Opaque, *mut *const c_void, u32) -> c_int,
    stream_dequeue_buffer: unsafe extern "C" fn(*mut Opaque) -> *mut PwBuffer,
    stream_queue_buffer: unsafe extern "C" fn(*mut Opaque, *mut PwBuffer) -> c_int,
    stream_disconnect: unsafe extern "C" fn(*mut Opaque) -> c_int,
    stream_destroy: unsafe extern "C" fn(*mut Opaque),
}

// SAFETY: the function pointers are plain C functions; PipeWire's own
// locking rules are kept by the callers.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn api() -> anyhow::Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))
}

fn load() -> anyhow::Result<Api> {
    // SAFETY: loading libpipewire runs its initializers, which have no
    // preconditions.
    let library = unsafe { libloading::Library::new("libpipewire-0.3.so.0") }
        .context("PipeWire (libpipewire-0.3) is not installed")?;
    macro_rules! symbol {
        ($name:literal) => {
            // SAFETY: the symbol has the signature of the field it is stored in.
            *unsafe { library.get(concat!($name, "\0").as_bytes()) }
                .context(concat!("libpipewire has no ", $name))?
        };
    }
    let init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) = symbol!("pw_init");
    // SAFETY: pw_init accepts null arguments and may be called repeatedly.
    unsafe { init(std::ptr::null_mut(), std::ptr::null_mut()) };
    Ok(Api {
        thread_loop_new: symbol!("pw_thread_loop_new"),
        thread_loop_get_loop: symbol!("pw_thread_loop_get_loop"),
        thread_loop_start: symbol!("pw_thread_loop_start"),
        thread_loop_stop: symbol!("pw_thread_loop_stop"),
        thread_loop_lock: symbol!("pw_thread_loop_lock"),
        thread_loop_unlock: symbol!("pw_thread_loop_unlock"),
        thread_loop_destroy: symbol!("pw_thread_loop_destroy"),
        context_new: symbol!("pw_context_new"),
        context_connect: symbol!("pw_context_connect"),
        context_connect_fd: symbol!("pw_context_connect_fd"),
        context_destroy: symbol!("pw_context_destroy"),
        core_disconnect: symbol!("pw_core_disconnect"),
        properties_new_string: symbol!("pw_properties_new_string"),
        stream_new: symbol!("pw_stream_new"),
        stream_add_listener: symbol!("pw_stream_add_listener"),
        stream_connect: symbol!("pw_stream_connect"),
        stream_update_params: symbol!("pw_stream_update_params"),
        stream_dequeue_buffer: symbol!("pw_stream_dequeue_buffer"),
        stream_queue_buffer: symbol!("pw_stream_queue_buffer"),
        stream_disconnect: symbol!("pw_stream_disconnect"),
        stream_destroy: symbol!("pw_stream_destroy"),
        _library: library,
    })
}

/// Writes SPA pods: each a size and a type, then the body, padded to eight
/// bytes.
#[derive(Default)]
struct PodWriter(Vec<u8>);

impl PodWriter {
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_ne_bytes());
    }

    fn pad(&mut self) {
        while !self.0.len().is_multiple_of(8) {
            self.0.push(0);
        }
    }

    fn id(&mut self, value: u32) {
        self.u32(4);
        self.u32(SPA_TYPE_ID);
        self.u32(value);
        self.pad();
    }

    /// A choice of `kind` among `values`, each `child_size` bytes of `child_type`.
    fn choice(&mut self, kind: u32, child_type: u32, child_size: u32, values: &[&[u32]]) {
        self.u32(16 + child_size * values.len() as u32);
        self.u32(SPA_TYPE_CHOICE);
        self.u32(kind);
        self.u32(0);
        self.u32(child_size);
        self.u32(child_type);
        for value in values {
            for &word in *value {
                self.u32(word);
            }
        }
        self.pad();
    }

    /// An object of `object_type` for param `id`; `properties` writes its
    /// properties, each a key and flags followed by a value pod.
    fn object(&mut self, object_type: u32, id: u32, properties: impl FnOnce(&mut Self)) {
        let start = self.0.len();
        self.u32(0);
        self.u32(SPA_TYPE_OBJECT);
        self.u32(object_type);
        self.u32(id);
        properties(self);
        let size = (self.0.len() - start - 8) as u32;
        self.0[start..start + 4].copy_from_slice(&size.to_ne_bytes());
    }

    fn property(&mut self, key: u32) {
        self.u32(key);
        self.u32(0);
    }
}

/// The formats to offer: 32-bit RGB in any order, of any size and rate,
/// in shared memory rather than GPU buffers.
fn enum_format() -> Vec<u8> {
    let mut pod = PodWriter::default();
    pod.object(SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT, |pod| {
        pod.property(SPA_FORMAT_MEDIA_TYPE);
        pod.id(SPA_MEDIA_TYPE_VIDEO);
        pod.property(SPA_FORMAT_MEDIA_SUBTYPE);
        pod.id(SPA_MEDIA_SUBTYPE_RAW);
        pod.property(SPA_FORMAT_VIDEO_FORMAT);
        pod.choice(
            SPA_CHOICE_ENUM,
            SPA_TYPE_ID,
            4,
            &[
                &[SPA_VIDEO_FORMAT_BGRA],
                &[SPA_VIDEO_FORMAT_BGRA],
                &[SPA_VIDEO_FORMAT_BGRX],
                &[SPA_VIDEO_FORMAT_RGBX],
                &[SPA_VIDEO_FORMAT_RGBA],
                &[SPA_VIDEO_FORMAT_XRGB],
                &[SPA_VIDEO_FORMAT_ARGB],
                &[SPA_VIDEO_FORMAT_XBGR],
                &[SPA_VIDEO_FORMAT_ABGR],
            ],
        );
        pod.property(SPA_FORMAT_VIDEO_SIZE);
        pod.choice(
            SPA_CHOICE_RANGE,
            SPA_TYPE_RECTANGLE,
            8,
            &[&[1920, 1080], &[1, 1], &[8192, 8192]],
        );
        pod.property(SPA_FORMAT_VIDEO_FRAMERATE);
        pod.choice(
            SPA_CHOICE_RANGE,
            SPA_TYPE_FRACTION,
            8,
            &[&[0, 1], &[0, 1], &[1000, 1]],
        );
    });
    pod.0
}

/// The buffers to ask for once the format is set: shared memory only.
fn buffers() -> Vec<u8> {
    let mut pod = PodWriter::default();
    pod.object(SPA_TYPE_OBJECT_PARAM_BUFFERS, SPA_PARAM_BUFFERS, |pod| {
        pod.property(SPA_PARAM_BUFFERS_DATA_TYPE);
        pod.choice(
            SPA_CHOICE_FLAGS,
            SPA_TYPE_INT,
            4,
            &[&[(1 << SPA_DATA_MEM_PTR) | (1 << SPA_DATA_MEM_FD)]],
        );
    });
    pod.0
}

/// The largest cursor image the metadata may carry, in pixels a side.
const LARGEST_CURSOR: u32 = 512;

/// The cursor metadata to ask for, for a compositor that sends the pointer
/// beside the frames rather than in them: room for a cursor of up to
/// [`LARGEST_CURSOR`] pixels a side.
fn cursor_meta() -> Vec<u8> {
    let size = |side: u32| {
        (std::mem::size_of::<SpaMetaCursor>() + std::mem::size_of::<SpaMetaBitmap>()) as u32
            + side * side * 4
    };
    let mut pod = PodWriter::default();
    pod.object(SPA_TYPE_OBJECT_PARAM_META, SPA_PARAM_META, |pod| {
        pod.property(SPA_PARAM_META_TYPE);
        pod.id(SPA_META_CURSOR);
        pod.property(SPA_PARAM_META_SIZE);
        pod.choice(
            SPA_CHOICE_RANGE,
            SPA_TYPE_INT,
            4,
            &[&[size(64)], &[size(1)], &[size(LARGEST_CURSOR)]],
        );
    });
    pod.0
}

/// The pixel layout and size of a negotiated format pod.
fn parse_format(pod: &[u8]) -> Option<(PixelLayout, u32, u32)> {
    let word = |offset: usize| -> Option<u32> {
        Some(u32::from_ne_bytes(
            pod.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let size = word(0)? as usize;
    if word(4)? != SPA_TYPE_OBJECT || word(8)? != SPA_TYPE_OBJECT_FORMAT {
        return None;
    }
    let end = (8 + size).min(pod.len());
    let mut offset = 16;
    let (mut layout, mut dimensions) = (None, None);
    while offset + 16 <= end {
        let key = word(offset)?;
        let value_size = word(offset + 8)? as usize;
        let mut value_type = word(offset + 12)?;
        let mut body = offset + 16;
        // A choice of none holds the value as its first element.
        if value_type == SPA_TYPE_CHOICE && word(body)? == SPA_CHOICE_NONE {
            value_type = word(body + 12)?;
            body += 16;
        }
        match (key, value_type) {
            (SPA_FORMAT_VIDEO_FORMAT, SPA_TYPE_ID) => {
                layout = match word(body)? {
                    SPA_VIDEO_FORMAT_BGRX | SPA_VIDEO_FORMAT_BGRA => Some(PixelLayout::Bgrx),
                    SPA_VIDEO_FORMAT_RGBX | SPA_VIDEO_FORMAT_RGBA => Some(PixelLayout::Rgbx),
                    SPA_VIDEO_FORMAT_XRGB | SPA_VIDEO_FORMAT_ARGB => Some(PixelLayout::Xrgb),
                    SPA_VIDEO_FORMAT_XBGR | SPA_VIDEO_FORMAT_ABGR => Some(PixelLayout::Xbgr),
                    _ => None,
                };
            }
            (SPA_FORMAT_VIDEO_SIZE, SPA_TYPE_RECTANGLE) => {
                dimensions = Some((word(body)?, word(body + 4)?));
            }
            _ => {}
        }
        offset += 16 + value_size.div_ceil(8) * 8;
    }
    let (width, height) = dimensions?;
    Some((layout?, width, height))
}

/// State the PipeWire thread shares with the stream's owner.
struct Callbacks {
    api: &'static Api,
    stream: *mut Opaque,
    sink: FrameSink,
    format: Mutex<Option<(PixelLayout, u32, u32)>>,
    /// The pointer's shape, from the cursor metadata.
    cursor: Arc<Mutex<CursorShape>>,
    shapes: Mutex<ThemeShapes>,
    ended: Mutex<Option<String>>,
    stopped: AtomicBool,
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: c_int,
    state: c_int,
    error: *const c_char,
) {
    // SAFETY: `data` is the Callbacks this listener was added with.
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    if callbacks.stopped.load(Ordering::SeqCst) {
        return;
    }
    let reason = match state {
        PW_STREAM_STATE_ERROR if !error.is_null() => {
            // SAFETY: PipeWire passes a NUL-terminated message.
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        }
        PW_STREAM_STATE_ERROR => "the PipeWire stream failed".to_owned(),
        PW_STREAM_STATE_UNCONNECTED => "the compositor ended the screencast".to_owned(),
        _ => return,
    };
    *callbacks.ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const c_void) {
    if id != SPA_PARAM_FORMAT || param.is_null() {
        return;
    }
    // SAFETY: `data` is the Callbacks this listener was added with.
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    // SAFETY: a pod starts with its body's size.
    let size = unsafe { *param.cast::<u32>() } as usize + 8;
    // SAFETY: the pod is `size` bytes long.
    let pod = unsafe { std::slice::from_raw_parts(param.cast::<u8>(), size) };
    let format = parse_format(pod);
    tracing::info!(?format, "the screencast's format is set");
    *callbacks.format.lock().unwrap_or_else(|e| e.into_inner()) = format;
    let (buffers, meta) = (buffers(), cursor_meta());
    let mut params = [
        buffers.as_ptr().cast::<c_void>(),
        meta.as_ptr().cast::<c_void>(),
    ];
    // SAFETY: called on the PipeWire thread with a live stream; the pods
    // outlive the call, which copies them.
    unsafe {
        (callbacks.api.stream_update_params)(
            callbacks.stream,
            params.as_mut_ptr(),
            params.len() as u32,
        )
    };
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    // SAFETY: `data` is the Callbacks this listener was added with.
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let api = callbacks.api;
    // Only the newest buffer is wanted; older ones go straight back.
    let mut newest: *mut PwBuffer = std::ptr::null_mut();
    loop {
        // SAFETY: called on the PipeWire thread with a live stream.
        let buffer = unsafe { (api.stream_dequeue_buffer)(callbacks.stream) };
        if buffer.is_null() {
            break;
        }
        // Each buffer may carry a new cursor, even one not delivered.
        // SAFETY: a dequeued buffer stays valid until it is queued again.
        unsafe { read_cursor(callbacks, &*buffer) };
        if !newest.is_null() {
            // SAFETY: the buffer was dequeued from this stream.
            unsafe { (api.stream_queue_buffer)(callbacks.stream, newest) };
        }
        newest = buffer;
    }
    if newest.is_null() {
        return;
    }
    // SAFETY: a dequeued buffer stays valid until it is queued again.
    unsafe { deliver(callbacks, &*newest) };
    // SAFETY: as above.
    unsafe { (api.stream_queue_buffer)(callbacks.stream, newest) };
}

/// Names the cursor in a buffer's metadata, when it carries a new image.
///
/// # Safety
///
/// `buffer` must be dequeued from the stream.
unsafe fn read_cursor(callbacks: &Callbacks, buffer: &PwBuffer) {
    // SAFETY: a dequeued buffer has a valid spa_buffer.
    let spa = unsafe { &*buffer.buffer };
    if spa.metas.is_null() {
        return;
    }
    // SAFETY: the buffer has `meta_count` metadata entries.
    let metas = unsafe { std::slice::from_raw_parts(spa.metas, spa.meta_count as usize) };
    let Some(meta) = metas.iter().find(|meta| {
        meta.kind == SPA_META_CURSOR
            && !meta.data.is_null()
            && meta.size as usize >= std::mem::size_of::<SpaMetaCursor>()
    }) else {
        return;
    };
    // SAFETY: the metadata holds `size` bytes, at least a cursor's.
    let bytes = unsafe { std::slice::from_raw_parts(meta.data.cast::<u8>(), meta.size as usize) };
    if let Some(shape) = cursor_image(bytes, |width, height, alpha| {
        callbacks
            .shapes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .classify(width, height, alpha)
    }) {
        *callbacks.cursor.lock().unwrap_or_else(|e| e.into_inner()) = shape;
    }
}

/// The shape `classify` names a cursor metadata's new image, if it has one:
/// its width, height and the alpha of each pixel.
fn cursor_image(
    meta: &[u8],
    classify: impl FnOnce(usize, usize, &[u8]) -> CursorShape,
) -> Option<CursorShape> {
    let word = |offset: usize| -> Option<u32> {
        Some(u32::from_ne_bytes(
            meta.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    // An ID of 0 carries nothing new.
    if word(std::mem::offset_of!(SpaMetaCursor, id))? == 0 {
        return None;
    }
    let bitmap = word(std::mem::offset_of!(SpaMetaCursor, bitmap_offset))? as usize;
    if bitmap < std::mem::size_of::<SpaMetaCursor>() {
        return None;
    }
    let field = |name: usize| word(bitmap + name);
    let format = field(std::mem::offset_of!(SpaMetaBitmap, format))?;
    let width = field(std::mem::offset_of!(SpaMetaBitmap, size))? as usize;
    let height = field(std::mem::offset_of!(SpaMetaBitmap, size) + 4)? as usize;
    let stride = field(std::mem::offset_of!(SpaMetaBitmap, stride))? as i32;
    let pixels = field(std::mem::offset_of!(SpaMetaBitmap, offset))? as usize;
    // Which byte of a pixel is alpha. Formats without alpha can't be named.
    let alpha = match format {
        SPA_VIDEO_FORMAT_RGBA | SPA_VIDEO_FORMAT_BGRA => 3,
        SPA_VIDEO_FORMAT_ARGB | SPA_VIDEO_FORMAT_ABGR => 0,
        _ => return None,
    };
    // An offset of 0 is an invisible cursor, which keeps its last shape.
    if pixels < std::mem::size_of::<SpaMetaBitmap>()
        || width == 0
        || height == 0
        || width > LARGEST_CURSOR as usize
        || height > LARGEST_CURSOR as usize
    {
        return None;
    }
    let stride = if stride > 0 {
        stride as usize
    } else {
        width * 4
    };
    let start = bitmap + pixels;
    let end = start + stride * (height - 1) + width * 4;
    let image = meta.get(start..end)?;
    let mask: Vec<u8> = (0..height)
        .flat_map(|row| (0..width).map(move |column| row * stride + column * 4 + alpha))
        .map(|index| image[index])
        .collect();
    Some(classify(width, height, &mask))
}

/// Sends a buffer's frame to the sink.
///
/// # Safety
///
/// `buffer` must be dequeued from the stream and mapped.
unsafe fn deliver(callbacks: &Callbacks, buffer: &PwBuffer) {
    let Some((layout, width, height)) = *callbacks.format.lock().unwrap_or_else(|e| e.into_inner())
    else {
        return;
    };
    // SAFETY: a dequeued buffer has a valid spa_buffer.
    let spa = unsafe { &*buffer.buffer };
    if spa.data_count == 0 || spa.datas.is_null() {
        return;
    }
    // SAFETY: there is at least one data entry.
    let data = unsafe { &*spa.datas };
    if data.data.is_null() || data.chunk.is_null() {
        return;
    }
    // SAFETY: the chunk belongs to the data entry.
    let chunk = unsafe { &*data.chunk };
    // An empty chunk updates only the cursor metadata.
    if chunk.size == 0 || chunk.flags & SPA_CHUNK_FLAG_CORRUPTED != 0 {
        return;
    }
    let stride = if chunk.stride > 0 {
        chunk.stride as usize
    } else {
        width as usize * 4
    };
    let offset = (chunk.offset % data.max_size.max(1)) as usize;
    let length = (data.max_size as usize).saturating_sub(offset);
    // SAFETY: MAP_BUFFERS mapped `max_size` bytes at `data`.
    let bytes = unsafe { std::slice::from_raw_parts(data.data.cast::<u8>().add(offset), length) };
    (callbacks.sink)(&RawFrame {
        data: bytes,
        width,
        height,
        stride,
        layout,
        timestamp_us: crate::remote::linux::monotonic_timestamp_us(),
    });
}

/// A connected screencast stream; it stops when dropped.
pub(crate) struct Stream {
    api: &'static Api,
    thread_loop: *mut Opaque,
    context: *mut Opaque,
    core: *mut Opaque,
    stream: *mut Opaque,
    _hook: Box<SpaHook>,
    callbacks: Box<Callbacks>,
    _events: Box<StreamEvents>,
}

// SAFETY: every PipeWire call after setup happens under the thread loop's
// lock, from whichever thread owns the stream.
unsafe impl Send for Stream {}

impl Stream {
    /// Connects to screencast node `node`, through `fd` when a portal handed
    /// one out, or to the session's PipeWire daemon. `cursor` follows the
    /// pointer's shape when the compositor sends it as metadata.
    pub(crate) fn connect(
        fd: Option<c_int>,
        node: u32,
        sink: FrameSink,
        cursor: Arc<Mutex<CursorShape>>,
    ) -> anyhow::Result<Self> {
        let api = api()?;
        // SAFETY: the calls follow PipeWire's documented setup order; every
        // pointer is checked before it is used.
        unsafe {
            let thread_loop = (api.thread_loop_new)(c"meshrmm-pipewire".as_ptr(), std::ptr::null());
            if thread_loop.is_null() {
                bail!("could not create a PipeWire loop");
            }
            let context = (api.context_new)(
                (api.thread_loop_get_loop)(thread_loop),
                std::ptr::null_mut(),
                0,
            );
            if context.is_null() {
                (api.thread_loop_destroy)(thread_loop);
                bail!("could not create a PipeWire context");
            }
            if (api.thread_loop_start)(thread_loop) < 0 {
                (api.context_destroy)(context);
                (api.thread_loop_destroy)(thread_loop);
                bail!("could not start the PipeWire loop");
            }
            (api.thread_loop_lock)(thread_loop);
            let core = match fd {
                Some(fd) => (api.context_connect_fd)(context, fd, std::ptr::null_mut(), 0),
                None => (api.context_connect)(context, std::ptr::null_mut(), 0),
            };
            let mut stream = Self {
                api,
                thread_loop,
                context,
                core,
                stream: std::ptr::null_mut(),
                _hook: Box::new(std::mem::zeroed()),
                callbacks: Box::new(Callbacks {
                    api,
                    stream: std::ptr::null_mut(),
                    sink,
                    format: Mutex::new(None),
                    cursor,
                    shapes: Mutex::default(),
                    ended: Mutex::new(None),
                    stopped: AtomicBool::new(false),
                }),
                _events: Box::new(StreamEvents {
                    version: PW_VERSION_STREAM_EVENTS,
                    destroy: None,
                    state_changed: Some(on_state_changed),
                    control_info: None,
                    io_changed: None,
                    param_changed: Some(on_param_changed),
                    add_buffer: None,
                    remove_buffer: None,
                    process: Some(on_process),
                    drained: None,
                    command: None,
                    trigger_done: None,
                }),
            };
            if core.is_null() {
                (api.thread_loop_unlock)(thread_loop);
                bail!("could not connect to PipeWire");
            }
            let properties = (api.properties_new_string)(
                c"media.type=Video media.category=Capture media.role=Screen".as_ptr(),
            );
            stream.stream = (api.stream_new)(core, c"MeshRMM screen capture".as_ptr(), properties);
            if stream.stream.is_null() {
                (api.thread_loop_unlock)(thread_loop);
                bail!("could not create a PipeWire stream");
            }
            stream.callbacks.stream = stream.stream;
            (api.stream_add_listener)(
                stream.stream,
                &raw mut *stream._hook,
                &raw const *stream._events,
                (&raw const *stream.callbacks).cast_mut().cast(),
            );
            let format = enum_format();
            let mut params = [format.as_ptr().cast::<c_void>()];
            let result = (api.stream_connect)(
                stream.stream,
                PW_DIRECTION_INPUT,
                node,
                PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS,
                params.as_mut_ptr(),
                1,
            );
            (api.thread_loop_unlock)(thread_loop);
            if result < 0 {
                bail!("could not connect to the screencast's PipeWire node ({result})");
            }
            Ok(stream)
        }
    }

    /// Why the stream stopped, once it has.
    pub(crate) fn ended(&self) -> Option<String> {
        self.callbacks
            .ended
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let api = self.api;
        self.callbacks.stopped.store(true, Ordering::SeqCst);
        // SAFETY: tearing down in the reverse order of setup, holding the
        // loop's lock for the objects it runs.
        unsafe {
            (api.thread_loop_lock)(self.thread_loop);
            if !self.stream.is_null() {
                (api.stream_disconnect)(self.stream);
                (api.stream_destroy)(self.stream);
            }
            if !self.core.is_null() {
                (api.core_disconnect)(self.core);
            }
            (api.thread_loop_unlock)(self.thread_loop);
            (api.thread_loop_stop)(self.thread_loop);
            (api.context_destroy)(self.context);
            (api.thread_loop_destroy)(self.thread_loop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offered_format_is_a_well_formed_pod() {
        let pod = enum_format();
        assert_eq!(pod.len() % 8, 0);
        let size = u32::from_ne_bytes(pod[..4].try_into().unwrap()) as usize;
        assert_eq!(size + 8, pod.len());
        assert_eq!(
            u32::from_ne_bytes(pod[4..8].try_into().unwrap()),
            SPA_TYPE_OBJECT
        );
    }

    #[test]
    fn reads_the_negotiated_format() {
        let mut pod = PodWriter::default();
        pod.object(SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_FORMAT, |pod| {
            pod.property(SPA_FORMAT_MEDIA_TYPE);
            pod.id(SPA_MEDIA_TYPE_VIDEO);
            pod.property(SPA_FORMAT_VIDEO_FORMAT);
            pod.id(SPA_VIDEO_FORMAT_RGBX);
            pod.property(SPA_FORMAT_VIDEO_SIZE);
            pod.u32(8);
            pod.u32(SPA_TYPE_RECTANGLE);
            pod.u32(2560);
            pod.u32(1440);
            pod.property(SPA_FORMAT_VIDEO_FRAMERATE);
            pod.choice(SPA_CHOICE_NONE, SPA_TYPE_FRACTION, 8, &[&[0, 1]]);
        });
        assert_eq!(parse_format(&pod.0), Some((PixelLayout::Rgbx, 2560, 1440)));
        assert_eq!(parse_format(&buffers()), None);
    }

    #[test]
    fn the_cursor_metadata_request_is_a_well_formed_pod() {
        let pod = cursor_meta();
        assert_eq!(pod.len() % 8, 0);
        let size = u32::from_ne_bytes(pod[..4].try_into().unwrap()) as usize;
        assert_eq!(size + 8, pod.len());
    }

    /// Cursor metadata with a `width` by `height` RGBA image whose alpha is
    /// `alpha`.
    fn cursor_meta_bytes(id: u32, width: u32, height: u32, alpha: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for word in [id, 0, 10, 20, 1, 1, 28] {
            bytes.extend_from_slice(&word.to_ne_bytes());
        }
        for word in [SPA_VIDEO_FORMAT_RGBA, width, height, width * 4, 20] {
            bytes.extend_from_slice(&word.to_ne_bytes());
        }
        for &value in alpha {
            bytes.extend_from_slice(&[1, 2, 3, value]);
        }
        bytes
    }

    #[test]
    fn reads_the_alpha_of_a_new_cursor_image() {
        let alpha = [0, 255, 128, 7];
        let meta = cursor_meta_bytes(1, 2, 2, &alpha);
        let shape = cursor_image(&meta, |width, height, mask| {
            assert_eq!((width, height, mask), (2, 2, &alpha[..]));
            CursorShape::Text
        });
        assert_eq!(shape, Some(CursorShape::Text));
        // No new cursor, or an image cut short.
        let unnamed = |_: usize, _: usize, _: &[u8]| -> CursorShape { unreachable!() };
        assert_eq!(
            cursor_image(&cursor_meta_bytes(0, 2, 2, &alpha), unnamed),
            None
        );
        assert_eq!(cursor_image(&meta[..meta.len() - 1], unnamed), None);
        assert_eq!(cursor_image(&meta[..20], unnamed), None);
    }
}
