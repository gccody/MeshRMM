use super::*;
use meshrmm_protocol::PointerButton;

#[cfg(test)]
mod chat_tests;
#[cfg(test)]
mod credential_tests;
mod headless_tests;
#[cfg(test)]
mod isolation_tests;
#[cfg(test)]
mod service_command_tests;
#[cfg(test)]
mod service_event_tests;

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
    let mut streamer =
        DesktopCaptureStreamer::new(String::new(), String::new(), true, None, PathBuf::new());
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
    let mut streamer =
        DesktopCaptureStreamer::new(String::new(), String::new(), true, None, PathBuf::new());
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
                headless: None,
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
            headless: Some(HEADLESS_TARGET),
        },
        ParentCommand::StartClipboard,
        ParentCommand::StartChatHelper {
            viewer_name: "Zoë 王".into(),
            show_banner: false,
        },
        ParentCommand::StartChatHelper {
            viewer_name: "Zoë 王".into(),
            show_banner: true,
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
        ParentCommand::Annotate(meshrmm_protocol::Annotation::Start {
            display_id: DisplayId(3),
            x: 1,
            y: 65_535,
        }),
        ParentCommand::Annotate(meshrmm_protocol::Annotation::Clear),
        ParentCommand::ReleaseInput,
        ParentCommand::BlockInput(true),
        ParentCommand::BlockInput(false),
        ParentCommand::Clipboard("winget install Example.Package\n".into()),
        ParentCommand::ShowConnectionNotification {
            text: "Zoë 王 has connected\nSay hi".into(),
        },
        ParentCommand::PromptConnectionApproval {
            text: "Zoë 王 would like to connect.".into(),
            reason: "Printer queue\nticket 42".into(),
            timeout_seconds: 30,
            lock_idle_seconds: 0,
        },
        ParentCommand::Stop,
    ];
    for command in commands {
        let mut bytes = Vec::new();
        write_command(&mut bytes, &command).unwrap();
        let decoded = read_command(bytes.as_slice()).unwrap();
        assert_eq!(command_name(&decoded), command_name(&command));
        if let ParentCommand::Start { viewer_name, .. }
        | ParentCommand::StartInput { viewer_name, .. }
        | ParentCommand::StartChatHelper { viewer_name, .. } = &decoded
        {
            assert_eq!(viewer_name, "Zoë 王");
        }
        if let (
            ParentCommand::StartChatHelper { show_banner, .. },
            ParentCommand::StartChatHelper {
                show_banner: expected,
                ..
            },
        ) = (&decoded, &command)
        {
            assert_eq!(show_banner, expected);
        }
        if let ParentCommand::Start { headless, .. } = &decoded {
            assert_eq!(*headless, Some(HEADLESS_TARGET));
        }
        if let ParentCommand::ShowConnectionNotification { text } = &decoded {
            assert_eq!(text, "Zoë 王 has connected\nSay hi");
        }
        if let ParentCommand::PromptConnectionApproval {
            text,
            reason,
            timeout_seconds,
            lock_idle_seconds,
        } = &decoded
        {
            assert_eq!(text, "Zoë 王 would like to connect.");
            assert_eq!(reason, "Printer queue\nticket 42");
            assert_eq!((*timeout_seconds, *lock_idle_seconds), (30, 0));
        }
    }
}

#[test]
fn approval_decisions_round_trip_and_invalid_ones_are_rejected() {
    use crate::remote::connection_approval::Decision;
    for decision in [
        Decision::Accepted,
        Decision::Declined,
        Decision::TimedOut,
        Decision::LockedAndIdle,
    ] {
        let mut bytes = Vec::new();
        write_event(&mut bytes, &ChildEvent::ApprovalDecision(decision)).unwrap();
        assert!(matches!(
            read_event(bytes.as_slice()).unwrap(),
            ChildEvent::ApprovalDecision(decoded) if decoded == decision
        ));
    }
    assert!(read_event([EVENT_APPROVAL_DECISION, 9].as_slice()).is_err());
}

#[test]
fn only_helper_start_commands_choose_a_helper() {
    assert_eq!(
        HelperKind::started_by(&ParentCommand::ShowConnectionNotification {
            text: "Hello".into()
        }),
        Some(HelperKind::Notification)
    );
    assert_eq!(
        HelperKind::started_by(&ParentCommand::StartFiles),
        Some(HelperKind::Files)
    );
    assert_eq!(HelperKind::started_by(&ParentCommand::Stop), None);
    assert_eq!(
        HelperKind::started_by(&ParentCommand::EnumerateDisplays),
        None
    );
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
fn helpers_report_a_console_without_displays() {
    let mut bytes = Vec::new();
    write_event(&mut bytes, &ChildEvent::NoDisplays).unwrap();
    assert_eq!(bytes, [EVENT_NO_DISPLAYS]);
    let (started_tx, started_rx) = mpsc::channel();
    let status = Arc::new(Mutex::new(None));
    dispatch_child_events(
        bytes.as_slice(),
        Arc::new(Mutex::new(None)),
        started_tx,
        Arc::clone(&status),
        Arc::new(Mutex::new((CursorShape::Default, false, None))),
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(Instant::now())),
    );
    let Err(failure) = started_rx.recv().unwrap() else {
        panic!("the helper reported a start");
    };
    assert!(anyhow::Error::from(failure).is::<NoDisplays>());
    assert!(matches!(*status.lock().unwrap(), Some(Err(_))));
}

#[test]
fn headless_targets_survive_negative_adapter_ids() {
    for target in [
        None,
        Some(HEADLESS_TARGET),
        Some(HeadlessTarget {
            adapter_low: u32::MAX,
            adapter_high: i32::MIN,
            target_id: 0,
            resolution: meshrmm_protocol::HeadlessResolution::new(7680, 4320),
        }),
    ] {
        let mut bytes = Vec::new();
        write_command(
            &mut bytes,
            &ParentCommand::Start {
                viewer_name: String::new(),
                display_id: None,
                frames_per_second: 30,
                bitrate_bits_per_second: 1,
                codec: VideoCodec::H264,
                pixel_format: VideoPixelFormat::Yuv420,
                capture_cursor: false,
                grayscale: false,
                headless: target,
            },
        )
        .unwrap();
        write_command(&mut bytes, &ParentCommand::Stop).unwrap();
        let mut reader = bytes.as_slice();
        assert!(matches!(
            read_command(&mut reader).unwrap(),
            ParentCommand::Start { headless, .. } if headless == target
        ));
        assert!(matches!(
            read_command(&mut reader).unwrap(),
            ParentCommand::Stop
        ));
        assert!(reader.is_empty());
    }
}

const HEADLESS_TARGET: HeadlessTarget = HeadlessTarget {
    adapter_low: 0x1234_5678,
    adapter_high: -2,
    target_id: 260,
    resolution: meshrmm_protocol::HeadlessResolution::HD,
};

#[test]
fn desktop_targets_alternate() {
    assert_eq!(DesktopTarget::Default.alternate(), DesktopTarget::Winlogon);
    assert_eq!(DesktopTarget::Winlogon.alternate(), DesktopTarget::Default);
}

fn command_name(command: &ParentCommand) -> u8 {
    match command {
        ParentCommand::EnumerateDisplays => COMMAND_ENUMERATE_DISPLAYS,
        ParentCommand::CaptureThumbnail => COMMAND_CAPTURE_THUMBNAIL,
        ParentCommand::Start { .. } => COMMAND_START,
        ParentCommand::SetWallpaperHidden(_) => 19,
        ParentCommand::SetPreventIdleLock(_) => 21,
        ParentCommand::SetCursorCapture(_) => 18,
        ParentCommand::SetDisplayBorder(_) => 20,
        ParentCommand::RequestKeyframe => COMMAND_REQUEST_KEYFRAME,
        ParentCommand::SetBitrate(_) => COMMAND_SET_BITRATE,
        ParentCommand::StartInput { .. } => COMMAND_START_INPUT,
        ParentCommand::Input(_) => COMMAND_INPUT,
        ParentCommand::Annotate(_) => COMMAND_ANNOTATE,
        ParentCommand::ReleaseInput => COMMAND_RELEASE_INPUT,
        ParentCommand::BlockInput(_) => COMMAND_BLOCK_INPUT,
        ParentCommand::Blackout { .. } => COMMAND_BLACKOUT,
        ParentCommand::Clipboard(_) => COMMAND_CLIPBOARD,
        ParentCommand::StartFiles => 13,
        ParentCommand::StartClipboard => 16,
        ParentCommand::StartChatHelper { .. } => 17,
        ParentCommand::ShowConnectionNotification { .. } => 25,
        ParentCommand::PromptConnectionApproval { .. } => COMMAND_PROMPT_CONNECTION_APPROVAL,
        ParentCommand::PromptCredentials => 23,
        ParentCommand::AutofillCredentials(_) => 24,
        ParentCommand::Files(_) => 12,
        ParentCommand::Chat(_) => COMMAND_CHAT,
        ParentCommand::StartChat => COMMAND_START_CHAT,
        ParentCommand::StopChat => COMMAND_STOP_CHAT,
        ParentCommand::Stop => COMMAND_STOP,
    }
}

#[test]
fn thumbnail_replies_round_trip_within_their_bounds() {
    let mut bytes = Vec::new();
    write_command(&mut bytes, &ParentCommand::CaptureThumbnail).unwrap();
    assert!(matches!(
        read_command(bytes.as_slice()).unwrap(),
        ParentCommand::CaptureThumbnail
    ));
    for reply in [
        Ok(vec![0xff, 0xd8, 0xff, 0xe0]),
        Err("no display".to_owned()),
    ] {
        let mut bytes = Vec::new();
        write_thumbnail(&mut bytes, &reply).unwrap();
        assert_eq!(read_thumbnail(&mut bytes.as_slice()).unwrap(), reply);
    }
    let maximum = crate::remote::thumbnail::MAX_BYTES;
    assert!(write_thumbnail(&mut Vec::new(), &Ok(vec![0; maximum + 1])).is_err());
    let mut oversized = vec![0];
    oversized.extend_from_slice(&(maximum as u32 + 1).to_le_bytes());
    assert!(read_thumbnail(&mut oversized.as_slice()).is_err());
    assert!(read_thumbnail(&mut [2_u8, 0, 0, 0, 0].as_slice()).is_err());
}
