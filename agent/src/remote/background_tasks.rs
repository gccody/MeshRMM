//! Windows 10-style Task Manager for the private Session 0 desktop.
//! Native controls/GDI remain capturable without an interactive shell or GPU.
mod data;
mod paint;
mod telemetry;
mod theme;
mod window;

use paint::*;
use window::*;
// The file browser shares the Task Manager's list scrollbars.
pub(super) use window::install_list_scrollbars;
pub(super) fn stop_telemetry(pid: u32) {
    telemetry::stop(pid);
}

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::mpsc;

use crate::win32::{OwnedHandle, wide};
use anyhow::{Context, ensure};
use data::{Process, Snapshot};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::RemoteDesktop::WTSDisconnectSession;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};

const LIST: usize = 101;
const REFRESH: usize = 102;
const END_TASK: usize = 103;
const TABS: usize = 104;
const COMPACT: usize = 105;
const CANCEL: usize = 106;
const RUN_EDIT: usize = 107;
const RUN: usize = 108;
const RUN_TASK: usize = 201;
const EXIT: usize = 202;
const TOPMOST: usize = 203;
const SPEED_HIGH: usize = 210;
const SPEED_NORMAL: usize = 211;
const SPEED_LOW: usize = 212;
const SPEED_PAUSED: usize = 213;
const GROUP: usize = 214;
const GO_DETAILS: usize = 220;
const END_TREE: usize = 221;
const COPY: usize = 222;
const PRIORITY_NORMAL: usize = 230;
const PRIORITY_LOW: usize = 231;
const PRIORITY_HIGH: usize = 232;
const RESET_HISTORY: usize = 240;
const SERVICE_START: usize = 241;
const SERVICE_STOP: usize = 242;
const SERVICE_RESTART: usize = 243;
const GRAPH: usize = 250;
const HEADER: usize = 251;
const TAB_NAMES: [&str; 7] = [
    "Processes",
    "Performance",
    "App history",
    "Startup",
    "Users",
    "Details",
    "Services",
];
const WHITE: COLORREF = COLORREF(0xffffff);
const BLUE: COLORREF = COLORREF(0xa75b00);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Processes,
    Performance,
    History,
    Startup,
    Users,
    Details,
    Services,
}
impl Tab {
    fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Performance,
            2 => Self::History,
            3 => Self::Startup,
            4 => Self::Users,
            5 => Self::Details,
            6 => Self::Services,
            _ => Self::Processes,
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Processes => 0,
            Self::Performance => 1,
            Self::History => 2,
            Self::Startup => 3,
            Self::Users => 4,
            Self::Details => 5,
            Self::Services => 6,
        }
    }
    fn columns(self) -> Vec<(&'static str, i32)> {
        match self {
            Self::Processes => vec![
                ("Name", 248),
                ("Status", 85),
                ("CPU", 74),
                ("Memory", 74),
                ("Disk", 74),
                ("Network", 74),
            ],
            Self::History => vec![
                ("Name", 250),
                ("CPU time", 120),
                ("I/O bytes", 130),
                ("Processes", 100),
            ],
            Self::Startup => vec![
                ("Name", 230),
                ("User / scope", 170),
                ("Status", 100),
                ("Startup impact", 110),
                ("Command line", 500),
            ],
            Self::Users => vec![
                ("User", 250),
                ("ID", 70),
                ("Status", 115),
                ("CPU", 85),
                ("Memory", 110),
            ],
            Self::Details => vec![
                ("Name", 210),
                ("PID", 70),
                ("Status", 85),
                ("User name", 130),
                ("CPU", 75),
                ("Memory (working set)", 145),
                ("Threads", 75),
                ("Handles", 80),
                ("I/O", 100),
                ("Session ID", 90),
                ("Base priority", 100),
                ("Image path name", 450),
            ],
            Self::Services => vec![
                ("Name", 190),
                ("PID", 70),
                ("Description", 350),
                ("Status", 110),
            ],
            Self::Performance => vec![],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Process(u32, Option<u64>),
    Group(String),
    Section(u8),
    Service(String),
    Startup(String, String, String),
    User(u32),
    History(String),
}
#[derive(Clone)]
struct Row {
    key: Key,
    cells: Vec<String>,
    processes: Vec<usize>,
    section: bool,
    expandable: bool,
    indent: i32,
    heat: Vec<f64>,
}
impl Row {
    fn plain(key: Key, cells: Vec<String>) -> Self {
        Self {
            key,
            cells,
            processes: Vec::new(),
            section: false,
            expandable: false,
            indent: 0,
            heat: Vec::new(),
        }
    }
}
#[derive(Clone, Default)]
struct History {
    cpu: u64,
    io: u64,
    instances: HashSet<(u32, Option<u64>)>,
}
fn percent(value: Option<f64>) -> String {
    value.map_or_else(
        || "—".into(),
        |v| {
            if v < 0.05 {
                "0%".into()
            } else {
                format!("{v:.1}%")
            }
        },
    )
}
fn memory(value: Option<usize>) -> String {
    value.map_or_else(|| "—".into(), |v| format!("{:.1} MB", v as f64 / 1048576.0))
}
fn process_key(p: &Process) -> Key {
    Key::Process(p.pid, p.created)
}
fn priority(value: u32) -> &'static str {
    match value {
        0x40 => "Low",
        0x4000 => "Below normal",
        0x20 => "Normal",
        0x8000 => "Above normal",
        0x80 => "High",
        0x100 => "Realtime",
        _ => "—",
    }
}

// The action captures identities/handles before confirmation. Refreshing or
// selecting another row cannot redirect it to a different process or service.
enum Pending {
    End(Vec<OwnedHandle>),
    Service(String, data::ServiceAction),
    Startup(data::Startup),
    Disconnect(u32),
    Priority(Process, u32),
}
struct State {
    hwnd: HWND,
    menu: HMENU,
    list: HWND,
    tabs: HWND,
    status: HWND,
    header: HWND,
    graph: HWND,
    font: HFONT,
    images: HIMAGELIST,
    heading_font: HFONT,
    icon_indices: HashMap<String, i32>,
    rows: Vec<Row>,
    snapshot: Snapshot,
    receiver: Option<mpsc::Receiver<anyhow::Result<Snapshot>>>,
    action: Option<mpsc::Receiver<anyhow::Result<String>>>,
    pending: Option<Pending>,
    notice: String,
    tab: Tab,
    compact: bool,
    resizing: Option<(u32, POINT, RECT)>,
    expanded_size: (i32, i32),
    grouped: bool,
    expanded: HashSet<Key>,
    sort: usize,
    descending: bool,
    interval: u32,
    tick: u32,
    run_visible: bool,
    topmost: bool,
    samples: VecDeque<[f64; 4]>,
    performance: usize,
    history: BTreeMap<String, History>,
    history_baseline: HashMap<(u32, Option<u64>), (u64, u64)>,
}
impl State {
    fn refresh(&mut self) {
        if self.receiver.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        let previous = self.snapshot.clone();
        let inventory = self.tick.is_multiple_of(3)
            || matches!(self.tab, Tab::Services | Tab::Startup | Tab::Users);
        self.tick = self.tick.wrapping_add(1);
        std::thread::spawn(move || {
            let _ = sender.send(data::sample(&previous, inventory));
        });
    }
    fn poll(&mut self) -> anyhow::Result<()> {
        if let Some(receiver) = &self.action {
            match receiver.try_recv() {
                Ok(result) => {
                    self.action = None;
                    self.notice = result.unwrap_or_else(|e| format!("{e:#}"));
                    self.tick = 0;
                    self.refresh();
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(e) => {
                    self.action = None;
                    self.notice = e.to_string();
                }
            }
        }
        let Some(receiver) = &self.receiver else {
            return Ok(());
        };
        let result = match receiver.try_recv() {
            Ok(v) => v,
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
            Err(e) => Err(e.into()),
        };
        self.receiver = None;
        let next = result?;
        for p in &next.processes {
            let identity = (p.pid, p.created);
            if p.created.is_none() {
                continue;
            }
            let current = (p.cpu_time, p.io_bytes.unwrap_or(0));
            let baseline = self
                .history_baseline
                .insert(identity, current)
                .unwrap_or(current);
            let entry = self.history.entry(p.description.clone()).or_default();
            entry.cpu = entry
                .cpu
                .saturating_add(current.0.saturating_sub(baseline.0));
            entry.io = entry
                .io
                .saturating_add(current.1.saturating_sub(baseline.1));
            entry.instances.insert(identity);
        }
        let alive: HashSet<_> = next.processes.iter().map(|p| (p.pid, p.created)).collect();
        self.history_baseline
            .retain(|identity, _| alive.contains(identity));
        let used = if next.memory_total > 0 {
            100.0 * (next.memory_total - next.memory_available) as f64 / next.memory_total as f64
        } else {
            0.0
        };
        self.samples.push_back([
            next.cpu.unwrap_or(0.0),
            used,
            next.disk.unwrap_or(0.0),
            next.network_rate.unwrap_or(0.0),
        ]);
        while self.samples.len() > 60 {
            self.samples.pop_front();
        }
        if self.notice.is_empty() && !next.errors.is_empty() {
            self.notice = next.errors.join("; ");
        }
        self.snapshot = next;
        self.rebuild();
        Ok(())
    }
    fn selected(&self) -> Option<&Row> {
        let index = unsafe {
            SendMessageW(
                self.list,
                LVM_GETNEXTITEM,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM(LVNI_SELECTED as isize)),
            )
        }
        .0;
        self.rows.get(index as usize).filter(|r| !r.section)
    }
    fn process_row(
        &self,
        indices: Vec<usize>,
        key: Key,
        label: String,
        indent: i32,
        expandable: bool,
    ) -> Row {
        let processes: Vec<_> = indices
            .iter()
            .map(|i| &self.snapshot.processes[*i])
            .collect();
        let cpu = processes
            .iter()
            .map(|p| p.cpu)
            .collect::<Option<Vec<_>>>()
            .map(|v| v.iter().sum());
        let mem = processes
            .iter()
            .map(|p| p.memory)
            .collect::<Option<Vec<_>>>()
            .map(|v| v.iter().sum());
        let disk = processes
            .iter()
            .map(|p| p.disk_rate)
            .collect::<Option<Vec<_>>>()
            .map(|v| v.iter().sum::<f64>());
        let network = processes
            .iter()
            .map(|p| p.network_rate)
            .collect::<Option<Vec<_>>>()
            .map(|v| v.iter().sum::<f64>());
        Row {
            key,
            cells: vec![
                label,
                if processes.iter().any(|p| p.hung) {
                    "Not responding".into()
                } else {
                    String::new()
                },
                percent(cpu),
                memory(mem),
                disk.map_or_else(|| "—".into(), |v| format!("{:.1} MB/s", v / 1048576.0)),
                network.map_or_else(|| "—".into(), |v| format!("{:.1} Mbps", v * 8.0 / 1e6)),
            ],
            processes: indices,
            section: false,
            expandable,
            indent,
            heat: vec![
                0.0,
                0.0,
                cpu.unwrap_or(0.0) / 100.0,
                mem.unwrap_or(0) as f64 / self.snapshot.memory_total.max(1) as f64,
                disk.unwrap_or(0.0) / self.snapshot.disk_rate.unwrap_or(1.0).max(1.0),
                network.unwrap_or(0.0) * 8.0 / self.snapshot.network_capacity.max(1) as f64,
            ],
        }
    }
    fn make_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        match self.tab {
            Tab::Processes => {
                let mut visible = visible_processes();
                loop {
                    let before = visible.len();
                    for p in &self.snapshot.processes {
                        if visible.contains(&p.parent)
                            && self.snapshot.processes.iter().any(|parent| {
                                parent.pid == p.parent
                                    && parent.created.zip(p.created).is_some_and(|(a, b)| a <= b)
                            })
                        {
                            visible.insert(p.pid);
                        }
                    }
                    if before == visible.len() {
                        break;
                    }
                }
                let mut groups: BTreeMap<(u8, String), Vec<usize>> = BTreeMap::new();
                let system = crate::win32::windows_directory()
                    .map(|directory| directory.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "C:\\Windows".into())
                    .to_lowercase();
                for (index, p) in self.snapshot.processes.iter().enumerate() {
                    if p.pid == 0 {
                        continue;
                    }
                    let category = if visible.contains(&p.pid) {
                        0
                    } else if p.path.to_lowercase().starts_with(&format!("{system}\\"))
                        || p.pid == 4
                    {
                        2
                    } else {
                        1
                    };
                    if self.compact && category != 0 {
                        continue;
                    }
                    let category = if self.grouped { category } else { 0 };
                    groups
                        .entry((category, p.description.clone()))
                        .or_default()
                        .push(index);
                }
                for category in 0..3 {
                    let mut members = Vec::new();
                    for ((kind, name), indices) in &groups {
                        if *kind != category {
                            continue;
                        }
                        let key = if indices.len() > 1 {
                            Key::Group(format!("{kind}:{name}"))
                        } else {
                            process_key(&self.snapshot.processes[indices[0]])
                        };
                        let label = if indices.len() > 1 {
                            format!("{name} ({})", indices.len())
                        } else {
                            name.clone()
                        };
                        members.push(self.process_row(
                            indices.clone(),
                            key,
                            label,
                            0,
                            indices.len() > 1,
                        ));
                    }
                    self.sort_rows(&mut members);
                    if !members.is_empty() && self.grouped && !self.compact {
                        let title = ["Apps", "Background processes", "Windows processes"]
                            [category as usize];
                        let count: usize = members.iter().map(|r| r.processes.len()).sum();
                        let mut section =
                            Row::plain(Key::Section(category), vec![format!("{title} ({count})")]);
                        section.section = true;
                        rows.push(section);
                    }
                    for member in members {
                        let children = if member.expandable && self.expanded.contains(&member.key) {
                            member.processes.clone()
                        } else {
                            Vec::new()
                        };
                        rows.push(member);
                        for index in children {
                            let p = &self.snapshot.processes[index];
                            rows.push(self.process_row(
                                vec![index],
                                process_key(p),
                                format!("{} ({})", p.name, p.pid),
                                1,
                                false,
                            ));
                        }
                    }
                }
            }
            Tab::Details => {
                for (index, p) in self.snapshot.processes.iter().enumerate() {
                    let mut row = Row::plain(
                        process_key(p),
                        vec![
                            p.name.clone(),
                            p.pid.to_string(),
                            if p.hung { "Not responding" } else { "Running" }.into(),
                            p.user.clone(),
                            percent(p.cpu),
                            memory(p.memory),
                            p.threads.to_string(),
                            p.handles.to_string(),
                            p.io_rate.map_or_else(
                                || "—".into(),
                                |v| format!("{:.1} MB/s", v / 1048576.0),
                            ),
                            p.session.map_or_else(|| "—".into(), |v| v.to_string()),
                            priority(p.priority).into(),
                            p.path.clone(),
                        ],
                    );
                    row.processes.push(index);
                    rows.push(row);
                }
            }
            Tab::Services => {
                for s in &self.snapshot.services {
                    rows.push(Row::plain(
                        Key::Service(s.name.clone()),
                        vec![
                            s.name.clone(),
                            if s.pid == 0 {
                                String::new()
                            } else {
                                s.pid.to_string()
                            },
                            s.description.clone(),
                            s.status().into(),
                        ],
                    ));
                }
            }
            Tab::Startup => {
                for s in &self.snapshot.startups {
                    rows.push(Row::plain(
                        Key::Startup(s.root.clone(), s.run_key.clone(), s.name.clone()),
                        vec![
                            s.name.clone(),
                            s.location.clone(),
                            if s.enabled { "Enabled" } else { "Disabled" }.into(),
                            "Not measured".into(),
                            s.command.clone(),
                        ],
                    ));
                }
            }
            Tab::Users => {
                let mut members = Vec::new();
                for user in &self.snapshot.users {
                    let indices: Vec<_> = self
                        .snapshot
                        .processes
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| p.session == Some(user.id))
                        .map(|(i, _)| i)
                        .collect();
                    let cpu: f64 = indices
                        .iter()
                        .filter_map(|i| self.snapshot.processes[*i].cpu)
                        .sum();
                    let mem: usize = indices
                        .iter()
                        .filter_map(|i| self.snapshot.processes[*i].memory)
                        .sum();
                    let mut row = Row::plain(
                        Key::User(user.id),
                        vec![
                            format!("{} ({})", user.name, indices.len()),
                            user.id.to_string(),
                            user.status.clone(),
                            percent(Some(cpu)),
                            memory(Some(mem)),
                        ],
                    );
                    row.processes = indices.clone();
                    row.expandable = true;
                    members.push(row);
                }
                self.sort_rows(&mut members);
                for row in members {
                    let children = if self.expanded.contains(&row.key) {
                        row.processes.clone()
                    } else {
                        Vec::new()
                    };
                    rows.push(row);
                    {
                        for i in children {
                            let p = &self.snapshot.processes[i];
                            let mut row = Row::plain(
                                process_key(p),
                                vec![
                                    p.description.clone(),
                                    p.pid.to_string(),
                                    String::new(),
                                    percent(p.cpu),
                                    memory(p.memory),
                                ],
                            );
                            row.processes.push(i);
                            row.indent = 1;
                            rows.push(row);
                        }
                    }
                }
            }
            Tab::History => {
                for (name, h) in &self.history {
                    rows.push(Row::plain(
                        Key::History(name.clone()),
                        vec![
                            name.clone(),
                            format!(
                                "{}:{:02}:{:02}",
                                h.cpu / 36_000_000_000,
                                h.cpu / 600_000_000 % 60,
                                h.cpu / 10_000_000 % 60
                            ),
                            format!("{:.1} MB", h.io as f64 / 1048576.0),
                            h.instances.len().to_string(),
                        ],
                    ));
                }
            }
            Tab::Performance => {}
        }
        if !matches!(self.tab, Tab::Processes | Tab::Users) {
            self.sort_rows(&mut rows);
        }
        rows
    }
    fn sort_rows(&self, rows: &mut [Row]) {
        let numeric = match self.tab {
            Tab::Processes => self.sort >= 2,
            Tab::Details => matches!(self.sort, 1 | 4..=9),
            Tab::Services => self.sort == 1,
            Tab::History => self.sort >= 2,
            Tab::Users => matches!(self.sort, 1 | 3 | 4),
            _ => false,
        };
        rows.sort_by(|a, b| {
            let left = a.cells.get(self.sort).map(String::as_str).unwrap_or("");
            let right = b.cells.get(self.sort).map(String::as_str).unwrap_or("");
            let order = if self.tab == Tab::History && self.sort == 1 {
                fn seconds(s: &str) -> u64 {
                    s.split(':').fold(0u64, |n, part| {
                        n.saturating_mul(60)
                            .saturating_add(part.parse().unwrap_or(0))
                    })
                }
                seconds(left).cmp(&seconds(right))
            } else if numeric {
                fn number(s: &str) -> f64 {
                    s.trim_end_matches('%')
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .parse()
                        .unwrap_or(-1.0)
                }
                number(left).total_cmp(&number(right))
            } else {
                left.to_lowercase().cmp(&right.to_lowercase())
            };
            let order = if self.descending {
                order.reverse()
            } else {
                order
            };
            order.then_with(|| a.cells[0].cmp(&b.cells[0]))
        });
    }
    fn rebuild(&mut self) {
        let rows = self.make_rows();
        unsafe {
            let selected = self.selected().map(|r| r.key.clone());
            let top = SendMessageW(self.list, LVM_GETTOPINDEX, None, None)
                .0
                .max(0) as usize;
            let anchor = self.rows.get(top).map(|r| r.key.clone());
            let mut rect = RECT::default();
            SendMessageW(
                self.list,
                LVM_GETITEMRECT,
                Some(WPARAM(top)),
                Some(LPARAM((&mut rect as *mut RECT) as isize)),
            );
            let height = (rect.bottom - rect.top).max(0);
            SendMessageW(self.list, WM_SETREDRAW, Some(WPARAM(0)), None);
            if self.icon_indices.len() > 512 {
                let _ = ImageList_Remove(self.images, -1);
                self.icon_indices.clear();
                if let Ok(icon) = LoadIconW(None, IDI_APPLICATION) {
                    padded_icon(self.images, icon);
                }
            }
            for index in (rows.len()..self.rows.len()).rev() {
                SendMessageW(self.list, LVM_DELETEITEM, Some(WPARAM(index)), None);
            }
            for (index, row) in rows.iter().enumerate() {
                let image = if row.section {
                    I_IMAGENONE
                } else {
                    row.processes
                        .first()
                        .and_then(|i| {
                            self.snapshot.processes[*i]
                                .icon
                                .as_ref()
                                .map(|icon| (&self.snapshot.processes[*i].path, icon))
                        })
                        .map(|(path, icon)| {
                            *self.icon_indices.entry(path.clone()).or_insert_with(|| {
                                padded_icon(self.images, HICON(icon.0 as *mut _))
                            })
                        })
                        .filter(|i| *i >= 0)
                        .unwrap_or(0)
                };
                for column in 0..self.tab.columns().len().max(1) {
                    let text = row.cells.get(column).cloned().unwrap_or_default();
                    let text = if column == 0 && row.expandable {
                        format!(
                            "{}  {text}",
                            if self.expanded.contains(&row.key) {
                                "⌄"
                            } else {
                                "›"
                            }
                        )
                    } else {
                        text
                    };
                    let mut text = wide(&text);
                    let item = LVITEMW {
                        mask: LVIF_TEXT
                            | if column == 0 {
                                LVIF_INDENT | LVIF_IMAGE
                            } else {
                                LIST_VIEW_ITEM_FLAGS(0)
                            },
                        iImage: image,
                        iItem: index as i32,
                        iSubItem: column as i32,
                        pszText: PWSTR(text.as_mut_ptr()),
                        iIndent: if column == 0 { row.indent } else { 0 },
                        ..Default::default()
                    };
                    SendMessageW(
                        self.list,
                        if column == 0 && index >= self.rows.len() {
                            LVM_INSERTITEMW
                        } else {
                            LVM_SETITEMW
                        },
                        None,
                        Some(LPARAM((&item as *const LVITEMW) as isize)),
                    );
                }
            }
            let mut item = LVITEMW {
                stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                ..Default::default()
            };
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM((&item as *const LVITEMW) as isize)),
            );
            if let Some(index) = selected.and_then(|key| rows.iter().position(|r| r.key == key)) {
                item.state = item.stateMask;
                SendMessageW(
                    self.list,
                    LVM_SETITEMSTATE,
                    Some(WPARAM(index)),
                    Some(LPARAM((&item as *const LVITEMW) as isize)),
                );
            }
            let new_top = anchor
                .and_then(|key| rows.iter().position(|r| r.key == key))
                .unwrap_or(top);
            SendMessageW(
                self.list,
                LVM_SCROLL,
                None,
                Some(LPARAM((new_top as isize - top as isize) * height as isize)),
            );
            self.rows = rows;
            SendMessageW(self.list, WM_SETREDRAW, Some(WPARAM(1)), None);
            // Batched updates also invalidate the nonclient scrollbars. A client-only
            // invalidation leaves them blank in Session 0's captured window.
            let _ = RedrawWindow(
                Some(self.list),
                None,
                None,
                RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN,
            );
            if self.tab == Tab::Performance && !self.compact {
                let _ = ShowWindow(self.list, SW_HIDE);
            }
            let _ = InvalidateRect(Some(self.header), None, false);
            let _ = InvalidateRect(Some(self.graph), None, false);
        }
    }
    fn configure(&mut self) {
        unsafe {
            SendMessageW(self.list, LVM_DELETEALLITEMS, None, None);
            self.rows.clear();
            while SendMessageW(self.list, LVM_DELETECOLUMN, Some(WPARAM(0)), None).0 != 0 {}
            let columns = if self.compact {
                vec![("Name", 600)]
            } else {
                self.tab.columns()
            };
            for (index, (label, width)) in columns.iter().enumerate() {
                let mut text = wide(label);
                let column = LVCOLUMNW {
                    mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
                    cx: *width,
                    fmt: if self.tab == Tab::Processes && index >= 2 {
                        LVCFMT_RIGHT
                    } else {
                        LVCFMT_LEFT
                    },
                    pszText: PWSTR(text.as_mut_ptr()),
                    ..Default::default()
                };
                SendMessageW(
                    self.list,
                    LVM_INSERTCOLUMNW,
                    Some(WPARAM(index)),
                    Some(LPARAM((&column as *const LVCOLUMNW) as isize)),
                );
            }
            let style = GetWindowLongW(self.list, GWL_STYLE) as u32;
            let hide_header = self.tab == Tab::Processes || self.compact;
            SetWindowLongW(
                self.list,
                GWL_STYLE,
                if hide_header {
                    style | LVS_NOCOLUMNHEADER
                } else {
                    style & !LVS_NOCOLUMNHEADER
                } as i32,
            );
            let _ = SetWindowPos(
                self.list,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
            self.layout();
            self.rebuild();
        }
    }
    fn layout(&self) {
        unsafe {
            let mut rect = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rect);
            let (width, height) = (rect.right, rect.bottom);
            let _ = MoveWindow(self.tabs, 6, 1, width - 12, 25, true);
            let top = if self.compact { 0 } else { 27 };
            let note = !self.notice.is_empty() || self.pending.is_some() || self.run_visible;
            let footer = if note { 80 } else { 45 };
            let header = if self.tab == Tab::Processes && !self.compact {
                56
            } else {
                0
            };
            let _ = MoveWindow(self.header, 1, top, width - 2, header, true);
            let _ = MoveWindow(
                self.list,
                1,
                top + header,
                width - 2,
                (height - top - header - footer).max(1),
                true,
            );
            if self.compact {
                SendMessageW(
                    self.list,
                    LVM_SETCOLUMNWIDTH,
                    Some(WPARAM(0)),
                    Some(LPARAM((width - 20).max(100) as isize)),
                );
            }
            let _ = MoveWindow(
                self.graph,
                1,
                top,
                width - 2,
                (height - top - footer).max(1),
                true,
            );
            let _ = ShowWindow(self.tabs, if self.compact { SW_HIDE } else { SW_SHOW });
            let _ = ShowWindow(self.header, if header > 0 { SW_SHOW } else { SW_HIDE });
            let _ = ShowWindow(
                self.graph,
                if self.tab == Tab::Performance && !self.compact {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
            let _ = ShowWindow(
                self.list,
                if self.tab == Tab::Performance && !self.compact {
                    SW_HIDE
                } else {
                    SW_SHOW
                },
            );
            let _ = MoveWindow(
                self.status,
                12,
                height - footer + 7,
                (width - 115).max(1),
                30,
                true,
            );
            let _ = ShowWindow(
                self.status,
                if note && !self.run_visible {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
            for (id, x, w) in [
                (COMPACT, 10, 115),
                (REFRESH, width - 220, 95),
                (END_TASK, width - 115, 100),
                (CANCEL, width - 100, 85),
            ] {
                if let Ok(control) = GetDlgItem(Some(self.hwnd), id as i32) {
                    let _ = MoveWindow(
                        control,
                        x,
                        height - if id == CANCEL { 74 } else { 34 },
                        w,
                        24,
                        true,
                    );
                }
            }
            if let Ok(edit) = GetDlgItem(Some(self.hwnd), RUN_EDIT as i32) {
                let _ = MoveWindow(edit, 12, height - 72, width - 218, 25, true);
                let _ = ShowWindow(edit, if self.run_visible { SW_SHOW } else { SW_HIDE });
            }
            if let Ok(button) = GetDlgItem(Some(self.hwnd), RUN as i32) {
                let _ = MoveWindow(button, width - 196, height - 72, 85, 25, true);
                let _ = ShowWindow(button, if self.run_visible { SW_SHOW } else { SW_HIDE });
            }
            if let Ok(button) = GetDlgItem(Some(self.hwnd), CANCEL as i32) {
                let _ = ShowWindow(button, if note { SW_SHOW } else { SW_HIDE });
            }
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }
    fn footer(&self) {
        unsafe {
            let _ = set_text_if_changed(self.status, &self.notice);
            if let Ok(button) = GetDlgItem(Some(self.hwnd), COMPACT as i32) {
                let _ = set_text_if_changed(
                    button,
                    if self.compact {
                        "⌄  More details"
                    } else {
                        "⌃  Fewer details"
                    },
                );
            }
            if let Ok(button) = GetDlgItem(Some(self.hwnd), END_TASK as i32) {
                let label = if self.pending.is_some() {
                    "Confirm"
                } else {
                    match self.tab {
                        Tab::Startup => self
                            .selected()
                            .and_then(|r| self.startup(&r.key))
                            .map_or("Enable", |s| if s.enabled { "Disable" } else { "Enable" }),
                        Tab::Services => self
                            .selected()
                            .and_then(|r| self.service(&r.key))
                            .map_or("Start", |s| if s.state == 1 { "Start" } else { "Stop" }),
                        Tab::Users => {
                            if self
                                .selected()
                                .is_some_and(|r| matches!(r.key, Key::Process(..)))
                            {
                                "End task"
                            } else {
                                "Disconnect"
                            }
                        }
                        Tab::History => "Delete history",
                        _ => "End task",
                    }
                };
                let _ = set_text_if_changed(button, label);
                let enabled = self.action.is_none()
                    && (self.pending.is_some()
                        || self.tab == Tab::History
                        || (self.selected().is_some() && self.tab != Tab::Performance));
                let _ = EnableWindow(button, enabled);
            }
        }
    }
    fn startup(&self, key: &Key) -> Option<&data::Startup> {
        if let Key::Startup(root, path, name) = key {
            self.snapshot
                .startups
                .iter()
                .find(|s| &s.root == root && &s.run_key == path && &s.name == name)
        } else {
            None
        }
    }
    fn service(&self, key: &Key) -> Option<&data::Service> {
        if let Key::Service(name) = key {
            self.snapshot.services.iter().find(|s| &s.name == name)
        } else {
            None
        }
    }
    fn execute(&mut self, pending: Pending) {
        let (sender, receiver) = mpsc::channel();
        self.action = Some(receiver);
        self.notice = "Working…".into();
        // HANDLEs are transferable between threads in this process. Transfer
        // ownership as integer values; the worker restores RAII immediately.
        let action: Box<dyn FnOnce() -> anyhow::Result<String> + Send> = match pending {
            Pending::End(handles) => {
                let handles: Vec<usize> = handles
                    .into_iter()
                    .map(|h| {
                        let raw = h.0.0 as usize;
                        std::mem::forget(h);
                        raw
                    })
                    .collect();
                Box::new(move || {
                    let handles: Vec<_> = handles
                        .into_iter()
                        .map(|h| OwnedHandle(HANDLE(h as *mut _)))
                        .collect();
                    for handle in &handles {
                        unsafe {
                            TerminateProcess(handle.0, 1)?;
                        }
                    }
                    Ok(format!("Ended {} process(es).", handles.len()))
                })
            }
            Pending::Service(name, action) => Box::new(move || {
                data::service_action(&name, action)?;
                Ok(format!(
                    "{} requested for {name}.",
                    match action {
                        data::ServiceAction::Start => "Start",
                        data::ServiceAction::Stop => "Stop",
                        data::ServiceAction::Restart => "Restart",
                    }
                ))
            }),
            Pending::Startup(row) => Box::new(move || {
                data::startup_action(&row)?;
                Ok(format!(
                    "{} {}.",
                    row.name,
                    if row.enabled { "disabled" } else { "enabled" }
                ))
            }),
            Pending::Disconnect(id) => Box::new(move || {
                ensure!(id != 0, "Session 0 cannot be disconnected.");
                unsafe {
                    WTSDisconnectSession(None, id, false)?;
                }
                Ok("User disconnected.".into())
            }),
            Pending::Priority(row, class) => Box::new(move || {
                let handle = data::identified_handle(&row, PROCESS_SET_INFORMATION)?;
                unsafe {
                    SetPriorityClass(handle.0, PROCESS_CREATION_FLAGS(class))?;
                }
                Ok("Process priority changed.".into())
            }),
        };
        std::thread::spawn(move || {
            let _ = sender.send(action());
        });
    }
    fn request_end(&mut self, tree: bool) -> anyhow::Result<()> {
        let row = self.selected().context("Select a process first.")?.clone();
        let mut indices = row.processes.clone();
        ensure!(!indices.is_empty(), "Select a process first.");
        if tree {
            let mut identities: HashSet<u32> = indices
                .iter()
                .map(|i| self.snapshot.processes[*i].pid)
                .collect();
            loop {
                let mut added = false;
                for (i, p) in self.snapshot.processes.iter().enumerate() {
                    if identities.contains(&p.parent) && !identities.contains(&p.pid) {
                        // A child must be newer than its parent; do not follow a recycled PID.
                        let parent = self
                            .snapshot
                            .processes
                            .iter()
                            .find(|parent| parent.pid == p.parent);
                        if parent.is_some_and(|parent| {
                            parent.created.zip(p.created).is_some_and(|(a, b)| a <= b)
                        }) {
                            identities.insert(p.pid);
                            indices.push(i);
                            added = true;
                        }
                    }
                }
                if !added {
                    break;
                }
            }
        }
        let protected = data::ancestors(&self.snapshot.processes, std::process::id());
        let mut handles = Vec::new();
        for index in indices.iter().rev() {
            handles.push(data::termination_handle(
                &self.snapshot.processes[*index],
                &protected,
            )?);
        }
        self.notice = format!(
            "End {} — {} process(es)? Unsaved work will be lost.",
            row.cells[0],
            handles.len()
        );
        self.pending = Some(Pending::End(handles));
        Ok(())
    }
    fn activate(&mut self) -> anyhow::Result<()> {
        if let Some(pending) = self.pending.take() {
            self.execute(pending);
            return Ok(());
        }
        if self.tab == Tab::History {
            self.history.clear();
            return Ok(());
        }
        let row = self.selected().context("Select an item first.")?.clone();
        match (&self.tab, &row.key) {
            (Tab::Startup, _) => {
                let s = self
                    .startup(&row.key)
                    .context("Startup entry no longer exists")?
                    .clone();
                self.notice = format!(
                    "{} {} at next sign-in?",
                    if s.enabled { "Disable" } else { "Enable" },
                    s.name
                );
                self.pending = Some(Pending::Startup(s));
            }
            (Tab::Services, _) => {
                let s = self
                    .service(&row.key)
                    .context("Service no longer exists")?
                    .clone();
                let start = s.state == 1;
                ensure!(
                    s.state == 1 || s.state == 4 || s.state == 7,
                    "Wait for the pending service operation."
                );
                ensure!(
                    !crate::service::is_agent_service(&s.name),
                    "The remote connection service cannot be changed here."
                );
                self.notice = format!(
                    "{} service {}?",
                    if start { "Start" } else { "Stop" },
                    s.name
                );
                self.pending = Some(Pending::Service(
                    s.name.clone(),
                    if start {
                        data::ServiceAction::Start
                    } else {
                        data::ServiceAction::Stop
                    },
                ));
            }
            (Tab::Users, Key::User(id)) => {
                ensure!(*id != 0, "Session 0 cannot be disconnected.");
                self.notice = format!(
                    "Disconnect {}? Applications will keep running.",
                    row.cells[0]
                );
                self.pending = Some(Pending::Disconnect(*id));
            }
            _ => self.request_end(false)?,
        }
        Ok(())
    }
    fn change_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.sort = 0;
        self.descending = false;
        self.pending = None;
        self.run_visible = false;
        self.notice = match tab {
            Tab::History => "Resource usage since this Task Manager opened. I/O includes disk, network and device transfers.".into(),
            Tab::Startup => "Run and Startup-folder entries for all users and loaded profiles. Packaged-app entries and impact history are unavailable in Session 0.".into(),
            Tab::Performance => String::new(),
            _ => String::new(),
        };
        self.configure();
        self.tick = 0;
        self.refresh();
    }
    fn command(&mut self, id: usize) -> anyhow::Result<()> {
        match id {
            REFRESH => {
                self.tick = 0;
                self.refresh();
            }
            END_TASK => self.activate()?,
            CANCEL => {
                self.pending = None;
                self.notice.clear();
                self.run_visible = false;
            }
            COMPACT => {
                if !self.compact {
                    let mut rect = RECT::default();
                    unsafe {
                        GetWindowRect(self.hwnd, &mut rect)?;
                    }
                    self.expanded_size = (rect.right - rect.left, rect.bottom - rect.top);
                }
                self.compact = !self.compact;
                if self.compact {
                    self.tab = Tab::Processes;
                    unsafe {
                        SendMessageW(self.tabs, TCM_SETCURSEL, Some(WPARAM(0)), None);
                    }
                }
                self.pending = None;
                self.notice.clear();
                unsafe {
                    SetMenu(self.hwnd, if self.compact { None } else { Some(self.menu) })?;
                }
                let (width, height) = if self.compact {
                    (350, 320)
                } else {
                    self.expanded_size
                };
                unsafe {
                    SetWindowPos(
                        self.hwnd,
                        None,
                        0,
                        0,
                        width,
                        height,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    )?;
                }
                self.configure();
            }
            RUN_TASK => {
                self.run_visible = true;
                self.pending = None;
                unsafe {
                    if let Ok(edit) = GetDlgItem(Some(self.hwnd), RUN_EDIT as i32) {
                        let _ = SetFocus(Some(edit));
                    }
                }
            }
            RUN => {
                let edit = unsafe { GetDlgItem(Some(self.hwnd), RUN_EDIT as i32)? };
                let mut text = [0u16; 32768];
                let count = unsafe { GetWindowTextW(edit, &mut text) };
                let command = String::from_utf16_lossy(&text[..count as usize]);
                ensure!(
                    !command.trim().is_empty(),
                    "Enter a program and optional arguments."
                );
                launch(&command)?;
                self.run_visible = false;
                self.notice = "Task started on the isolated background desktop as SYSTEM.".into();
                self.refresh();
            }
            EXIT => unsafe {
                PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0))?;
            },
            TOPMOST => {
                self.topmost = !self.topmost;
                unsafe {
                    SetWindowPos(
                        self.hwnd,
                        Some(if self.topmost {
                            HWND_TOPMOST
                        } else {
                            HWND_NOTOPMOST
                        }),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    )?;
                }
            }
            SPEED_HIGH..=SPEED_PAUSED => {
                self.interval = [500, 1000, 4000, 0][id - SPEED_HIGH];
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), 1);
                    if self.interval > 0 {
                        SetTimer(Some(self.hwnd), 1, self.interval, None);
                    }
                }
            }
            GROUP => {
                self.grouped = !self.grouped;
                self.rebuild();
            }
            END_TREE => self.request_end(true)?,
            GO_DETAILS => {
                let row = self
                    .selected()
                    .context("Select a process or service first.")?
                    .clone();
                let pid = row
                    .processes
                    .first()
                    .map(|i| self.snapshot.processes[*i].pid)
                    .or_else(|| self.service(&row.key).map(|s| s.pid))
                    .context("No process is associated with this item.")?;
                self.change_tab(Tab::Details);
                unsafe {
                    SendMessageW(
                        self.tabs,
                        TCM_SETCURSEL,
                        Some(WPARAM(Tab::Details.index())),
                        None,
                    );
                }
                if let Some(index) = self
                    .rows
                    .iter()
                    .position(|r| matches!(r.key, Key::Process(id, _) if id == pid))
                {
                    self.select(index);
                }
            }
            PRIORITY_NORMAL..=PRIORITY_HIGH => {
                let row = self
                    .selected()
                    .and_then(|r| r.processes.first())
                    .map(|i| self.snapshot.processes[*i].clone())
                    .context("Select a process first.")?;
                let class = [
                    NORMAL_PRIORITY_CLASS.0,
                    IDLE_PRIORITY_CLASS.0,
                    HIGH_PRIORITY_CLASS.0,
                ][id - PRIORITY_NORMAL];
                self.notice = format!("Change {} priority to {}?", row.name, priority(class));
                self.pending = Some(Pending::Priority(row, class));
            }
            RESET_HISTORY => {
                self.history.clear();
                self.rebuild();
            }
            SERVICE_START | SERVICE_STOP | SERVICE_RESTART => {
                let s = self
                    .selected()
                    .and_then(|r| self.service(&r.key))
                    .context("Select a service first.")?
                    .clone();
                let start = id == SERVICE_START;
                ensure!(
                    !crate::service::is_agent_service(&s.name),
                    "The remote connection service cannot be changed here."
                );
                self.notice = format!(
                    "{} service {}?",
                    if id == SERVICE_RESTART {
                        "Restart"
                    } else if start {
                        "Start"
                    } else {
                        "Stop"
                    },
                    s.name
                );
                self.pending = Some(Pending::Service(
                    s.name.clone(),
                    if id == SERVICE_RESTART {
                        data::ServiceAction::Restart
                    } else if start {
                        data::ServiceAction::Start
                    } else {
                        data::ServiceAction::Stop
                    },
                ));
            }
            COPY => {
                if let Some(row) = self.selected() {
                    self.notice = row.cells.join("  |  ");
                }
            }
            _ => {}
        }
        unsafe {
            let menu = self.menu;
            let _ = CheckMenuItem(
                menu,
                TOPMOST as u32,
                MF_BYCOMMAND.0
                    | if self.topmost {
                        MF_CHECKED.0
                    } else {
                        MF_UNCHECKED.0
                    },
            );
            let _ = CheckMenuItem(
                menu,
                GROUP as u32,
                MF_BYCOMMAND.0
                    | if self.grouped {
                        MF_CHECKED.0
                    } else {
                        MF_UNCHECKED.0
                    },
            );
            for (id, speed) in [
                (SPEED_HIGH, 500),
                (SPEED_NORMAL, 1000),
                (SPEED_LOW, 4000),
                (SPEED_PAUSED, 0),
            ] {
                let _ = CheckMenuItem(
                    menu,
                    id as u32,
                    MF_BYCOMMAND.0
                        | if self.interval == speed {
                            MF_CHECKED.0
                        } else {
                            MF_UNCHECKED.0
                        },
                );
            }
        }
        Ok(())
    }
    fn select(&self, index: usize) {
        unsafe {
            let item = LVITEMW {
                stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                state: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                ..Default::default()
            };
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                Some(WPARAM(index)),
                Some(LPARAM((&item as *const LVITEMW) as isize)),
            );
            SendMessageW(self.list, LVM_ENSUREVISIBLE, Some(WPARAM(index)), None);
        }
    }
    fn expand(&mut self) {
        if let Some(row) = self.selected().filter(|r| r.expandable) {
            let key = row.key.clone();
            if !self.expanded.remove(&key) {
                self.expanded.insert(key);
            }
            self.rebuild();
        }
    }
}

fn set_text_if_changed(hwnd: HWND, value: &str) -> anyhow::Result<()> {
    unsafe {
        let mut current = vec![0u16; GetWindowTextLengthW(hwnd) as usize + 1];
        let count = GetWindowTextW(hwnd, &mut current);
        if String::from_utf16_lossy(&current[..count as usize]) != value {
            let text = wide(value);
            SetWindowTextW(hwnd, PCWSTR(text.as_ptr()))?;
        }
    }
    Ok(())
}

fn visible_processes() -> HashSet<u32> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
        unsafe {
            if IsWindowVisible(hwnd).as_bool()
                && GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 == 0
                && GetWindow(hwnd, GW_OWNER).is_err()
                && GetWindowTextLengthW(hwnd) > 0
            {
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                (*(data.0 as *mut HashSet<u32>)).insert(pid);
            }
        }
        windows::core::BOOL(1)
    }
    let mut pids = HashSet::new();
    let _ = unsafe {
        EnumWindows(
            Some(collect),
            LPARAM((&mut pids as *mut HashSet<u32>) as isize),
        )
    };
    pids
}
fn launch(command: &str) -> anyhow::Result<()> {
    super::background::launch::launch(super::background::launch::Launch {
        command,
        flags: CREATE_NEW_CONSOLE,
        ..Default::default()
    })?;
    Ok(())
}

pub fn run() -> anyhow::Result<()> {
    meshrmm_remote_screen::background::require_session_zero()?;
    let _desktop = meshrmm_remote_screen::background::Desktop::bind()?;
    let _theme = theme::Theme::activate()?;
    let state = create_window()?;
    let monitor = telemetry::Monitor::start();
    match &monitor {
        Ok(monitor) => state.borrow_mut().snapshot.telemetry = Some(monitor.counters.clone()),
        Err(error) => {
            state.borrow_mut().notice =
                format!("Per-process disk/network telemetry unavailable: {error}")
        }
    }
    let hwnd = state.borrow().hwnd;
    state.borrow_mut().refresh();
    unsafe {
        SetTimer(Some(hwnd), 1, 1000, None);
        SetTimer(Some(hwnd), 2, 100, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            if message.message == WM_KEYDOWN {
                if message.wParam.0 == 0x1b {
                    let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(CANCEL), LPARAM(0));
                    continue;
                }
                if message.wParam.0 == 0x0d && state.borrow().run_visible {
                    let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(RUN), LPARAM(0));
                    continue;
                }
                if message.wParam.0 == 0x09
                    && windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x11) < 0
                {
                    let mut s = state.borrow_mut();
                    let backwards =
                        windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x10) < 0;
                    let index = (s.tab.index() + if backwards { 6 } else { 1 }) % 7;
                    SendMessageW(s.tabs, TCM_SETCURSEL, Some(WPARAM(index)), None);
                    s.change_tab(Tab::from_index(index));
                    continue;
                }
            }
            if message.message == WM_KEYDOWN && message.wParam.0 == 0x74 {
                let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(REFRESH), LPARAM(0));
                continue;
            }
            if !IsDialogMessageW(hwnd, &message).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
        let state = state.into_inner();
        if state.compact {
            let _ = DestroyMenu(state.menu);
        }
        let _ = ImageList_Destroy(Some(state.images));
        let _ = DeleteObject(state.font.into());
        let _ = DeleteObject(state.heading_font.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
