use super::*;

pub(super) fn codec_subtype(codec: Codec) -> windows::core::GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::H265 => MFVideoFormat_HEVC,
    }
}

pub(super) fn decoded_subtype(pixel_format: meshrmm_protocol::PixelFormat) -> windows::core::GUID {
    match pixel_format {
        meshrmm_protocol::PixelFormat::Nv12 => MFVideoFormat_NV12,
        meshrmm_protocol::PixelFormat::Ayuv => MFVideoFormat_AYUV,
    }
}

pub(super) fn provides_samples(info: &MFT_OUTPUT_STREAM_INFO) -> bool {
    info.dwFlags
        & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
            | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
        != 0
}

/// Gives `transform` the D3D11 device so it decodes into GPU surfaces.
pub(super) unsafe fn attach_device(
    transform: &IMFTransform,
    device: &ID3D11Device,
) -> anyhow::Result<IMFDXGIDeviceManager> {
    // The renderer and asynchronous decoder use the same immediate
    // context. Protect it before the MFT receives the D3D device manager.
    let context =
        unsafe { device.GetImmediateContext() }.context("decoder immediate context unavailable")?;
    let multithread: ID3D11Multithread = context
        .cast()
        .context("decoder D3D multithread protection unavailable")?;
    let _ = unsafe { multithread.SetMultithreadProtected(true) };
    let mut reset_token = 0;
    let mut manager = None;
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager) }
        .context("decoder D3D manager creation failed")?;
    let manager = manager.context("Media Foundation returned no decoder D3D manager")?;
    unsafe { manager.ResetDevice(device, reset_token) }
        .context("decoder D3D manager reset failed")?;
    unsafe {
        transform.ProcessMessage(
            MFT_MESSAGE_SET_D3D_MANAGER,
            Interface::as_raw(&manager) as usize,
        )
    }
    .context("failed to attach D3D manager to decoder")?;
    Ok(manager)
}

/// Sets the first output type `transform` offers with subtype `wanted`.
pub(super) unsafe fn select_output_type(
    transform: &IMFTransform,
    wanted: windows::core::GUID,
) -> anyhow::Result<()> {
    for index in 0.. {
        let media_type = match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(media_type) => media_type,
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
            Err(error) => {
                return Err(error).context("decoder output-type enumeration failed");
            }
        };
        if unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(wanted) {
            unsafe { transform.SetOutputType(0, &media_type, 0) }
                .context("decoder rejected its available output type")?;
            return Ok(());
        }
    }
    bail!("video decoder offers no output type with the requested YUV format")
}

/// The picture size of `transform`'s current output type.
pub(super) unsafe fn output_size(transform: &IMFTransform) -> anyhow::Result<(u32, u32)> {
    let media_type =
        unsafe { transform.GetOutputCurrentType(0) }.context("decoder output type unavailable")?;
    let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
        .context("decoder output type has no frame size")?;
    Ok(((size >> 32) as u32, size as u32))
}

pub(super) unsafe fn video_type(
    subtype: windows::core::GUID,
    format: VideoFormat,
) -> anyhow::Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }.context("video media type creation failed")?;
    unsafe { media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }?;
    unsafe { media_type.SetGUID(&MF_MT_SUBTYPE, &subtype) }?;
    if subtype == MFVideoFormat_H264 || subtype == MFVideoFormat_HEVC {
        match (format.codec, format.pixel_format) {
            (Codec::H264, meshrmm_protocol::PixelFormat::Nv12) => unsafe {
                media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
            }?,
            (Codec::H264, meshrmm_protocol::PixelFormat::Ayuv) => unsafe {
                media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_444.0 as u32)
            }?,
            (Codec::H265, meshrmm_protocol::PixelFormat::Nv12) => unsafe {
                media_type.SetUINT32(&MF_MT_VIDEO_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)
            }?,
            (Codec::H265, meshrmm_protocol::PixelFormat::Ayuv) => unsafe {
                media_type.SetUINT32(&MF_MT_VIDEO_PROFILE, eAVEncH265VProfile_Main_444_8.0 as u32)
            }?,
        }
    }
    unsafe {
        media_type.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(format.width) << 32) | u64::from(format.height),
        )
    }?;
    unsafe {
        media_type.SetUINT64(
            &MF_MT_FRAME_RATE,
            (u64::from(format.frames_per_second) << 32) | 1,
        )
    }?;
    unsafe { media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1_u64 << 32) | 1) }?;
    unsafe { media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32) }?;
    unsafe { media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32) }?;
    unsafe { media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32) }?;
    unsafe { media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32) }?;
    unsafe { media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32) }?;
    Ok(media_type)
}
