// GUI subsystem: no console window on launch. Startup tlog output still
// reaches stderr when one is attached (e.g. `tm-app 2>log` from a shell).
#![windows_subsystem = "windows"]

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    actions, div, px, rgb, size, App, Application, Bounds, ClipboardItem, Context, Entity,
    SharedString, Window, WindowBounds, WindowOptions,
};
use gpui::prelude::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::PopupMenu;
use gpui_component::table::{Column, Table, TableDelegate, TableState};
use gpui_component::theme::{Theme, ThemeMode};
use gpui_component::Root;
use tm_collector::{fmt_bytes, Sampler, Snapshot, SystemStats};

actions!(tm, [EndTask, CopyPid, CopyName]);

static START: OnceLock<Instant> = OnceLock::new();

/// Startup timing to stderr; invisible in normal use, `tm-app 2>log` to see.
fn tlog(label: &str) {
    let start = *START.get_or_init(Instant::now);
    eprintln!("[startup {:>10.1?}] {label}", start.elapsed());
}

const BG_HEADER: u32 = 0x1e1e2e;
const TEXT: u32 = 0xcdd6f4;
const TEXT_DIM: u32 = 0x7f849c;
const ACCENT: u32 = 0x89b4fa;

/// Disk rates in fixed MiB/s, Task Manager style.
fn fmt_mibs(bytes_per_sec: u64) -> String {
    format!("{:.2} MiB/s", bytes_per_sec as f64 / (1024.0 * 1024.0))
}

/// Network rates in fixed megabits/sec, Task Manager style.
fn fmt_mbps(bytes_per_sec: u64) -> String {
    format!("{:.2} Mbps", bytes_per_sec as f64 * 8.0 / 1_000_000.0)
}

/// One display row. All strings are formatted exactly once, when the snapshot
/// arrives; render only clones refcounted `SharedString`s.
#[derive(Clone)]
struct ProcRow {
    /// Display name: file description when known, exe name otherwise.
    name: SharedString,
    /// The executable file name.
    exe_s: SharedString,
    icon: Option<std::sync::Arc<Vec<u8>>>,
    pid: u32,
    parent: u32,
    create: i64,
    pid_s: SharedString,
    user_s: SharedString,
    cpu: f32,
    cpu_s: SharedString,
    gpu: f32,
    gpu_s: SharedString,
    has_window: bool,
    /// Image under %SystemRoot% (or pathless kernel process).
    windows_dir: bool,
    session: u32,
    mem: u64,
    mem_s: SharedString,
    disk: u64,
    disk_s: SharedString,
    net: u64,
    net_s: SharedString,
    threads: u32,
    threads_s: SharedString,
    handles: u32,
    handles_s: SharedString,
}

/// A visible table row: either a collapsible section header or a process.
#[derive(Clone)]
enum Row {
    Section {
        label: SharedString,
        collapsed: bool,
        /// 0 = Apps, 1 = Background, 2 = Windows processes.
        id: u8,
    },
    Proc {
        row: ProcRow,
        /// Indented child of an expanded group.
        child: bool,
        /// For group rows: (expanded, member count, toggle key).
        group: Option<(bool, usize, SharedString)>,
    },
}

/// Aggregated totals shown on the second header line, Task Manager style.
#[derive(Default)]
struct HeaderTotals {
    cpu: SharedString,
    gpu: SharedString,
    mem: SharedString,
    disk: SharedString,
    net: SharedString,
}

struct ProcessTableDelegate {
    columns: Vec<Column>,
    totals: HeaderTotals,
    /// pid → (source-bytes identity, decoded image), so gpui sees a stable
    /// image id per icon instead of re-decoding every refresh.
    icon_cache: rustc_hash::FxHashMap<u32, (usize, std::sync::Arc<gpui::Image>)>,
    /// Every process from the latest snapshot, unfiltered and unsorted.
    all_rows: Vec<ProcRow>,
    /// The visible view: sections + filtered, sorted processes.
    rows: Vec<Row>,
    /// (column key, ascending); keyed by name, not index, so column
    /// drag-reordering can't desync sorting from data.
    sort: Option<(SharedString, bool)>,
    /// Lowercased needle matched against name/user/description/PID.
    filter: String,
    /// Row the open context menu refers to: (pid, name).
    menu_row: Option<(u32, SharedString)>,
    /// Collapse state for the Apps/Background/Windows sections.
    collapsed: [bool; 3],
    /// Group keys (app-root pid or section:name) whose members are shown.
    expanded: rustc_hash::FxHashSet<String>,
}

impl ProcessTableDelegate {
    fn new() -> Self {
        ProcessTableDelegate {
            // Sorting is fully delegate-owned (see render_th/toggle_sort):
            // gpui-component's built-in sort only triggers on a small header
            // icon, which is undiscoverable — we make the whole header cell
            // clickable instead.
            columns: vec![
                Column::new("name", "Name").width(px(240.)),
                Column::new("pid", "PID").width(px(80.)).text_right(),
                Column::new("user", "User").width(px(110.)),
                Column::new("cpu", "CPU").width(px(80.)).text_right(),
                Column::new("gpu", "GPU").width(px(80.)).text_right(),
                Column::new("mem", "Memory").width(px(110.)).text_right(),
                Column::new("disk", "Disk").width(px(110.)).text_right(),
                Column::new("net", "Network").width(px(110.)).text_right(),
                Column::new("threads", "Threads").width(px(80.)).text_right(),
                Column::new("handles", "Handles").width(px(80.)).text_right(),
                Column::new("exe", "Process name").width(px(280.)),
            ],
            all_rows: Vec::new(),
            totals: HeaderTotals::default(),
            icon_cache: rustc_hash::FxHashMap::default(),
            rows: Vec::new(),
            sort: Some(("pid".into(), true)),
            filter: String::new(),
            menu_row: None,
            collapsed: [false; 3],
            expanded: rustc_hash::FxHashSet::default(),
        }
    }

    fn set_snapshot(&mut self, snap: &Snapshot) {
        let gpu_total: f32 = snap.processes.iter().map(|p| p.gpu_percent).sum();
        let disk_total: u64 = snap.processes.iter().map(|p| p.disk_bytes_per_sec).sum();
        let net_total: u64 = snap.processes.iter().map(|p| p.net_bytes_per_sec).sum();
        self.totals = HeaderTotals {
            cpu: format!("{:.1}%", snap.system.cpu_percent).into(),
            gpu: format!("{gpu_total:.1}%").into(),
            mem: format!("{:.1}%", snap.system.mem_percent()).into(),
            disk: fmt_mibs(disk_total).into(),
            net: fmt_mbps(net_total).into(),
        };
        self.all_rows.clear();
        self.all_rows.reserve(snap.processes.len());
        for p in snap.processes.iter().filter(|p| p.raw.pid != 0) {
            let (user, desc, icon, is_windows) = p
                .enriched
                .as_ref()
                .map(|e| {
                    (
                        e.user.to_string(),
                        e.description.to_string(),
                        e.icon_png.clone(),
                        e.windows_process,
                    )
                })
                .unwrap_or_default();
            let exe = p.raw.name.to_string();
            self.all_rows.push(ProcRow {
                name: if desc.is_empty() {
                    exe.clone().into()
                } else {
                    desc.into()
                },
                exe_s: exe.into(),
                icon,
                pid: p.raw.pid,
                parent: p.raw.parent_pid,
                create: p.raw.create_time,
                pid_s: p.raw.pid.to_string().into(),
                user_s: user.into(),
                cpu: p.cpu_percent,
                cpu_s: format!("{:.1}%", p.cpu_percent).into(),
                gpu: p.gpu_percent,
                gpu_s: format!("{:.1}%", p.gpu_percent).into(),
                has_window: p.has_window,
                windows_dir: is_windows,
                session: p.raw.session_id,
                mem: p.raw.private_working_set,
                mem_s: fmt_bytes(p.raw.private_working_set).into(),
                disk: p.disk_bytes_per_sec,
                disk_s: fmt_mibs(p.disk_bytes_per_sec).into(),
                net: p.net_bytes_per_sec,
                net_s: fmt_mbps(p.net_bytes_per_sec).into(),
                threads: p.raw.threads,
                threads_s: p.raw.threads.to_string().into(),
                handles: p.raw.handles,
                handles_s: p.raw.handles.to_string().into(),
            });
        }
        let live: rustc_hash::FxHashSet<u32> = self.all_rows.iter().map(|r| r.pid).collect();
        self.icon_cache.retain(|pid, _| live.contains(pid));
        self.rebuild_view();
    }

    fn set_filter(&mut self, needle: &str) {
        self.filter = needle.trim().to_lowercase();
        self.rebuild_view();
    }

    fn matches(&self, row: &ProcRow) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        row.name.as_ref().to_lowercase().contains(&self.filter)
            || row.exe_s.as_ref().to_lowercase().contains(&self.filter)
            || row.user_s.as_ref().to_lowercase().contains(&self.filter)
            || row.pid_s.as_ref().contains(&self.filter)
    }

    fn is_text_column(key: &str) -> bool {
        matches!(key, "name" | "user" | "exe")
    }

    /// Text columns (name/user/description) sort ascending on first click;
    /// numeric columns descending, Task Manager style. Clicking the active
    /// column flips direction.
    fn toggle_sort(&mut self, col_ix: usize) {
        let key = self.columns[col_ix].key.clone();
        let text_col = Self::is_text_column(key.as_ref());
        self.sort = match &self.sort {
            Some((k, asc)) if *k == key => Some((key, !asc)),
            _ => Some((key, text_col)),
        };
        self.rebuild_view();
    }

    /// Decode-once icon lookup: gpui identifies images by id, so handing it
    /// a fresh Image every refresh would re-decode and re-upload each tick.
    fn icon_image(&mut self, pid: u32, bytes: &std::sync::Arc<Vec<u8>>) -> std::sync::Arc<gpui::Image> {
        let identity = std::sync::Arc::as_ptr(bytes) as usize;
        if let Some((cached_id, image)) = self.icon_cache.get(&pid) {
            if *cached_id == identity {
                return image.clone();
            }
        }
        let image = std::sync::Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            (**bytes).clone(),
        ));
        self.icon_cache.insert(pid, (identity, image.clone()));
        image
    }

    /// "Windows processes" = OS core and services, not merely "lives in the
    /// Windows folder": session-0 system processes plus the few core
    /// session-bound components. Per-user helpers from the Windows dir
    /// (RuntimeBroker, conhost, cmd, dllhost, ...) belong in Background,
    /// matching Task Manager.
    fn is_windows_section(row: &ProcRow) -> bool {
        const CORE: [&str; 6] = [
            "dwm.exe",
            "csrss.exe",
            "fontdrvhost.exe",
            "winlogon.exe",
            "smss.exe",
            "LogonUI.exe",
        ];
        row.windows_dir
            && (row.session == 0
                || CORE
                    .iter()
                    .any(|c| row.exe_s.as_ref().eq_ignore_ascii_case(c)))
    }

    /// Fold `others` into `base`: summed metrics, "(N)" name suffix and
    /// "pid +K" pid display.
    fn aggregate(base: ProcRow, others: &[ProcRow]) -> ProcRow {
        let mut agg = base;
        if others.is_empty() {
            return agg;
        }
        for c in others {
            agg.cpu += c.cpu;
            agg.gpu += c.gpu;
            agg.mem += c.mem;
            agg.disk += c.disk;
            agg.net += c.net;
            agg.threads += c.threads;
            agg.handles += c.handles;
        }
        agg.cpu_s = format!("{:.1}%", agg.cpu).into();
        agg.gpu_s = format!("{:.1}%", agg.gpu).into();
        agg.mem_s = fmt_bytes(agg.mem).into();
        agg.disk_s = fmt_mibs(agg.disk).into();
        agg.net_s = fmt_mbps(agg.net).into();
        agg.threads_s = agg.threads.to_string().into();
        agg.handles_s = agg.handles.to_string().into();
        agg.pid_s = format!("{} +{}", agg.pid, others.len()).into();
        agg.name = format!("{} ({})", agg.name, others.len() + 1).into();
        agg
    }

    /// Group a flat section's rows by exe name: duplicates become one
    /// aggregate row with the individuals as expandable members.
    fn name_groups(
        &self,
        procs: Vec<ProcRow>,
        section: u8,
    ) -> Vec<(ProcRow, Vec<ProcRow>, Option<SharedString>)> {
        let mut by_name: rustc_hash::FxHashMap<String, Vec<ProcRow>> =
            rustc_hash::FxHashMap::default();
        for row in procs {
            by_name
                .entry(row.exe_s.as_ref().to_ascii_lowercase())
                .or_default()
                .push(row);
        }
        let mut items: Vec<(ProcRow, Vec<ProcRow>, Option<SharedString>)> = by_name
            .into_iter()
            .map(|(name, mut members)| {
                if members.len() == 1 {
                    (members.pop().unwrap(), Vec::new(), None)
                } else {
                    self.sort_procs(&mut members);
                    let display = Self::aggregate(members[0].clone(), &members[1..]);
                    let key: SharedString = format!("{section}:{name}").into();
                    (display, members, Some(key))
                }
            })
            .collect();
        items.sort_by(|a, b| self.cmp_procs(&a.0, &b.0));
        items
    }

    fn toggle_section(&mut self, id: u8) {
        self.collapsed[id as usize] = !self.collapsed[id as usize];
        self.rebuild_view();
    }

    /// Nearest app-root ancestor for a process: itself if it owns a window,
    /// otherwise the first windowed ancestor (parent links validated by
    /// create-time ordering so a reused parent pid can't corrupt the tree).
    fn app_root_of(
        row: &ProcRow,
        by_pid: &rustc_hash::FxHashMap<u32, (i64, u32, bool)>,
    ) -> Option<u32> {
        if row.has_window {
            return Some(row.pid);
        }
        let (mut create, mut parent) = (row.create, row.parent);
        for _ in 0..64 {
            let &(p_create, p_parent, p_window) = by_pid.get(&parent)?;
            if p_create > create {
                return None; // parent pid was reused after we were born
            }
            if p_window {
                return Some(parent);
            }
            create = p_create;
            parent = p_parent;
        }
        None
    }

    fn rebuild_view(&mut self) {
        let mut filtered: Vec<ProcRow> = self
            .all_rows
            .iter()
            .filter(|r| self.matches(r))
            .cloned()
            .collect();
        // While filtering, show a flat list — groups just get in the way.
        if !self.filter.is_empty() {
            self.sort_procs(&mut filtered);
            self.rows = filtered
                .into_iter()
                .map(|row| Row::Proc {
                    row,
                    child: false,
                    group: None,
                })
                .collect();
            return;
        }

        let by_pid: rustc_hash::FxHashMap<u32, (i64, u32, bool)> = filtered
            .iter()
            .map(|r| (r.pid, (r.create, r.parent, r.has_window)))
            .collect();
        let mut groups: rustc_hash::FxHashMap<u32, Vec<ProcRow>> =
            rustc_hash::FxHashMap::default();
        let mut roots: Vec<ProcRow> = Vec::new();
        let mut bg: Vec<ProcRow> = Vec::new();
        let mut win: Vec<ProcRow> = Vec::new();
        for row in filtered {
            match Self::app_root_of(&row, &by_pid) {
                Some(root) if root == row.pid => roots.push(row),
                Some(root) => groups.entry(root).or_default().push(row),
                None if Self::is_windows_section(&row) => win.push(row),
                None => bg.push(row),
            }
        }

        // Aggregate each root over its descendants.
        let mut apps: Vec<(ProcRow, Vec<ProcRow>)> = roots
            .into_iter()
            .map(|root| {
                let mut children = groups.remove(&root.pid).unwrap_or_default();
                self.sort_procs(&mut children);
                let agg = Self::aggregate(root, &children);
                (agg, children)
            })
            .collect();
        // Orphaned children whose root got filtered out (shouldn't happen
        // without a filter, but stay safe) go to background.
        for (_, mut orphans) in groups.drain() {
            bg.append(&mut orphans);
        }

        apps.sort_by(|a, b| self.cmp_procs(&a.0, &b.0));
        let apps: Vec<(ProcRow, Vec<ProcRow>, Option<SharedString>)> = apps
            .into_iter()
            .map(|(root, children)| {
                let key: Option<SharedString> = if children.is_empty() {
                    None
                } else {
                    Some(root.pid.to_string().into())
                };
                (root, children, key)
            })
            .collect();
        let bg_count = bg.len();
        let win_count = win.len();
        let bg = self.name_groups(bg, 1);
        let win = self.name_groups(win, 2);

        let mut rows = Vec::new();
        for (id, label, count, items) in [
            (0u8, "Apps", apps.len(), apps),
            (1u8, "Background processes", bg_count, bg),
            (2u8, "Windows processes", win_count, win),
        ] {
            rows.push(Row::Section {
                label: format!("{label} ({count})").into(),
                collapsed: self.collapsed[id as usize],
                id,
            });
            if self.collapsed[id as usize] {
                continue;
            }
            for (display, members, key) in items {
                match key {
                    Some(key) => {
                        let expanded = self.expanded.contains(key.as_ref());
                        rows.push(Row::Proc {
                            row: display,
                            child: false,
                            group: Some((expanded, members.len(), key)),
                        });
                        if expanded {
                            rows.extend(members.into_iter().map(|row| Row::Proc {
                                row,
                                child: true,
                                group: None,
                            }));
                        }
                    }
                    None => rows.push(Row::Proc {
                        row: display,
                        child: false,
                        group: None,
                    }),
                }
            }
        }
        self.rows = rows;
    }

    fn sort_procs(&self, procs: &mut [ProcRow]) {
        if self.sort.is_some() {
            procs.sort_by(|a, b| self.cmp_procs(a, b));
        }
    }

    fn cmp_procs(&self, a: &ProcRow, b: &ProcRow) -> std::cmp::Ordering {
        let Some((key, asc)) = &self.sort else {
            return std::cmp::Ordering::Equal;
        };
        let asc = *asc;
        {
            let ord = match key.as_ref() {
                "name" => a
                    .name
                    .as_ref()
                    .to_ascii_lowercase()
                    .cmp(&b.name.as_ref().to_ascii_lowercase()),
                "pid" => a.pid.cmp(&b.pid),
                "user" => a
                    .user_s
                    .as_ref()
                    .to_ascii_lowercase()
                    .cmp(&b.user_s.as_ref().to_ascii_lowercase()),
                "cpu" => a.cpu.total_cmp(&b.cpu),
                "gpu" => a.gpu.total_cmp(&b.gpu),
                "mem" => a.mem.cmp(&b.mem),
                "disk" => a.disk.cmp(&b.disk),
                "net" => a.net.cmp(&b.net),
                "threads" => a.threads.cmp(&b.threads),
                "handles" => a.handles.cmp(&b.handles),
                _ => a
                    .exe_s
                    .as_ref()
                    .to_ascii_lowercase()
                    .cmp(&b.exe_s.as_ref().to_ascii_lowercase()),
            };
            if asc {
                ord
            } else {
                ord.reverse()
            }
        }
    }
}

impl TableDelegate for ProcessTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        match &self.rows[row_ix] {
            Row::Section {
                label,
                collapsed,
                id,
            } => {
                if col_ix != 0 {
                    return div().into_any_element();
                }
                let chevron = if *collapsed { "▶" } else { "▼" };
                let section_id = *id;
                div()
                    .id(("section", row_ix))
                    .flex()
                    .items_center()
                    .text_color(rgb(ACCENT))
                    .cursor_pointer()
                    .on_click(cx.listener(move |state, _, _, cx| {
                        state.delegate_mut().toggle_section(section_id);
                        cx.notify();
                    }))
                    .child(format!("{chevron}  {label}"))
                    .into_any_element()
            }
            Row::Proc { row, child, group } => {
                let (row, is_child, group) = (row.clone(), *child, group.clone());
                match self.columns[col_ix].key.as_ref() {
                    "name" => {
                        let icon = row
                            .icon
                            .as_ref()
                            .map(|bytes| self.icon_image(row.pid, bytes));
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .when(is_child, |d| d.pl(px(22.)))
                            .child(match group {
                                Some((expanded, _, key)) => div()
                                    .id(("expand", row_ix))
                                    .w(px(16.))
                                    .flex_none()
                                    .text_color(rgb(TEXT_DIM))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |state, _, _, cx| {
                                        let d = state.delegate_mut();
                                        let k = key.to_string();
                                        if !d.expanded.insert(k.clone()) {
                                            d.expanded.remove(&k);
                                        }
                                        d.rebuild_view();
                                        cx.notify();
                                    }))
                                    .child(if expanded { "▼" } else { "▶" })
                                    .into_any_element(),
                                None => div().w(px(16.)).flex_none().into_any_element(),
                            })
                            .child(match icon {
                                Some(image) => gpui::img(image)
                                    .w(px(16.))
                                    .h(px(16.))
                                    .flex_none()
                                    .into_any_element(),
                                None => div().w(px(16.)).flex_none().into_any_element(),
                            })
                            .child(row.name.clone())
                            .into_any_element()
                    }
                    "pid" => row.pid_s.clone().into_any_element(),
                    "user" => row.user_s.clone().into_any_element(),
                    "cpu" => row.cpu_s.clone().into_any_element(),
                    "gpu" => row.gpu_s.clone().into_any_element(),
                    "mem" => row.mem_s.clone().into_any_element(),
                    "disk" => row.disk_s.clone().into_any_element(),
                    "net" => row.net_s.clone().into_any_element(),
                    "threads" => row.threads_s.clone().into_any_element(),
                    "handles" => row.handles_s.clone().into_any_element(),
                    _ => row.exe_s.clone().into_any_element(),
                }
            }
        }
    }

    fn move_column(
        &mut self,
        col_ix: usize,
        to_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        // Mirror the framework's col_groups reorder so key lookups stay
        // aligned with visual positions.
        let col = self.columns.remove(col_ix);
        self.columns.insert(to_ix, col);
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        // Taller two-line header (name + aggregated total) in readable text
        // instead of the theme's muted header color.
        div().id("header").h(px(46.)).text_color(rgb(TEXT))
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let name = self.columns[col_ix].name.clone();
        let key = self.columns[col_ix].key.clone();
        let indicator = match &self.sort {
            Some((k, asc)) if *k == key => {
                if *asc {
                    " ▲"
                } else {
                    " ▼"
                }
            }
            _ => "",
        };
        let total = match key.as_ref() {
            "cpu" => Some(self.totals.cpu.clone()),
            "gpu" => Some(self.totals.gpu.clone()),
            "mem" => Some(self.totals.mem.clone()),
            "disk" => Some(self.totals.disk.clone()),
            "net" => Some(self.totals.net.clone()),
            _ => None,
        };
        let right_aligned = !Self::is_text_column(key.as_ref());
        div()
            .id(("proc-th", col_ix))
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .when(right_aligned, |d| d.items_end())
            .cursor_pointer()
            .on_click(cx.listener(move |state, _, _, cx| {
                state.delegate_mut().toggle_sort(col_ix);
                cx.notify();
            }))
            .child(format!("{name}{indicator}"))
            .when_some(total, |d, total| d.child(total))
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(Row::Proc { row, .. }) = self.rows.get(row_ix) else {
            return menu;
        };
        self.menu_row = Some((row.pid, row.name.clone()));
        menu.label(format!("{}  ({})", row.name, row.pid))
            .separator()
            .menu("End Task", Box::new(EndTask))
            .separator()
            .menu("Copy PID", Box::new(CopyPid))
            .menu("Copy Name", Box::new(CopyName))
    }

}

struct TaskManagerApp {
    table: Entity<TableState<ProcessTableDelegate>>,
    filter_input: Entity<InputState>,
    sys: SystemStats,
    status: Option<SharedString>,
    first_snapshot: bool,
}

/// True when our main window is minimized. The sampler thread discovers its
/// own process's top-level window and polls `IsIconic` — no cross-thread
/// plumbing with the UI needed.
fn window_minimized(hwnd: &mut isize) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsIconic, IsWindow,
    };
    unsafe {
        if *hwnd != 0 && IsWindow(*hwnd as _) == 0 {
            *hwnd = 0;
        }
        if *hwnd == 0 {
            unsafe extern "system" fn enum_cb(h: windows_sys::Win32::Foundation::HWND, l: isize) -> i32 {
                let mut pid = 0u32;
                GetWindowThreadProcessId(h, &mut pid);
                if pid == std::process::id() {
                    *(l as *mut isize) = h as isize;
                    return 0;
                }
                1
            }
            let _ = EnumWindows(Some(enum_cb), hwnd as *mut isize as isize);
        }
        *hwnd != 0 && IsIconic(*hwnd as _) != 0
    }
}

/// Spawn the sampler thread. Called first thing in `main`, before gpui
/// initializes DirectX/DirectWrite, so collection warms up in parallel with
/// window creation and data is already waiting when the first frame renders.
///
/// Cadence is adaptive: 1s while the window is visible, 2.5s while
/// minimized — the kernel process query is the app's dominant CPU cost, so
/// don't pay it for a window nobody can see. Restoring samples immediately.
fn spawn_sampler() -> futures::channel::mpsc::Receiver<Snapshot> {
    let (tx, rx) = futures::channel::mpsc::channel::<Snapshot>(2);
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let mut tx = tx;
            let mut sampler = Sampler::new();
            sampler.sample(); // baseline for deltas
            // Immediate snapshot (rates read 0) so the first frame has rows,
            // then a short-delta snapshot for real rates, then the loop.
            let _ = tx.try_send(sampler.sample());
            std::thread::sleep(Duration::from_millis(150));
            let _ = tx.try_send(sampler.sample());
            let mut hwnd: isize = 0;
            loop {
                let minimized = window_minimized(&mut hwnd);
                let interval = if minimized { 2500 } else { 1000 };
                // Sleep in slices so a restore is noticed within 250ms and
                // sampled immediately instead of finishing the long sleep.
                let mut slept = 0;
                while slept < interval {
                    std::thread::sleep(Duration::from_millis(250));
                    slept += 250;
                    if minimized && !window_minimized(&mut hwnd) {
                        break;
                    }
                }
                // try_send: if the UI is behind, drop this tick rather than
                // block the sampler.
                match tx.try_send(sampler.sample()) {
                    Err(e) if e.is_disconnected() => break,
                    _ => {}
                }
            }
        })
        .expect("spawn sampler thread");
    rx
}

impl TaskManagerApp {
    fn new(
        mut rx: futures::channel::mpsc::Receiver<Snapshot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| {
            TableState::new(ProcessTableDelegate::new(), window, cx).col_selectable(false)
        });

        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter by name, user, description, or PID")
        });
        cx.subscribe(&filter_input, |this: &mut Self, input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                let needle = input.read(cx).value().to_string();
                this.table.update(cx, |state, cx| {
                    state.delegate_mut().set_filter(&needle);
                    cx.notify();
                });
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            while let Some(snap) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    if !this.first_snapshot {
                        this.first_snapshot = true;
                        tlog("first snapshot applied");
                    }
                    this.sys = snap.system.clone();
                    this.table.update(cx, |state, cx| {
                        state.delegate_mut().set_snapshot(&snap);
                        cx.notify();
                    });
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            table,
            filter_input,
            sys: SystemStats::default(),
            status: None,
            first_snapshot: false,
        }
    }

    fn menu_row(&self, cx: &App) -> Option<(u32, SharedString)> {
        self.table.read(cx).delegate().menu_row.clone()
    }

    fn end_task(&mut self, cx: &mut Context<Self>) {
        let Some((pid, name)) = self.menu_row(cx) else {
            return;
        };
        self.status = Some(match tm_collector::kill_process(pid) {
            Ok(()) => format!("Ended {name} ({pid})").into(),
            Err(e) => format!("Could not end {name} ({pid}): {e}").into(),
        });
        cx.notify();
    }

    fn copy_to_clipboard(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}

impl Render for TaskManagerApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = &self.sys;
        let stats_bar = div()
            .flex()
            .flex_none()
            .gap(px(24.))
            .px(px(12.))
            .py(px(8.))
            .bg(rgb(BG_HEADER))
            .text_color(rgb(ACCENT))
            .text_size(px(13.))
            .child(format!("CPU {:.1}%", s.cpu_percent))
            .child(format!(
                "Mem {} / {} ({:.0}%)",
                fmt_bytes(s.mem_used()),
                fmt_bytes(s.mem_total),
                s.mem_percent()
            ))
            .child(div().text_color(rgb(TEXT_DIM)).child(format!(
                "{} processes · {} threads · {} handles",
                s.process_count, s.thread_count, s.handle_count
            )));

        let toolbar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .px(px(8.))
            .py(px(6.))
            .bg(rgb(BG_HEADER))
            .child(div().w(px(340.)).child(Input::new(&self.filter_input)))
            .when_some(self.status.clone(), |this, status| {
                this.child(div().text_color(rgb(TEXT_DIM)).child(status))
            });

        div()
            .flex()
            .flex_col()
            .size_full()
            .text_size(px(13.))
            .on_action(cx.listener(|this, _: &EndTask, _, cx| this.end_task(cx)))
            .on_action(cx.listener(|this, _: &CopyPid, _, cx| {
                if let Some((pid, _)) = this.menu_row(cx) {
                    this.copy_to_clipboard(pid.to_string(), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CopyName, _, cx| {
                if let Some((_, name)) = this.menu_row(cx) {
                    this.copy_to_clipboard(name.to_string(), cx);
                }
            }))
            .child(stats_bar)
            .child(toolbar)
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(Table::new(&self.table).stripe(true)),
            )
    }
}

fn main() {
    tlog("main entry");
    let rx = spawn_sampler();
    Application::new().run(move |cx: &mut App| {
        tlog("gpui run callback");
        gpui_component::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        tlog("gpui_component init done");
        let bounds = Bounds::centered(None, size(px(1400.), px(1200.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Task Manager".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                tlog("window opened");
                let view = cx.new(|cx| TaskManagerApp::new(rx, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
