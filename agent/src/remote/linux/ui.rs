//! What a session shows the computer's user, with the desktop's own tools:
//! notifications through the freedesktop notification service, which GNOME,
//! KDE, Xfce and most others run, and dialogs through zenity or kdialog.
//! The session banner is a notification that stays until the session ends;
//! the technician's chat messages arrive as notifications with a Reply
//! button, which asks for the answer in a dialog.
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context;
use zbus::zvariant::Value;

const DESTINATION: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const APPLICATION: &str = "MeshRMM";

type ActionHandler = Box<dyn Fn(&str) + Send>;

/// The session's notification service, with the actions its notifications
/// offer.
struct Notifications {
    bus: zbus::blocking::Connection,
    handlers: Arc<Mutex<HashMap<u32, ActionHandler>>>,
}

fn notifications() -> Option<&'static Notifications> {
    static NOTIFICATIONS: OnceLock<Option<Notifications>> = OnceLock::new();
    NOTIFICATIONS
        .get_or_init(|| {
            Notifications::connect()
                .inspect_err(
                    |error| tracing::warn!(error = ?error, "desktop notifications are unavailable"),
                )
                .ok()
        })
        .as_ref()
}

impl Notifications {
    fn connect() -> anyhow::Result<Self> {
        let bus = zbus::blocking::Connection::session()
            .context("could not connect to the session's D-Bus")?;
        let handlers: Arc<Mutex<HashMap<u32, ActionHandler>>> = Arc::default();
        let proxy = zbus::blocking::Proxy::new(&bus, DESTINATION, PATH, DESTINATION)?;
        let mut actions = proxy.receive_signal("ActionInvoked")?;
        let mut closed = proxy.receive_signal("NotificationClosed")?;
        let invoked = Arc::clone(&handlers);
        std::thread::Builder::new()
            .name("meshrmm-notification-actions".into())
            .spawn(move || {
                for message in actions.by_ref() {
                    let Ok((id, action)) = message.body().deserialize::<(u32, String)>() else {
                        continue;
                    };
                    if let Some(handler) =
                        invoked.lock().unwrap_or_else(|e| e.into_inner()).get(&id)
                    {
                        handler(&action);
                    }
                }
            })?;
        let forgotten = Arc::clone(&handlers);
        std::thread::Builder::new()
            .name("meshrmm-notification-closed".into())
            .spawn(move || {
                for message in closed.by_ref() {
                    if let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() {
                        forgotten
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&id);
                    }
                }
            })?;
        Ok(Self { bus, handlers })
    }

    /// Shows a notification and returns its ID. A persistent one stays until
    /// it is closed; `actions` are its buttons, by key and label.
    fn show(
        &self,
        summary: &str,
        body: &str,
        persistent: bool,
        actions: &[(&str, &str)],
        handler: Option<ActionHandler>,
    ) -> anyhow::Result<u32> {
        let actions: Vec<&str> = actions
            .iter()
            .flat_map(|(key, label)| [*key, *label])
            .collect();
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", Value::from("meshrmm-agent"));
        if persistent {
            hints.insert("resident", Value::from(true));
            // Critical notifications stay on screen until they are closed.
            hints.insert("urgency", Value::from(2_u8));
        }
        let id: u32 = self
            .bus
            .call_method(
                Some(DESTINATION),
                PATH,
                Some(DESTINATION),
                "Notify",
                &(
                    APPLICATION,
                    0_u32,
                    "",
                    summary,
                    escape(body),
                    actions,
                    hints,
                    if persistent { 0_i32 } else { -1_i32 },
                ),
            )
            .context("the notification service refused the notification")?
            .body()
            .deserialize()?;
        if let Some(handler) = handler {
            self.handlers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, handler);
        }
        Ok(id)
    }

    fn close(&self, id: u32) {
        self.handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        let _ = self.bus.call_method(
            Some(DESTINATION),
            PATH,
            Some(DESTINATION),
            "CloseNotification",
            &(id,),
        );
    }
}

/// Notification bodies may hold markup, so text is escaped.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A notification that is closed when dropped.
pub(crate) struct Notice(u32);

impl Notice {
    fn show(
        summary: &str,
        body: &str,
        persistent: bool,
        actions: &[(&str, &str)],
        handler: Option<ActionHandler>,
    ) -> anyhow::Result<Self> {
        notifications()
            .context("this desktop has no notification service")?
            .show(summary, body, persistent, actions, handler)
            .map(Self)
    }
}

impl Drop for Notice {
    fn drop(&mut self) {
        if let Some(notifications) = notifications() {
            notifications.close(self.0);
        }
    }
}

/// Tells the user a technician connected, as company policy words it.
pub(crate) fn connection_notification(text: &str) -> anyhow::Result<Notice> {
    Notice::show("Remote connection", text, false, &[], None)
}

/// The banner and chat of a remote session.
pub(crate) struct SessionIndicator {
    viewer_name: String,
    chat: meshrmm_chat::ChatSession,
    /// Shown for as long as the session lasts.
    _banner: Option<Notice>,
    /// The latest message's notification, replaced by the next.
    message: Mutex<Option<Notice>>,
}

impl SessionIndicator {
    pub(crate) fn show(
        viewer_name: &str,
        chat: meshrmm_chat::ChatSession,
        show_banner: bool,
    ) -> anyhow::Result<Self> {
        let banner = if show_banner {
            let replies = chat.clone();
            let name = viewer_name.to_owned();
            Some(Notice::show(
                "Remote session",
                &format!("{viewer_name} is connected to this computer."),
                true,
                &[("chat", "Chat")],
                Some(Box::new(move |_| ask_for_reply(&name, &replies))),
            )?)
        } else {
            None
        };
        Ok(Self {
            viewer_name: viewer_name.to_owned(),
            chat,
            _banner: banner,
            message: Mutex::default(),
        })
    }

    /// Shows a message from the technician with a Reply button.
    pub(crate) fn show_message(&self, text: &str) {
        let replies = self.chat.clone();
        let name = self.viewer_name.clone();
        let shown = Notice::show(
            &format!("Message from {}", self.viewer_name),
            text,
            false,
            &[("reply", "Reply")],
            Some(Box::new(move |_| ask_for_reply(&name, &replies))),
        );
        match shown {
            Ok(notice) => {
                *self.message.lock().unwrap_or_else(|e| e.into_inner()) = Some(notice);
            }
            Err(error) => tracing::warn!(error = ?error, "could not show a chat message"),
        }
    }
}

/// Asks the user for a reply in a dialog, off the notification thread, and
/// sends it.
fn ask_for_reply(viewer_name: &str, chat: &meshrmm_chat::ChatSession) {
    let (viewer_name, chat) = (viewer_name.to_owned(), chat.clone());
    let _ = std::thread::Builder::new()
        .name("meshrmm-chat-reply".into())
        .spawn(move || {
            if !chat.available() {
                return;
            }
            match dialog::entry("MeshRMM chat", &format!("Reply to {viewer_name}:")) {
                Ok(Some(reply)) if !reply.trim().is_empty() => {
                    if !chat.send(reply) {
                        tracing::warn!("the chat reply was too long or the queue was full");
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(error = ?error, "could not ask for a chat reply"),
            }
        });
}

/// Dialogs from zenity, which GTK desktops ship, or kdialog on KDE.
pub(crate) mod dialog {
    use super::*;

    fn kde() -> bool {
        std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktop| desktop.contains("KDE"))
    }

    fn installed(program: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
        })
    }

    /// Which dialog program to use, KDE's own on KDE.
    fn program() -> Option<&'static str> {
        let order: [&str; 2] = if kde() {
            ["kdialog", "zenity"]
        } else {
            ["zenity", "kdialog"]
        };
        order.into_iter().find(|program| installed(program))
    }

    /// Starts a question with Accept and Deny buttons; it exits with 0 for
    /// Accept.
    pub(crate) fn question(
        title: &str,
        text: &str,
        accept: &str,
        deny: &str,
    ) -> anyhow::Result<Child> {
        let mut command = match program().context("this desktop has neither zenity nor kdialog")? {
            "kdialog" => {
                let mut command = Command::new("kdialog");
                command.args([
                    "--title",
                    title,
                    "--yes-label",
                    accept,
                    "--no-label",
                    deny,
                    "--yesno",
                    text,
                ]);
                command
            }
            _ => {
                let mut command = Command::new("zenity");
                command.args([
                    "--question",
                    "--no-markup",
                    "--title",
                    title,
                    "--text",
                    text,
                    "--ok-label",
                    accept,
                    "--cancel-label",
                    deny,
                ]);
                command
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("could not show the dialog")
    }

    /// Asks for a line of text; `None` when the user cancels.
    pub(crate) fn entry(title: &str, text: &str) -> anyhow::Result<Option<String>> {
        let output = match program().context("this desktop has neither zenity nor kdialog")? {
            "kdialog" => Command::new("kdialog")
                .args(["--title", title, "--inputbox", text])
                .stdin(Stdio::null())
                .output(),
            _ => Command::new("zenity")
                .args(["--entry", "--title", title, "--text", text])
                .stdin(Stdio::null())
                .output(),
        }
        .context("could not show the dialog")?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&output.stdout)
                .trim_end_matches('\n')
                .to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_bodies_are_escaped() {
        assert_eq!(escape("<b>Ada & co</b>"), "&lt;b&gt;Ada &amp; co&lt;/b&gt;");
    }
}
