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
    assert!(!helper_uses_user_token(
        HelperKind::Notification,
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
        unsafe { windows::Win32::Foundation::GetHandleInformation(handle.0, &mut flags) }.unwrap();
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
