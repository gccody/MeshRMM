use super::*;
use meshrmm_protocol::PointerButton;

#[test]
fn rdp_monitor_ids_are_stable_and_do_not_alias_console_or_other_users() {
    assert_ne!(rdp_display_id(3, DisplayId(1)), DisplayId(1));
    assert_ne!(
        rdp_display_id(3, DisplayId(1)),
        rdp_display_id(4, DisplayId(1))
    );
    assert_ne!(
        rdp_display_id(3, DisplayId(1)),
        rdp_display_id(3, DisplayId(2))
    );
    assert_ne!(
        rdp_display_id(3, DisplayId(u32::MAX - 1)),
        meshrmm_protocol::BACKGROUND_DISPLAY_ID
    );
    assert_eq!(
        DesktopTarget::Rdp(3, false).alternate(),
        DesktopTarget::Rdp(3, true)
    );
    assert!(helper_uses_user_token(
        HelperKind::Files,
        DesktopTarget::Rdp(3, true)
    ));
    assert!(helper_uses_user_token(
        HelperKind::Clipboard,
        DesktopTarget::Rdp(3, false)
    ));
    assert!(!helper_uses_user_token(
        HelperKind::Clipboard,
        DesktopTarget::Rdp(3, true)
    ));
}

#[test]
fn rdp_catalog_preserves_all_monitors_and_maps_the_active_monitor() {
    let session = meshrmm_protocol::DesktopSession::Rdp {
        id: 3,
        user: "Alice".into(),
    };
    let console = Display {
        session: meshrmm_protocol::DesktopSession::Console,
        id: DisplayId(1),
        name: "Console".into(),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        primary: true,
    };
    let mut streamer = DesktopCaptureStreamer::new(String::new(), String::new(), PathBuf::new());
    streamer.console_displays = vec![console.clone()];
    streamer.selected_session = Some(3);
    streamer.session_displays = vec![(
        Display {
            session: session.clone(),
            id: rdp_display_id(3, console.id),
            ..console.clone()
        },
        console.id,
    )];
    let monitors: Vec<_> = (1..=3)
        .map(|id| Display {
            id: DisplayId(id),
            x: (id as i32 - 1) * 1920,
            ..console.clone()
        })
        .collect();
    let started = streamer.with_background_display(StartedDesktop {
        active_display: monitors[1].clone(),
        displays: monitors,
        format: ActiveFormat {
            width: 1920,
            height: 1080,
            frames_per_second: 30,
            bitrate_bits_per_second: 8_000_000,
            codec: VideoCodec::H264,
            pixel_format: VideoPixelFormat::Yuv420,
        },
    });
    assert_eq!(started.active_display.id, rdp_display_id(3, DisplayId(2)));
    assert_eq!(started.active_display.session, session);
    assert_eq!(
        started
            .active_display
            .session_displays(&started.displays)
            .len(),
        3
    );
    assert_eq!(started.displays.len(), 5);
    let input = streamer.input_controller();
    assert!(!input.is_console_session());
    *streamer.cursor.lock().unwrap() = (CursorShape::Default, false, Some(DisplayId(3)));
    assert_eq!(
        input.agent_pointer_display(),
        Some(rdp_display_id(3, DisplayId(3)))
    );
    streamer.selected_session = None;
    let console_started = streamer.with_background_display(StartedDesktop {
        active_display: console.clone(),
        displays: vec![console],
        format: started.format,
    });
    assert_eq!(
        console_started.active_display.session,
        meshrmm_protocol::DesktopSession::Console
    );
    assert!(streamer.display_routes.lock().unwrap().is_empty());
    assert!(input.is_console_session());
}

#[test]
fn background_start_preserves_all_console_displays_when_switching() {
    let console: Vec<_> = (1..=3)
        .map(|id| Display {
            session: meshrmm_protocol::DesktopSession::Console,
            id: DisplayId(id),
            name: format!("Display {id}"),
            x: (id as i32 - 1) * 1920,
            y: 0,
            width: 1920,
            height: 1080,
            primary: id == 1,
        })
        .collect();
    let mut streamer = DesktopCaptureStreamer::new(String::new(), String::new(), PathBuf::new());
    streamer.console_displays = console.clone();
    let format = ActiveFormat {
        width: 1920,
        height: 1080,
        frames_per_second: 20,
        bitrate_bits_per_second: 12_000_000,
        codec: VideoCodec::H264,
        pixel_format: VideoPixelFormat::Yuv420,
    };
    for background in [true, false, true] {
        streamer
            .background_active
            .store(background, Ordering::Release);
        let active = if background {
            background_display()
        } else {
            console[2].clone()
        };
        let started = streamer.with_background_display(StartedDesktop {
            format,
            displays: if background {
                vec![active.clone()]
            } else {
                console.clone()
            },
            active_display: active.clone(),
        });
        assert_eq!(started.displays.len(), 4);
        assert_eq!(started.active_display.id, active.id);
        for (actual, expected) in started.displays.iter().zip(&console) {
            assert_eq!(actual.id, expected.id);
            assert_eq!(actual.x, expected.x);
        }
        assert_eq!(started.displays[3].id, background_display().id);
    }
    let mut bytes = Vec::new();
    write_command(&mut bytes, &ParentCommand::EnumerateDisplays).unwrap();
    assert!(matches!(
        read_command(bytes.as_slice()).unwrap(),
        ParentCommand::EnumerateDisplays
    ));
}

#[test]
fn capture_reader_accepts_reconfiguration_and_discards_frames_while_unrouted() {
    let display = Display {
        session: meshrmm_protocol::DesktopSession::Console,
        id: DisplayId(1),
        name: "Display".into(),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        primary: true,
    };
    let mut bytes = Vec::new();
    for id in [1, 2] {
        let selected = Display {
            session: meshrmm_protocol::DesktopSession::Console,
            id: DisplayId(id),
            ..display.clone()
        };
        write_event(
            &mut bytes,
            &ChildEvent::Started(StartedDesktop {
                format: ActiveFormat {
                    width: 1920,
                    height: 1080,
                    frames_per_second: 60,
                    bitrate_bits_per_second: 12_000_000,
                    codec: VideoCodec::H264,
                    pixel_format: VideoPixelFormat::Yuv420,
                },
                displays: vec![selected.clone()],
                active_display: selected,
            }),
        )
        .unwrap();
        write_event(
            &mut bytes,
            &ChildEvent::Frame(EncodedAccessUnit {
                capture_timestamp_us: id as u64,
                encode_complete_timestamp_us: id as u64,
                keyframe: true,
                codec_config: None,
                data: vec![id as u8],
            }),
        )
        .unwrap();
    }
    write_event(&mut bytes, &ChildEvent::Stopped).unwrap();
    for enabled in [false, true] {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&frames);
        let sink: EncodedFrameSink = Arc::new(move |frame| {
            received.lock().unwrap().push(frame.data);
        });
        let (started_tx, started_rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(None));
        dispatch_child_events(
            bytes.as_slice(),
            Arc::new(Mutex::new(enabled.then_some(sink))),
            started_tx,
            Arc::clone(&status),
            Arc::new(Mutex::new((CursorShape::Default, false, None))),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(Instant::now())),
        );
        assert_eq!(
            started_rx.recv().unwrap().unwrap().active_display.id,
            DisplayId(1)
        );
        assert_eq!(
            started_rx.recv().unwrap().unwrap().active_display.id,
            DisplayId(2)
        );
        assert_eq!(*status.lock().unwrap(), Some(Ok(())));
        assert_eq!(
            *frames.lock().unwrap(),
            if enabled {
                vec![vec![1], vec![2]]
            } else {
                vec![]
            }
        );
    }
}

#[test]
fn maintenance_ipc_preserves_flags_and_unicode_and_rejects_bad_flags() {
    for blocked in [false, true] {
        let mut bytes = Vec::new();
        write_command(&mut bytes, &ParentCommand::BlockInput(blocked)).unwrap();
        assert!(
            matches!(read_command(bytes.as_slice()).unwrap(), ParentCommand::BlockInput(value) if value == blocked)
        );
    }
    assert!(read_command([COMMAND_BLOCK_INPUT, 2].as_slice()).is_err());
    assert!(read_command([COMMAND_BLACKOUT, 2].as_slice()).is_err());
    let text = "Maintenance by Zoë 王\nPlease wait";
    let mut bytes = Vec::new();
    write_command(
        &mut bytes,
        &ParentCommand::Blackout {
            enabled: true,
            text: text.into(),
        },
    )
    .unwrap();
    assert!(
        matches!(read_command(bytes.as_slice()).unwrap(), ParentCommand::Blackout { enabled: true, text: value } if value == text)
    );
    let mut bytes = Vec::new();
    write_event(
        &mut bytes,
        &ChildEvent::MaintenanceState {
            agent_input_blocked: true,
            blacked_out: false,
        },
    )
    .unwrap();
    assert!(matches!(
        read_event(bytes.as_slice()).unwrap(),
        ChildEvent::MaintenanceState {
            agent_input_blocked: true,
            blacked_out: false
        }
    ));
    let mut bytes = Vec::new();
    write_event(
        &mut bytes,
        &ChildEvent::MaintenanceError("access denied".into()),
    )
    .unwrap();
    assert!(
        matches!(read_event(bytes.as_slice()).unwrap(), ChildEvent::MaintenanceError(reason) if reason == "access denied")
    );
}

#[test]
fn wallpaper_commands_preserve_pipe_alignment_and_reject_invalid_flags() {
    for hidden in [false, true] {
        let mut bytes = Vec::new();
        write_command(&mut bytes, &ParentCommand::SetWallpaperHidden(hidden)).unwrap();
        write_command(&mut bytes, &ParentCommand::Stop).unwrap();
        let mut reader = bytes.as_slice();
        assert!(
            matches!(read_command(&mut reader).unwrap(), ParentCommand::SetWallpaperHidden(value) if value == hidden)
        );
        assert!(matches!(
            read_command(&mut reader).unwrap(),
            ParentCommand::Stop
        ));
        assert!(reader.is_empty());
    }
    assert!(read_command([19, 2].as_slice()).is_err());
}

#[test]
fn cursor_ownership_and_visibility_round_trip_without_desynchronizing_ipc() {
    for viewer in [false, true] {
        let mut bytes = Vec::new();
        write_event(
            &mut bytes,
            &ChildEvent::Cursor(CursorShape::Text, viewer, Some(DisplayId(2))),
        )
        .unwrap();
        write_event(&mut bytes, &ChildEvent::Stopped).unwrap();
        let mut reader = bytes.as_slice();
        assert!(
            matches!(read_event(&mut reader).unwrap(), ChildEvent::Cursor(CursorShape::Text, owner, Some(DisplayId(2))) if owner == viewer)
        );
        assert!(matches!(
            read_event(&mut reader).unwrap(),
            ChildEvent::Stopped
        ));
        assert!(reader.is_empty());

        let mut bytes = Vec::new();
        write_command(&mut bytes, &ParentCommand::SetCursorCapture(viewer)).unwrap();
        write_command(&mut bytes, &ParentCommand::RequestKeyframe).unwrap();
        let mut reader = bytes.as_slice();
        assert!(
            matches!(read_command(&mut reader).unwrap(), ParentCommand::SetCursorCapture(enabled) if enabled == viewer)
        );
        assert!(matches!(
            read_command(&mut reader).unwrap(),
            ParentCommand::RequestKeyframe
        ));
        assert!(reader.is_empty());
    }
    assert!(read_command([18, 2].as_slice()).is_err());
    assert!(read_event([EVENT_CURSOR, 2].as_slice()).is_err());
}

#[test]
fn capture_flags_survive_helper_start_and_leave_next_command_aligned() {
    for (cursor, monochrome) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut bytes = Vec::new();
        write_command(
            &mut bytes,
            &ParentCommand::Start {
                viewer_name: "Viewer".into(),
                display_id: Some(DisplayId(1)),
                frames_per_second: 24,
                bitrate_bits_per_second: 1_000_000,
                codec: VideoCodec::H264,
                pixel_format: VideoPixelFormat::Yuv420,
                capture_cursor: cursor,
                grayscale: monochrome,
            },
        )
        .unwrap();
        write_command(&mut bytes, &ParentCommand::RequestKeyframe).unwrap();
        let mut reader = bytes.as_slice();
        assert!(
            matches!(read_command(&mut reader).unwrap(), ParentCommand::Start {
                    capture_cursor, grayscale, frames_per_second: 24, bitrate_bits_per_second: 1_000_000, ..
                } if capture_cursor == cursor && grayscale == monochrome)
        );
        assert!(matches!(
            read_command(&mut reader).unwrap(),
            ParentCommand::RequestKeyframe
        ));
        assert!(reader.is_empty());
    }
}

#[test]
fn command_protocol_round_trips_desktop_input() {
    let commands = [
        ParentCommand::Start {
            viewer_name: "Zoë 王".into(),
            display_id: Some(DisplayId(3)),
            frames_per_second: 60,
            bitrate_bits_per_second: 12_000_000,
            codec: VideoCodec::H265,
            pixel_format: VideoPixelFormat::Yuv444,
            capture_cursor: true,
            grayscale: false,
        },
        ParentCommand::StartClipboard,
        ParentCommand::StartChatHelper {
            viewer_name: "Zoë 王".into(),
        },
        ParentCommand::RequestKeyframe,
        ParentCommand::SetBitrate(4_000_000),
        ParentCommand::StartInput {
            viewer_name: "Zoë 王".into(),
            display_id: DisplayId(3),
        },
        ParentCommand::Input(RemoteInput::PointerButton {
            display_id: DisplayId(3),
            button: PointerButton::Left,
            pressed: true,
        }),
        ParentCommand::ReleaseInput,
        ParentCommand::BlockInput(true),
        ParentCommand::BlockInput(false),
        ParentCommand::Clipboard("winget install Example.Package\n".into()),
        ParentCommand::Stop,
    ];
    for command in commands {
        let mut bytes = Vec::new();
        write_command(&mut bytes, &command).unwrap();
        let decoded = read_command(bytes.as_slice()).unwrap();
        assert_eq!(command_name(&decoded), command_name(&command));
        if let ParentCommand::Start { viewer_name, .. }
        | ParentCommand::StartInput { viewer_name, .. } = decoded
        {
            assert_eq!(viewer_name, "Zoë 王");
        }
    }
}

#[test]
fn started_event_round_trips_display_metadata() {
    let display = Display {
        session: meshrmm_protocol::DesktopSession::Console,
        id: DisplayId(2),
        name: "Secure display".into(),
        x: -1920,
        y: 0,
        width: 1920,
        height: 1080,
        primary: true,
    };
    let event = ChildEvent::Started(StartedDesktop {
        format: ActiveFormat {
            width: 1920,
            height: 1080,
            frames_per_second: 60,
            bitrate_bits_per_second: 12_000_000,
            codec: VideoCodec::H265,
            pixel_format: VideoPixelFormat::Yuv444,
        },
        displays: vec![display.clone()],
        active_display: display,
    });
    let mut bytes = Vec::new();
    write_event(&mut bytes, &event).unwrap();
    let ChildEvent::Started(decoded) = read_event(bytes.as_slice()).unwrap() else {
        panic!("expected started event");
    };
    assert_eq!(decoded.active_display.id, DisplayId(2));
    assert_eq!(decoded.displays[0].x, -1920);
    assert_eq!(decoded.format.codec, VideoCodec::H265);
    assert_eq!(decoded.format.pixel_format, VideoPixelFormat::Yuv444);
}

#[test]
fn frame_event_round_trips() {
    let event = ChildEvent::Frame(EncodedAccessUnit {
        capture_timestamp_us: 11,
        encode_complete_timestamp_us: 22,
        keyframe: true,
        codec_config: Some(vec![1, 2, 3]),
        data: vec![4, 5, 6, 7],
    });
    let mut bytes = Vec::new();
    write_event(&mut bytes, &event).unwrap();
    let ChildEvent::Frame(decoded) = read_event(bytes.as_slice()).unwrap() else {
        panic!("expected frame event");
    };
    assert_eq!(decoded.capture_timestamp_us, 11);
    assert_eq!(decoded.codec_config, Some(vec![1, 2, 3]));
    assert_eq!(decoded.data, vec![4, 5, 6, 7]);
}

#[test]
fn input_started_event_round_trips() {
    let mut bytes = Vec::new();
    write_event(&mut bytes, &ChildEvent::InputStarted).unwrap();
    assert!(matches!(
        read_event(bytes.as_slice()).unwrap(),
        ChildEvent::InputStarted
    ));
}

#[test]
fn clipboard_commands_and_events_round_trip_all_formats() {
    for content in [
        ClipboardContent::from("winget install Example.Package\n"),
        ClipboardContent::Html {
            html: "<b>Zoë 王</b>".repeat(10000),
            text: "Zoë 王".into(),
        },
        ClipboardContent::Image {
            width: 512,
            height: 512,
            rgba: vec![255; 512 * 512 * 4],
        },
    ] {
        let mut bytes = Vec::new();
        write_event(&mut bytes, &ChildEvent::Clipboard(content.clone())).unwrap();
        let ChildEvent::Clipboard(decoded) = read_event(bytes.as_slice()).unwrap() else {
            panic!("expected clipboard event");
        };
        assert_eq!(decoded, content);
        let mut bytes = Vec::new();
        write_command(&mut bytes, &ParentCommand::Clipboard(content.clone())).unwrap();
        let ParentCommand::Clipboard(decoded) = read_command(bytes.as_slice()).unwrap() else {
            panic!("expected clipboard command");
        };
        assert_eq!(decoded, content);
    }
}

#[test]
fn desktop_targets_alternate() {
    assert_eq!(DesktopTarget::Default.alternate(), DesktopTarget::Winlogon);
    assert_eq!(DesktopTarget::Winlogon.alternate(), DesktopTarget::Default);
}

fn command_name(command: &ParentCommand) -> u8 {
    match command {
        ParentCommand::EnumerateDisplays => COMMAND_ENUMERATE_DISPLAYS,
        ParentCommand::Start { .. } => COMMAND_START,
        ParentCommand::SetWallpaperHidden(_) => 19,
        ParentCommand::SetPreventIdleLock(_) => 21,
        ParentCommand::SetCursorCapture(_) => 18,
        ParentCommand::SetDisplayBorder(_) => 20,
        ParentCommand::RequestKeyframe => COMMAND_REQUEST_KEYFRAME,
        ParentCommand::SetBitrate(_) => COMMAND_SET_BITRATE,
        ParentCommand::StartInput { .. } => COMMAND_START_INPUT,
        ParentCommand::Input(_) => COMMAND_INPUT,
        ParentCommand::ReleaseInput => COMMAND_RELEASE_INPUT,
        ParentCommand::BlockInput(_) => COMMAND_BLOCK_INPUT,
        ParentCommand::Blackout { .. } => COMMAND_BLACKOUT,
        ParentCommand::Clipboard(_) => COMMAND_CLIPBOARD,
        ParentCommand::StartFiles => 13,
        ParentCommand::StartClipboard => 16,
        ParentCommand::StartChatHelper { .. } => 17,
        ParentCommand::PromptCredentials => 23,
        ParentCommand::AutofillCredentials(_) => 24,
        ParentCommand::Files(_) => 12,
        ParentCommand::Chat(_) => COMMAND_CHAT,
        ParentCommand::StartChat => COMMAND_START_CHAT,
        ParentCommand::StopChat => COMMAND_STOP_CHAT,
        ParentCommand::Stop => COMMAND_STOP,
    }
}

#[cfg(test)]
mod chat_tests {
    use super::*;
    #[test]
    fn chat_commands_and_events_round_trip() {
        let text = "Hello 👋\nReply from the other computer";
        let mut bytes = Vec::new();
        write_command(&mut bytes, &ParentCommand::StartChat).unwrap();
        assert!(matches!(
            read_command(bytes.as_slice()).unwrap(),
            ParentCommand::StartChat
        ));
        bytes.clear();
        write_command(&mut bytes, &ParentCommand::Chat(text.into())).unwrap();
        assert!(
            matches!(read_command(bytes.as_slice()).unwrap(), ParentCommand::Chat(value) if value == text)
        );
        bytes.clear();
        write_event(&mut bytes, &ChildEvent::Chat(text.into())).unwrap();
        assert!(
            matches!(read_event(bytes.as_slice()).unwrap(), ChildEvent::Chat(value) if value == text)
        );
    }
    #[test]
    fn oversized_chat_is_rejected_before_reading_payload() {
        let mut command = vec![COMMAND_CHAT];
        command
            .extend_from_slice(&(meshrmm_protocol::MAX_CHAT_TEXT_BYTES as u32 + 1).to_le_bytes());
        assert!(
            matches!(read_command(command.as_slice()), Err(e) if e.kind() == io::ErrorKind::InvalidData)
        );
        command[0] = EVENT_CHAT;
        assert!(
            matches!(read_event(command.as_slice()), Err(e) if e.kind() == io::ErrorKind::InvalidData)
        );
    }
}

#[cfg(test)]
mod isolation_tests {
    use super::*;
    #[test]
    fn desktop_switch_discards_unrouted_input_and_resumes_without_replay() {
        let streamer = DesktopCaptureStreamer::default();
        let controller = streamer.input_controller();
        let event = |pressed| RemoteInput::PointerButton {
            display_id: DisplayId(1),
            button: meshrmm_protocol::PointerButton::Left,
            pressed,
        };
        // A release arriving between helpers must not disconnect the viewer.
        controller.apply(event(false)).unwrap();
        controller.release_all().unwrap();

        let (reader, writer) = create_pipe().unwrap();
        *streamer.input_route.lock().unwrap() =
            Some(Arc::new(CommandWriter::new(writer.into_file()).unwrap()));
        controller.apply(event(true)).unwrap();
        controller.release_all().unwrap();
        let mut reader = reader.into_file();
        assert!(matches!(
            read_command(&mut reader).unwrap(),
            ParentCommand::Input(RemoteInput::PointerButton { pressed: true, .. })
        ));
        assert!(matches!(
            read_command(&mut reader).unwrap(),
            ParentCommand::ReleaseInput
        ));
    }

    #[test]
    fn interactive_clipboard_uses_user_token_but_secure_input_does_not() {
        assert!(helper_uses_user_token(
            HelperKind::Clipboard,
            DesktopTarget::Default
        ));
        assert!(!helper_uses_user_token(
            HelperKind::Clipboard,
            DesktopTarget::Winlogon
        ));
        assert!(!helper_uses_user_token(
            HelperKind::Input,
            DesktopTarget::Default
        ));
        assert!(!helper_uses_user_token(
            HelperKind::Chat,
            DesktopTarget::Default
        ));
        assert!(helper_uses_user_token(
            HelperKind::Files,
            DesktopTarget::Default
        ));
    }

    #[test]
    fn helper_pipes_start_non_inheritable() {
        let (read, write) = create_pipe().unwrap();
        for handle in [&read, &write] {
            let mut flags = 0;
            unsafe { windows::Win32::Foundation::GetHandleInformation(handle.0, &mut flags) }
                .unwrap();
            assert_eq!(flags & HANDLE_FLAG_INHERIT.0, 0);
        }
        let handles = [read.0, write.0];
        // The attribute list accepts the pipe ends once a launch marks them inheritable.
        for handle in handles {
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
                .unwrap();
        }
        HandleListAttribute::new(&handles).unwrap();
    }

    #[test]
    fn stalled_clipboard_pipe_does_not_block_input_pipe() {
        let (blocked_read, blocked_write) = create_pipe().unwrap();
        let clipboard = CommandWriter::new(blocked_write.into_file()).unwrap();
        // Far larger than the anonymous pipe buffer; its writer must wait until
        // the reader drains/closes it. The caller only enqueues these bytes.
        clipboard.send(vec![0; 1024 * 1024]).unwrap();
        let (input_read, input_write) = create_pipe().unwrap();
        let input = Arc::new(CommandWriter::new(input_write.into_file()).unwrap());
        let (received, result) = mpsc::channel();
        let reader = thread::spawn(move || {
            let command = read_command(input_read.into_file()).unwrap();
            received
                .send(matches!(command, ParentCommand::ReleaseInput))
                .unwrap();
        });
        send_command(&input, &ParentCommand::ReleaseInput).unwrap();
        assert!(result.recv_timeout(Duration::from_secs(2)).unwrap());
        drop(blocked_read); // Unblock and close the clipboard writer too.
        reader.join().unwrap();
    }

    #[test]
    fn pipe_byte_budget_rejects_oversized_work_without_waiting() {
        let writer = CommandWriter::new(io::sink()).unwrap();
        assert!(
            writer
                .send(vec![
                    0;
                    2 * MAX_CLIPBOARD_WIRE_BYTES + MAX_CONTROL_BYTES + 1
                ])
                .is_err()
        );
        writer.send(vec![COMMAND_RELEASE_INPUT]).unwrap();
    }
}

#[cfg(test)]
mod service_command_tests {
    use super::*;

    #[tokio::test]
    async fn command_bridge_preserves_order_and_closes_after_stop() {
        let (sender, commands) = mpsc::sync_channel(64);
        let mut receiver = async_helper_commands(commands).unwrap();
        sender.send(Ok(ParentCommand::StartChat)).unwrap();
        sender
            .send(Ok(ParentCommand::Chat("hello".into())))
            .unwrap();
        sender.send(Ok(ParentCommand::Stop)).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            assert!(matches!(receiver.recv().await, Some(Ok(ParentCommand::StartChat))));
            assert!(matches!(receiver.recv().await, Some(Ok(ParentCommand::Chat(text))) if text == "hello"));
            assert!(matches!(receiver.recv().await, Some(Ok(ParentCommand::Stop))));
            assert!(receiver.recv().await.is_none());
        }).await.unwrap();
    }

    #[tokio::test]
    async fn command_bridge_wakes_on_pipe_disconnect() {
        let (sender, commands) = mpsc::sync_channel(64);
        let mut receiver = async_helper_commands(commands).unwrap();
        drop(sender);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod service_event_tests {
    use super::*;

    struct Dispatched {
        startup: Result<(), String>,
        status: Option<Result<(), String>>,
        cursor: HelperCursor,
        clipboard: HelperClipboard,
        files: HelperFiles,
        chat: HelperChat,
        maintenance: HelperMaintenance,
        credentials: HelperCredentials,
    }

    fn dispatch(kind: HelperKind, events: &[ChildEvent]) -> Dispatched {
        let (read, write) = create_pipe().unwrap();
        let (started, startup) = mpsc::sync_channel(1);
        let status: HelperStatus = Arc::new(Mutex::new(None));
        let cursor: HelperCursor = Arc::new(Mutex::new((CursorShape::Default, false, None)));
        let clipboard = Arc::new(ClipboardEvents::default());
        let files = Arc::new(FileEvents::default());
        let chat = Arc::new(ChatEvents::default());
        let maintenance: HelperMaintenance = Arc::new(Mutex::new(None));
        let credentials = Arc::new(Mutex::new(Credentials::default()));
        let reader = {
            let (status, cursor, clipboard, files, chat, maintenance, credentials) = (
                status.clone(),
                cursor.clone(),
                clipboard.clone(),
                files.clone(),
                chat.clone(),
                maintenance.clone(),
                credentials.clone(),
            );
            thread::spawn(move || {
                dispatch_input_events(
                    read.into_file(),
                    started,
                    status,
                    cursor,
                    clipboard,
                    files,
                    chat,
                    maintenance,
                    credentials,
                    kind,
                )
            })
        };
        let mut writer = write.into_file();
        for event in events {
            // The reader may already have stopped after a rejected event.
            if write_event(&mut writer, event).is_err() {
                break;
            }
        }
        drop(writer);
        reader.join().unwrap();
        let status = status.lock().unwrap().take();
        Dispatched {
            startup: startup.recv().unwrap(),
            status,
            cursor,
            clipboard,
            files,
            chat,
            maintenance,
            credentials,
        }
    }

    #[tokio::test]
    async fn helper_pipe_notifies_services_and_coalesces_clipboard_changes() {
        let clipboard = dispatch(
            HelperKind::Clipboard,
            &[
                ChildEvent::InputStarted,
                ChildEvent::Clipboard(ClipboardContent::Text("first".into())),
                ChildEvent::Clipboard(ClipboardContent::Text("latest".into())),
                ChildEvent::Stopped,
            ],
        );
        let chat = dispatch(
            HelperKind::Chat,
            &[
                ChildEvent::InputStarted,
                ChildEvent::Chat("hello".into()),
                ChildEvent::Stopped,
            ],
        );
        let files = dispatch(
            HelperKind::Files,
            &[
                ChildEvent::InputStarted,
                ChildEvent::Files(meshrmm_protocol::FileMessage::Available),
                ChildEvent::Stopped,
            ],
        );
        for helper in [&clipboard, &chat, &files] {
            assert_eq!(helper.startup, Ok(()));
            assert_eq!(helper.status, Some(Ok(())));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            clipboard.clipboard.ready.notified().await;
            files.files.ready.notified().await;
            chat.chat.ready.notified().await;
        })
        .await
        .unwrap();
        assert_eq!(
            clipboard.clipboard.latest.lock().unwrap().take(),
            Some(ClipboardContent::Text("latest".into()))
        );
        assert_eq!(
            chat.chat.queue.lock().unwrap().pop_front().as_deref(),
            Some("hello")
        );
        assert!(matches!(
            files.files.queue.lock().unwrap().pop_front(),
            Some(meshrmm_protocol::FileMessage::Available)
        ));
        assert!(clipboard.clipboard.latest.lock().unwrap().is_none());
    }

    #[test]
    fn helpers_are_trusted_only_for_events_their_run_loops_send() {
        use HelperKind::*;
        let events = [
            ChildEvent::InputStarted,
            ChildEvent::Error("failed".into()),
            ChildEvent::Stopped,
            ChildEvent::Cursor(CursorShape::Default, true, None),
            ChildEvent::MaintenanceState {
                agent_input_blocked: false,
                blacked_out: false,
            },
            ChildEvent::CredentialPrompt(true),
            ChildEvent::MaintenanceError("wallpaper".into()),
            ChildEvent::Credentials(CredentialResult {
                encrypted: None,
                message: "done".into(),
            }),
            ChildEvent::Files(meshrmm_protocol::FileMessage::Available),
            ChildEvent::Clipboard(ClipboardContent::Text("copied".into())),
            ChildEvent::Chat("hello".into()),
        ];
        let expected: [&[HelperKind]; 11] = [
            &[Input, Files, Clipboard, Chat],
            &[Input, Files, Clipboard, Chat],
            &[Input, Files, Clipboard, Chat],
            &[Input],
            &[Input],
            &[Input],
            &[Input, Files],
            &[Input, Chat],
            &[Files],
            &[Clipboard],
            &[Chat],
        ];
        for (event, senders) in events.iter().zip(expected) {
            for kind in [Input, Files, Clipboard, Chat] {
                assert_eq!(
                    helper_sends(kind, event),
                    senders.contains(&kind),
                    "{kind:?} helper, {} event",
                    child_event_name(event)
                );
            }
        }
    }

    #[test]
    fn user_token_helpers_cannot_forge_other_helpers_events() {
        let forged = [
            (
                HelperKind::Clipboard,
                ChildEvent::Chat("forged chat".into()),
            ),
            (
                HelperKind::Clipboard,
                ChildEvent::MaintenanceError("forged".into()),
            ),
            (
                HelperKind::Files,
                ChildEvent::Cursor(CursorShape::Text, true, None),
            ),
            (
                HelperKind::Files,
                ChildEvent::MaintenanceState {
                    agent_input_blocked: true,
                    blacked_out: true,
                },
            ),
            (
                HelperKind::Files,
                ChildEvent::Clipboard(ClipboardContent::Text("forged".into())),
            ),
            (HelperKind::Files, ChildEvent::CredentialPrompt(true)),
            (
                HelperKind::Clipboard,
                ChildEvent::Files(meshrmm_protocol::FileMessage::Available),
            ),
            (
                HelperKind::Chat,
                ChildEvent::Clipboard(ClipboardContent::Text("forged".into())),
            ),
            (HelperKind::Input, ChildEvent::Chat("forged chat".into())),
        ];
        for (kind, event) in forged {
            let name = child_event_name(&event);
            let result = dispatch(
                kind,
                &[
                    ChildEvent::InputStarted,
                    event,
                    ChildEvent::Chat("after".into()),
                    ChildEvent::Stopped,
                ],
            );
            assert_eq!(result.startup, Ok(()));
            let Some(Err(message)) = result.status else {
                panic!("{kind:?} helper's {name} event was accepted");
            };
            assert!(message.contains("unexpected"), "{message}");
            assert_eq!(
                *result.cursor.lock().unwrap(),
                (CursorShape::Default, false, None)
            );
            assert!(result.clipboard.latest.lock().unwrap().is_none());
            assert!(result.files.queue.lock().unwrap().is_empty());
            assert!(result.chat.queue.lock().unwrap().is_empty());
            assert!(result.maintenance.lock().unwrap().is_none());
            assert!(!result.credentials.lock().unwrap().state.can_autofill);
        }
    }

    #[test]
    fn stderr_lines_are_bounded_without_losing_the_next_line() {
        let mut input = Vec::new();
        input.extend(std::iter::repeat_n(b'x', 10_000));
        input.extend_from_slice(b"\r\nnext\npartial");
        // A small buffer exercises lines that span several reads.
        let mut reader = BufReader::with_capacity(7, &input[..]);
        let mut line = Vec::new();
        assert_eq!(
            read_bounded_line(&mut reader, &mut line, 16).unwrap(),
            Some(true)
        );
        assert_eq!(line, vec![b'x'; 16]);
        assert_eq!(
            read_bounded_line(&mut reader, &mut line, 16).unwrap(),
            Some(false)
        );
        assert_eq!(line, b"next");
        assert_eq!(
            read_bounded_line(&mut reader, &mut line, 16).unwrap(),
            Some(false)
        );
        assert_eq!(line, b"partial");
        assert_eq!(read_bounded_line(&mut reader, &mut line, 16).unwrap(), None);
    }

    #[test]
    fn stderr_budget_suppresses_floods_and_reports_them_per_window() {
        let start = Instant::now();
        let window = Duration::from_secs(60);
        let mut budget = LineBudget::new(2, window, start);
        assert_eq!(budget.admit(start), (true, 0));
        assert_eq!(budget.admit(start), (true, 0));
        assert_eq!(budget.admit(start), (false, 0));
        assert_eq!(budget.admit(start + Duration::from_secs(59)), (false, 0));
        assert_eq!(budget.admit(start + window), (true, 2));
        assert_eq!(budget.admit(start + window), (true, 0));
        assert_eq!(budget.admit(start + window), (false, 0));
        assert_eq!(budget.take_suppressed(), 1);
        assert_eq!(budget.take_suppressed(), 0);
    }

    #[test]
    fn forged_event_before_start_fails_startup() {
        let result = dispatch(
            HelperKind::Clipboard,
            &[ChildEvent::Files(meshrmm_protocol::FileMessage::Available)],
        );
        assert!(result.startup.is_err());
        assert!(matches!(result.status, Some(Err(_))));
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn credential_ipc_round_trips_and_bounds_payloads() {
        let encrypted = vec![17, 42, 0, 255];
        let mut bytes = Vec::new();
        write_command(
            &mut bytes,
            &ParentCommand::AutofillCredentials(encrypted.clone()),
        )
        .unwrap();
        assert!(
            matches!(read_command(&bytes[..]).unwrap(), ParentCommand::AutofillCredentials(value) if value == encrypted)
        );
        bytes.clear();
        write_event(
            &mut bytes,
            &ChildEvent::Credentials(CredentialResult {
                encrypted: Some(encrypted.clone()),
                message: "Saved".into(),
            }),
        )
        .unwrap();
        assert!(
            matches!(read_event(&bytes[..]).unwrap(), ChildEvent::Credentials(value) if value.encrypted == Some(encrypted))
        );
        assert!(read_command(&[24, 1, 32, 0, 0][..]).is_err());
        assert!(read_event(&[12, 1, 128, 0, 0][..]).is_err());
        assert!(read_event(&[13, 2][..]).is_err());
    }
}
