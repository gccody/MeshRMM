#![cfg(windows)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn chat_helper_process_starts_handles_commands_and_stops_cleanly() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_meshrmm-agent"))
        .arg("--capture-helper")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = child.stdout.take().unwrap();
    let (finished, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output.read_to_end(&mut bytes).map(|_| bytes);
        let _ = finished.send(result);
    });
    // Exercise the real executable entry point and the helper wire protocol.
    let name = b"MeshRMM helper regression test";
    let mut commands = vec![17]; // StartChatHelper
    commands.extend_from_slice(&(name.len() as u32).to_le_bytes());
    commands.extend_from_slice(name);
    commands.push(10); // StartChat
    let message = b"event-driven helper lifecycle";
    commands.push(9); // Chat
    commands.extend_from_slice(&(message.len() as u32).to_le_bytes());
    commands.extend_from_slice(message);
    commands.push(4); // Stop
    child.stdin.take().unwrap().write_all(&commands).unwrap();
    let result = done.recv_timeout(Duration::from_secs(10));
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().unwrap();
    let mut errors = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut errors)
        .unwrap();
    assert!(status.success(), "helper failed: {errors}");
    assert_eq!(
        result.unwrap().unwrap(),
        [6, 4],
        "expected InputStarted and Stopped: {errors}"
    );
}
