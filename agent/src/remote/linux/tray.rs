//! The Agent's icon in the desktop's tray while a session helper runs, as
//! the Windows Agent keeps one in the notification area. It is a
//! StatusNotifierItem, which KDE Plasma, Ubuntu's GNOME (through its
//! AppIndicator extension) and other StatusNotifierItem trays show.
//! During a remote session, clicking it, or its menu's Reply item, asks the
//! user for a chat reply. The menu is a `com.canonical.dbusmenu`, which GNOME
//! opens on a click.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Context;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Type, Value};

const ICON: &[u8] = include_bytes!("../../../assets/tray.ico");
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
/// The menu's items: what it is, and the chat reply.
const RUNNING_ITEM: i32 = 1;
const REPLY_ITEM: i32 = 2;
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
/// How often chat availability is checked for the tooltip.
const CHAT_CHECK: Duration = Duration::from_secs(1);

/// What a click on the icon does, and whether it can do it now.
pub(crate) trait TrayChat: Send + Sync + 'static {
    fn chat_available(&self) -> bool;
    fn ask_for_reply(&self);
}

/// An ARGB32 image as the item's properties carry it: width, height and the
/// pixels in network byte order.
type Pixmap = (i32, i32, Vec<u8>);

struct Item {
    chat: Arc<dyn TrayChat>,
    icon: Vec<Pixmap>,
}

fn tooltip(chat_available: bool) -> &'static str {
    if chat_available {
        "Click to chat with the remote viewer"
    } else {
        "MeshRMM Agent is running"
    }
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "meshrmm-agent"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "MeshRMM Agent"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<Pixmap> {
        self.icon.clone()
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<Pixmap>, String, String) {
        (
            String::new(),
            Vec::new(),
            "MeshRMM Agent".to_owned(),
            tooltip(self.chat.chat_available()).to_owned(),
        )
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        ObjectPath::from_static_str_unchecked(MENU_PATH).into()
    }

    fn activate(&self, _x: i32, _y: i32) {
        if self.chat.chat_available() {
            self.chat.ask_for_reply();
        }
    }

    fn secondary_activate(&self, x: i32, y: i32) {
        self.activate(x, y);
    }

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: &str) {}

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// One entry of the menu's layout: its ID, properties and child entries.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Type, Value, OwnedValue)]
struct Layout {
    id: i32,
    properties: HashMap<String, OwnedValue>,
    children: Vec<OwnedValue>,
}

fn item_properties(label: &str, enabled: bool) -> HashMap<String, OwnedValue> {
    HashMap::from([
        (
            "label".to_owned(),
            OwnedValue::from(zbus::zvariant::Str::from(label.to_owned())),
        ),
        ("enabled".to_owned(), OwnedValue::from(enabled)),
    ])
}

/// Each item's properties: the reply is enabled only while chat is.
fn items(chat_available: bool) -> [(i32, HashMap<String, OwnedValue>); 2] {
    [
        (
            RUNNING_ITEM,
            item_properties("MeshRMM Agent is running", false),
        ),
        (
            REPLY_ITEM,
            item_properties("Reply to the technician…", chat_available),
        ),
    ]
}

/// The menu's root and its items.
fn layout(chat_available: bool) -> Layout {
    let children = items(chat_available)
        .into_iter()
        .filter_map(|(id, properties)| {
            OwnedValue::try_from(Value::from(Layout {
                id,
                properties,
                children: Vec::new(),
            }))
            .ok()
        })
        .collect();
    Layout {
        id: 0,
        properties: HashMap::from([(
            "children-display".to_owned(),
            OwnedValue::from(zbus::zvariant::Str::from_static("submenu")),
        )]),
        children,
    }
}

struct Menu {
    chat: Arc<dyn TrayChat>,
    /// Counts layout changes, which hosts fetch again.
    revision: Arc<AtomicU32>,
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    fn get_layout(
        &self,
        parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, Layout) {
        let mut layout = layout(self.chat.chat_available());
        if parent_id != 0 {
            // Items have no children of their own.
            layout = Layout {
                id: parent_id,
                properties: HashMap::new(),
                children: Vec::new(),
            };
        }
        (self.revision.load(Ordering::SeqCst), layout)
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        items(self.chat.chat_available())
            .into_iter()
            .filter(|(id, _)| ids.is_empty() || ids.contains(id))
            .collect()
    }

    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<OwnedValue> {
        items(self.chat.chat_available())
            .into_iter()
            .find(|(item, _)| *item == id)
            .and_then(|(_, mut properties)| properties.remove(name))
            .ok_or_else(|| {
                zbus::fdo::Error::InvalidArgs(format!("no property {name} on item {id}"))
            })
    }

    fn event(&self, id: i32, event_id: &str, _data: OwnedValue, _timestamp: u32) {
        if id == REPLY_ITEM && event_id == "clicked" && self.chat.chat_available() {
            self.chat.ask_for_reply();
        }
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, event_id, data, timestamp) in events {
            self.event(id, &event_id, data, timestamp);
        }
        Vec::new()
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(signal)]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}

/// The icon, shown until dropped.
pub(crate) struct Tray {
    _connection: zbus::blocking::Connection,
    stopped: Arc<AtomicBool>,
}

impl Tray {
    /// Puts the icon in the tray, now and whenever a tray host starts again.
    pub(crate) fn start(chat: Arc<dyn TrayChat>) -> anyhow::Result<Self> {
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let item = Item {
            chat: Arc::clone(&chat),
            icon: icon_pixmaps(),
        };
        let revision = Arc::new(AtomicU32::new(1));
        let menu = Menu {
            chat: Arc::clone(&chat),
            revision: Arc::clone(&revision),
        };
        let connection = zbus::blocking::connection::Builder::session()?
            .name(name.as_str())?
            .serve_at(ITEM_PATH, item)?
            .serve_at(MENU_PATH, menu)?
            .build()
            .context("could not connect to the session's D-Bus")?;
        let stopped = Arc::new(AtomicBool::new(false));
        // The watcher may start after the helper, or restart with the shell.
        let changes = zbus::blocking::fdo::DBusProxy::new(&connection)?
            .receive_name_owner_changed_with_args(&[(0, WATCHER)])?;
        let (registering, item_name) = (connection.clone(), name.clone());
        std::thread::Builder::new()
            .name("meshrmm-tray-watcher".into())
            .spawn(move || {
                register(&registering, &item_name);
                for change in changes {
                    if change.args().is_ok_and(|args| args.new_owner().is_some()) {
                        register(&registering, &item_name);
                    }
                }
            })?;
        let (updating, running) = (connection.clone(), Arc::clone(&stopped));
        std::thread::Builder::new()
            .name("meshrmm-tray-tooltip".into())
            .spawn(move || {
                let mut available = chat.chat_available();
                while !running.load(Ordering::SeqCst) {
                    std::thread::sleep(CHAT_CHECK);
                    let now = chat.chat_available();
                    if now != available {
                        available = now;
                        let server = updating.object_server();
                        if let Ok(item) = server.interface::<_, Item>(ITEM_PATH) {
                            let _ = zbus::block_on(Item::new_tool_tip(item.signal_emitter()));
                        }
                        let revision = revision.fetch_add(1, Ordering::SeqCst) + 1;
                        if let Ok(menu) = server.interface::<_, Menu>(MENU_PATH) {
                            let _ = zbus::block_on(Menu::layout_updated(
                                menu.signal_emitter(),
                                revision,
                                0,
                            ));
                        }
                    }
                }
            })?;
        Ok(Self {
            _connection: connection,
            stopped,
        })
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// Asks the tray host to show the item. Without one, such as GNOME without
/// its AppIndicator extension, nothing is shown.
fn register(connection: &zbus::blocking::Connection, name: &str) {
    match connection.call_method(
        Some(WATCHER),
        WATCHER_PATH,
        Some(WATCHER),
        "RegisterStatusNotifierItem",
        &(name,),
    ) {
        Ok(_) => tracing::info!("the Agent's icon is in the tray"),
        Err(error) => tracing::info!(%error, "this desktop shows no tray icons"),
    }
}

/// The tray icon as ARGB32 pixmaps: the Windows icon's 32-bit image, which
/// stores its rows bottom up as BGRA.
fn icon_pixmaps() -> Vec<Pixmap> {
    pixmap_from_ico(ICON).into_iter().collect()
}

fn pixmap_from_ico(ico: &[u8]) -> Option<Pixmap> {
    let word = |offset: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            ico.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    // The first directory entry's image.
    let offset = word(18)? as usize;
    let header = word(offset)? as usize;
    let width = word(offset + 4)? as usize;
    // The height counts the colour image and its mask.
    let height = word(offset + 8)? as usize / 2;
    let bits = u16::from_le_bytes(ico.get(offset + 14..offset + 16)?.try_into().ok()?);
    if bits != 32 || width == 0 || height == 0 {
        return None;
    }
    let pixels = ico.get(offset + header..offset + header + width * height * 4)?;
    let mut argb = Vec::with_capacity(width * height * 4);
    for row in pixels.chunks_exact(width * 4).rev() {
        for pixel in row.chunks_exact(4) {
            argb.extend_from_slice(&[pixel[3], pixel[2], pixel[1], pixel[0]]);
        }
    }
    Some((width as i32, height as i32, argb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_a_32_pixel_argb_image() {
        let (width, height, pixels) = pixmap_from_ico(ICON).expect("the tray icon decodes");
        assert_eq!((width, height), (32, 32));
        assert_eq!(pixels.len(), 32 * 32 * 4);
        // Not blank: some pixels are opaque.
        assert!(pixels.chunks_exact(4).any(|pixel| pixel[0] == 255));
        assert!(pixmap_from_ico(&ICON[..40]).is_none());
    }

    #[test]
    fn rows_are_turned_top_down_and_bytes_into_argb() {
        // A 1 by 2 image: bottom row blue, top row red, in BGRA, then the mask.
        let mut ico = vec![0, 0, 1, 0, 1, 0, 1, 2, 0, 0, 1, 0, 32, 0];
        ico.extend_from_slice(&48_u32.to_le_bytes());
        ico.extend_from_slice(&22_u32.to_le_bytes());
        for word in [40_u32, 1, 4] {
            ico.extend_from_slice(&word.to_le_bytes());
        }
        ico.extend_from_slice(&[1, 0, 32, 0]);
        ico.extend_from_slice(&[0; 24]);
        ico.extend_from_slice(&[255, 0, 0, 255, 0, 0, 255, 255]);
        let (width, height, pixels) = pixmap_from_ico(&ico).unwrap();
        assert_eq!((width, height), (1, 2));
        assert_eq!(pixels, [255, 255, 0, 0, 255, 0, 0, 255]);
    }

    #[test]
    fn the_menu_layout_has_the_dbusmenu_shape() {
        assert_eq!(Layout::SIGNATURE.to_string(), "(ia{sv}av)");
        let menu = layout(false);
        assert_eq!((menu.id, menu.children.len()), (0, 2));
        let reply = |available| {
            items(available)
                .into_iter()
                .find(|(id, _)| *id == REPLY_ITEM)
                .and_then(|(_, properties)| properties.get("enabled").cloned())
                .and_then(|enabled| bool::try_from(enabled).ok())
        };
        assert_eq!(reply(false), Some(false));
        assert_eq!(reply(true), Some(true));
    }

    #[test]
    fn the_tooltip_says_when_a_click_opens_chat() {
        assert_eq!(tooltip(false), "MeshRMM Agent is running");
        assert_eq!(tooltip(true), "Click to chat with the remote viewer");
    }
}
