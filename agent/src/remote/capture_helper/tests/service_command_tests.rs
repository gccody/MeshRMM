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
        assert!(matches!(
            receiver.recv().await,
            Some(Ok(ParentCommand::StartChat))
        ));
        assert!(
            matches!(receiver.recv().await, Some(Ok(ParentCommand::Chat(text))) if text == "hello")
        );
        assert!(matches!(
            receiver.recv().await,
            Some(Ok(ParentCommand::Stop))
        ));
        assert!(receiver.recv().await.is_none());
    })
    .await
    .unwrap();
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
