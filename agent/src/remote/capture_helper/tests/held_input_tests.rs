use std::collections::BTreeSet;

use super::*;

const OLD_DISPLAY: DisplayId = DisplayId(1);
const NEW_DISPLAY: DisplayId = DisplayId(2);
const CTRL: (u16, bool) = (0x1d, false);
const WIN: (u16, bool) = (0x5b, true);
const KEY_A: (u16, bool) = (0x1e, false);

fn key((scan_code, extended): (u16, bool), pressed: bool) -> RemoteInput {
    RemoteInput::Key {
        display_id: OLD_DISPLAY,
        scan_code,
        extended,
        pressed,
    }
}

/// A command as the tests compare it.
#[derive(Debug, PartialEq)]
enum Sent {
    Input(RemoteInput),
    ReleaseInput,
    Other,
}

fn sent(commands: &[ParentCommand]) -> Vec<Sent> {
    commands
        .iter()
        .map(|command| match command {
            ParentCommand::Input(input) => Sent::Input(input.clone()),
            ParentCommand::ReleaseInput => Sent::ReleaseInput,
            _ => Sent::Other,
        })
        .collect()
}

fn key_up_on(display_id: DisplayId, (scan_code, extended): (u16, bool)) -> Sent {
    Sent::Input(RemoteInput::Key {
        display_id,
        scan_code,
        extended,
        pressed: false,
    })
}

/// A desktop input helper's end of its pipe.
struct FakeHelper(File);

fn fake_helper() -> (FakeHelper, InputWriter) {
    let (reader, writer) = create_pipe().unwrap();
    (
        FakeHelper(reader.into_file()),
        Arc::new(CommandWriter::new(writer.into_file()).unwrap()),
    )
}

impl FakeHelper {
    fn next(&mut self) -> ParentCommand {
        read_command(&mut self.0).unwrap()
    }

    /// Everything the helper was sent, once the Agent has let its pipe go.
    fn commands(mut self) -> Vec<ParentCommand> {
        let mut commands = Vec::new();
        while let Ok(command) = read_command(&mut self.0) {
            commands.push(command);
        }
        commands
    }
}

/// A scan code and whether it is extended.
type Key = (u16, bool);

/// The session's keyboard as Windows keeps it, one state for every desktop,
/// driven as `WindowsInputController` drives it.
#[derive(Default)]
struct Device {
    down: BTreeSet<Key>,
    buttons: Vec<meshrmm_protocol::PointerButton>,
    /// Each key press, with the keys already down when it was pressed.
    presses: Vec<(Key, BTreeSet<Key>)>,
}

impl Device {
    /// Applies one helper's commands. A helper releases what it pressed
    /// when told to and when it stops, unless `releases_arrive` is false:
    /// its desktop no longer takes input.
    fn run(&mut self, commands: &[ParentCommand], releases_arrive: bool) {
        let mut pressed = BTreeSet::new();
        for command in commands {
            match command {
                ParentCommand::Input(RemoteInput::Key {
                    scan_code,
                    extended,
                    pressed: down,
                    ..
                }) => {
                    let key = (*scan_code, *extended);
                    if *down {
                        self.presses.push((key, self.down.clone()));
                        self.down.insert(key);
                        pressed.insert(key);
                    } else {
                        self.down.remove(&key);
                        pressed.remove(&key);
                    }
                }
                ParentCommand::Input(
                    RemoteInput::PointerButton {
                        button,
                        pressed: down,
                        ..
                    }
                    | RemoteInput::PointerButtonAt {
                        button,
                        pressed: down,
                        ..
                    },
                ) => {
                    self.buttons.retain(|held| held != button);
                    if *down {
                        self.buttons.push(*button);
                    }
                }
                ParentCommand::ReleaseInput | ParentCommand::Stop if releases_arrive => {
                    for key in std::mem::take(&mut pressed) {
                        self.down.remove(&key);
                    }
                    self.buttons.clear();
                }
                _ => {}
            }
        }
    }
}

#[test]
fn a_key_held_across_a_helper_replacement_is_released_before_the_next_keys() {
    let streamer = DesktopCaptureStreamer::default();
    let controller = streamer.input_controller();
    let (old, old_writer) = fake_helper();
    lock_route(&streamer.input_route).route_to(old_writer, OLD_DISPLAY);
    controller.apply(key(CTRL, true)).unwrap();

    // A UAC prompt: the helper on the old desktop goes, and the Agent's
    // release there finds a desktop that no longer takes input.
    let old_writer = lock_route(&streamer.input_route).unroute().unwrap();
    send_command(&old_writer, &ParentCommand::ReleaseInput).unwrap();
    send_command(&old_writer, &ParentCommand::Stop).unwrap();
    drop(old_writer);
    let (new, new_writer) = fake_helper();
    lock_route(&streamer.input_route).route_to(new_writer, NEW_DISPLAY);

    // The technician types A on the new desktop and only then lets Ctrl go.
    let mut a = key(KEY_A, true);
    a.set_display_id(NEW_DISPLAY);
    controller.apply(a.clone()).unwrap();
    controller.apply(key(CTRL, false)).unwrap();
    let mut a_up = key(KEY_A, false);
    a_up.set_display_id(NEW_DISPLAY);
    controller.apply(a_up.clone()).unwrap();
    assert!(lock_route(&streamer.input_route).held().is_empty());
    drop(lock_route(&streamer.input_route).unroute());

    let new = new.commands();
    // The new helper releases Ctrl first and never gets the viewer's late
    // Ctrl key-up, which is for a key it never pressed.
    assert_eq!(
        sent(&new),
        vec![
            key_up_on(NEW_DISPLAY, CTRL),
            Sent::Input(a),
            Sent::Input(a_up),
        ]
    );
    let mut device = Device::default();
    device.run(&old.commands(), false);
    device.run(&new, true);
    // A is typed alone, not as Ctrl+A.
    assert_eq!(device.presses.last(), Some(&(KEY_A, BTreeSet::new())));
    assert!(device.down.is_empty());
}

#[test]
fn a_helper_that_dies_holding_keys_leaves_them_to_its_replacement() {
    let streamer = DesktopCaptureStreamer::default();
    let controller = streamer.input_controller();
    let (mut old, old_writer) = fake_helper();
    lock_route(&streamer.input_route).route_to(old_writer, OLD_DISPLAY);
    controller.apply(key(CTRL, true)).unwrap();
    controller.apply(key(WIN, true)).unwrap();
    controller
        .apply(RemoteInput::PointerButtonAt {
            display_id: OLD_DISPLAY,
            x: 1,
            y: 2,
            button: meshrmm_protocol::PointerButton::Left,
            pressed: true,
        })
        .unwrap();
    let mut device = Device::default();
    // The helper presses all three and is killed: it releases nothing.
    let pressed = [old.next(), old.next(), old.next()];
    device.run(&pressed, true);
    drop(old);

    drop(lock_route(&streamer.input_route).unroute());
    let (new, new_writer) = fake_helper();
    lock_route(&streamer.input_route).route_to(new_writer, NEW_DISPLAY);
    assert!(lock_route(&streamer.input_route).held().is_empty());
    controller.apply(key(CTRL, false)).unwrap();
    controller.apply(key(WIN, false)).unwrap();
    drop(lock_route(&streamer.input_route).unroute());

    let new = new.commands();
    assert_eq!(
        sent(&new),
        vec![
            key_up_on(NEW_DISPLAY, CTRL),
            key_up_on(NEW_DISPLAY, WIN),
            Sent::Input(RemoteInput::PointerButton {
                display_id: NEW_DISPLAY,
                button: meshrmm_protocol::PointerButton::Left,
                pressed: false,
            }),
        ]
    );
    device.run(&new, true);
    assert!(device.down.is_empty());
    assert!(device.buttons.is_empty());
}

#[test]
fn the_session_ending_releases_everything_once() {
    let streamer = DesktopCaptureStreamer::default();
    let controller = streamer.input_controller();
    let (helper, writer) = fake_helper();
    lock_route(&streamer.input_route).route_to(writer, OLD_DISPLAY);
    controller.apply(key(CTRL, true)).unwrap();
    controller
        .apply(RemoteInput::PointerButtonAt {
            display_id: OLD_DISPLAY,
            x: 1,
            y: 2,
            button: meshrmm_protocol::PointerButton::Right,
            pressed: true,
        })
        .unwrap();

    // The viewer's connection drops: the input worker releases everything.
    controller.release_all().unwrap();
    assert!(lock_route(&streamer.input_route).held().is_empty());
    // A late release from the viewer is not sent again.
    controller.apply(key(CTRL, false)).unwrap();
    drop(lock_route(&streamer.input_route).unroute());
    let (next, next_writer) = fake_helper();
    lock_route(&streamer.input_route).route_to(next_writer, NEW_DISPLAY);
    drop(lock_route(&streamer.input_route).unroute());

    let commands = helper.commands();
    assert_eq!(commands.len(), 3);
    assert_eq!(sent(&commands)[2], Sent::ReleaseInput);
    let mut device = Device::default();
    device.run(&commands, true);
    assert!(device.down.is_empty());
    assert!(device.buttons.is_empty());
    assert!(next.commands().is_empty());
}

#[test]
fn the_record_follows_what_the_helper_was_told() {
    let mut held = HeldOnDevice::default();
    assert!(!held.admits(&key(CTRL, false)));
    assert!(held.admits(&key(CTRL, true)));
    held.note(&key(CTRL, true));
    held.note(&key(CTRL, true));
    assert!(held.admits(&key(CTRL, false)));
    held.note(&key(CTRL, false));
    assert!(held.is_empty());

    // A helper releases everything before typing text.
    held.note(&key(WIN, true));
    held.note(&RemoteInput::TypeText {
        display_id: OLD_DISPLAY,
        text: "abc".into(),
    });
    assert!(held.is_empty());
    assert!(!held.admits(&key(WIN, false)));
    let up = RemoteInput::PointerButton {
        display_id: OLD_DISPLAY,
        button: meshrmm_protocol::PointerButton::Left,
        pressed: false,
    };
    assert!(!held.admits(&up));
    assert!(held.admits(&RemoteInput::PointerMove {
        display_id: OLD_DISPLAY,
        x: 0,
        y: 0
    }));
}
