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
    name: SharedString,
    pid: u32,
    pid_s: SharedString,
    user_s: SharedString,
    desc_s: SharedString,
    cpu: f32,
    cpu_s: SharedString,
    gpu: f32,
    gpu_s: SharedString,
    has_window: bool,
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
        apps: bool,
    },
    Proc(ProcRow),
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
    collapsed_apps: bool,
    collapsed_bg: bool,
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
                Column::new("desc", "Description").width(px(320.)),
            ],
            all_rows: Vec::new(),
            totals: HeaderTotals::default(),
            rows: Vec::new(),
            sort: Some(("pid".into(), true)),
            filter: String::new(),
            menu_row: None,
            collapsed_apps: false,
            collapsed_bg: false,
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
            let (user, desc) = p
                .enriched
                .as_ref()
                .map(|e| (e.user.to_string(), e.description.to_string()))
                .unwrap_or_default();
            self.all_rows.push(ProcRow {
                name: p.raw.name.to_string().into(),
                pid: p.raw.pid,
                pid_s: p.raw.pid.to_string().into(),
                user_s: user.into(),
                desc_s: desc.into(),
                cpu: p.cpu_percent,
                cpu_s: format!("{:.1}%", p.cpu_percent).into(),
                gpu: p.gpu_percent,
                gpu_s: format!("{:.1}%", p.gpu_percent).into(),
                has_window: p.has_window,
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
            || row.user_s.as_ref().to_lowercase().contains(&self.filter)
            || row.desc_s.as_ref().to_lowercase().contains(&self.filter)
            || row.pid_s.as_ref().contains(&self.filter)
    }

    fn is_text_column(key: &str) -> bool {
        matches!(key, "name" | "user" | "desc")
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

    fn toggle_section(&mut self, apps: bool) {
        if apps {
            self.collapsed_apps = !self.collapsed_apps;
        } else {
            self.collapsed_bg = !self.collapsed_bg;
        }
        self.rebuild_view();
    }

    fn rebuild_view(&mut self) {
        let mut filtered: Vec<ProcRow> = self
            .all_rows
            .iter()
            .filter(|r| self.matches(r))
            .cloned()
            .collect();
        self.sort_procs(&mut filtered);
        // While filtering, show a flat list — sections just get in the way.
        if !self.filter.is_empty() {
            self.rows = filtered.into_iter().map(Row::Proc).collect();
            return;
        }
        let (apps, bg): (Vec<_>, Vec<_>) = filtered.into_iter().partition(|r| r.has_window);
        let mut rows = Vec::with_capacity(apps.len() + bg.len() + 2);
        rows.push(Row::Section {
            label: format!("Apps ({})", apps.len()).into(),
            collapsed: self.collapsed_apps,
            apps: true,
        });
        if !self.collapsed_apps {
            rows.extend(apps.into_iter().map(Row::Proc));
        }
        rows.push(Row::Section {
            label: format!("Background processes ({})", bg.len()).into(),
            collapsed: self.collapsed_bg,
            apps: false,
        });
        if !self.collapsed_bg {
            rows.extend(bg.into_iter().map(Row::Proc));
        }
        self.rows = rows;
    }

    fn sort_procs(&self, procs: &mut [ProcRow]) {
        let Some((key, asc)) = &self.sort else { return };
        let asc = *asc;
        procs.sort_by(|a, b| {
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
                    .desc_s
                    .as_ref()
                    .to_ascii_lowercase()
                    .cmp(&b.desc_s.as_ref().to_ascii_lowercase()),
            };
            if asc {
                ord
            } else {
                ord.reverse()
            }
        });
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
                apps,
            } => {
                if col_ix != 0 {
                    return div().into_any_element();
                }
                let chevron = if *collapsed { "▶" } else { "▼" };
                let apps_flag = *apps;
                div()
                    .id(("section", row_ix))
                    .flex()
                    .items_center()
                    .text_color(rgb(ACCENT))
                    .cursor_pointer()
                    .on_click(cx.listener(move |state, _, _, cx| {
                        state.delegate_mut().toggle_section(apps_flag);
                        cx.notify();
                    }))
                    .child(format!("{chevron}  {label}"))
                    .into_any_element()
            }
            Row::Proc(row) => match self.columns[col_ix].key.as_ref() {
                "name" => row.name.clone(),
                "pid" => row.pid_s.clone(),
                "user" => row.user_s.clone(),
                "cpu" => row.cpu_s.clone(),
                "gpu" => row.gpu_s.clone(),
                "mem" => row.mem_s.clone(),
                "disk" => row.disk_s.clone(),
                "net" => row.net_s.clone(),
                "threads" => row.threads_s.clone(),
                "handles" => row.handles_s.clone(),
                _ => row.desc_s.clone(),
            }
            .into_any_element(),
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
        let Some(Row::Proc(row)) = self.rows.get(row_ix) else {
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
