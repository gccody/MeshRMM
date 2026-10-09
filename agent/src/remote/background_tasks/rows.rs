//! List rows for each Task Manager tab, built from the latest snapshot.
use super::*;

impl State {
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
    pub(super) fn make_rows(&self) -> Vec<Row> {
        let mut rows = match self.tab {
            Tab::Processes => self.process_rows(),
            Tab::Details => self.detail_rows(),
            Tab::Services => self.service_rows(),
            Tab::Startup => self.startup_rows(),
            Tab::Users => self.user_rows(),
            Tab::History => self.history_rows(),
            Tab::Performance => Vec::new(),
        };
        if !matches!(self.tab, Tab::Processes | Tab::Users) {
            self.sort_rows(&mut rows);
        }
        rows
    }
    /// Processes with a visible top-level window, plus their descendants.
    fn app_processes(&self) -> HashSet<u32> {
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
        visible
    }
    /// Process indices keyed by (category, description), where the category is
    /// apps, background processes or Windows processes.
    fn process_groups(&self) -> BTreeMap<(u8, String), Vec<usize>> {
        let visible = self.app_processes();
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
            } else if p.path.to_lowercase().starts_with(&format!("{system}\\")) || p.pid == 4 {
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
        groups
    }
    fn process_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let groups = self.process_groups();
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
                members.push(self.process_row(indices.clone(), key, label, 0, indices.len() > 1));
            }
            self.sort_rows(&mut members);
            if !members.is_empty() && self.grouped && !self.compact {
                let title =
                    ["Apps", "Background processes", "Windows processes"][category as usize];
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
        rows
    }
    fn detail_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
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
                    p.io_rate
                        .map_or_else(|| "—".into(), |v| format!("{:.1} MB/s", v / 1048576.0)),
                    p.session.map_or_else(|| "—".into(), |v| v.to_string()),
                    priority(p.priority).into(),
                    p.path.clone(),
                ],
            );
            row.processes.push(index);
            rows.push(row);
        }
        rows
    }
    fn service_rows(&self) -> Vec<Row> {
        self.snapshot
            .services
            .iter()
            .map(|s| {
                Row::plain(
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
                )
            })
            .collect()
    }
    fn startup_rows(&self) -> Vec<Row> {
        self.snapshot
            .startups
            .iter()
            .map(|s| {
                Row::plain(
                    Key::Startup(s.root.clone(), s.run_key.clone(), s.name.clone()),
                    vec![
                        s.name.clone(),
                        s.location.clone(),
                        if s.enabled { "Enabled" } else { "Disabled" }.into(),
                        "Not measured".into(),
                        s.command.clone(),
                    ],
                )
            })
            .collect()
    }
    fn user_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
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
        rows
    }
    fn history_rows(&self) -> Vec<Row> {
        self.history
            .iter()
            .map(|(name, h)| {
                Row::plain(
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
                )
            })
            .collect()
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
