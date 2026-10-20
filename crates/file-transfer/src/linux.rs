//! Linux parts of file transfer. The Agent's session helper runs as the
//! signed-in user with their desktop's environment, so the folders, the
//! clipboard and the file chooser are that user's.
use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct NativeGuard;
pub fn initialize() -> anyhow::Result<NativeGuard> {
    Ok(NativeGuard)
}

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// The user's Documents folder from `xdg-user-dirs`, or `~/Documents`.
pub fn documents() -> anyhow::Result<PathBuf> {
    let home = home()?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let configured = std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|dirs| documents_from_user_dirs(&dirs, &home));
    Ok(configured.unwrap_or_else(|| home.join("Documents")))
}

/// `XDG_DOCUMENTS_DIR` from a `user-dirs.dirs` file, which holds shell
/// assignments such as `XDG_DOCUMENTS_DIR="$HOME/Documents"`.
fn documents_from_user_dirs(dirs: &str, home: &Path) -> Option<PathBuf> {
    let value = dirs
        .lines()
        .find_map(|line| line.trim().strip_prefix("XDG_DOCUMENTS_DIR="))?
        .trim()
        .trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None if value.starts_with('/') => PathBuf::from(value),
        None => return None,
    };
    // xdg-user-dirs points unused folders at the home folder itself.
    (path != home).then_some(path)
}

pub fn cache() -> anyhow::Result<PathBuf> {
    let cache = match std::env::var_os("XDG_CACHE_HOME").filter(|cache| !cache.is_empty()) {
        Some(cache) => PathBuf::from(cache),
        None => home()?.join(".cache"),
    };
    Ok(cache.join("meshrmm"))
}

/// The desktop's own dialogs: zenity on GTK desktops, kdialog on KDE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialogs {
    Zenity,
    Kdialog,
}

/// What the user chose to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Files,
    Folders,
}

/// Asks with the desktop's own file chooser. Neither zenity nor kdialog picks
/// files and folders in one dialog, so the user first chooses which to send.
pub fn pick() -> anyhow::Result<Vec<PathBuf>> {
    let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktop| desktop.contains("KDE"));
    let order = if kde {
        [Dialogs::Kdialog, Dialogs::Zenity]
    } else {
        [Dialogs::Zenity, Dialogs::Kdialog]
    };
    for dialogs in order {
        let choice = match ask_kind(dialogs) {
            Ok(choice) => choice,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("could not show the file chooser"),
        };
        let Some(kind) = choice else {
            return Ok(Vec::new());
        };
        let output = choose(dialogs, kind).context("could not show the file chooser")?;
        // Both exit with 1 when the user cancels.
        return Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect());
    }
    bail!("this desktop has no file chooser the Agent can show (zenity or kdialog)")
}

/// Asks whether to send files or a folder; `None` when the user cancels.
fn ask_kind(dialogs: Dialogs) -> std::io::Result<Option<Kind>> {
    const QUESTION: &str = "Send files, or a whole folder?";
    match dialogs {
        Dialogs::Zenity => {
            // The extra button prints its label and exits with 1, as Cancel
            // does without printing anything.
            let output = std::process::Command::new("zenity")
                .args([
                    "--question",
                    "--title=Send files",
                    &format!("--text={QUESTION}"),
                    "--ok-label=Files",
                    "--extra-button=Folder",
                    "--cancel-label=Cancel",
                ])
                .output()?;
            Ok(if output.status.success() {
                Some(Kind::Files)
            } else if String::from_utf8_lossy(&output.stdout).trim() == "Folder" {
                Some(Kind::Folders)
            } else {
                None
            })
        }
        Dialogs::Kdialog => {
            let status = std::process::Command::new("kdialog")
                .args([
                    "--title",
                    "Send files",
                    "--yes-label",
                    "Files",
                    "--no-label",
                    "Folder",
                    "--yesnocancel",
                    QUESTION,
                ])
                .status()?;
            Ok(match status.code() {
                Some(0) => Some(Kind::Files),
                Some(1) => Some(Kind::Folders),
                _ => None,
            })
        }
    }
}

/// Shows the chooser for `kind`; its output lists the paths, one per line.
fn choose(dialogs: Dialogs, kind: Kind) -> std::io::Result<std::process::Output> {
    let mut command = match dialogs {
        Dialogs::Zenity => std::process::Command::new("zenity"),
        Dialogs::Kdialog => std::process::Command::new("kdialog"),
    };
    match (dialogs, kind) {
        (Dialogs::Zenity, Kind::Files) => command.args([
            "--file-selection",
            "--multiple",
            "--separator=\n",
            "--title=Send files",
        ]),
        (Dialogs::Zenity, Kind::Folders) => command.args([
            "--file-selection",
            "--directory",
            "--multiple",
            "--separator=\n",
            "--title=Send folders",
        ]),
        (Dialogs::Kdialog, Kind::Files) => command.args([
            "--getopenfilename",
            ".",
            "--multiple",
            "--separate-output",
            "--title",
            "Send files",
        ]),
        // kdialog picks one folder at a time.
        (Dialogs::Kdialog, Kind::Folders) => {
            command.args(["--getexistingdirectory", ".", "--title", "Send a folder"])
        }
    };
    command.output()
}

/// Counts the clipboard's changes, which the display server announces:
/// selection events of the `ext-data-control` or `wlr-data-control` protocol
/// on Wayland compositors that have one (KDE, wlroots), else XFixes selection
/// notifications from the X server, which on GNOME is Xwayland mirroring the
/// Wayland clipboard.
pub fn clipboard_sequence() -> u64 {
    static WATCHER: std::sync::Once = std::sync::Once::new();
    WATCHER.call_once(|| {
        let spawn = |watch: Box<dyn FnOnce() -> anyhow::Result<()> + Send>| {
            let _ = std::thread::Builder::new()
                .name("meshrmm-clipboard-watcher".into())
                .spawn(move || {
                    if let Err(error) = watch() {
                        tracing::warn!(%error, "clipboard changes cannot be watched");
                    }
                });
        };
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
            && match clipboard_watch::wayland() {
                Ok(watch) => {
                    spawn(Box::new(watch));
                    true
                }
                Err(error) => {
                    tracing::info!(%error, "watching the clipboard through X11 instead of Wayland");
                    false
                }
            };
        clipboard_watch::DATA_CONTROL.store(wayland, std::sync::atomic::Ordering::SeqCst);
        if !wayland && std::env::var_os("DISPLAY").is_some() {
            spawn(Box::new(clipboard_watch::x11));
        }
    });
    clipboard_watch::SEQUENCE.load(std::sync::atomic::Ordering::SeqCst)
}

mod clipboard_watch {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use anyhow::Context;
    use wayland_client::protocol::{wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, QueueHandle, event_created_child};
    use wayland_protocols::ext::data_control::v1::client::{
        ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_offer_v1,
    };
    use wayland_protocols_wlr::data_control::v1::client::{
        zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    };
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
    use x11rb::protocol::xproto::ConnectionExt as _;

    pub(super) static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    /// Whether the clipboard is watched, and arboard reaches it, through
    /// Wayland data control rather than X11; set before the first sequence is
    /// read.
    pub(super) static DATA_CONTROL: AtomicBool = AtomicBool::new(false);

    fn changed() {
        SEQUENCE.fetch_add(1, Ordering::SeqCst);
    }

    #[derive(Default)]
    struct State {
        seat: Option<wl_seat::WlSeat>,
        ext_manager: Option<ext_data_control_manager_v1::ExtDataControlManagerV1>,
        wlr_manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
        ext_offer: Option<ext_data_control_offer_v1::ExtDataControlOfferV1>,
        wlr_offer: Option<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1>,
        finished: bool,
    }

    /// Connects to the compositor's data-control protocol, and returns what
    /// counts its selection events until the compositor goes away.
    pub(super) fn wayland() -> anyhow::Result<impl FnOnce() -> anyhow::Result<()> + Send> {
        let connection = Connection::connect_to_env()?;
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());
        let mut state = State::default();
        queue.roundtrip(&mut state)?;
        let seat = state.seat.clone().context("the compositor has no seat")?;
        if let Some(manager) = &state.ext_manager {
            manager.get_data_device(&seat, &handle, ());
        } else if let Some(manager) = &state.wlr_manager {
            manager.get_data_device(&seat, &handle, ());
        } else {
            anyhow::bail!("the compositor has no data-control protocol");
        }
        tracing::info!("watching the clipboard through Wayland data control");
        Ok(move || {
            // The connection lives as long as the watch.
            let _connection = connection;
            while !state.finished {
                queue.blocking_dispatch(&mut state)?;
            }
            Ok(())
        })
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for State {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            handle: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                match interface.as_str() {
                    "wl_seat" if state.seat.is_none() => {
                        state.seat = Some(registry.bind(name, version.min(1), handle, ()));
                    }
                    "ext_data_control_manager_v1" => {
                        state.ext_manager = Some(registry.bind(name, version.min(1), handle, ()));
                    }
                    "zwlr_data_control_manager_v1" => {
                        state.wlr_manager = Some(registry.bind(name, version.min(1), handle, ()));
                    }
                    _ => {}
                }
            }
        }
    }

    impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for State {
        fn event(
            state: &mut Self,
            _: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
            event: zwlr_data_control_device_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                zwlr_data_control_device_v1::Event::Selection { id } => {
                    // The previous selection's offer is no longer needed.
                    if let Some(offer) = std::mem::replace(&mut state.wlr_offer, id) {
                        offer.destroy();
                    }
                    changed();
                }
                zwlr_data_control_device_v1::Event::Finished => state.finished = true,
                _ => {}
            }
        }

        event_created_child!(State, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
            zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
        ]);
    }

    impl Dispatch<ext_data_control_device_v1::ExtDataControlDeviceV1, ()> for State {
        fn event(
            state: &mut Self,
            _: &ext_data_control_device_v1::ExtDataControlDeviceV1,
            event: ext_data_control_device_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                ext_data_control_device_v1::Event::Selection { id } => {
                    if let Some(offer) = std::mem::replace(&mut state.ext_offer, id) {
                        offer.destroy();
                    }
                    changed();
                }
                ext_data_control_device_v1::Event::Finished => state.finished = true,
                _ => {}
            }
        }

        event_created_child!(State, ext_data_control_device_v1::ExtDataControlDeviceV1, [
            ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ext_data_control_offer_v1::ExtDataControlOfferV1, ()),
        ]);
    }

    macro_rules! ignore_events {
        ($($interface:ty),* $(,)?) => {
            $(
                impl Dispatch<$interface, ()> for State {
                    fn event(
                        _: &mut Self,
                        _: &$interface,
                        _: <$interface as wayland_client::Proxy>::Event,
                        _: &(),
                        _: &Connection,
                        _: &QueueHandle<Self>,
                    ) {
                    }
                }
            )*
        };
    }

    ignore_events!(
        wl_seat::WlSeat,
        zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
        zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        ext_data_control_manager_v1::ExtDataControlManagerV1,
        ext_data_control_offer_v1::ExtDataControlOfferV1,
    );

    /// Counts XFixes selection notifications for the clipboard until the X
    /// server goes away.
    pub(super) fn x11() -> anyhow::Result<()> {
        let (connection, screen) = x11rb::rust_connection::RustConnection::connect(None)
            .context("could not connect to the X server")?;
        let root = connection.setup().roots[screen].root;
        connection
            .xfixes_query_version(5, 0)?
            .reply()
            .context("the X server has no XFixes")?;
        let clipboard = connection.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
        connection
            .xfixes_select_selection_input(
                root,
                clipboard,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )?
            .check()?;
        loop {
            if let x11rb::protocol::Event::XfixesSelectionNotify(_) = connection.wait_for_event()? {
                changed();
            }
        }
    }
}

/// Copied files on the X11 clipboard, in the formats file managers paste:
/// `x-special/gnome-copied-files`, the only one Thunar, Caja and Nemo read,
/// `text/uri-list`, and the paths as text. arboard offers only
/// `text/uri-list`.
mod x11_files {
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    use anyhow::{Context, ensure};
    use x11rb::connection::{Connection as _, RequestConnection as _};
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{
        AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SELECTION_NOTIFY_EVENT,
        SelectionNotifyEvent, WindowClass,
    };
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

    /// Takes the clipboard and serves `paths` from a thread until another
    /// program takes it.
    pub(super) fn serve(paths: &[PathBuf]) -> anyhow::Result<()> {
        let uris: Vec<String> = paths.iter().map(|path| file_uri(path)).collect();
        let text = paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        let (connection, screen) =
            RustConnection::connect(None).context("could not connect to the X server")?;
        let root = connection.setup().roots[screen].root;
        let atom = |name: &[u8]| -> anyhow::Result<u32> {
            Ok(connection.intern_atom(false, name)?.reply()?.atom)
        };
        let clipboard = atom(b"CLIPBOARD")?;
        let targets = atom(b"TARGETS")?;
        let formats = [
            (
                atom(b"x-special/gnome-copied-files")?,
                format!("copy\n{}", uris.join("\n")).into_bytes(),
            ),
            (
                atom(b"text/uri-list")?,
                (uris.join("\r\n") + "\r\n").into_bytes(),
            ),
            (atom(b"UTF8_STRING")?, text.clone().into_bytes()),
            (atom(b"text/plain;charset=utf-8")?, text.into_bytes()),
        ];
        let window = connection.generate_id()?;
        connection.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )?;
        connection.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)?;
        ensure!(
            connection.get_selection_owner(clipboard)?.reply()?.owner == window,
            "another program kept the clipboard"
        );
        std::thread::Builder::new()
            .name("meshrmm-clipboard-files".into())
            .spawn(move || {
                let offered: Vec<u32> = std::iter::once(targets)
                    .chain(formats.iter().map(|(format, _)| *format))
                    .collect();
                let maximum = connection.maximum_request_bytes();
                loop {
                    let request = match connection.wait_for_event() {
                        Ok(Event::SelectionRequest(request)) => request,
                        Ok(Event::SelectionClear(_)) | Err(_) => return,
                        Ok(_) => continue,
                    };
                    // Old clients name no property; the target stands in.
                    let property = if request.property == u32::from(AtomEnum::NONE) {
                        request.target
                    } else {
                        request.property
                    };
                    let written = if request.target == targets {
                        connection
                            .change_property32(
                                PropMode::REPLACE,
                                request.requestor,
                                property,
                                AtomEnum::ATOM,
                                &offered,
                            )
                            .is_ok()
                    } else {
                        formats
                            .iter()
                            .find(|(format, data)| {
                                *format == request.target && data.len() + 64 < maximum
                            })
                            .is_some_and(|(format, data)| {
                                connection
                                    .change_property8(
                                        PropMode::REPLACE,
                                        request.requestor,
                                        property,
                                        *format,
                                        data,
                                    )
                                    .is_ok()
                            })
                    };
                    let notify = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: request.time,
                        requestor: request.requestor,
                        selection: request.selection,
                        target: request.target,
                        property: if written {
                            property
                        } else {
                            AtomEnum::NONE.into()
                        },
                    };
                    let _ = connection.send_event(
                        false,
                        request.requestor,
                        EventMask::NO_EVENT,
                        notify,
                    );
                    let _ = connection.flush();
                }
            })?;
        Ok(())
    }

    /// A `file://` URI, with everything but unreserved characters and `/`
    /// percent-encoded.
    pub(super) fn file_uri(path: &std::path::Path) -> String {
        let mut uri = String::from("file://");
        for &byte in path.as_os_str().as_bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
                uri.push(char::from(byte));
            } else {
                uri.push_str(&format!("%{byte:02X}"));
            }
        }
        uri
    }
}

/// One clipboard connection for the process: arboard connects again, and logs
/// its Wayland fallback, every time one is made.
fn with_clipboard<T>(work: impl FnOnce(&mut arboard::Clipboard) -> T) -> anyhow::Result<T> {
    static CLIPBOARD: std::sync::Mutex<Option<arboard::Clipboard>> = std::sync::Mutex::new(None);
    let mut clipboard = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
    if clipboard.is_none() {
        *clipboard = Some(arboard::Clipboard::new()?);
    }
    Ok(work(
        clipboard.as_mut().expect("the clipboard was just opened"),
    ))
}

pub fn clipboard_files() -> anyhow::Result<Vec<PathBuf>> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    match with_clipboard(|clipboard| clipboard.get().file_list())? {
        // arboard splits `text/uri-list` at line feeds, and its lines end in
        // carriage return, line feed.
        Ok(paths) => Ok(paths
            .into_iter()
            .map(
                |path| match path.as_os_str().as_bytes().strip_suffix(b"\r") {
                    Some(path) => PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())),
                    None => path,
                },
            )
            .collect()),
        Err(arboard::Error::ContentNotAvailable | arboard::Error::ClipboardNotSupported) => {
            Ok(Vec::new())
        }
        Err(error) => Err(error.into()),
    }
}

pub fn set_clipboard_files(paths: &[PathBuf]) -> anyhow::Result<()> {
    let before = clipboard_sequence();
    if clipboard_watch::DATA_CONTROL.load(std::sync::atomic::Ordering::SeqCst)
        || std::env::var_os("DISPLAY").is_none()
    {
        with_clipboard(|clipboard| clipboard.set().file_list(paths))?
            .context("could not publish files to the clipboard")?;
    } else {
        x11_files::serve(paths).context("could not publish files to the clipboard")?;
    }
    // The display server announces the change a moment later; waiting for it
    // keeps the caller from taking the files it published for a local copy.
    let deadline = Instant::now() + std::time::Duration::from_millis(500);
    while clipboard_sequence() == before && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

pub fn drop_files(
    _: &[PathBuf],
    _: meshrmm_protocol::DisplayId,
    _: u16,
    _: u16,
) -> anyhow::Result<bool> {
    Ok(false)
}

fn c_path(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(std::ffi::CString::new(path.as_os_str().as_bytes())?)
}

pub fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both paths are NUL-terminated and outlive the call; renameat2
    // takes two directory descriptors, two paths and flags.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // Some file systems, such as older network ones, refuse the flag.
    match error.raw_os_error() {
        Some(libc::EINVAL | libc::ENOSYS) => Err(std::io::ErrorKind::Unsupported.into()),
        _ => Err(error),
    }
}

/// Linux has no quarantine for downloads to join, so received files stay as
/// they are.
pub fn mark_received(_: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Bytes available to this user on the volume holding `path`.
pub fn available_space(path: &Path) -> std::io::Result<u64> {
    let path = c_path(path)?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and `stats` is writable for one statvfs.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: statvfs succeeded and initialized the structure.
    let stats = unsafe { stats.assume_init() };
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

/// Received files show no progress window on Linux; the viewer shows the
/// transfer's progress to the technician.
pub struct Progress;
impl Progress {
    pub fn new(_: u64) -> anyhow::Result<Self> {
        Ok(Self)
    }
    pub fn update(&self, _: u64, _: u64, _: &str, _: usize, _: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run in a desktop session and copy something within 20 seconds.
    #[test]
    #[ignore = "watches the session's clipboard"]
    fn sees_clipboard_changes() {
        let first = clipboard_sequence();
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while clipboard_sequence() == first {
            assert!(
                Instant::now() < deadline,
                "no clipboard change was announced"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    #[test]
    fn file_uris_escape_reserved_characters() {
        assert_eq!(
            x11_files::file_uri(Path::new("/home/ada/My Files/a#1 €.txt")),
            "file:///home/ada/My%20Files/a%231%20%E2%82%AC.txt"
        );
    }

    #[test]
    fn documents_come_from_user_dirs() {
        let home = Path::new("/home/ada");
        let dirs =
            "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOCUMENTS_DIR=\"$HOME/Dokumente\"\n";
        assert_eq!(
            documents_from_user_dirs(dirs, home),
            Some(PathBuf::from("/home/ada/Dokumente"))
        );
        assert_eq!(
            documents_from_user_dirs("XDG_DOCUMENTS_DIR=\"/srv/docs\"", home),
            Some(PathBuf::from("/srv/docs"))
        );
        assert_eq!(
            documents_from_user_dirs("XDG_DOCUMENTS_DIR=\"$HOME/\"", home),
            None
        );
        assert_eq!(documents_from_user_dirs("", home), None);
    }

    #[test]
    fn renames_refuse_to_replace() {
        let folder = std::env::temp_dir().join(format!("meshrmm-rename-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let (from, to) = (folder.join("a"), folder.join("b"));
        std::fs::write(&from, "a").unwrap();
        std::fs::write(&to, "b").unwrap();
        let error = rename_no_replace(&from, &to).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        std::fs::remove_file(&to).unwrap();
        rename_no_replace(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "a");
        std::fs::remove_dir_all(folder).unwrap();
    }
}
