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
