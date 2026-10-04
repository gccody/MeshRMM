//! VideoToolbox H.264/HEVC encoding of captured frames into Annex-B access
//! units, configured like the Windows hardware encoders: real time, low
//! latency, no frame reordering, keyframes only on request, and an
//! approximately one-frame rate-control buffer. VideoToolbox picks the
//! hardware encoder and falls back to its software encoder by itself.
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use anyhow::bail;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex, kCMVideoCodecType_H264,
    kCMVideoCodecType_HEVC,
};
use objc2_core_video::CVPixelBuffer;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};

use meshrmm_protocol::Codec;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

pub(crate) struct EncodedAccessUnit {
    pub data: Vec<u8>,
    /// Annex-B parameter sets, present on keyframes.
    pub codec_config: Option<Vec<u8>>,
    pub keyframe: bool,
    pub capture_timestamp_us: u64,
    pub encode_complete_timestamp_us: u64,
}

pub(crate) type FrameSink = Arc<dyn Fn(EncodedAccessUnit) + Send + Sync + 'static>;

struct Output {
    codec: Codec,
    sink: FrameSink,
    failure: Mutex<Option<String>>,
}

pub(crate) struct Encoder {
    session: CFRetained<VTCompressionSession>,
    output: *const Output,
    frames_per_second: u32,
}

// SAFETY: VideoToolbox compression sessions may be used from any thread, and
// `Encoder` is only used behind a lock. `output` is shared with the callback.
unsafe impl Send for Encoder {}

impl Encoder {
    pub(crate) fn new(
        codec: Codec,
        width: u32,
        height: u32,
        frames_per_second: u32,
        bits_per_second: u32,
        sink: FrameSink,
    ) -> anyhow::Result<Self> {
        let output = Arc::into_raw(Arc::new(Output {
            codec,
            sink,
            failure: Mutex::new(None),
        }));
        let codec_type = match codec {
            Codec::H264 => kCMVideoCodecType_H264,
            Codec::H265 => kCMVideoCodecType_HEVC,
        };
        let create = |low_latency: bool| {
            // SAFETY: the key is a valid static VideoToolbox constant.
            let specification = low_latency.then(|| {
                CFDictionary::<CFString, CFType>::from_slices(
                    &[unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl }],
                    &[CFBoolean::new(true)],
                )
            });
            let mut session = std::ptr::null_mut();
            // SAFETY: the callback matches VTCompressionOutputCallback and its
            // reference stays valid until the session is invalidated in Drop.
            let status = unsafe {
                VTCompressionSession::create(
                    None,
                    width as i32,
                    height as i32,
                    codec_type,
                    specification.as_deref().map(|d| d.as_opaque()),
                    None,
                    None,
                    Some(compressed),
                    output.cast_mut().cast(),
                    NonNull::from(&mut session),
                )
            };
            NonNull::new(session)
                .filter(|_| status == 0)
                // SAFETY: VTCompressionSessionCreate returned an owned (+1) session.
                .map(|session| unsafe { CFRetained::from_raw(session) })
                .ok_or(status)
        };
        // Low-latency rate control is what real-time screen sharing wants, but
        // not every encoder offers it.
        let session = match create(true).or_else(|_| create(false)) {
            Ok(session) => session,
            Err(status) => {
                // SAFETY: no session holds the output reference.
                drop(unsafe { Arc::from_raw(output) });
                bail!("VideoToolbox could not create a {codec:?} encoder ({status})");
            }
        };
        let encoder = Self {
            session,
            output,
            frames_per_second: frames_per_second.max(1),
        };
        // SAFETY: the keys are valid static VideoToolbox constants.
        unsafe {
            encoder.set(kVTCompressionPropertyKey_RealTime, CFBoolean::new(true))?;
            encoder.set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                CFBoolean::new(false),
            )?;
            encoder.set(
                kVTCompressionPropertyKey_ProfileLevel,
                match codec {
                    Codec::H264 => kVTProfileLevel_H264_High_AutoLevel,
                    Codec::H265 => kVTProfileLevel_HEVC_Main_AutoLevel,
                },
            )?;
            // Periodic keyframes discard the sharp desktop reference and blur
            // the picture; startup and recovery request their own.
            encoder.set(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                &CFNumber::new_i32(0),
            )?;
            encoder.set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &CFNumber::new_i32(frames_per_second as i32),
            )?;
        }
        encoder.set_bitrate(bits_per_second)?;
        Ok(encoder)
    }

    fn set(&self, key: &CFString, value: &CFType) -> anyhow::Result<()> {
        // SAFETY: the session and both CF values are valid.
        let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
        if status != 0 {
            bail!("VideoToolbox rejected the {key} setting ({status})");
        }
        Ok(())
    }

    /// Sets the average bitrate with a rate-control buffer of about one frame,
    /// keeping the Windows encoders' 16 KiB floor for detailed text.
    pub(crate) fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()> {
        let frame_bytes = bits_per_second
            .div_ceil(8)
            .div_ceil(self.frames_per_second)
            .max(16 * 1024);
        let limits = CFArray::<CFNumber>::from_retained_objects(&[
            CFNumber::new_i64(i64::from(frame_bytes)),
            CFNumber::new_f64(1.0 / f64::from(self.frames_per_second)),
        ]);
        // SAFETY: the keys are valid static VideoToolbox constants.
        unsafe {
            self.set(
                kVTCompressionPropertyKey_AverageBitRate,
                &CFNumber::new_i64(i64::from(bits_per_second)),
            )?;
            if let Err(error) = self.set(kVTCompressionPropertyKey_DataRateLimits, &limits) {
                tracing::debug!(%error, "encoder keeps its default rate-control buffer");
            }
        }
        Ok(())
    }

    /// Encodes `frame`, captured at `capture_timestamp_us`. Results arrive at
    /// the sink, on a VideoToolbox thread.
    pub(crate) fn encode(
        &self,
        frame: &CVPixelBuffer,
        capture_timestamp_us: u64,
        keyframe: bool,
    ) -> anyhow::Result<()> {
        // SAFETY: `output` lives until Drop.
        if let Some(failure) = unsafe { &*self.output }
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            bail!("VideoToolbox encoding failed: {failure}");
        }
        let options = keyframe.then(|| {
            CFDictionary::<CFString, CFType>::from_slices(
                // SAFETY: the key is a valid static VideoToolbox constant.
                &[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }],
                &[CFBoolean::new(true)],
            )
        });
        // Re-encoding an unchanged frame reuses its capture time, so the
        // presentation time comes from the clock instead.
        // SAFETY: CMTimeMake has no preconditions.
        let presentation =
            unsafe { CMTime::new(super::monotonic_timestamp_us() as i64, 1_000_000) };
        // SAFETY: the frame and options are valid; the reference is a plain
        // integer handed back to the callback.
        let status = unsafe {
            self.session.encode_frame(
                frame,
                presentation,
                CMTime::new(1, self.frames_per_second as i32),
                options.as_deref().map(|d| d.as_opaque()),
                capture_timestamp_us as usize as *mut c_void,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            bail!("VideoToolbox could not encode a frame ({status})");
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: invalidation waits for pending callbacks, after which none
        // can use the output reference.
        unsafe {
            self.session
                .complete_frames(objc2_core_media::kCMTimeInvalid);
            self.session.invalidate();
            drop(Arc::from_raw(self.output));
        }
    }
}

unsafe extern "C-unwind" fn compressed(
    output: *mut c_void,
    capture_timestamp: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: the session was created with a live `Output` as its reference.
    let output = unsafe { &*output.cast::<Output>() };
    let unit = if status != 0 {
        Err(format!("status {status}"))
    } else if let Some(sample) = NonNull::new(sample) {
        // SAFETY: VideoToolbox passes a valid sample buffer for the call.
        access_unit(
            output.codec,
            unsafe { sample.as_ref() },
            capture_timestamp as u64,
        )
    } else {
        // The encoder dropped the frame.
        return;
    };
    match unit {
        Ok(unit) => (output.sink)(unit),
        Err(error) => {
            *output.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
        }
    }
}

fn access_unit(
    codec: Codec,
    sample: &CMSampleBuffer,
    capture_timestamp_us: u64,
) -> Result<EncodedAccessUnit, String> {
    // SAFETY: the sample buffer is valid for the duration of this call.
    let block = unsafe { sample.data_buffer() }.ok_or("the encoded frame has no data")?;
    // SAFETY: the block buffer is valid.
    let length = unsafe { block.data_length() };
    let mut bytes = vec![0_u8; length];
    // SAFETY: `bytes` holds `length` bytes.
    let status = unsafe { block.copy_data_bytes(0, length, NonNull::from(&mut bytes[..]).cast()) };
    if status != 0 {
        return Err(format!("could not read the encoded frame ({status})"));
    }
    let data = annex_b(&bytes).ok_or("the encoded frame has malformed NAL units")?;
    let keyframe = contains_keyframe(codec, &data);
    let codec_config = if keyframe {
        // SAFETY: the sample buffer is valid.
        let description = unsafe { sample.format_description() }
            .ok_or("the encoded keyframe has no format description")?;
        Some(parameter_sets(codec, &description)?)
    } else {
        None
    };
    Ok(EncodedAccessUnit {
        data,
        codec_config,
        keyframe,
        capture_timestamp_us,
        encode_complete_timestamp_us: super::monotonic_timestamp_us(),
    })
}

/// Converts VideoToolbox's 4-byte length-prefixed NAL units to Annex-B.
fn annex_b(mut avcc: &[u8]) -> Option<Vec<u8>> {
    let mut annex_b = Vec::with_capacity(avcc.len() + 16);
    while !avcc.is_empty() {
        let length = u32::from_be_bytes(avcc.get(..4)?.try_into().ok()?) as usize;
        let unit = avcc.get(4..4 + length)?;
        annex_b.extend_from_slice(&START_CODE);
        annex_b.extend_from_slice(unit);
        avcc = &avcc[4 + length..];
    }
    Some(annex_b)
}

/// Whether Annex-B `data` holds an IDR picture (H.264) or IRAP picture (HEVC).
fn contains_keyframe(codec: Codec, data: &[u8]) -> bool {
    data.windows(4)
        .enumerate()
        .filter(|(_, window)| *window == START_CODE)
        .filter_map(|(index, _)| data.get(index + 4))
        .any(|&header| match codec {
            Codec::H264 => header & 0x1f == 5,
            Codec::H265 => (16..=21).contains(&((header >> 1) & 0x3f)),
        })
}

fn parameter_sets(codec: Codec, description: &CMFormatDescription) -> Result<Vec<u8>, String> {
    let get = match codec {
        Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        Codec::H265 => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    };
    let mut count = 0;
    // SAFETY: only the count is requested.
    let status = unsafe {
        get(
            description,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut count,
            std::ptr::null_mut(),
        )
    };
    if status != 0 || count == 0 {
        return Err(format!("the encoder reported no parameter sets ({status})"));
    }
    let mut config = Vec::new();
    for index in 0..count {
        let mut pointer = std::ptr::null();
        let mut size = 0;
        // SAFETY: the returned pointer stays valid while `description` is retained.
        let status = unsafe {
            get(
                description,
                index,
                &mut pointer,
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 || pointer.is_null() {
            return Err(format!("could not read parameter set {index} ({status})"));
        }
        config.extend_from_slice(&START_CODE);
        // SAFETY: VideoToolbox reported `size` readable bytes at `pointer`.
        config.extend_from_slice(unsafe { std::slice::from_raw_parts(pointer, size) });
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_length_prefixes_to_start_codes() {
        let avcc = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x41];
        assert_eq!(
            annex_b(&avcc).unwrap(),
            [0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1, 0x41]
        );
        assert_eq!(annex_b(&[0, 0, 0, 5, 1]), None);
    }

    #[test]
    fn finds_keyframes_by_nal_type() {
        assert!(contains_keyframe(Codec::H264, &[0, 0, 0, 1, 0x65, 0]));
        assert!(!contains_keyframe(Codec::H264, &[0, 0, 0, 1, 0x41, 0]));
        // HEVC IDR_W_RADL is type 19, a trailing picture type 1.
        assert!(contains_keyframe(Codec::H265, &[0, 0, 0, 1, 19 << 1, 1]));
        assert!(!contains_keyframe(Codec::H265, &[0, 0, 0, 1, 1 << 1, 1]));
    }
}
