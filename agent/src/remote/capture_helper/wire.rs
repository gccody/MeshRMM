//! How commands and events are framed on the helpers' pipes.
use super::*;

pub(super) fn write_command(mut writer: impl Write, command: &ParentCommand) -> io::Result<()> {
    match command {
        ParentCommand::PromptCredentials => writer.write_all(&[23]),
        ParentCommand::AutofillCredentials(bytes) => {
            checked_len(bytes.len(), 8192, "protected credentials")?;
            writer.write_all(&[24])?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(bytes)
        }
        ParentCommand::EnumerateDisplays => writer.write_all(&[COMMAND_ENUMERATE_DISPLAYS]),
        ParentCommand::StartFiles => writer.write_all(&[13]),
        ParentCommand::StartClipboard => writer.write_all(&[16]),
        ParentCommand::StartChatHelper { viewer_name } => {
            checked_len(viewer_name.len(), MAX_CONTROL_BYTES, "viewer name")?;
            writer.write_all(&[17])?;
            write_u32(&mut writer, viewer_name.len() as u32)?;
            writer.write_all(viewer_name.as_bytes())
        }
        ParentCommand::Files(message) => {
            writer.write_all(&[12])?;
            write_file_message(&mut writer, message)
        }
        ParentCommand::Start {
            viewer_name,
            display_id,
            frames_per_second,
            bitrate_bits_per_second,
            codec,
            pixel_format,
            capture_cursor,
            grayscale,
        } => {
            writer.write_all(&[COMMAND_START])?;
            checked_len(viewer_name.len(), MAX_DISPLAY_NAME_BYTES, "viewer name")?;
            write_u32(&mut writer, viewer_name.len() as u32)?;
            writer.write_all(viewer_name.as_bytes())?;
            write_u32(&mut writer, display_id.map_or(NO_DISPLAY, |id| id.0))?;
            write_u32(&mut writer, *frames_per_second)?;
            write_u32(&mut writer, *bitrate_bits_per_second)
                .and_then(|()| writer.write_all(&[codec_byte(*codec)]))
                .and_then(|()| writer.write_all(&[pixel_format_byte(*pixel_format)]))
                .and_then(|()| writer.write_all(&[u8::from(*capture_cursor)]))
                .and_then(|()| writer.write_all(&[u8::from(*grayscale)]))
        }
        ParentCommand::SetWallpaperHidden(hidden) => writer.write_all(&[19, u8::from(*hidden)]),
        ParentCommand::SetPreventIdleLock(enabled) => writer.write_all(&[21, u8::from(*enabled)]),
        ParentCommand::SetCursorCapture(enabled) => writer.write_all(&[18, u8::from(*enabled)]),
        ParentCommand::SetDisplayBorder(enabled) => writer.write_all(&[20, u8::from(*enabled)]),
        ParentCommand::RequestKeyframe => writer.write_all(&[COMMAND_REQUEST_KEYFRAME]),
        ParentCommand::SetBitrate(bits_per_second) => {
            writer.write_all(&[COMMAND_SET_BITRATE])?;
            write_u32(&mut writer, *bits_per_second)
        }
        ParentCommand::StartInput {
            display_id,
            viewer_name,
        } => {
            checked_len(viewer_name.len(), MAX_DISPLAY_NAME_BYTES, "viewer name")?;
            writer.write_all(&[COMMAND_START_INPUT])?;
            write_u32(&mut writer, display_id.0)?;
            write_u32(&mut writer, viewer_name.len() as u32)?;
            writer.write_all(viewer_name.as_bytes())
        }
        ParentCommand::Input(input) => {
            let bytes = SessionMessage::Input(input.clone())
                .encode()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            checked_len(bytes.len(), MAX_CONTROL_BYTES, "desktop input")?;
            writer.write_all(&[COMMAND_INPUT])?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(&bytes)
        }
        ParentCommand::Blackout { enabled, text } => {
            checked_len(text.len(), MAX_CONTROL_BYTES, "blackout message")?;
            writer.write_all(&[COMMAND_BLACKOUT, u8::from(*enabled)])?;
            write_u32(&mut writer, text.len() as u32)?;
            writer.write_all(text.as_bytes())
        }
        ParentCommand::BlockInput(blocked) => {
            writer.write_all(&[COMMAND_BLOCK_INPUT, u8::from(*blocked)])
        }
        ParentCommand::ReleaseInput => writer.write_all(&[COMMAND_RELEASE_INPUT]),
        ParentCommand::Clipboard(text) => {
            let bytes = text
                .encode()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            checked_len(bytes.len(), MAX_CLIPBOARD_WIRE_BYTES, "desktop clipboard")?;
            writer.write_all(&[COMMAND_CLIPBOARD])?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(&bytes)
        }
        ParentCommand::StopChat => writer.write_all(&[COMMAND_STOP_CHAT]),
        ParentCommand::StartChat => writer.write_all(&[COMMAND_START_CHAT]),
        ParentCommand::Chat(text) => {
            if !meshrmm_protocol::valid_chat_text(text) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid chat text",
                ));
            }
            writer.write_all(&[COMMAND_CHAT])?;
            write_u32(&mut writer, text.len() as u32)?;
            writer.write_all(text.as_bytes())
        }
        ParentCommand::Stop => writer.write_all(&[COMMAND_STOP]),
    }
}

pub(super) fn read_command(mut reader: impl Read) -> io::Result<ParentCommand> {
    match read_u8(&mut reader)? {
        23 => Ok(ParentCommand::PromptCredentials),
        24 => {
            let length = bounded_len(read_u32(&mut reader)?, 8192, "protected credentials")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            Ok(ParentCommand::AutofillCredentials(bytes))
        }
        COMMAND_ENUMERATE_DISPLAYS => Ok(ParentCommand::EnumerateDisplays),
        16 => Ok(ParentCommand::StartClipboard),
        17 => {
            let length = bounded_len(read_u32(&mut reader)?, MAX_CONTROL_BYTES, "viewer name")?;
            let mut name = vec![0; length];
            reader.read_exact(&mut name)?;
            let viewer_name = String::from_utf8(name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(ParentCommand::StartChatHelper { viewer_name })
        }
        COMMAND_START => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_DISPLAY_NAME_BYTES,
                "viewer name",
            )?;
            let mut name = vec![0; length];
            reader.read_exact(&mut name)?;
            let viewer_name = String::from_utf8(name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let display_id = read_u32(&mut reader)?;
            Ok(ParentCommand::Start {
                viewer_name,
                display_id: (display_id != NO_DISPLAY).then_some(DisplayId(display_id)),
                frames_per_second: read_u32(&mut reader)?,
                bitrate_bits_per_second: read_u32(&mut reader)?,
                codec: read_codec(&mut reader)?,
                pixel_format: read_pixel_format(&mut reader)?,
                capture_cursor: read_bool(&mut reader)?,
                grayscale: read_bool(&mut reader)?,
            })
        }
        19 => Ok(ParentCommand::SetWallpaperHidden(read_bool(&mut reader)?)),
        21 => Ok(ParentCommand::SetPreventIdleLock(read_bool(&mut reader)?)),
        18 => Ok(ParentCommand::SetCursorCapture(read_bool(&mut reader)?)),
        20 => Ok(ParentCommand::SetDisplayBorder(read_bool(&mut reader)?)),
        COMMAND_REQUEST_KEYFRAME => Ok(ParentCommand::RequestKeyframe),
        COMMAND_SET_BITRATE => Ok(ParentCommand::SetBitrate(read_u32(&mut reader)?)),
        COMMAND_START_INPUT => {
            let display_id = DisplayId(read_u32(&mut reader)?);
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_DISPLAY_NAME_BYTES,
                "viewer name",
            )?;
            let mut name = vec![0; length];
            reader.read_exact(&mut name)?;
            let viewer_name = String::from_utf8(name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(ParentCommand::StartInput {
                display_id,
                viewer_name,
            })
        }
        COMMAND_INPUT => {
            let length = bounded_len(read_u32(&mut reader)?, MAX_CONTROL_BYTES, "desktop input")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            match SessionMessage::decode(&bytes) {
                Ok(SessionMessage::Input(input)) => Ok(ParentCommand::Input(input)),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "desktop input contained a non-input message",
                )),
                Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
        }
        COMMAND_BLACKOUT => {
            let enabled = read_u8(&mut reader)?;
            if enabled > 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid blackout flag",
                ));
            }
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_CONTROL_BYTES,
                "blackout message",
            )?;
            let mut text = vec![0; length];
            reader.read_exact(&mut text)?;
            let text = String::from_utf8(text)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(ParentCommand::Blackout {
                enabled: enabled == 1,
                text,
            })
        }
        COMMAND_BLOCK_INPUT => {
            let mut value = [0];
            reader.read_exact(&mut value)?;
            match value[0] {
                0 => Ok(ParentCommand::BlockInput(false)),
                1 => Ok(ParentCommand::BlockInput(true)),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid input block flag",
                )),
            }
        }
        COMMAND_RELEASE_INPUT => Ok(ParentCommand::ReleaseInput),
        COMMAND_CLIPBOARD => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_CLIPBOARD_WIRE_BYTES,
                "desktop clipboard",
            )?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            ClipboardContent::decode(&bytes)
                .map(ParentCommand::Clipboard)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
        13 => Ok(ParentCommand::StartFiles),
        12 => Ok(ParentCommand::Files(read_file_message(&mut reader)?)),
        COMMAND_STOP_CHAT => Ok(ParentCommand::StopChat),
        COMMAND_START_CHAT => Ok(ParentCommand::StartChat),
        COMMAND_CHAT => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                meshrmm_protocol::MAX_CHAT_TEXT_BYTES,
                "chat text",
            )?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            let text = String::from_utf8(bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if !meshrmm_protocol::valid_chat_text(&text) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid chat text",
                ));
            }
            Ok(ParentCommand::Chat(text))
        }
        COMMAND_STOP => Ok(ParentCommand::Stop),
        opcode => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown desktop-helper command opcode {opcode}"),
        )),
    }
}

pub(super) fn write_event(mut writer: impl Write, event: &ChildEvent) -> io::Result<()> {
    match event {
        ChildEvent::CredentialPrompt(ready) => writer.write_all(&[13, u8::from(*ready)]),
        ChildEvent::Credentials(result) => {
            let bytes = serde_json::to_vec(result).map_err(io::Error::other)?;
            checked_len(bytes.len(), 32768, "credential result")?;
            writer.write_all(&[12])?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(&bytes)
        }
        ChildEvent::Files(message) => {
            writer.write_all(&[9])?;
            write_file_message(&mut writer, message)
        }
        ChildEvent::Started(started) => {
            writer.write_all(&[EVENT_STARTED])?;
            write_u32(&mut writer, started.format.width)?;
            write_u32(&mut writer, started.format.height)?;
            write_u32(&mut writer, started.format.frames_per_second)?;
            write_u32(&mut writer, started.format.bitrate_bits_per_second)?;
            writer.write_all(&[codec_byte(started.format.codec)])?;
            writer.write_all(&[pixel_format_byte(started.format.pixel_format)])?;
            write_u32(&mut writer, started.active_display.id.0)?;
            checked_len(started.displays.len(), MAX_DISPLAYS, "display list")?;
            write_u32(&mut writer, started.displays.len() as u32)?;
            for display in &started.displays {
                write_display(&mut writer, display)?;
            }
            Ok(())
        }
        ChildEvent::MaintenanceError(reason) => {
            checked_len(reason.len(), MAX_ERROR_BYTES, "maintenance error")?;
            writer.write_all(&[11])?;
            write_u32(&mut writer, reason.len() as u32)?;
            writer.write_all(reason.as_bytes())
        }
        ChildEvent::MaintenanceState {
            agent_input_blocked,
            blacked_out,
        } => writer.write_all(&[10, u8::from(*agent_input_blocked), u8::from(*blacked_out)]),
        ChildEvent::InputStarted => writer.write_all(&[EVENT_INPUT_STARTED]),
        ChildEvent::Frame(frame) => {
            let codec_config = frame.codec_config.as_deref().unwrap_or_default();
            checked_len(
                codec_config.len(),
                MAX_CODEC_CONFIG_BYTES,
                "codec configuration",
            )?;
            checked_len(frame.data.len(), MAX_FRAME_BYTES, "encoded frame")?;
            writer.write_all(&[EVENT_FRAME])?;
            write_u64(&mut writer, frame.capture_timestamp_us)?;
            write_u64(&mut writer, frame.encode_complete_timestamp_us)?;
            writer.write_all(&[u8::from(frame.keyframe)])?;
            write_u32(&mut writer, codec_config.len() as u32)?;
            write_u32(&mut writer, frame.data.len() as u32)?;
            writer.write_all(codec_config)?;
            writer.write_all(&frame.data)
        }
        ChildEvent::Cursor(shape, viewer_controls_input, pointer_display) => {
            let bytes = SessionMessage::CursorShape { shape: *shape }
                .encode()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            checked_len(bytes.len(), MAX_CONTROL_BYTES, "cursor shape")?;
            writer.write_all(&[EVENT_CURSOR, u8::from(*viewer_controls_input)])?;
            write_u32(&mut writer, pointer_display.map_or(u32::MAX, |id| id.0))?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(&bytes)
        }
        ChildEvent::Clipboard(text) => {
            let bytes = text
                .encode()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            checked_len(bytes.len(), MAX_CLIPBOARD_WIRE_BYTES, "clipboard content")?;
            writer.write_all(&[EVENT_CLIPBOARD])?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(&bytes)
        }
        ChildEvent::Chat(text) => {
            if !meshrmm_protocol::valid_chat_text(text) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid chat text",
                ));
            }
            writer.write_all(&[EVENT_CHAT])?;
            write_u32(&mut writer, text.len() as u32)?;
            writer.write_all(text.as_bytes())
        }
        ChildEvent::Error(message) => {
            let message = message.as_bytes();
            checked_len(message.len(), MAX_ERROR_BYTES, "desktop-helper error")?;
            writer.write_all(&[EVENT_ERROR])?;
            write_u32(&mut writer, message.len() as u32)?;
            writer.write_all(message)
        }
        ChildEvent::Stopped => writer.write_all(&[EVENT_STOPPED]),
    }
}

pub(super) fn read_event(mut reader: impl Read) -> io::Result<ChildEvent> {
    match read_u8(&mut reader)? {
        12 => {
            let length = bounded_len(read_u32(&mut reader)?, 32768, "credential result")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            Ok(ChildEvent::Credentials(
                serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            ))
        }
        13 => match read_u8(&mut reader)? {
            0 => Ok(ChildEvent::CredentialPrompt(false)),
            1 => Ok(ChildEvent::CredentialPrompt(true)),
            _ => Err(io::Error::other("invalid credential prompt state")),
        },
        11 => {
            let length = bounded_len(read_u32(&mut reader)?, MAX_ERROR_BYTES, "maintenance error")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            Ok(ChildEvent::MaintenanceError(
                String::from_utf8(bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            ))
        }
        10 => {
            let a = read_u8(&mut reader)?;
            let b = read_u8(&mut reader)?;
            if a > 1 || b > 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid maintenance state",
                ));
            }
            Ok(ChildEvent::MaintenanceState {
                agent_input_blocked: a == 1,
                blacked_out: b == 1,
            })
        }
        EVENT_STARTED => {
            let format = ActiveFormat {
                width: read_u32(&mut reader)?,
                height: read_u32(&mut reader)?,
                frames_per_second: read_u32(&mut reader)?,
                bitrate_bits_per_second: read_u32(&mut reader)?,
                codec: read_codec(&mut reader)?,
                pixel_format: read_pixel_format(&mut reader)?,
            };
            let active_display_id = DisplayId(read_u32(&mut reader)?);
            let count = bounded_len(read_u32(&mut reader)?, MAX_DISPLAYS, "display list")?;
            let mut displays = Vec::with_capacity(count);
            for _ in 0..count {
                displays.push(read_display(&mut reader)?);
            }
            let active_display = displays
                .iter()
                .find(|display| display.id == active_display_id)
                .cloned()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "desktop helper selected an unknown display",
                    )
                })?;
            Ok(ChildEvent::Started(StartedDesktop {
                format,
                displays,
                active_display,
            }))
        }
        EVENT_INPUT_STARTED => Ok(ChildEvent::InputStarted),
        EVENT_FRAME => {
            let capture_timestamp_us = read_u64(&mut reader)?;
            let encode_complete_timestamp_us = read_u64(&mut reader)?;
            let keyframe = match read_u8(&mut reader)? {
                0 => false,
                1 => true,
                value => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid keyframe flag {value}"),
                    ));
                }
            };
            let codec_config_len = bounded_len(
                read_u32(&mut reader)?,
                MAX_CODEC_CONFIG_BYTES,
                "codec configuration",
            )?;
            let frame_len = bounded_len(read_u32(&mut reader)?, MAX_FRAME_BYTES, "encoded frame")?;
            let mut codec_config = vec![0; codec_config_len];
            let mut data = vec![0; frame_len];
            reader.read_exact(&mut codec_config)?;
            reader.read_exact(&mut data)?;
            Ok(ChildEvent::Frame(EncodedAccessUnit {
                capture_timestamp_us,
                encode_complete_timestamp_us,
                keyframe,
                codec_config: (!codec_config.is_empty()).then_some(codec_config),
                data,
            }))
        }
        EVENT_CURSOR => {
            let viewer_controls_input = read_bool(&mut reader)?;
            let pointer_id = read_u32(&mut reader)?;
            let pointer_display = (pointer_id != u32::MAX).then_some(DisplayId(pointer_id));
            let length = bounded_len(read_u32(&mut reader)?, MAX_CONTROL_BYTES, "cursor shape")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            match SessionMessage::decode(&bytes) {
                Ok(SessionMessage::CursorShape { shape }) => Ok(ChildEvent::Cursor(
                    shape,
                    viewer_controls_input,
                    pointer_display,
                )),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cursor event contained an unexpected message",
                )),
                Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
        }
        EVENT_CLIPBOARD => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_CLIPBOARD_WIRE_BYTES,
                "clipboard content",
            )?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            ClipboardContent::decode(&bytes)
                .map(ChildEvent::Clipboard)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
        9 => Ok(ChildEvent::Files(read_file_message(&mut reader)?)),
        EVENT_CHAT => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                meshrmm_protocol::MAX_CHAT_TEXT_BYTES,
                "chat text",
            )?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            let text = String::from_utf8(bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if !meshrmm_protocol::valid_chat_text(&text) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid chat text",
                ));
            }
            Ok(ChildEvent::Chat(text))
        }
        EVENT_ERROR => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_ERROR_BYTES,
                "desktop-helper error",
            )?;
            let mut message = vec![0; length];
            reader.read_exact(&mut message)?;
            String::from_utf8(message)
                .map(ChildEvent::Error)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
        EVENT_STOPPED => Ok(ChildEvent::Stopped),
        opcode => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown desktop-helper event opcode {opcode}"),
        )),
    }
}

pub(super) fn write_display(writer: &mut impl Write, display: &Display) -> io::Result<()> {
    let name = display.name.as_bytes();
    checked_len(name.len(), MAX_DISPLAY_NAME_BYTES, "display name")?;
    write_u32(writer, display.id.0)?;
    write_u32(writer, display.x as u32)?;
    write_u32(writer, display.y as u32)?;
    write_u32(writer, display.width)?;
    write_u32(writer, display.height)?;
    writer.write_all(&[u8::from(display.primary)])?;
    write_u32(writer, name.len() as u32)?;
    writer.write_all(name)
}

pub(super) fn read_display(reader: &mut impl Read) -> io::Result<Display> {
    let id = DisplayId(read_u32(reader)?);
    let x = read_u32(reader)? as i32;
    let y = read_u32(reader)? as i32;
    let width = read_u32(reader)?;
    let height = read_u32(reader)?;
    let primary = match read_u8(reader)? {
        0 => false,
        1 => true,
        value => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid display primary flag {value}"),
            ));
        }
    };
    let name_len = bounded_len(read_u32(reader)?, MAX_DISPLAY_NAME_BYTES, "display name")?;
    let mut name = vec![0; name_len];
    reader.read_exact(&mut name)?;
    let name = String::from_utf8(name)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Display {
        session: if id == meshrmm_protocol::BACKGROUND_DISPLAY_ID {
            meshrmm_protocol::DesktopSession::Background
        } else {
            meshrmm_protocol::DesktopSession::Console
        },
        id,
        name,
        x,
        y,
        width,
        height,
        primary,
    })
}

pub(super) fn checked_len(length: usize, maximum: usize, label: &str) -> io::Result<()> {
    if length > maximum || length > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} exceeds the IPC size limit"),
        ));
    }
    Ok(())
}

pub(super) fn bounded_len(length: u32, maximum: usize, label: &str) -> io::Result<usize> {
    let length = length as usize;
    checked_len(length, maximum, label)?;
    Ok(length)
}

pub(super) fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

pub(super) fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

pub(super) fn read_bool(reader: &mut impl Read) -> io::Result<bool> {
    match read_u8(reader)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid boolean",
        )),
    }
}

pub(super) fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut value = [0; 1];
    reader.read_exact(&mut value)?;
    Ok(value[0])
}

pub(super) fn codec_byte(codec: VideoCodec) -> u8 {
    match codec {
        VideoCodec::H264 => 1,
        VideoCodec::H265 => 2,
    }
}

pub(super) fn read_codec(reader: &mut impl Read) -> io::Result<VideoCodec> {
    match read_u8(reader)? {
        1 => Ok(VideoCodec::H264),
        2 => Ok(VideoCodec::H265),
        value => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid desktop-helper codec {value}"),
        )),
    }
}

pub(super) fn pixel_format_byte(pixel_format: VideoPixelFormat) -> u8 {
    match pixel_format {
        VideoPixelFormat::Yuv420 => 1,
        VideoPixelFormat::Yuv444 => 2,
    }
}

pub(super) fn read_pixel_format(reader: &mut impl Read) -> io::Result<VideoPixelFormat> {
    match read_u8(reader)? {
        1 => Ok(VideoPixelFormat::Yuv420),
        2 => Ok(VideoPixelFormat::Yuv444),
        value => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid desktop-helper pixel format {value}"),
        )),
    }
}

pub(super) fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut value = [0; 4];
    reader.read_exact(&mut value)?;
    Ok(u32::from_le_bytes(value))
}

pub(super) fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = [0; 8];
    reader.read_exact(&mut value)?;
    Ok(u64::from_le_bytes(value))
}

pub(super) fn write_file_message(
    mut writer: impl Write,
    message: &meshrmm_protocol::FileMessage,
) -> io::Result<()> {
    let bytes = SessionMessage::FileTransfer(message.clone())
        .encode()
        .map_err(io::Error::other)?;
    checked_len(bytes.len(), MAX_CONTROL_BYTES, "file transfer")?;
    write_u32(&mut writer, bytes.len() as u32)?;
    writer.write_all(&bytes)
}

pub(super) fn read_file_message(
    mut reader: impl Read,
) -> io::Result<meshrmm_protocol::FileMessage> {
    let length = bounded_len(read_u32(&mut reader)?, MAX_CONTROL_BYTES, "file transfer")?;
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    match SessionMessage::decode(&bytes).map_err(io::Error::other)? {
        SessionMessage::FileTransfer(message) => Ok(message),
        _ => Err(io::Error::other("invalid file transfer packet")),
    }
}
