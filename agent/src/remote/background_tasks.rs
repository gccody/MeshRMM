//! Windows 10-style Task Manager for the private Session 0 desktop.
//! Native controls/GDI remain capturable without an interactive shell or GPU.
mod data;
mod telemetry;
mod theme;
pub(super) fn stop_telemetry(pid: u32) {
    telemetry::stop(pid);
}

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::mpsc;

use anyhow::{Context, ensure};
use data::{Handle, Process, Snapshot, wide};
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
    End(Vec<Handle>),
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
                let system = std::env::var("SystemRoot")
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
                        .map(|h| Handle(HANDLE(h as *mut _)))
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
        let mut handles = Vec::new();
        for index in indices.iter().rev() {
            handles.push(data::termination_handle(&self.snapshot.processes[*index])?);
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
                    !s.name.eq_ignore_ascii_case("MeshRMMAgent"),
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
                    !s.name.eq_ignore_ascii_case("MeshRMMAgent"),
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
    let mut command = wide(command);
    let mut desktop = wide(&meshrmm_remote_screen::background::desktop_path()?);
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE,
            None,
            PCWSTR::null(),
            &startup,
            &mut process,
        )?;
        let _thread = Handle(process.hThread);
        let _process = Handle(process.hProcess);
    }
    Ok(())
}

unsafe fn padded_icon(images: HIMAGELIST, icon: HICON) -> i32 {
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return -1;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 16,
                biHeight: -28,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0)
        else {
            let _ = DeleteDC(dc);
            return -1;
        };
        let old = SelectObject(dc, bitmap.into());
        let mask = COLORREF(0xff00ff);
        fill(
            dc,
            &RECT {
                left: 0,
                top: 0,
                right: 16,
                bottom: 28,
            },
            mask,
        );
        let result = if DrawIconEx(dc, 0, 6, icon, 16, 16, 0, None, DI_NORMAL).is_ok() {
            // Classic Session 0 controls need an explicit color-key mask;
            // their PrintWindow path does not composite image-list alpha.
            let _ = GdiFlush();
            SelectObject(dc, old);
            ImageList_AddMasked(images, bitmap, mask)
        } else {
            -1
        };
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        result
    }
}

unsafe fn draw_text(dc: HDC, text: &str, rect: RECT, color: COLORREF, flags: DRAW_TEXT_FORMAT) {
    unsafe {
        let text = wide(text);
        SetTextColor(dc, color);
        SetBkMode(dc, TRANSPARENT);
        let mut rect = rect;
        DrawTextW(
            dc,
            &mut text[..text.len() - 1].to_vec(),
            &mut rect,
            flags | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }
}
unsafe fn fill(dc: HDC, rect: &RECT, color: COLORREF) {
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(dc, rect, brush);
        let _ = DeleteObject(brush.into());
    }
}
unsafe fn header_paint(state: &State, hwnd: HWND, dc: HDC) {
    unsafe {
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        fill(dc, &rect, WHITE);
        SelectObject(dc, state.font.into());
        let mut x = -GetScrollPos(state.list, SB_HORZ);
        for (index, (name, _)) in Tab::Processes.columns().iter().enumerate() {
            let width =
                SendMessageW(state.list, LVM_GETCOLUMNWIDTH, Some(WPARAM(index)), None).0 as i32;
            let cell = RECT {
                left: x,
                top: 0,
                right: x + width,
                bottom: rect.bottom - 1,
            };
            if index >= 2 {
                let total = match index {
                    2 => state
                        .snapshot
                        .cpu
                        .map_or_else(|| "—".into(), |v| format!("{v:.0}%")),
                    3 => {
                        if state.snapshot.memory_total > 0 {
                            format!(
                                "{:.0}%",
                                100.0
                                    * (state.snapshot.memory_total
                                        - state.snapshot.memory_available)
                                        as f64
                                    / state.snapshot.memory_total as f64
                            )
                        } else {
                            "—".into()
                        }
                    }
                    4 => state
                        .snapshot
                        .disk
                        .map_or_else(|| "—".into(), |v| format!("{v:.0}%")),
                    5 => state
                        .snapshot
                        .network_rate
                        .filter(|_| state.snapshot.network_capacity > 0)
                        .map_or_else(
                            || "—".into(),
                            |v| {
                                format!(
                                    "{:.0}%",
                                    (v * 800.0 / state.snapshot.network_capacity as f64).min(100.0)
                                )
                            },
                        ),
                    _ => "—".into(),
                };
                SelectObject(dc, state.heading_font.into());
                draw_text(
                    dc,
                    &total,
                    RECT {
                        left: x + 5,
                        top: 5,
                        right: x + width - 7,
                        bottom: 29,
                    },
                    COLORREF(0x222222),
                    DT_RIGHT,
                );
            }
            SelectObject(dc, state.font.into());
            draw_text(
                dc,
                name,
                RECT {
                    left: x + 8,
                    top: 27,
                    right: x + width - 7,
                    bottom: rect.bottom - 3,
                },
                COLORREF(0x66594e),
                if index >= 2 { DT_RIGHT } else { DT_LEFT },
            );
            fill(
                dc,
                &RECT {
                    left: cell.right - 1,
                    top: 12,
                    right: cell.right,
                    bottom: cell.bottom,
                },
                COLORREF(0xe5e5e5),
            );
            if state.sort == index {
                draw_text(
                    dc,
                    if state.descending { "⌄" } else { "⌃" },
                    RECT {
                        left: x,
                        top: 0,
                        right: x + width,
                        bottom: 12,
                    },
                    COLORREF(0x777777),
                    DT_CENTER,
                );
            }
            x += width;
        }
        fill(
            dc,
            &RECT {
                top: rect.bottom - 1,
                ..rect
            },
            COLORREF(0xaaaaaa),
        );
    }
}
// Session 0 common controls can leave nonclient scrollbar pixels unpainted.
// Paint their actual native geometry after native list painting. Native scroll
// commands retain the list control's range, selection, keyboard and wheel behavior.
unsafe fn captured_scrollbars(list: HWND, dc: HDC) {
    unsafe {
        if !IsWindowVisible(list).as_bool() {
            return;
        }
        let mut frame = RECT::default();
        if GetWindowRect(list, &mut frame).is_err() {
            return;
        }
        let saved = SaveDC(dc);
        let _ = SelectClipRgn(dc, None);
        let _ = SetViewportOrgEx(dc, 0, 0, None);
        for (id, vertical) in [(OBJID_VSCROLL, true), (OBJID_HSCROLL, false)] {
            let mut info = SCROLLBARINFO {
                cbSize: std::mem::size_of::<SCROLLBARINFO>() as u32,
                ..Default::default()
            };
            if GetScrollBarInfo(list, id, &mut info).is_err() || info.rgstate[0] & 0x8000 != 0
            // STATE_SYSTEM_INVISIBLE
            {
                continue;
            }
            let mut bar = info.rcScrollBar;
            let _ = OffsetRect(&mut bar, -frame.left, -frame.top);
            fill(dc, &bar, COLORREF(0xf0f0f0));
            let mut first = bar;
            let mut last = bar;
            let mut thumb = bar;
            if vertical {
                first.bottom = first.top + info.dxyLineButton;
                last.top = last.bottom - info.dxyLineButton;
                thumb.top = bar.top + info.xyThumbTop;
                thumb.bottom = bar.top + info.xyThumbBottom;
                thumb.left += 2;
                thumb.right -= 2;
            } else {
                first.right = first.left + info.dxyLineButton;
                last.left = last.right - info.dxyLineButton;
                thumb.left = bar.left + info.xyThumbTop;
                thumb.right = bar.left + info.xyThumbBottom;
                thumb.top += 2;
                thumb.bottom -= 2;
            }
            for (rect, arrow, part) in [
                (
                    &mut first,
                    if vertical {
                        DFCS_SCROLLUP
                    } else {
                        DFCS_SCROLLLEFT
                    },
                    1,
                ),
                (
                    &mut last,
                    if vertical {
                        DFCS_SCROLLDOWN
                    } else {
                        DFCS_SCROLLRIGHT
                    },
                    5,
                ),
            ] {
                let flags = arrow
                    | DFCS_FLAT
                    | if info.rgstate[part] & 1 != 0 {
                        DFCS_INACTIVE
                    } else {
                        DFCS_STATE(0)
                    }
                    | if info.rgstate[part] & 8 != 0 {
                        DFCS_PUSHED
                    } else {
                        DFCS_STATE(0)
                    };
                let _ = DrawFrameControl(dc, rect, DFC_SCROLL, flags);
            }
            if info.xyThumbBottom > info.xyThumbTop && info.rgstate[3] & 0x8000 == 0 {
                fill(dc, &thumb, COLORREF(0xc8c8c8));
            }
        }
        let _ = RestoreDC(dc, saved);
    }
}

#[derive(Clone, Copy)]
enum ScrollInput {
    Drag {
        vertical: bool,
        start: i32,
        position: i32,
        travel: i32,
        minimum: i32,
        maximum: i32,
    },
    Repeat {
        vertical: bool,
        command: SCROLLBAR_COMMAND,
    },
}
const SCROLL_REPEAT: usize = 0x4d524d54;
unsafe fn scroll_command(hwnd: HWND, vertical: bool, command: SCROLLBAR_COMMAND, position: i32) {
    unsafe {
        if command == SB_THUMBPOSITION {
            // Report list views consult native tracking state for thumb messages;
            // posted background input never enters that modal tracking loop.
            // Their vertical range is in rows, while LVM_SCROLL takes pixels.
            let delta = position - GetScrollPos(hwnd, if vertical { SB_VERT } else { SB_HORZ });
            let (x, y) = if vertical {
                let top = SendMessageW(hwnd, LVM_GETTOPINDEX, None, None).0;
                let mut rect = RECT::default();
                SendMessageW(
                    hwnd,
                    LVM_GETITEMRECT,
                    Some(WPARAM(top.max(0) as usize)),
                    Some(LPARAM((&mut rect as *mut RECT) as isize)),
                );
                (0, delta.saturating_mul((rect.bottom - rect.top).max(1)))
            } else {
                (delta, 0)
            };
            SendMessageW(
                hwnd,
                LVM_SCROLL,
                Some(WPARAM(x as usize)),
                Some(LPARAM(y as isize)),
            );
            return;
        }
        SendMessageW(
            hwnd,
            if vertical { WM_VSCROLL } else { WM_HSCROLL },
            Some(WPARAM(
                command.0 as usize | ((position.clamp(0, 65535) as usize) << 16),
            )),
            None,
        );
    }
}
unsafe extern "system" fn list_paint(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    unsafe {
        let input = &*(data as *const Cell<Option<ScrollInput>>);
        if message == WM_NCDESTROY {
            let _ = KillTimer(Some(hwnd), SCROLL_REPEAT);
            let _ = RemoveWindowSubclass(hwnd, Some(list_paint), id);
            let result = DefSubclassProc(hwnd, message, wparam, lparam);
            drop(Box::from_raw(data as *mut Cell<Option<ScrollInput>>));
            return result;
        }
        if message == WM_TIMER && wparam.0 == SCROLL_REPEAT {
            if let Some(ScrollInput::Repeat { vertical, command }) = input.get() {
                scroll_command(hwnd, vertical, command, 0);
                SetTimer(Some(hwnd), SCROLL_REPEAT, 60, None);
            }
            return LRESULT(0);
        }
        if message == WM_LBUTTONUP && input.take().is_some() {
            let _ = KillTimer(Some(hwnd), SCROLL_REPEAT);
            return LRESULT(0);
        }
        if message == WM_CANCELMODE {
            input.set(None);
            let _ = KillTimer(Some(hwnd), SCROLL_REPEAT);
        }
        // The isolated desktop routes client mouse messages even for nonclient
        // scrollbar coordinates. Adapt only this list, without changing routing
        // or invoking the native modal scrollbar loop on other applications.
        if matches!(message, WM_LBUTTONDOWN | WM_MOUSEMOVE) {
            let mut point = POINT {
                x: lparam.0 as i16 as i32,
                y: (lparam.0 >> 16) as i16 as i32,
            };
            let _ = ClientToScreen(hwnd, &mut point);
            if message == WM_MOUSEMOVE {
                if let Some(ScrollInput::Drag {
                    vertical,
                    start,
                    position,
                    travel,
                    minimum,
                    maximum,
                }) = input.get()
                {
                    let current = if vertical { point.y } else { point.x };
                    let delta = i64::from(current - start) * i64::from(maximum - minimum)
                        / i64::from(travel.max(1));
                    let position = (i64::from(position) + delta)
                        .clamp(i64::from(minimum), i64::from(maximum))
                        as i32;
                    scroll_command(hwnd, vertical, SB_THUMBPOSITION, position);
                    return LRESULT(0);
                }
            } else {
                for (object, bar, vertical) in [
                    (OBJID_VSCROLL, SB_VERT, true),
                    (OBJID_HSCROLL, SB_HORZ, false),
                ] {
                    let mut info = SCROLLBARINFO {
                        cbSize: std::mem::size_of::<SCROLLBARINFO>() as u32,
                        ..Default::default()
                    };
                    if GetScrollBarInfo(hwnd, object, &mut info).is_err()
                        || info.rgstate[0] & 0x8001 != 0
                        || !PtInRect(&info.rcScrollBar, point).as_bool()
                    {
                        continue;
                    }
                    let current = if vertical { point.y } else { point.x };
                    let start = if vertical {
                        info.rcScrollBar.top
                    } else {
                        info.rcScrollBar.left
                    };
                    let end = if vertical {
                        info.rcScrollBar.bottom
                    } else {
                        info.rcScrollBar.right
                    };
                    let command = if current < start + info.dxyLineButton {
                        Some(SB_LINEUP)
                    } else if current >= end - info.dxyLineButton {
                        Some(SB_LINEDOWN)
                    } else if current < start + info.xyThumbTop {
                        Some(SB_PAGEUP)
                    } else if current >= start + info.xyThumbBottom {
                        Some(SB_PAGEDOWN)
                    } else {
                        None
                    };
                    if let Some(command) = command {
                        input.set(Some(ScrollInput::Repeat { vertical, command }));
                        scroll_command(hwnd, vertical, command, 0);
                        SetTimer(Some(hwnd), SCROLL_REPEAT, 400, None);
                    } else {
                        let mut range = SCROLLINFO {
                            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                            fMask: SIF_ALL,
                            ..Default::default()
                        };
                        if GetScrollInfo(hwnd, bar, &mut range).is_ok() {
                            input.set(Some(ScrollInput::Drag {
                                vertical,
                                start: current,
                                position: range.nPos,
                                travel: end
                                    - start
                                    - 2 * info.dxyLineButton
                                    - (info.xyThumbBottom - info.xyThumbTop),
                                minimum: range.nMin,
                                maximum: (range.nMax - range.nPage.saturating_sub(1) as i32)
                                    .max(range.nMin),
                            }));
                        }
                    }
                    return LRESULT(0);
                }
            }
        }
        let result = DefSubclassProc(hwnd, message, wparam, lparam);
        if matches!(message, WM_PAINT | WM_NCPAINT | WM_HSCROLL | WM_VSCROLL) {
            let dc = GetWindowDC(Some(hwnd));
            if !dc.is_invalid() {
                captured_scrollbars(hwnd, dc);
                ReleaseDC(Some(hwnd), dc);
            }
        }
        result
    }
}

unsafe fn performance_paint(state: &State, hwnd: HWND, dc: HDC) {
    unsafe {
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        fill(dc, &rect, WHITE);
        SelectObject(dc, state.font.into());
        let sidebar = 180;
        let selected = state.performance;
        let used = state
            .snapshot
            .memory_total
            .saturating_sub(state.snapshot.memory_available);
        let values = [
            percent(state.snapshot.cpu),
            format!(
                "{:.1} / {:.1} GB",
                used as f64 / 1073741824.0,
                state.snapshot.memory_total as f64 / 1073741824.0
            ),
            percent(state.snapshot.disk),
            state
                .snapshot
                .network_rate
                .map_or_else(|| "—".into(), |n| format!("{:.2} Mbps", n * 8.0 / 1e6)),
        ];
        let titles = ["CPU", "Memory", "Disk", "Ethernet / Wi-Fi"];
        for i in 0..4 {
            let top = 10 + i as i32 * 66;
            if i == selected {
                fill(
                    dc,
                    &RECT {
                        left: 6,
                        top,
                        right: sidebar - 6,
                        bottom: top + 60,
                    },
                    COLORREF(0xf2e7d9),
                );
            }
            draw_text(
                dc,
                titles[i],
                RECT {
                    left: 18,
                    top: top + 4,
                    right: sidebar - 10,
                    bottom: top + 29,
                },
                COLORREF(0x222222),
                DT_LEFT,
            );
            draw_text(
                dc,
                &values[i],
                RECT {
                    left: 18,
                    top: top + 29,
                    right: sidebar - 10,
                    bottom: top + 51,
                },
                COLORREF(0x555555),
                DT_LEFT,
            );
        }
        let left = sidebar + 20;
        let right = rect.right - 24;
        let top = 56;
        draw_text(
            dc,
            titles[selected],
            RECT {
                left,
                top: 8,
                right,
                bottom: 35,
            },
            BLUE,
            DT_LEFT,
        );
        let subtitle = [
            "% Utilization",
            "% Physical memory in use",
            "% Active time • all physical disks",
            "Throughput • hardware adapters",
        ][selected];
        draw_text(
            dc,
            subtitle,
            RECT {
                left,
                top: 33,
                right,
                bottom: 54,
            },
            COLORREF(0x777777),
            DT_LEFT,
        );
        let graph = RECT {
            left,
            top,
            right,
            bottom: (rect.bottom - 126).max(top + 60),
        };
        fill(dc, &graph, COLORREF(0xfffcf8));
        for col in 0..=12 {
            let x = left + (right - left) * col / 12;
            fill(
                dc,
                &RECT {
                    left: x,
                    top,
                    right: x + 1,
                    bottom: graph.bottom,
                },
                COLORREF(0xe8d8ca),
            );
        }
        for row in 0..=10 {
            let y = top + (graph.bottom - top) * row / 10;
            fill(
                dc,
                &RECT {
                    left,
                    top: y,
                    right,
                    bottom: y + 1,
                },
                COLORREF(0xe8d8ca),
            );
        }
        let scale = if selected == 3 {
            state.samples.iter().map(|s| s[3]).fold(125000.0, f64::max)
        } else {
            100.0
        };
        let pen = CreatePen(PS_SOLID, 2, BLUE);
        let old = SelectObject(dc, pen.into());
        let offset = 60 - state.samples.len();
        for (i, sample) in state.samples.iter().enumerate() {
            let x = left + (offset + i) as i32 * (right - left) / 59;
            let y = graph.bottom
                - 1
                - ((sample[selected] / scale).clamp(0.0, 1.0) * (graph.bottom - top - 2) as f64)
                    as i32;
            if i == 0 {
                let _ = MoveToEx(dc, x, y, None);
            } else {
                let _ = LineTo(dc, x, y);
            }
        }
        SelectObject(dc, old);
        let _ = DeleteObject(pen.into());
        draw_text(
            dc,
            &format!(
                "60 samples • {}",
                if state.interval == 0 {
                    "Paused".into()
                } else {
                    format!("{:.1} second interval", state.interval as f64 / 1000.0)
                }
            ),
            RECT {
                left,
                top: graph.bottom,
                right,
                bottom: graph.bottom + 23,
            },
            COLORREF(0x777777),
            DT_LEFT,
        );
        let summary = match selected {
            0 => vec![
                format!("Utilization    {}", values[0]),
                format!(
                    "Processes    {}     Threads    {}     Handles    {}",
                    state.snapshot.processes.len(),
                    state.snapshot.threads,
                    state.snapshot.handles
                ),
                format!(
                    "Up time    {}:{:02}:{:02}:{:02}",
                    state.snapshot.uptime / 86400,
                    state.snapshot.uptime / 3600 % 24,
                    state.snapshot.uptime / 60 % 60,
                    state.snapshot.uptime % 60
                ),
            ],
            1 => vec![
                format!(
                    "In use    {:.1} GB       Available    {:.1} GB",
                    used as f64 / 1073741824.0,
                    state.snapshot.memory_available as f64 / 1073741824.0
                ),
                format!(
                    "Committed    {:.1} / {:.1} GB",
                    state.snapshot.commit as f64 / 1073741824.0,
                    state.snapshot.commit_limit as f64 / 1073741824.0
                ),
                format!(
                    "Total physical memory    {:.1} GB",
                    state.snapshot.memory_total as f64 / 1073741824.0
                ),
            ],
            2 => vec![
                format!("Active time    {}", values[2]),
                state.snapshot.disk_rate.map_or_else(
                    || "Transfer rate    —".into(),
                    |v| format!("Transfer rate    {:.2} MB/s", v / 1048576.0),
                ),
                "All physical disks combined".into(),
            ],
            _ => vec![
                format!("Send + receive    {}", values[3]),
                format!(
                    "Combined link capacity    {:.0} Mbps",
                    state.snapshot.network_capacity as f64 / 1e6
                ),
                "Loopback and virtual interfaces excluded".into(),
            ],
        };
        for (i, line) in summary.iter().enumerate() {
            draw_text(
                dc,
                line,
                RECT {
                    left,
                    top: graph.bottom + 30 + i as i32 * 25,
                    right,
                    bottom: graph.bottom + 55 + i as i32 * 25,
                },
                COLORREF(0x333333),
                DT_LEFT,
            );
        }
    }
}

unsafe extern "system" fn panel_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let parent = GetParent(hwnd).unwrap_or_default();
        let cell = GetWindowLongPtrW(parent, GWLP_USERDATA) as *const RefCell<State>;
        if !cell.is_null() {
            if message == WM_LBUTTONUP
                && let Ok(mut state) = (*cell).try_borrow_mut()
                && hwnd == state.graph
            {
                let x = lparam.0 as i16 as i32;
                let y = (lparam.0 >> 16) as i16 as i32;
                if (0..180).contains(&x) && (10..274).contains(&y) {
                    state.performance = ((y - 10) / 66) as usize;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return LRESULT(0);
            }
            if matches!(message, WM_PAINT | WM_PRINTCLIENT)
                && let Ok(state) = (*cell).try_borrow()
            {
                let mut paint = PAINTSTRUCT::default();
                let dc = if message == WM_PAINT {
                    BeginPaint(hwnd, &mut paint)
                } else {
                    HDC(wparam.0 as *mut _)
                };
                if hwnd == state.header {
                    header_paint(&state, hwnd, dc);
                } else {
                    performance_paint(&state, hwnd, dc);
                }
                if message == WM_PAINT {
                    let _ = EndPaint(hwnd, &paint);
                }
                return LRESULT(0);
            }
            if message == WM_LBUTTONUP
                && let Ok(mut state) = (*cell).try_borrow_mut()
                && hwnd == state.header
            {
                let click = (lparam.0 as i16) as i32 + GetScrollPos(state.list, SB_HORZ);
                let mut x = 0;
                for i in 0..6 {
                    x += SendMessageW(state.list, LVM_GETCOLUMNWIDTH, Some(WPARAM(i)), None).0
                        as i32;
                    if click < x {
                        if state.sort == i {
                            state.descending = !state.descending;
                        } else {
                            state.sort = i;
                            state.descending = i >= 2;
                        }
                        state.rebuild();
                        break;
                    }
                }
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
unsafe fn context_menu(state: &State, point: POINT) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let items: Vec<(usize, &str)> = match state.tab {
            Tab::Processes | Tab::Details => vec![
                (END_TASK, "End task"),
                (END_TREE, "End process tree"),
                (GO_DETAILS, "Go to details"),
                (PRIORITY_LOW, "Set priority: Low"),
                (PRIORITY_NORMAL, "Set priority: Normal"),
                (PRIORITY_HIGH, "Set priority: High"),
                (COPY, "Show process information"),
            ],
            Tab::Services => vec![
                (SERVICE_START, "Start"),
                (SERVICE_STOP, "Stop"),
                (SERVICE_RESTART, "Restart"),
                (GO_DETAILS, "Go to details"),
            ],
            Tab::Startup => vec![(END_TASK, "Enable / Disable"), (COPY, "Show command line")],
            Tab::Users => vec![
                (END_TASK, "Disconnect / End task"),
                (GO_DETAILS, "Go to details"),
            ],
            Tab::History => vec![(RESET_HISTORY, "Delete usage history")],
            _ => vec![],
        };
        for (id, text) in items {
            let text = wide(text);
            let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(text.as_ptr()));
        }
        // TPM_RETURNCMD avoids synchronous command re-entry while borrowing state.
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            state.hwnd,
            None,
        )
        .0;
        let _ = DestroyMenu(menu);
        if command != 0 {
            let _ = PostMessageW(
                Some(state.hwnd),
                WM_COMMAND,
                WPARAM(command as usize),
                LPARAM(0),
            );
        }
    }
}
unsafe fn caption(state: &State, dc: HDC) {
    unsafe {
        let mut window = RECT::default();
        if GetWindowRect(state.hwnd, &mut window).is_err() {
            return;
        }
        let width = window.right - window.left;
        let mut menu_rect = RECT::default();
        let menu = GetMenu(state.hwnd);
        let bottom = if GetMenuItemRect(Some(state.hwnd), menu, 0, &mut menu_rect).is_ok() {
            menu_rect.top - window.top
        } else {
            31
        };
        fill(
            dc,
            &RECT {
                left: 1,
                top: 1,
                right: width - 1,
                bottom,
            },
            COLORREF(0xe8a32c),
        );
        SelectObject(dc, state.font.into());
        if let Ok(icon) = LoadIconW(None, IDI_APPLICATION) {
            let _ = DrawIconEx(dc, 8, (bottom - 16) / 2, icon, 16, 16, 0, None, DI_NORMAL);
        }
        draw_text(
            dc,
            "Task Manager",
            RECT {
                left: 30,
                top: 1,
                right: width - 140,
                bottom,
            },
            COLORREF(0x111111),
            DT_LEFT,
        );
        for (i, label) in ["—", "□", "×"].iter().enumerate() {
            draw_text(
                dc,
                label,
                RECT {
                    left: width - 139 + i as i32 * 46,
                    top: 1,
                    right: width - 1 - (2 - i as i32) * 46,
                    bottom,
                },
                COLORREF(0x111111),
                DT_CENTER,
            );
        }
        for (i, label) in ["File", "Options", "View"].iter().enumerate() {
            let mut rect = RECT::default();
            if GetMenuItemRect(Some(state.hwnd), menu, i as u32, &mut rect).is_ok() {
                rect.left -= window.left;
                rect.right -= window.left;
                rect.top -= window.top;
                rect.bottom -= window.top;
                fill(dc, &rect, WHITE);
                draw_text(dc, label, rect, COLORREF(0x111111), DT_CENTER);
            }
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let cell = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if message == WM_NCCALCSIZE {
            let rect = if wparam.0 != 0 {
                &mut (*(lparam.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0]
            } else {
                &mut *(lparam.0 as *mut RECT)
            };
            let outer = *rect;
            let result = DefWindowProcW(hwnd, message, wparam, lparam);
            rect.left = outer.left + 1;
            rect.right = outer.right - 1;
            rect.bottom = outer.bottom - 1;
            return result;
        }
        if message == WM_GETMINMAXINFO && lparam.0 != 0 {
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            info.ptMaxPosition = POINT { x: 0, y: 0 };
            info.ptMaxSize = POINT {
                x: meshrmm_remote_screen::background::WIDTH as i32,
                y: meshrmm_remote_screen::background::HEIGHT as i32
                    - super::background::TASKBAR_HEIGHT,
            };
            info.ptMaxTrackSize = info.ptMaxSize;
            let compact = !cell.is_null() && (*cell).try_borrow().map_or(true, |s| s.compact);
            info.ptMinTrackSize = if compact {
                POINT { x: 280, y: 200 }
            } else {
                POINT { x: 650, y: 390 }
            };
            return LRESULT(0);
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        if !cell.is_null() {
            // Background input routes caption clicks as client messages except
            // for the workspace's move/maximize/close handling.
            if message == WM_LBUTTONDOWN {
                let mut point = POINT {
                    x: lparam.0 as i16 as i32,
                    y: (lparam.0 >> 16) as i16 as i32,
                };
                let _ = ClientToScreen(hwnd, &mut point);
                let hit = SendMessageW(
                    hwnd,
                    WM_NCHITTEST,
                    None,
                    Some(LPARAM(
                        ((point.y as u32) << 16 | (point.x as u32 & 0xffff)) as isize,
                    )),
                );
                if hit.0 == HTMINBUTTON as isize {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_SYSCOMMAND,
                        WPARAM(SC_MINIMIZE as usize),
                        LPARAM(0),
                    );
                    return LRESULT(0);
                }
                if matches!(
                    hit.0 as u32,
                    HTLEFT
                        | HTRIGHT
                        | HTTOP
                        | HTTOPLEFT
                        | HTTOPRIGHT
                        | HTBOTTOM
                        | HTBOTTOMLEFT
                        | HTBOTTOMRIGHT
                ) && let Ok(mut state) = (*cell).try_borrow_mut()
                {
                    let mut bounds = RECT::default();
                    if GetWindowRect(hwnd, &mut bounds).is_ok() {
                        state.resizing = Some((hit.0 as u32, point, bounds));
                    }
                    return LRESULT(0);
                }
            }
            if message == WM_DRAWITEM
                && lparam.0 != 0
                && let Ok(state) = (*cell).try_borrow()
            {
                let item = &*(lparam.0 as *const DRAWITEMSTRUCT);
                if item.CtlID == COMPACT as u32 {
                    fill(item.hDC, &item.rcItem, WHITE);
                    SelectObject(item.hDC, state.font.into());
                    let pen = CreatePen(PS_SOLID, 1, COLORREF(0x999999));
                    let old = SelectObject(item.hDC, pen.into());
                    let brush = SelectObject(item.hDC, GetStockObject(WHITE_BRUSH));
                    let _ = Ellipse(item.hDC, 1, 3, 19, 21);
                    SelectObject(item.hDC, brush);
                    SelectObject(item.hDC, old);
                    let _ = DeleteObject(pen.into());
                    draw_text(
                        item.hDC,
                        if state.compact { "⌄" } else { "⌃" },
                        RECT {
                            left: 1,
                            top: 2,
                            right: 19,
                            bottom: 20,
                        },
                        COLORREF(0x555555),
                        DT_CENTER,
                    );
                    draw_text(
                        item.hDC,
                        if state.compact {
                            "More details"
                        } else {
                            "Fewer details"
                        },
                        RECT {
                            left: 25,
                            ..item.rcItem
                        },
                        COLORREF(0x222222),
                        DT_LEFT,
                    );
                    if item.itemState.0 & ODS_FOCUS.0 != 0 {
                        let _ = DrawFocusRect(item.hDC, &item.rcItem);
                    }
                    return LRESULT(1);
                }
            }
            if message == WM_NCHITTEST {
                let hit = DefWindowProcW(hwnd, message, wparam, lparam);
                let mut bounds = RECT::default();
                let _ = GetWindowRect(hwnd, &mut bounds);
                let x = lparam.0 as i16 as i32 - bounds.left;
                let y = (lparam.0 >> 16) as i16 as i32 - bounds.top;
                if !IsZoomed(hwnd).as_bool() {
                    let left = x < 4;
                    let right = x >= bounds.right - bounds.left - 4;
                    let top = y < 4;
                    let bottom = y >= bounds.bottom - bounds.top - 4;
                    let edge = match (left, right, top, bottom) {
                        (true, _, true, _) => HTTOPLEFT,
                        (_, true, true, _) => HTTOPRIGHT,
                        (true, _, _, true) => HTBOTTOMLEFT,
                        (_, true, _, true) => HTBOTTOMRIGHT,
                        (true, _, _, _) => HTLEFT,
                        (_, true, _, _) => HTRIGHT,
                        (_, _, true, _) => HTTOP,
                        (_, _, _, true) => HTBOTTOM,
                        _ => HTNOWHERE,
                    };
                    if edge != HTNOWHERE {
                        return LRESULT(edge as isize);
                    }
                }
                if (4..30).contains(&y) && x >= bounds.right - bounds.left - 139 {
                    return LRESULT(if x >= bounds.right - bounds.left - 47 {
                        HTCLOSE
                    } else if x >= bounds.right - bounds.left - 93 {
                        HTMAXBUTTON
                    } else {
                        HTMINBUTTON
                    } as isize);
                }
                return hit;
            }
            if message == WM_NCLBUTTONDOWN
                && matches!(wparam.0 as u32, HTCLOSE | HTMAXBUTTON | HTMINBUTTON)
            {
                let command = match wparam.0 as u32 {
                    HTCLOSE => SC_CLOSE,
                    HTMINBUTTON => SC_MINIMIZE,
                    _ => {
                        if IsZoomed(hwnd).as_bool() {
                            SC_RESTORE
                        } else {
                            SC_MAXIMIZE
                        }
                    }
                };
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_SYSCOMMAND,
                    WPARAM(command as usize),
                    LPARAM(0),
                );
                return LRESULT(0);
            }
            if matches!(message, WM_NCPAINT | WM_NCACTIVATE | WM_PRINT) {
                let result = DefWindowProcW(hwnd, message, wparam, lparam);
                if let Ok(state) = (*cell).try_borrow() {
                    let dc = if message == WM_PRINT {
                        HDC(wparam.0 as *mut _)
                    } else {
                        GetWindowDC(Some(hwnd))
                    };
                    if !dc.is_invalid() {
                        caption(&state, dc);
                        if message != WM_PRINT {
                            ReleaseDC(Some(hwnd), dc);
                        }
                    }
                }
                return result;
            }
            if message == WM_NOTIFY && lparam.0 != 0 {
                let notification = &*(lparam.0 as *const NMHDR);
                if notification.code == NM_CUSTOMDRAW
                    && let Ok(state) = (*cell).try_borrow()
                    && notification.hwndFrom == state.list
                {
                    let draw = &mut *(lparam.0 as *mut NMLVCUSTOMDRAW);
                    match draw.nmcd.dwDrawStage {
                        CDDS_PREPAINT => return LRESULT(CDRF_NOTIFYITEMDRAW as isize),
                        CDDS_ITEMPREPAINT => {
                            return LRESULT(CDRF_NOTIFYSUBITEMDRAW as isize);
                        }
                        // Keep the native cell renderer. Geometry/selection
                        // queries re-entering the list from this paint callback
                        // caused a user32 callback crash on the Session 0 desktop.
                        stage if stage.0 == CDDS_ITEMPREPAINT.0 | CDDS_SUBITEM.0 => {
                            if let Some(row) = state.rows.get(draw.nmcd.dwItemSpec) {
                                draw.clrText = if row.section {
                                    BLUE
                                } else {
                                    COLORREF(0x222222)
                                };
                                draw.clrTextBk = if !row.section
                                    && state.tab == Tab::Processes
                                    && draw.iSubItem >= 2
                                    && !state.compact
                                {
                                    let heat = row
                                        .heat
                                        .get(draw.iSubItem as usize)
                                        .copied()
                                        .unwrap_or(0.0)
                                        .sqrt()
                                        .clamp(0.0, 1.0);
                                    COLORREF(
                                        255 | ((249.0 - 65.0 * heat) as u32) << 8
                                            | ((215.0 - 170.0 * heat) as u32) << 16,
                                    )
                                } else {
                                    WHITE
                                };
                                SelectObject(
                                    draw.nmcd.hdc,
                                    if row.section {
                                        state.heading_font
                                    } else {
                                        state.font
                                    }
                                    .into(),
                                );
                                return LRESULT(CDRF_NEWFONT as isize);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if let Ok(mut state) = (*cell).try_borrow_mut() {
                let old_notice = state.notice.clone();
                let mut handled = true;
                let result = match message {
                    WM_MOUSEMOVE => {
                        if let Some((edge, origin, mut bounds)) = state.resizing {
                            let mut point = POINT {
                                x: lparam.0 as i16 as i32,
                                y: (lparam.0 >> 16) as i16 as i32,
                            };
                            let _ = ClientToScreen(hwnd, &mut point);
                            let (dx, dy) = (point.x - origin.x, point.y - origin.y);
                            let (min_width, min_height) = if state.compact {
                                (280, 200)
                            } else {
                                (650, 390)
                            };
                            if matches!(edge, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT) {
                                bounds.left = (bounds.left + dx).min(bounds.right - min_width);
                            }
                            if matches!(edge, HTRIGHT | HTTOPRIGHT | HTBOTTOMRIGHT) {
                                bounds.right = (bounds.right + dx).max(bounds.left + min_width);
                            }
                            if matches!(edge, HTTOP | HTTOPLEFT | HTTOPRIGHT) {
                                bounds.top = (bounds.top + dy).min(bounds.bottom - min_height);
                            }
                            if matches!(edge, HTBOTTOM | HTBOTTOMLEFT | HTBOTTOMRIGHT) {
                                bounds.bottom = (bounds.bottom + dy).max(bounds.top + min_height);
                            }
                            let _ = SetWindowPos(
                                hwnd,
                                None,
                                bounds.left,
                                bounds.top,
                                bounds.right - bounds.left,
                                bounds.bottom - bounds.top,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                            state.layout();
                        }
                        Ok(())
                    }
                    WM_LBUTTONUP | WM_CANCELMODE => {
                        state.resizing = None;
                        Ok(())
                    }
                    WM_TIMER => {
                        if wparam.0 == 1 {
                            state.refresh();
                        }
                        state.poll()
                    }
                    WM_SIZE => {
                        state.layout();
                        Ok(())
                    }
                    WM_COMMAND => state.command(wparam.0 & 0xffff),
                    WM_NOTIFY if lparam.0 != 0 => {
                        let notification = &*(lparam.0 as *const NMHDR);
                        if notification.hwndFrom == state.tabs && notification.code == TCN_SELCHANGE
                        {
                            let index = SendMessageW(state.tabs, TCM_GETCURSEL, None, None).0;
                            state.change_tab(Tab::from_index(index as usize));
                        } else if notification.hwndFrom == state.list {
                            match notification.code {
                                LVN_COLUMNCLICK => {
                                    let info = &*(lparam.0 as *const NMLISTVIEW);
                                    let column = info.iSubItem as usize;
                                    if state.sort == column {
                                        state.descending = !state.descending;
                                    } else {
                                        state.sort = column;
                                        state.descending = false;
                                    }
                                    state.rebuild();
                                }
                                NM_DBLCLK => state.expand(),
                                NM_CLICK => {
                                    let info = &*(lparam.0 as *const NMITEMACTIVATE);
                                    if info.ptAction.x < 28 {
                                        state.expand();
                                    }
                                }
                                LVN_KEYDOWN => {
                                    let info = &*(lparam.0 as *const NMLVKEYDOWN);
                                    match info.wVKey {
                                        0x2e => {
                                            let _ = PostMessageW(
                                                Some(hwnd),
                                                WM_COMMAND,
                                                WPARAM(END_TASK),
                                                LPARAM(0),
                                            );
                                        }
                                        0x25 | 0x27 | 0x0d => state.expand(),
                                        0x74 => state.refresh(),
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                        Ok(())
                    }
                    WM_CONTEXTMENU => {
                        let point = if lparam.0 == -1 {
                            let mut p = POINT::default();
                            let _ = GetCursorPos(&mut p);
                            p
                        } else {
                            POINT {
                                x: lparam.0 as i16 as i32,
                                y: (lparam.0 >> 16) as i16 as i32,
                            }
                        };
                        context_menu(&state, point);
                        Ok(())
                    }
                    _ => {
                        handled = false;
                        Ok(())
                    }
                };
                if let Err(e) = result {
                    state.notice = format!("{e:#}");
                }
                if handled {
                    state.footer();
                    if message == WM_COMMAND || state.notice != old_notice {
                        state.layout();
                    }
                    return LRESULT(0);
                }
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

unsafe fn menu() -> anyhow::Result<HMENU> {
    unsafe {
        let bar = CreateMenu()?;
        for (label, entries) in [
            ("&File", vec![(RUN_TASK, "Run &new task"), (EXIT, "E&xit")]),
            ("&Options", vec![(TOPMOST, "Always on &top")]),
            (
                "&View",
                vec![
                    (REFRESH, "&Refresh now\tF5"),
                    (SPEED_HIGH, "Update speed: High"),
                    (SPEED_NORMAL, "Update speed: Normal"),
                    (SPEED_LOW, "Update speed: Low"),
                    (SPEED_PAUSED, "Update speed: Paused"),
                    (GROUP, "Group by type"),
                ],
            ),
        ] {
            let submenu = CreatePopupMenu()?;
            for (id, label) in entries {
                let text = wide(label);
                AppendMenuW(submenu, MF_STRING, id, PCWSTR(text.as_ptr()))?;
            }
            let text = wide(label);
            AppendMenuW(bar, MF_POPUP, submenu.0 as usize, PCWSTR(text.as_ptr()))?;
        }
        Ok(bar)
    }
}
unsafe fn control(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    id: usize,
    style: WINDOW_STYLE,
    font: HFONT,
) -> anyhow::Result<HWND> {
    unsafe {
        let text = wide(text);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(text.as_ptr()),
            WS_CHILD | style,
            0,
            0,
            100,
            25,
            Some(parent),
            Some(HMENU(id as *mut _)),
            None,
            None,
        )?;
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        Ok(hwnd)
    }
}

fn create_window() -> anyhow::Result<Box<RefCell<State>>> {
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_TAB_CLASSES,
        })
        .ok()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: w!("MeshRMMBackgroundTasks"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            ensure!(
                GetLastError() == ERROR_CLASS_ALREADY_EXISTS,
                "Could not register Task Manager"
            );
        }
        let panel = WNDCLASSW {
            lpfnWndProc: Some(panel_proc),
            lpszClassName: w!("MeshRMMTaskPanel"),
            hCursor: class.hCursor,
            ..Default::default()
        };
        if RegisterClassW(&panel) == 0 {
            ensure!(
                GetLastError() == ERROR_CLASS_ALREADY_EXISTS,
                "Could not register Task Manager panels"
            );
        }
        let font = CreateFontW(
            -12,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        let hwnd = CreateWindowExW(
            WS_EX_COMPOSITED,
            class.lpszClassName,
            w!("Task Manager"),
            WS_OVERLAPPEDWINDOW,
            40,
            24,
            650,
            480,
            None,
            Some(menu()?),
            None,
            None,
        )?;
        let list = control(
            hwnd,
            w!("SysListView32"),
            "",
            LIST,
            WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS),
            font,
        )?;
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            None,
            Some(LPARAM(
                (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP) as isize,
            )),
        );
        SendMessageW(list, LVM_SETBKCOLOR, None, Some(LPARAM(WHITE.0 as isize)));
        SendMessageW(
            list,
            LVM_SETTEXTBKCOLOR,
            None,
            Some(LPARAM(WHITE.0 as isize)),
        );
        let images = ImageList_Create(16, 28, ILC_COLOR24 | ILC_MASK, 1, 16);
        padded_icon(images, LoadIconW(None, IDI_APPLICATION)?);
        SendMessageW(
            list,
            LVM_SETIMAGELIST,
            Some(WPARAM(LVSIL_SMALL as usize)),
            Some(LPARAM(images.0 as isize)),
        );
        let tabs = control(
            hwnd,
            w!("SysTabControl32"),
            "",
            TABS,
            WS_VISIBLE | WS_TABSTOP,
            font,
        )?;
        for (index, name) in TAB_NAMES.iter().enumerate() {
            let mut text = wide(name);
            let item = TCITEMW {
                mask: TCIF_TEXT,
                pszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            SendMessageW(
                tabs,
                TCM_INSERTITEMW,
                Some(WPARAM(index)),
                Some(LPARAM((&item as *const TCITEMW) as isize)),
            );
        }
        let scroll_input = Box::into_raw(Box::new(Cell::new(None::<ScrollInput>)));
        if !SetWindowSubclass(list, Some(list_paint), 1, scroll_input as usize).as_bool() {
            drop(Box::from_raw(scroll_input));
            anyhow::bail!("Could not initialize captured list scrollbars");
        }
        let status = control(
            hwnd,
            w!("STATIC"),
            "Loading processes…",
            109,
            WS_VISIBLE,
            font,
        )?;
        for (id, label) in [
            (COMPACT, "⌃  Fewer details"),
            (REFRESH, "Refresh"),
            (END_TASK, "End task"),
            (CANCEL, "Cancel"),
            (RUN, "Run as SYSTEM"),
        ] {
            control(
                hwnd,
                w!("BUTTON"),
                label,
                id,
                (if id == REFRESH {
                    WINDOW_STYLE(0)
                } else {
                    WS_VISIBLE
                }) | WS_TABSTOP
                    | if id == COMPACT {
                        WINDOW_STYLE(BS_OWNERDRAW as u32)
                    } else {
                        WINDOW_STYLE(0)
                    },
                font,
            )?;
        }
        control(
            hwnd,
            w!("EDIT"),
            "",
            RUN_EDIT,
            WS_TABSTOP | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            font,
        )?;
        let header = control(hwnd, panel.lpszClassName, "", HEADER, WS_VISIBLE, font)?;
        let graph = control(hwnd, panel.lpszClassName, "", GRAPH, WINDOW_STYLE(0), font)?;
        let state = Box::new(RefCell::new(State {
            hwnd,
            menu: GetMenu(hwnd),
            list,
            tabs,
            status,
            header,
            graph,
            font,
            images,
            icon_indices: HashMap::new(),
            heading_font: CreateFontW(
                -16,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32,
                w!("Segoe UI"),
            ),
            rows: Vec::new(),
            snapshot: Snapshot::default(),
            receiver: None,
            action: None,
            pending: None,
            notice: String::new(),
            tab: Tab::Processes,
            compact: false,
            resizing: None,
            expanded_size: (650, 480),
            grouped: true,
            expanded: HashSet::new(),
            sort: 0,
            descending: false,
            interval: 1000,
            tick: 0,
            run_visible: false,
            topmost: false,
            samples: VecDeque::new(),
            performance: 0,
            history: BTreeMap::new(),
            history_baseline: HashMap::new(),
        }));
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            (&*state as *const RefCell<State>) as isize,
        );
        state.borrow_mut().configure();
        Ok(state)
    }
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
mod tests {
    use super::*;
    fn fixture() -> Box<RefCell<State>> {
        let state = create_window().unwrap();
        state.borrow_mut().snapshot = Snapshot {
            memory_total: 8 * 1024 * 1024 * 1024,
            processes: (0..100)
                .map(|i| Process {
                    pid: 1000 + i,
                    created: Some(i as u64 + 1),
                    name: format!("Process {i:03}.exe"),
                    description: format!("Process {i:03}"),
                    memory: Some(i as usize * 1024 * 1024),
                    cpu: Some(i as f64 / 10.0),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        state.borrow_mut().rebuild();
        state
    }
    fn cleanup(state: Box<RefCell<State>>) {
        let state = state.into_inner();
        unsafe {
            SetWindowLongPtrW(state.hwnd, GWLP_USERDATA, 0);
            DestroyWindow(state.hwnd).unwrap();
            let _ = ImageList_Destroy(Some(state.images));
            let _ = DeleteObject(state.font.into());
            let _ = DeleteObject(state.heading_font.into());
        }
    }
    #[test]
    fn padded_icons_keep_color_and_transparent_row_padding() {
        unsafe {
            let images = ImageList_Create(16, 28, ILC_COLOR24 | ILC_MASK, 1, 1);
            let source = LoadIconW(None, IDI_APPLICATION).unwrap();
            assert_eq!(padded_icon(images, source), 0);
            let icon = ImageList_GetIcon(images, 0, ILD_TRANSPARENT);
            let mut info = ICONINFO::default();
            GetIconInfo(icon, &mut info).unwrap();
            let dc = CreateCompatibleDC(None);
            let mut bitmap = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 16,
                    biHeight: -28,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u8; 16 * 28 * 4];
            let count = GetDIBits(
                dc,
                info.hbmColor,
                0,
                28,
                Some(pixels.as_mut_ptr().cast()),
                &mut bitmap,
                DIB_RGB_COLORS,
            );
            let _ = DeleteDC(dc);
            let _ = DeleteObject(info.hbmColor.into());
            let _ = DeleteObject(info.hbmMask.into());
            let _ = DestroyIcon(icon);
            let _ = ImageList_Destroy(Some(images));
            assert_eq!(count, 28);
            assert!(
                pixels[6 * 16 * 4..22 * 16 * 4]
                    .chunks_exact(4)
                    .any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0),
                "Icon became a solid black block"
            );
            assert!(
                pixels[..6 * 16 * 4]
                    .chunks_exact(4)
                    .all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0),
                "Row padding must stay transparent"
            );
        }
    }

    #[test]
    fn refresh_preserves_selected_identity_and_scroll_anchor() {
        let state = fixture();
        {
            let mut s = state.borrow_mut();
            s.tab = Tab::Details;
            s.configure();
            s.select(70);
            let selected = s.selected().unwrap().key.clone();
            let top = unsafe { SendMessageW(s.list, LVM_GETTOPINDEX, None, None).0 };
            assert!(top > 0);
            let anchor = s.rows[top as usize].key.clone();
            s.snapshot.processes.push(Process {
                pid: 999,
                created: Some(1),
                name: "A newly started process".into(),
                ..Default::default()
            });
            s.rebuild();
            assert_eq!(s.selected().unwrap().key, selected);
            let top = unsafe { SendMessageW(s.list, LVM_GETTOPINDEX, None, None).0 };
            assert_eq!(s.rows[top as usize].key, anchor);
            let p = s
                .snapshot
                .processes
                .iter_mut()
                .find(|p| process_key(p) == selected)
                .unwrap();
            p.created = Some(9000);
            s.rebuild();
            assert!(
                s.selected().is_none(),
                "A reused PID must not inherit selection"
            );
        }
        cleanup(state);
    }
    #[test]
    fn numeric_sort_and_expand_groups() {
        let state = fixture();
        {
            let mut s = state.borrow_mut();
            s.grouped = false;
            s.sort = 3;
            s.descending = true;
            let rows = s.make_rows();
            assert_eq!(rows[0].cells[3], "99.0 MB");
            s.snapshot.processes[0].description = "Same app".into();
            s.snapshot.processes[1].description = "Same app".into();
            s.rebuild();
            let index = s
                .rows
                .iter()
                .position(|r| r.cells[0] == "Same app (2)")
                .unwrap();
            s.select(index);
            s.expand();
            assert_eq!(s.rows.iter().filter(|r| r.indent == 1).count(), 2);
            let group = s
                .rows
                .iter()
                .find(|r| r.cells[0] == "Same app (2)")
                .unwrap();
            assert_eq!(group.cells[3], "1.0 MB");
            s.expand();
            assert_eq!(s.rows.iter().filter(|r| r.indent == 1).count(), 0);
        }
        cleanup(state);
    }
    #[test]
    fn tabs_compact_mode_and_paused_refresh() {
        let state = fixture();
        {
            let mut s = state.borrow_mut();
            for index in 0..7 {
                s.tab = Tab::from_index(index);
                s.configure();
                assert_eq!(s.tab.index(), index);
                let style = unsafe { GetWindowLongW(s.list, GWL_STYLE) as u32 };
                assert_eq!(style & WS_VISIBLE.0 != 0, s.tab != Tab::Performance);
                if s.tab == Tab::Details {
                    assert_ne!(style & WS_HSCROLL.0, 0, "Wide columns must be reachable");
                }
            }
            s.command(COMPACT).unwrap();
            assert!(s.compact);
            assert_eq!(s.tab, Tab::Processes);
            s.command(COMPACT).unwrap();
            assert!(!s.compact);
            s.command(SPEED_PAUSED).unwrap();
            assert_eq!(s.interval, 0);
            s.command(SPEED_NORMAL).unwrap();
            assert_eq!(s.interval, 1000);
        }
        cleanup(state);
    }
}
