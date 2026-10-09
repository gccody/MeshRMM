//! How commands and events are framed on the helpers' pipes.
use super::*;

pub(super) fn write_command(mut writer: impl Write, command: &ParentCommand) -> io::Result<()> {
    match command {
        ParentCommand::PromptCredentials => writer.write_all(&[23]),
        ParentCommand::AutofillCredentials(bytes) => {
            checked_len(bytes.len(), 8192, "protected credentials")?;
            writer.write_all(&[24])?;
            write_sized(&mut writer, bytes)
        }
        ParentCommand::EnumerateDisplays => writer.write_all(&[COMMAND_ENUMERATE_DISPLAYS]),
        ParentCommand::CaptureThumbnail => writer.write_all(&[COMMAND_CAPTURE_THUMBNAIL]),
        ParentCommand::StartFiles => writer.write_all(&[13]),
        ParentCommand::StartClipboard => writer.write_all(&[16]),
        ParentCommand::StartChatHelper {
            viewer_name,
            show_banner,
        } => {
            checked_len(viewer_name.len(), MAX_CONTROL_BYTES, "viewer name")?;
            writer.write_all(&[17])?;
            write_sized(&mut writer, viewer_name.as_bytes())?;
            writer.write_all(&[u8::from(*show_banner)])
        }
        ParentCommand::ShowConnectionNotification { text } => {
            checked_len(text.len(), MAX_CONTROL_BYTES, "connection notification")?;
            writer.write_all(&[25])?;
            write_sized(&mut writer, text.as_bytes())
        }
        ParentCommand::PromptConnectionApproval {
            text,
            reason,
            timeout_seconds,
            lock_idle_seconds,
        } => {
            checked_len(text.len(), MAX_CONTROL_BYTES, "connection approval message")?;
            checked_len(reason.len(), MAX_CONTROL_BYTES, "connection reason")?;
            writer.write_all(&[COMMAND_PROMPT_CONNECTION_APPROVAL])?;
            write_sized(&mut writer, text.as_bytes())?;
            write_sized(&mut writer, reason.as_bytes())?;
            write_u32(&mut writer, *timeout_seconds)?;
            write_u32(&mut writer, *lock_idle_seconds)
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
            headless,
        } => {
            writer.write_all(&[COMMAND_START])?;
            checked_len(viewer_name.len(), MAX_DISPLAY_NAME_BYTES, "viewer name")?;
            write_sized(&mut writer, viewer_name.as_bytes())?;
            write_u32(&mut writer, display_id.map_or(NO_DISPLAY, |id| id.0))?;
            write_u32(&mut writer, *frames_per_second)?;
            write_u32(&mut writer, *bitrate_bits_per_second)
                .and_then(|()| writer.write_all(&[codec_byte(*codec)]))
                .and_then(|()| writer.write_all(&[pixel_format_byte(*pixel_format)]))
                .and_then(|()| writer.write_all(&[u8::from(*capture_cursor)]))
                .and_then(|()| writer.write_all(&[u8::from(*grayscale)]))?;
            write_headless_target(&mut writer, headless.as_ref())
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
            write_sized(&mut writer, viewer_name.as_bytes())
        }
        ParentCommand::Input(input) => {
            let bytes = encode_message(SessionMessage::Input(input.clone()))?;
            checked_len(bytes.len(), MAX_CONTROL_BYTES, "desktop input")?;
            writer.write_all(&[COMMAND_INPUT])?;
            write_sized(&mut writer, &bytes)
        }
        ParentCommand::Annotate(annotation) => {
            let bytes = encode_message(SessionMessage::Annotate(*annotation))?;
            writer.write_all(&[COMMAND_ANNOTATE])?;
            write_sized(&mut writer, &bytes)
        }
        ParentCommand::Blackout { enabled, text } => {
            checked_len(text.len(), MAX_CONTROL_BYTES, "blackout message")?;
            writer.write_all(&[COMMAND_BLACKOUT, u8::from(*enabled)])?;
            write_sized(&mut writer, text.as_bytes())
        }
        ParentCommand::BlockInput(blocked) => {
            writer.write_all(&[COMMAND_BLOCK_INPUT, u8::from(*blocked)])
        }
        ParentCommand::ReleaseInput => writer.write_all(&[COMMAND_RELEASE_INPUT]),
        ParentCommand::Clipboard(content) => {
            write_clipboard(&mut writer, COMMAND_CLIPBOARD, content, "desktop clipboard")
        }
        ParentCommand::StopChat => writer.write_all(&[COMMAND_STOP_CHAT]),
        ParentCommand::StartChat => writer.write_all(&[COMMAND_START_CHAT]),
        ParentCommand::Chat(text) => write_chat_text(&mut writer, COMMAND_CHAT, text),
        ParentCommand::Stop => writer.write_all(&[COMMAND_STOP]),
    }
}

pub(super) fn read_command(mut reader: impl Read) -> io::Result<ParentCommand> {
    match read_u8(&mut reader)? {
        23 => Ok(ParentCommand::PromptCredentials),
        24 => Ok(ParentCommand::AutofillCredentials(read_sized(
            &mut reader,
            8192,
            "protected credentials",
        )?)),
        COMMAND_ENUMERATE_DISPLAYS => Ok(ParentCommand::EnumerateDisplays),
        COMMAND_CAPTURE_THUMBNAIL => Ok(ParentCommand::CaptureThumbnail),
        16 => Ok(ParentCommand::StartClipboard),
        17 => Ok(ParentCommand::StartChatHelper {
            viewer_name: read_sized_string(&mut reader, MAX_CONTROL_BYTES, "viewer name")?,
            show_banner: read_bool(&mut reader)?,
        }),
        25 => Ok(ParentCommand::ShowConnectionNotification {
            text: read_sized_string(&mut reader, MAX_CONTROL_BYTES, "connection notification")?,
        }),
        COMMAND_PROMPT_CONNECTION_APPROVAL => {
            let label = "connection approval text";
            Ok(ParentCommand::PromptConnectionApproval {
                text: read_sized_string(&mut reader, MAX_CONTROL_BYTES, label)?,
                reason: read_sized_string(&mut reader, MAX_CONTROL_BYTES, label)?,
                timeout_seconds: read_u32(&mut reader)?,
                lock_idle_seconds: read_u32(&mut reader)?,
            })
        }
        COMMAND_START => read_start_command(&mut reader),
        19 => Ok(ParentCommand::SetWallpaperHidden(read_bool(&mut reader)?)),
        21 => Ok(ParentCommand::SetPreventIdleLock(read_bool(&mut reader)?)),
        18 => Ok(ParentCommand::SetCursorCapture(read_bool(&mut reader)?)),
        20 => Ok(ParentCommand::SetDisplayBorder(read_bool(&mut reader)?)),
        COMMAND_REQUEST_KEYFRAME => Ok(ParentCommand::RequestKeyframe),
        COMMAND_SET_BITRATE => Ok(ParentCommand::SetBitrate(read_u32(&mut reader)?)),
        COMMAND_START_INPUT => Ok(ParentCommand::StartInput {
            display_id: DisplayId(read_u32(&mut reader)?),
            viewer_name: read_sized_string(&mut reader, MAX_DISPLAY_NAME_BYTES, "viewer name")?,
        }),
        COMMAND_INPUT => match read_session_message(&mut reader, "desktop input")? {
            SessionMessage::Input(input) => Ok(ParentCommand::Input(input)),
            _ => Err(invalid_data("desktop input contained a non-input message")),
        },
        COMMAND_ANNOTATE => match read_session_message(&mut reader, "annotation")? {
            SessionMessage::Annotate(annotation) => Ok(ParentCommand::Annotate(annotation)),
            _ => Err(invalid_data("annotation contained another message")),
        },
        COMMAND_BLACKOUT => {
            let enabled = read_u8(&mut reader)?;
            if enabled > 1 {
                return Err(invalid_data("invalid blackout flag"));
            }
            Ok(ParentCommand::Blackout {
                enabled: enabled == 1,
                text: read_sized_string(&mut reader, MAX_CONTROL_BYTES, "blackout message")?,
            })
        }
        COMMAND_BLOCK_INPUT => match read_u8(&mut reader)? {
            0 => Ok(ParentCommand::BlockInput(false)),
            1 => Ok(ParentCommand::BlockInput(true)),
            _ => Err(invalid_data("invalid input block flag")),
        },
        COMMAND_RELEASE_INPUT => Ok(ParentCommand::ReleaseInput),
        COMMAND_CLIPBOARD => {
            read_clipboard(&mut reader, "desktop clipboard").map(ParentCommand::Clipboard)
        }
        13 => Ok(ParentCommand::StartFiles),
        12 => Ok(ParentCommand::Files(read_file_message(&mut reader)?)),
        COMMAND_STOP_CHAT => Ok(ParentCommand::StopChat),
        COMMAND_START_CHAT => Ok(ParentCommand::StartChat),
        COMMAND_CHAT => read_chat_text(&mut reader).map(ParentCommand::Chat),
        COMMAND_STOP => Ok(ParentCommand::Stop),
        opcode => Err(invalid_data(format!(
            "unknown desktop-helper command opcode {opcode}"
        ))),
    }
}

fn read_start_command(reader: &mut impl Read) -> io::Result<ParentCommand> {
    let viewer_name = read_sized_string(reader, MAX_DISPLAY_NAME_BYTES, "viewer name")?;
    let display_id = read_u32(reader)?;
    Ok(ParentCommand::Start {
        viewer_name,
        display_id: (display_id != NO_DISPLAY).then_some(DisplayId(display_id)),
        frames_per_second: read_u32(reader)?,
        bitrate_bits_per_second: read_u32(reader)?,
        codec: read_codec(reader)?,
        pixel_format: read_pixel_format(reader)?,
        capture_cursor: read_bool(reader)?,
        grayscale: read_bool(reader)?,
        headless: read_headless_target(reader)?,
    })
}

fn write_headless_target(
    mut writer: impl Write,
    target: Option<&HeadlessTarget>,
) -> io::Result<()> {
    let Some(target) = target else {
        return writer.write_all(&[0]);
    };
    writer.write_all(&[1])?;
    write_u32(&mut writer, target.adapter_low)?;
    writer.write_all(&target.adapter_high.to_le_bytes())?;
    write_u32(&mut writer, target.target_id)?;
    write_u32(&mut writer, target.resolution.width)?;
    write_u32(&mut writer, target.resolution.height)
}

fn read_headless_target(mut reader: impl Read) -> io::Result<Option<HeadlessTarget>> {
    if !read_bool(&mut reader)? {
        return Ok(None);
    }
    let adapter_low = read_u32(&mut reader)?;
    let mut adapter_high = [0; 4];
    reader.read_exact(&mut adapter_high)?;
    Ok(Some(HeadlessTarget {
        adapter_low,
        adapter_high: i32::from_le_bytes(adapter_high),
        target_id: read_u32(&mut reader)?,
        resolution: meshrmm_protocol::HeadlessResolution::new(
            read_u32(&mut reader)?,
            read_u32(&mut reader)?,
        ),
    }))
}

pub(super) fn write_event(mut writer: impl Write, event: &ChildEvent) -> io::Result<()> {
    match event {
        ChildEvent::CredentialPrompt(ready) => writer.write_all(&[13, u8::from(*ready)]),
        ChildEvent::Credentials(result) => {
            let bytes = serde_json::to_vec(result).map_err(io::Error::other)?;
            checked_len(bytes.len(), 32768, "credential result")?;
            writer.write_all(&[12])?;
            write_sized(&mut writer, &bytes)
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
            write_sized(&mut writer, reason.as_bytes())
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
            let bytes = encode_message(SessionMessage::CursorShape { shape: *shape })?;
            checked_len(bytes.len(), MAX_CONTROL_BYTES, "cursor shape")?;
            writer.write_all(&[EVENT_CURSOR, u8::from(*viewer_controls_input)])?;
            write_u32(&mut writer, pointer_display.map_or(u32::MAX, |id| id.0))?;
            write_sized(&mut writer, &bytes)
        }
        ChildEvent::Clipboard(content) => {
            write_clipboard(&mut writer, EVENT_CLIPBOARD, content, "clipboard content")
        }
        ChildEvent::Chat(text) => write_chat_text(&mut writer, EVENT_CHAT, text),
        ChildEvent::ApprovalDecision(decision) => {
            writer.write_all(&[EVENT_APPROVAL_DECISION, decision.to_byte()])
        }
        ChildEvent::Error(message) => {
            let message = message.as_bytes();
            checked_len(message.len(), MAX_ERROR_BYTES, "desktop-helper error")?;
            writer.write_all(&[EVENT_ERROR])?;
            write_sized(&mut writer, message)
        }
        ChildEvent::NoDisplays => writer.write_all(&[EVENT_NO_DISPLAYS]),
        ChildEvent::Stopped => writer.write_all(&[EVENT_STOPPED]),
    }
}

pub(super) fn read_event(mut reader: impl Read) -> io::Result<ChildEvent> {
    match read_u8(&mut reader)? {
        12 => {
            let bytes = read_sized(&mut reader, 32768, "credential result")?;
            Ok(ChildEvent::Credentials(
                serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            ))
        }
        13 => match read_u8(&mut reader)? {
            0 => Ok(ChildEvent::CredentialPrompt(false)),
            1 => Ok(ChildEvent::CredentialPrompt(true)),
            _ => Err(io::Error::other("invalid credential prompt state")),
        },
        11 => Ok(ChildEvent::MaintenanceError(read_sized_string(
            &mut reader,
            MAX_ERROR_BYTES,
            "maintenance error",
        )?)),
        10 => {
            let a = read_u8(&mut reader)?;
            let b = read_u8(&mut reader)?;
            if a > 1 || b > 1 {
                return Err(invalid_data("invalid maintenance state"));
            }
            Ok(ChildEvent::MaintenanceState {
                agent_input_blocked: a == 1,
                blacked_out: b == 1,
            })
        }
        EVENT_STARTED => read_started_event(&mut reader),
        EVENT_INPUT_STARTED => Ok(ChildEvent::InputStarted),
        EVENT_FRAME => read_frame_event(&mut reader),
        EVENT_CURSOR => {
            let viewer_controls_input = read_bool(&mut reader)?;
            let pointer_id = read_u32(&mut reader)?;
            let pointer_display = (pointer_id != u32::MAX).then_some(DisplayId(pointer_id));
            match read_session_message(&mut reader, "cursor shape")? {
                SessionMessage::CursorShape { shape } => Ok(ChildEvent::Cursor(
                    shape,
                    viewer_controls_input,
                    pointer_display,
                )),
                _ => Err(invalid_data("cursor event contained an unexpected message")),
            }
        }
        EVENT_CLIPBOARD => {
            read_clipboard(&mut reader, "clipboard content").map(ChildEvent::Clipboard)
        }
        9 => Ok(ChildEvent::Files(read_file_message(&mut reader)?)),
        EVENT_CHAT => read_chat_text(&mut reader).map(ChildEvent::Chat),
        EVENT_ERROR => read_sized_string(&mut reader, MAX_ERROR_BYTES, "desktop-helper error")
            .map(ChildEvent::Error),
        EVENT_STOPPED => Ok(ChildEvent::Stopped),
        EVENT_NO_DISPLAYS => Ok(ChildEvent::NoDisplays),
        EVENT_APPROVAL_DECISION => Decision::from_byte(read_u8(&mut reader)?)
            .map(ChildEvent::ApprovalDecision)
            .ok_or_else(|| invalid_data("invalid approval decision")),
        opcode => Err(invalid_data(format!(
            "unknown desktop-helper event opcode {opcode}"
        ))),
    }
}

fn read_started_event(reader: &mut impl Read) -> io::Result<ChildEvent> {
    let format = ActiveFormat {
        width: read_u32(reader)?,
        height: read_u32(reader)?,
        frames_per_second: read_u32(reader)?,
        bitrate_bits_per_second: read_u32(reader)?,
        codec: read_codec(reader)?,
        pixel_format: read_pixel_format(reader)?,
    };
    let active_display_id = DisplayId(read_u32(reader)?);
    let count = bounded_len(read_u32(reader)?, MAX_DISPLAYS, "display list")?;
    let mut displays = Vec::with_capacity(count);
    for _ in 0..count {
        displays.push(read_display(reader)?);
    }
    let active_display = displays
        .iter()
        .find(|display| display.id == active_display_id)
        .cloned()
        .ok_or_else(|| invalid_data("desktop helper selected an unknown display"))?;
    Ok(ChildEvent::Started(StartedDesktop {
        format,
        displays,
        active_display,
    }))
}

fn read_frame_event(reader: &mut impl Read) -> io::Result<ChildEvent> {
    let capture_timestamp_us = read_u64(reader)?;
    let encode_complete_timestamp_us = read_u64(reader)?;
    let keyframe = match read_u8(reader)? {
        0 => false,
        1 => true,
        value => return Err(invalid_data(format!("invalid keyframe flag {value}"))),
    };
    let codec_config_len = bounded_len(
        read_u32(reader)?,
        MAX_CODEC_CONFIG_BYTES,
        "codec configuration",
    )?;
    let frame_len = bounded_len(read_u32(reader)?, MAX_FRAME_BYTES, "encoded frame")?;
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

fn write_chat_text(writer: &mut impl Write, opcode: u8, text: &str) -> io::Result<()> {
    if !meshrmm_protocol::valid_chat_text(text) {
        return Err(invalid_data("invalid chat text"));
    }
    writer.write_all(&[opcode])?;
    write_sized(writer, text.as_bytes())
}

fn read_chat_text(reader: &mut impl Read) -> io::Result<String> {
    let text = read_sized_string(reader, meshrmm_protocol::MAX_CHAT_TEXT_BYTES, "chat text")?;
    if !meshrmm_protocol::valid_chat_text(&text) {
        return Err(invalid_data("invalid chat text"));
    }
    Ok(text)
}

fn write_clipboard(
    writer: &mut impl Write,
    opcode: u8,
    content: &ClipboardContent,
    label: &str,
) -> io::Result<()> {
    let bytes = content.encode().map_err(invalid_data)?;
    checked_len(bytes.len(), MAX_CLIPBOARD_WIRE_BYTES, label)?;
    writer.write_all(&[opcode])?;
    write_sized(writer, &bytes)
}

fn read_clipboard(reader: &mut impl Read, label: &str) -> io::Result<ClipboardContent> {
    let bytes = read_sized(reader, MAX_CLIPBOARD_WIRE_BYTES, label)?;
    ClipboardContent::decode(&bytes).map_err(invalid_data)
}

fn encode_message(message: SessionMessage) -> io::Result<Vec<u8>> {
    message.encode().map_err(invalid_data)
}

/// Reads a length-prefixed control message, which the caller checks is the one it expects.
fn read_session_message(reader: &mut impl Read, label: &str) -> io::Result<SessionMessage> {
    let bytes = read_sized(reader, MAX_CONTROL_BYTES, label)?;
    SessionMessage::decode(&bytes).map_err(invalid_data)
}

fn write_sized(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    write_u32(writer, bytes.len() as u32)?;
    writer.write_all(bytes)
}

fn read_sized(reader: &mut impl Read, maximum: usize, label: &str) -> io::Result<Vec<u8>> {
    let length = bounded_len(read_u32(reader)?, maximum, label)?;
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_sized_string(reader: &mut impl Read, maximum: usize, label: &str) -> io::Result<String> {
    String::from_utf8(read_sized(reader, maximum, label)?).map_err(invalid_data)
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// The thumbnail helper's reply: a JPEG, or why there is none.
pub(super) fn write_thumbnail(
    writer: &mut impl Write,
    thumbnail: &Result<Vec<u8>, String>,
) -> io::Result<()> {
    let (status, bytes, maximum) = match thumbnail {
        Ok(jpeg) => (0, jpeg.as_slice(), crate::remote::thumbnail::MAX_BYTES),
        Err(message) => (1, message.as_bytes(), MAX_ERROR_BYTES),
    };
    checked_len(bytes.len(), maximum, "screen thumbnail")?;
    writer.write_all(&[status])?;
    write_u32(writer, bytes.len() as u32)?;
    writer.write_all(bytes)
}

pub(super) fn read_thumbnail(reader: &mut impl Read) -> io::Result<Result<Vec<u8>, String>> {
    let status = read_u8(reader)?;
    let maximum = match status {
        0 => crate::remote::thumbnail::MAX_BYTES,
        1 => MAX_ERROR_BYTES,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid screen thumbnail status",
            ));
        }
    };
    let length = bounded_len(read_u32(reader)?, maximum, "screen thumbnail")?;
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(if status == 0 {
        Ok(bytes)
    } else {
        Err(String::from_utf8_lossy(&bytes).into_owned())
    })
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
