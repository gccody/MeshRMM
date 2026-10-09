use super::renderer::shader::ShaderConversion;
use super::*;
use crate::video_layout::VideoRect;

/// A blue frame from the Agent's software encoder, which never holds
/// frames back for reordering.
const FIXTURE: &[u8] = include_bytes!("../../../../tests/fixtures/yuv420p-software.h264");

fn fixture_format() -> VideoFormat {
    VideoFormat {
        width: 64,
        height: 48,
        frames_per_second: 30,
        codec: Codec::H264,
        pixel_format: meshrmm_protocol::PixelFormat::Nv12,
        bitrate_bits_per_second: 1_000_000,
    }
}

unsafe fn bgra_target(device: &ID3D11Device, format: VideoFormat) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: format.width,
        Height: format.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.unwrap();
    texture.unwrap()
}

/// The top-left pixel of `target`.
unsafe fn first_pixel(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    target: &ID3D11Texture2D,
) -> [u8; 4] {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { target.GetDesc(&mut desc) };
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    let mut staging = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }.unwrap();
    let staging = staging.unwrap();
    unsafe { context.CopyResource(&staging, target) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.unwrap();
    let mut pixel = [0; 4];
    unsafe { ptr::copy_nonoverlapping(mapped.pData.cast::<u8>(), pixel.as_mut_ptr(), 4) };
    unsafe { context.Unmap(&staging, 0) };
    pixel
}

/// Converts `texture` with a video processor, as the renderer does on
/// a GPU.
unsafe fn processor_pixel(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    subresource: u32,
    format: VideoFormat,
) -> anyhow::Result<[u8; 4]> {
    let video_device: ID3D11VideoDevice = device.cast()?;
    let video_context: ID3D11VideoContext = context.cast()?;
    let rate = DXGI_RATIONAL {
        Numerator: 30,
        Denominator: 1,
    };
    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: rate,
        InputWidth: format.width,
        InputHeight: format.height,
        OutputFrameRate: rate,
        OutputWidth: format.width,
        OutputHeight: format.height,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
        .context("video processor enumeration")?;
    let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
        .context("video processor creation")?;
    let output = unsafe { bgra_target(device, format) };
    let mut output_view = None;
    unsafe {
        video_device.CreateVideoProcessorOutputView(
            &output,
            &enumerator,
            &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            },
            Some(&mut output_view),
        )
    }
    .context("output view")?;
    let mut input_view = None;
    unsafe {
        video_device.CreateVideoProcessorInputView(
            texture,
            &enumerator,
            &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: subresource,
                    },
                },
            },
            Some(&mut input_view),
        )
    }
    .context("decoded texture input view")?;
    let source = RECT {
        left: 0,
        top: 0,
        right: format.width as i32,
        bottom: format.height as i32,
    };
    unsafe { video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source)) };
    let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
        Enable: true.into(),
        pInputSurface: ManuallyDrop::new(input_view),
        ..Default::default()
    };
    let result = unsafe {
        video_context.VideoProcessorBlt(
            &processor,
            output_view.as_ref().context("no output view")?,
            0,
            std::slice::from_ref(&stream),
        )
    };
    let _ = unsafe { ManuallyDrop::take(&mut stream.pInputSurface) };
    result.context("YUV-to-BGRA blit")?;
    Ok(unsafe { first_pixel(device, context, &output) })
}

/// Converts `texture` with the shader the renderer uses without one.
unsafe fn shader_pixel(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    subresource: u32,
    format: VideoFormat,
) -> anyhow::Result<[u8; 4]> {
    let output = unsafe { bgra_target(device, format) };
    let mut shader = unsafe { ShaderConversion::new(device, format)? };
    let layout = window::ClientLayout {
        width: format.width,
        height: format.height,
        video: VideoRect {
            left: 0,
            top: 0,
            width: format.width as i32,
            height: format.height as i32,
        },
    };
    unsafe { shader.configure_output(&output, &layout)? };
    unsafe { shader.convert(context, texture, subresource, format)? };
    Ok(unsafe { first_pixel(device, context, &output) })
}

fn assert_blue(case: &str, pixel: [u8; 4]) {
    let [blue, green, red, _] = pixel;
    assert!(
        blue > 200 && green < 60 && red < 60,
        "{case}: {pixel:?} is not blue"
    );
}

/// Microsoft's decoder, on the GPU and in software, on every device the
/// viewer can get: each decodes the frame without waiting for more
/// input, and both conversions show it blue.
#[test]
#[ignore = "requires Windows Media Foundation"]
fn microsoft_h264_decoder_shows_the_fixture_on_every_device() {
    let _com = unsafe { ComRuntime::start() }.unwrap();
    let _mf = unsafe { MediaFoundationRuntime::start() }.unwrap();
    let format = fixture_format();
    let mut software = 0;
    for (driver_type, video) in [
        (D3D_DRIVER_TYPE_HARDWARE, true),
        (D3D_DRIVER_TYPE_HARDWARE, false),
        (D3D_DRIVER_TYPE_WARP, false),
    ] {
        let (device, context) = unsafe { create_device_of(driver_type, video) }
            .unwrap_or_else(|error| panic!("{error:#}"));
        for gpu in [true, false] {
            let case = format!("{driver_type:?} video={video} gpu={gpu}");
            let mut decoder = match unsafe { Decoder::microsoft_h264(&device, format, gpu) } {
                Ok(decoder) => decoder,
                Err(error) if gpu => {
                    println!("{case}: no DXVA decoder: {error:#}");
                    continue;
                }
                Err(error) => panic!("{case}: software decoder: {error:#}"),
            };
            let queued = QueuedFrame {
                frame: EncodedFrame {
                    stream_id: meshrmm_protocol::VideoStreamId(1),
                    frame_id: 1,
                    capture_timestamp_us: 0,
                    encode_complete_timestamp_us: 0,
                    send_timestamp_us: 0,
                    keyframe: true,
                    data: FIXTURE.to_vec(),
                },
                received_at_us: 0,
            };
            let decoded = unsafe { decoder.decode(&queued) }.unwrap();
            assert!(decoded.accepted);
            let frame = decoded
                .frames
                .last()
                .unwrap_or_else(|| panic!("{case}: the decoder held the frame back"));
            // Basic Render Driver accepts the video flag without
            // offering a video device.
            if device.cast::<ID3D11VideoDevice>().is_ok() {
                let pixel = unsafe {
                    processor_pixel(&device, &context, &frame.texture, frame.subresource, format)
                }
                .unwrap_or_else(|error| panic!("{case}: video processor: {error:#}"));
                assert_blue(&format!("{case} video processor"), pixel);
            }
            // Only devices without a video processor use the shader,
            // and their pictures always come from software decoding.
            if decoder.kind == DecoderKind::MicrosoftSoftware {
                let pixel = unsafe {
                    shader_pixel(&device, &context, &frame.texture, frame.subresource, format)
                }
                .unwrap_or_else(|error| panic!("{case}: shader: {error:#}"));
                assert_blue(&format!("{case} shader"), pixel);
            }
            println!(
                "{case}: {:?} decoder, output {:?}, subresource {}",
                decoder.kind, decoder.output_size, frame.subresource
            );
            if !gpu {
                software += 1;
            }
        }
    }
    assert_eq!(software, 3, "software decoding failed on a device");
}

/// The device the viewer gets on this machine, and that it decodes.
#[test]
#[ignore = "requires Windows Media Foundation"]
fn the_viewer_device_decodes_h264() {
    let _com = unsafe { ComRuntime::start() }.unwrap();
    let _mf = unsafe { MediaFoundationRuntime::start() }.unwrap();
    let (device, _context) = unsafe { create_device() }.unwrap();
    let decoder = unsafe { Decoder::new(&device, fixture_format()) }.unwrap();
    println!(
        "video device: {}, decoder: {:?}",
        device.cast::<ID3D11VideoDevice>().is_ok(),
        decoder.kind
    );
}
