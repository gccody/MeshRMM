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
    command.extend_from_slice(&(meshrmm_protocol::MAX_CHAT_TEXT_BYTES as u32 + 1).to_le_bytes());
    assert!(
        matches!(read_command(command.as_slice()), Err(e) if e.kind() == io::ErrorKind::InvalidData)
    );
    command[0] = EVENT_CHAT;
    assert!(
        matches!(read_event(command.as_slice()), Err(e) if e.kind() == io::ErrorKind::InvalidData)
    );
}
