use super::*;
use meshrmm_protocol::RunAs;

/// A stand-in for a 12.5-pixel UI font.
fn measure(text: &str) -> f64 {
    text.chars().count() as f64 * 7.0
}

fn state() -> State {
    State {
        sessions: vec!["Console".into(), "probe (RDP 2)".into()],
        session: 0,
        displays: vec!["Display 1".into(), "Display 2".into()],
        display: 1,
        pointer_display: Some(0),
        chroma: Some((ChromaMode::Yuv420, true)),
        chat_available: true,
        annotation_available: true,
        caption: Some(false),
        ..State::default()
    }
}

fn find(items: &[Item], action: Action) -> Option<&Item> {
    items.iter().find(|item| item.action == action)
}

#[test]
fn items_are_icons_with_short_labels() {
    let items = items(&state());
    let labels: Vec<_> = items
        .iter()
        .filter_map(|item| item.label.as_deref())
        .collect();
    assert_eq!(labels, ["Console", "2"]);
    assert!(items.iter().all(|item| !item.tooltip.is_empty()));
    let single = items_for(|state| {
        state.sessions.truncate(1);
        state.displays.truncate(1);
        state.display = 0;
    });
    let user = find(&single, Action::User).unwrap();
    assert_eq!(user.label, None);
    assert!(!user.enabled && !user.chevron);
    assert_eq!(user.tooltip, "User session: Console");
    let display = find(&single, Action::Display).unwrap();
    assert_eq!(display.label.as_deref(), Some("1"));
    assert!(!display.enabled);
}

fn items_for(change: impl FnOnce(&mut State)) -> Vec<Item> {
    let mut state = state();
    change(&mut state);
    items(&state)
}

#[test]
fn items_follow_the_session_state() {
    let items = items_for(|state| {
        state.recording = true;
        state.diagnostics = true;
        state.chat_unread = 2;
        state.credentials.can_autofill = true;
        state.credentials.message = "Waiting for the remote user…".into();
        state.input_blocked = true;
    });
    let recording = find(&items, Action::Recording).unwrap();
    assert_eq!(recording.label.as_deref(), Some("REC"));
    assert_eq!(style(recording, false, false).foreground, RED);
    assert!(find(&items, Action::Diagnostics).unwrap().active);
    let chat = find(&items, Action::Chat).unwrap();
    assert_eq!(chat.badge, Some(Badge::Unread));
    assert_eq!(chat.tooltip, "Chat: 2 unread messages");
    let key = find(&items, Action::Credentials).unwrap();
    assert_eq!(key.badge, Some(Badge::Attention));
    assert_eq!(key.tooltip, "Credentials: Waiting for the remote user…");
    for action in [
        Action::Credentials,
        Action::SecureAttention,
        Action::TypeClipboard,
    ] {
        assert!(!find(&items, action).unwrap().enabled, "{action:?}");
    }
    // Annotations are not input.
    assert!(find(&items, Action::Annotate).unwrap().enabled);
    assert!(find(&items_for(|_| {}), Action::Recording).is_none());
    let maximized = items_for(|state| state.caption = Some(true));
    assert_eq!(
        find(&maximized, Action::Maximize).unwrap().icon,
        Icon::Restore
    );
    assert!(find(&items_for(|state| state.caption = None), Action::Close).is_none());
}

#[test]
fn annotating_is_a_toggle_the_background_desktop_disables() {
    let off = items_for(|_| {});
    let annotate = find(&off, Action::Annotate).unwrap();
    assert!(annotate.enabled && !annotate.active && !annotate.menu);
    assert_eq!(annotate.icon, Icon::Pen);
    let on = items_for(|state| state.annotating = true);
    let annotate = find(&on, Action::Annotate).unwrap();
    assert!(annotate.active);
    assert_eq!(annotate.tooltip, "Stop annotating and erase the drawing");
    let background = items_for(|state| state.annotation_available = false);
    assert!(!find(&background, Action::Annotate).unwrap().enabled);
}

#[test]
fn layout_places_groups_without_overlap() {
    let items = items_for(|state| state.recording = true);
    let layout = layout(&items, 1000.0, 0.0, &measure);
    assert_eq!(layout.rects.len(), items.len());
    let mut sorted: Vec<_> = layout.rects.clone();
    sorted.sort_by(|a, b| a.x.total_cmp(&b.x));
    for pair in sorted.windows(2) {
        assert!(pair[0].right() <= pair[1].x, "{pair:?}");
    }
    // The caption buttons fill the top-right corner.
    let close = layout.rects[items.len() - 1];
    assert_eq!(
        (close.right(), close.y, close.height),
        (1000.0, 0.0, HEIGHT)
    );
    // One line between each pair of trailing groups before the captions.
    assert_eq!(layout.separators.len(), 3);
    // Each line is centered in the gap between two items.
    for x in &layout.separators {
        let left = sorted
            .iter()
            .filter(|rect| rect.right() <= *x)
            .map(|rect| rect.right());
        let right = sorted.iter().filter(|rect| rect.x >= *x).map(|rect| rect.x);
        let (left, right) = (
            left.fold(f64::MIN, f64::max),
            right.fold(f64::MAX, f64::min),
        );
        assert!(
            ((x - left) - (right - x)).abs() < 0.01,
            "{left} {x} {right}"
        );
    }
    for rect in &layout.rects {
        assert!(rect.y >= 0.0 && rect.bottom() <= HEIGHT);
    }
    assert_eq!(layout.hit(close.x + 1.0, 1.0), Some(items.len() - 1));
    // The space between the two sides moves the window.
    assert_eq!(layout.hit(300.0, 20.0), None);
}

#[test]
fn layout_leaves_room_for_the_macos_window_buttons() {
    let items = items_for(|state| state.caption = None);
    let layout = layout(&items, 900.0, 84.0, &measure);
    assert!(layout.rects[0].x >= 84.0);
    assert!(layout.rects.last().unwrap().right() <= 900.0 - EDGE);
}

#[test]
fn minimum_width_fits_every_item() {
    let items = items_for(|state| state.recording = true);
    let width = minimum_width(&items, 0.0, &measure);
    assert!(width < 850.0, "{width}");
    let layout = layout(&items, width, 0.0, &measure);
    let mut sorted: Vec<_> = layout.rects.clone();
    sorted.sort_by(|a, b| a.x.total_cmp(&b.x));
    for pair in sorted.windows(2) {
        assert!(pair[0].right() <= pair[1].x, "{pair:?}");
    }
}

#[test]
fn long_labels_fit_at_both_platform_minimum_widths() {
    for (width, inset, caption) in [(730.0, 84.0, None), (834.0, 0.0, Some(false))] {
        for session in [
            "administrator (RDP 2)",
            "非常に長いユーザー名".repeat(20).as_str(),
        ] {
            let state = State {
                sessions: vec!["Console".into(), session.into()],
                session: 1,
                displays: vec!["VeryLongDisplayName".repeat(20), "Display 2".into()],
                recording: true,
                caption,
                ..State::default()
            };
            let items = items(&state);
            let fitted = layout(&items, width, inset, &measure);
            for (index, rect) in fitted.rects.iter().enumerate() {
                assert!(rect.x >= inset && rect.right() <= width);
                assert_eq!(fitted.hit(rect.x + rect.width / 2.0, 20.0), Some(index));
                for other in &fitted.rects[index + 1..] {
                    assert!(rect.right() <= other.x || other.right() <= rect.x);
                }
                let text_width = fitted.labels[index].as_deref().map_or(0.0, &measure);
                assert!(item_width(&items[index], text_width) <= rect.width);
            }
            assert_eq!(items[0].label.as_deref(), Some(session));
            assert!(fitted.labels[0].as_ref().unwrap().len() < session.len());
            let recording = items
                .iter()
                .position(|i| i.action == Action::Recording)
                .unwrap();
            assert_eq!(fitted.labels[recording].as_deref(), Some("REC"));
        }
    }
}

#[test]
fn fitting_labels_preserves_unicode_and_full_labels_when_space_allows() {
    assert_eq!(fit_label("日本語の名前", 28.0, &measure), "日本語…");
    assert_eq!(fit_label("日本語", 21.0, &measure), "日本語");
    assert_eq!(fit_label("日本語", 0.0, &measure), "");
    let items = items(&state());
    assert_eq!(
        layout(&items, 1400.0, 0.0, &measure).labels,
        items.iter().map(|i| i.label.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn parts_fit_inside_their_item() {
    let items = items(&state());
    let layout = layout(&items, 1000.0, 0.0, &measure);
    for (item, rect) in items.iter().zip(&layout.rects) {
        let text = item.label.as_deref().map_or(0.0, measure);
        let parts = parts(item, *rect, text);
        for part in [Some(parts.icon), parts.label, parts.chevron]
            .into_iter()
            .flatten()
        {
            assert!(
                part.x >= rect.x && part.right() <= rect.right() + 0.01,
                "{item:?}"
            );
        }
    }
}

#[test]
fn styles_show_hover_press_and_disabled_states() {
    let items = items(&state());
    let chat = find(&items, Action::Chat).unwrap();
    assert_eq!(style(chat, false, false).background, None);
    assert_eq!(style(chat, true, false).background, Some(HOVER));
    assert_eq!(style(chat, true, true).background, Some(PRESSED));
    // Pressed, then dragged off the item.
    assert_eq!(style(chat, false, true).background, None);
    let close = find(&items, Action::Close).unwrap();
    assert_eq!(style(close, true, false).background, Some(CLOSE_HOVER));
    assert_eq!(style(close, true, false).foreground, WHITE);
    assert_eq!(style(close, true, false).radius, 0.0);
    let user = find(&items_for(|state| state.sessions.truncate(1)), Action::User)
        .unwrap()
        .clone();
    assert_eq!(style(&user, true, true).background, None);
    assert_eq!(style(&user, true, true).foreground, DISABLED);
}

#[test]
fn icons_stay_inside_the_grid() {
    let all = [
        Icon::User,
        Icon::Display,
        Icon::Quality,
        Icon::Record,
        Icon::Key,
        Icon::Keyboard,
        Icon::Clipboard,
        Icon::Pen,
        Icon::Folder,
        Icon::Chat,
        Icon::Pulse,
        Icon::Gear,
        Icon::Minimize,
        Icon::Maximize,
        Icon::Restore,
        Icon::Close,
        Icon::Chevron,
    ];
    for icon in all {
        let shapes = super::icon(icon);
        assert!(!shapes.is_empty());
        for shape in shapes {
            assert!(matches!(shape.segments.first(), Some(Segment::Move(..))));
            for segment in shape.segments {
                let points: Vec<(f64, f64)> = match segment {
                    Segment::Move(x, y) | Segment::Line(x, y) => vec![(x, y)],
                    Segment::Cubic(ax, ay, bx, by, x, y) => vec![(ax, ay), (bx, by), (x, y)],
                    Segment::Close => vec![],
                };
                for (x, y) in points {
                    assert!(
                        (0.5..=19.5).contains(&x) && (0.5..=19.5).contains(&y),
                        "{icon:?}: ({x}, {y})"
                    );
                }
            }
        }
    }
}

#[test]
fn circles_are_round() {
    let segments = circle(10.0, 10.0, 5.0);
    assert_eq!(segments.len(), 6);
    let Segment::Cubic(_, _, _, _, x, y) = segments[1] else {
        panic!("{segments:?}");
    };
    assert!(
        (x - 10.0).abs() < 1e-9 && (y - 15.0).abs() < 1e-9,
        "({x}, {y})"
    );
    let (placed, paint) = place(&stroke(segments), Rect::new(100.0, 10.0, 40.0, 40.0));
    assert_eq!(placed[0], Segment::Move(130.0, 30.0));
    assert_eq!(paint, Paint::Stroke(3.0));
}

#[test]
fn power_waits_for_the_agent_and_offers_both_restarts() {
    let unavailable = items_for(|_| {});
    let power = find(&unavailable, Action::Power).unwrap();
    assert!(!power.enabled && power.menu && power.label.is_none());
    assert!(menu(Action::Power, &state()).is_empty());

    // Restarting is not input, so blocking the technician's input leaves it.
    let normal = State {
        power: Some(false),
        input_blocked: true,
        ..state()
    };
    let power = find(&items(&normal), Action::Power).cloned().unwrap();
    assert!(power.enabled && power.label.is_none());
    assert_eq!(power.icon, Icon::Power);
    assert_eq!(
        commands(&menu(Action::Power, &normal)),
        [
            Some(Command::Restart { safe_mode: false }),
            Some(Command::Restart { safe_mode: true })
        ]
    );

    let safe = State {
        power: Some(true),
        ..state()
    };
    let power = find(&items(&safe), Action::Power).cloned().unwrap();
    assert_eq!(power.label.as_deref(), Some("Safe Mode"));
    let entries = menu(Action::Power, &safe);
    assert_eq!(
        commands(&entries),
        [
            None,
            None,
            Some(Command::Restart { safe_mode: false }),
            Some(Command::Restart { safe_mode: true })
        ]
    );
    assert!(matches!(
        &entries[2],
        MenuEntry::Item { label, .. } if label == "Restart normally…"
    ));
    for safe_mode in [false, true] {
        let (title, detail, button) = restart_confirmation(safe_mode);
        assert!(title.ends_with('?') && !detail.contains("  ") && !button.is_empty());
        assert_eq!(title.contains("Safe Mode"), safe_mode);
    }
}

#[test]
fn macs_have_no_ctrl_alt_del_or_safe_mode() {
    let windows = State {
        power: Some(false),
        ..state()
    };
    assert!(find(&items(&windows), Action::SecureAttention).is_some());
    let mac = State {
        power: Some(false),
        device_is_mac: true,
        ..state()
    };
    let items = items(&mac);
    assert!(find(&items, Action::SecureAttention).is_none());
    assert_eq!(
        find(&items, Action::Power).unwrap().tooltip,
        "Restart the remote Mac"
    );
    assert_eq!(
        commands(&menu(Action::Power, &mac)),
        [Some(Command::Restart { safe_mode: false })]
    );
}

#[test]
fn menus_list_choices_and_status() {
    let state = State {
        credentials: CredentialState {
            available: true,
            saved: true,
            prompt_active: false,
            can_autofill: true,
            message: "Saved".into(),
        },
        file_status: "Sent 2 files".into(),
        quality: QualityPreset::BestQuality,
        ..state()
    };
    let labels = |entries: Vec<MenuEntry>| -> Vec<(String, bool, bool)> {
        entries
            .into_iter()
            .map(|entry| match entry {
                MenuEntry::Item {
                    label,
                    checked,
                    enabled,
                    ..
                } => (label, checked, enabled),
                MenuEntry::Separator => ("-".into(), false, false),
                MenuEntry::Submenu { label, .. } => (format!("{label} ▸"), false, true),
            })
            .collect()
    };
    assert_eq!(
        labels(menu(Action::Display, &state)),
        [
            ("➤ Display 1".into(), false, true),
            ("Display 2".into(), true, true)
        ]
    );
    let quality = menu(Action::Quality, &state);
    assert_eq!(quality.len(), 7);
    assert_eq!(
        commands(&quality)[3],
        Some(Command::Quality(QualityPreset::BestQuality))
    );
    assert_eq!(labels(quality)[3], ("Best quality".into(), true, true));
    assert!(menu(Action::Quality, &State::default()).len() == 4);
    let credentials = menu(Action::Credentials, &state);
    assert_eq!(
        commands(&credentials),
        [
            Some(Command::AutofillCredentials),
            Some(Command::PromptCredentials),
            Some(Command::ForgetCredentials)
        ]
    );
    assert!(labels(credentials).iter().all(|(_, _, enabled)| *enabled));
    let no_prompt = State {
        credentials: CredentialState {
            can_autofill: false,
            ..state.credentials.clone()
        },
        ..state.clone()
    };
    assert_eq!(
        labels(menu(Action::Credentials, &no_prompt))[0],
        ("Autofill saved credentials".into(), false, false)
    );
    let blocked = State {
        input_blocked: true,
        ..state.clone()
    };
    assert!(
        labels(menu(Action::Credentials, &blocked))
            .iter()
            .all(|(_, _, enabled)| !enabled)
    );
    assert_eq!(
        labels(menu(Action::Files, &state)).last().unwrap().0,
        "Sent 2 files"
    );
    assert!(menu(Action::Settings, &state).is_empty());
}

fn listing() -> crate::toolbox::Snapshot {
    use meshrmm_protocol::{ScriptLanguage, ToolboxFile, ToolboxListing, ToolboxScript};
    let script = |id: &str, name: &str, folder: &str| ToolboxScript {
        id: id.into(),
        name: name.into(),
        folder: folder.into(),
        language: ScriptLanguage::Powershell,
        description: String::new(),
        shared: true,
    };
    crate::toolbox::Snapshot {
        available: true,
        listing: Some(ToolboxListing {
            scripts: vec![
                script("top", "Top level", ""),
                script("disk", "Disk report", "Maintenance/Disk"),
                script("temp", "Clear temp", "Maintenance"),
            ],
            files: vec![ToolboxFile {
                id: "setup".into(),
                name: "setup.exe".into(),
                folder: "Installers".into(),
                size_bytes: 3 * 1024 * 1024 / 2,
                shared: false,
            }],
        }),
        status: "Saved C:\\Users\\ada\\Documents\\setup.exe".into(),
        ..Default::default()
    }
}

/// The labels of `entries`, with submenus' entries indented below them.
fn outline(entries: &[MenuEntry], depth: usize, lines: &mut Vec<String>) {
    for entry in entries {
        let indent = "  ".repeat(depth);
        match entry {
            MenuEntry::Item { label, command, .. } => lines.push(format!(
                "{indent}{label}{}",
                if command.is_some() { "" } else { " (status)" }
            )),
            MenuEntry::Separator => lines.push(format!("{indent}-")),
            MenuEntry::Submenu { label, entries } => {
                lines.push(format!("{indent}{label} >"));
                outline(entries, depth + 1, lines);
            }
        }
    }
}

#[test]
fn the_toolbox_menu_nests_folders_and_offers_both_accounts() {
    let entries = toolbox_menu(&listing());
    let mut lines = Vec::new();
    outline(&entries, 0, &mut lines);
    assert_eq!(
        lines,
        [
            "Run a script (status)",
            "Maintenance >",
            "  Disk >",
            "    Disk report >",
            "      As the signed-in user",
            "      As SYSTEM",
            "  Clear temp >",
            "    As the signed-in user",
            "    As SYSTEM",
            "Top level >",
            "  As the signed-in user",
            "  As SYSTEM",
            "-",
            "Send a file to Documents (status)",
            "Installers >",
            "  setup.exe (1.5 MiB)",
            "-",
            "Refresh the toolbox",
            "Saved C:\\Users\\ada\\Documents\\setup.exe (status)",
        ]
    );
    // Commands are numbered in that order, submenus included, and name
    // items by their place in the listing.
    let commands = commands(&entries);
    assert_eq!(commands.len(), lines.len());
    assert_eq!(
        commands[4],
        Some(Command::RunScript {
            index: 1,
            run_as: RunAs::User
        })
    );
    assert_eq!(
        commands[8],
        Some(Command::RunScript {
            index: 2,
            run_as: RunAs::System
        })
    );
    assert_eq!(
        commands[11],
        Some(Command::RunScript {
            index: 0,
            run_as: RunAs::System
        })
    );
    assert_eq!(commands[15], Some(Command::SendToolboxFile(0)));
    assert_eq!(commands[17], Some(Command::RefreshToolbox));
    assert_eq!(commands[3], None, "a submenu is not a command");
}

#[test]
fn the_toolbox_menu_explains_an_empty_or_missing_toolbox() {
    let mut empty = listing();
    empty.listing = Some(Default::default());
    empty.status.clear();
    let mut lines = Vec::new();
    outline(&toolbox_menu(&empty), 0, &mut lines);
    assert_eq!(
        lines,
        [
            "Run a script (status)",
            "No scripts yet. Add them in the dashboard's Toolbox. (status)",
            "-",
            "Send a file to Documents (status)",
            "No files yet. Upload them in the dashboard's Toolbox. (status)",
            "-",
            "Refresh the toolbox",
        ]
    );
    let failed = crate::toolbox::Snapshot {
        available: true,
        error: Some("the remote session has ended".into()),
        ..Default::default()
    };
    let mut lines = Vec::new();
    outline(&toolbox_menu(&failed), 0, &mut lines);
    assert_eq!(
        lines[0],
        "The toolbox could not be loaded: the remote session has ended (status)"
    );
    let loading = crate::toolbox::Snapshot {
        available: true,
        loading: true,
        ..Default::default()
    };
    let entries = toolbox_menu(&loading);
    assert!(
        matches!(&entries[0], MenuEntry::Item { label, .. } if label == "Loading the toolbox…")
    );
    assert!(
        matches!(entries.last(), Some(MenuEntry::Item { enabled: false, .. })),
        "a refresh waits for the one running"
    );
}

#[test]
fn the_toolbox_item_shows_progress_and_waits_for_the_session() {
    let item = |state: &State| {
        items(state)
            .into_iter()
            .find(|item| item.action == Action::Toolbox)
            .unwrap()
    };
    let mut state = state();
    let unavailable = item(&state);
    assert!(!unavailable.enabled);
    state.toolbox_available = true;
    state.toolbox_busy = true;
    state.toolbox_status = "Running Disk report…".into();
    let busy = item(&state);
    assert!(busy.enabled && busy.active && busy.menu);
    assert_eq!(busy.tooltip, "Toolbox: Running Disk report…");
}
