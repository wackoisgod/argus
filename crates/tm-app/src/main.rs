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
use gpui_component::table::{Column, ColumnSort, Table, TableDelegate, TableState};
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
const TEXT_DIM: u32 = 0x7f849c;
const ACCENT: u32 = 0x89b4fa;

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
    mem: u64,
    mem_s: SharedString,
    read: u64,
    read_s: SharedString,
    write: u64,
    write_s: SharedString,
    threads: u32,
    threads_s: SharedString,
    handles: u32,
    handles_s: SharedString,
}

struct ProcessTableDelegate {
    columns: Vec<Column>,
    /// Every process from the latest snapshot, unfiltered and unsorted.
    all_rows: Vec<ProcRow>,
    /// The visible view: filtered by `filter`, ordered by `sort`.
    rows: Vec<ProcRow>,
    /// (column index, ascending); reapplied on every snapshot refresh.
    sort: Option<(usize, bool)>,
    /// Lowercased needle matched against name/user/description/PID.
    filter: String,
    /// Row the open context menu refers to: (pid, name).
    menu_row: Option<(u32, SharedString)>,
}

impl ProcessTableDelegate {
    fn new() -> Self {
        ProcessTableDelegate {
            columns: vec![
                Column::new("name", "Name").width(px(240.)).sortable(),
                Column::new("pid", "PID").width(px(80.)).text_right().sortable(),
                Column::new("user", "User").width(px(110.)).sortable(),
                Column::new("cpu", "CPU").width(px(80.)).text_right().sortable(),
                Column::new("mem", "Memory").width(px(110.)).text_right().sortable(),
                Column::new("read", "Read").width(px(110.)).text_right().sortable(),
                Column::new("write", "Write").width(px(110.)).text_right().sortable(),
                Column::new("threads", "Threads").width(px(80.)).text_right().sortable(),
                Column::new("handles", "Handles").width(px(80.)).text_right().sortable(),
                Column::new("desc", "Description").width(px(320.)).sortable(),
            ],
            all_rows: Vec::new(),
            rows: Vec::new(),
            sort: Some((3, false)), // CPU, descending — Task Manager's default
            filter: String::new(),
            menu_row: None,
        }
    }

    fn set_snapshot(&mut self, snap: &Snapshot) {
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
                mem: p.raw.private_working_set,
                mem_s: fmt_bytes(p.raw.private_working_set).into(),
                read: p.read_bytes_per_sec,
                read_s: format!("{}/s", fmt_bytes(p.read_bytes_per_sec)).into(),
                write: p.write_bytes_per_sec,
                write_s: format!("{}/s", fmt_bytes(p.write_bytes_per_sec)).into(),
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

    fn rebuild_view(&mut self) {
        self.rows = self
            .all_rows
            .iter()
            .filter(|r| self.matches(r))
            .cloned()
            .collect();
        self.apply_sort();
    }

    fn apply_sort(&mut self) {
        let Some((col, asc)) = self.sort else { return };
        self.rows.sort_by(|a, b| {
            let ord = match col {
                0 => a
                    .name
                    .as_ref()
                    .to_ascii_lowercase()
                    .cmp(&b.name.as_ref().to_ascii_lowercase()),
                1 => a.pid.cmp(&b.pid),
                2 => a
                    .user_s
                    .as_ref()
                    .to_ascii_lowercase()
                    .cmp(&b.user_s.as_ref().to_ascii_lowercase()),
                3 => a.cpu.total_cmp(&b.cpu),
                4 => a.mem.cmp(&b.mem),
                5 => a.read.cmp(&b.read),
                6 => a.write.cmp(&b.write),
                7 => a.threads.cmp(&b.threads),
                8 => a.handles.cmp(&b.handles),
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
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        match col_ix {
            0 => row.name.clone(),
            1 => row.pid_s.clone(),
            2 => row.user_s.clone(),
            3 => row.cpu_s.clone(),
            4 => row.mem_s.clone(),
            5 => row.read_s.clone(),
            6 => row.write_s.clone(),
            7 => row.threads_s.clone(),
            8 => row.handles_s.clone(),
            _ => row.desc_s.clone(),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(row) = self.rows.get(row_ix) else {
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

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        self.sort = match sort {
            ColumnSort::Ascending => Some((col_ix, true)),
            ColumnSort::Descending => Some((col_ix, false)),
            _ => None,
        };
        self.apply_sort();
    }
}

struct TaskManagerApp {
    table: Entity<TableState<ProcessTableDelegate>>,
    filter_input: Entity<InputState>,
    sys: SystemStats,
    status: Option<SharedString>,
    first_snapshot: bool,
}

/// Spawn the sampler thread. Called first thing in `main`, before gpui
/// initializes DirectX/DirectWrite, so collection warms up in parallel with
/// window creation and data is already waiting when the first frame renders.
fn spawn_sampler() -> futures::channel::mpsc::Receiver<Snapshot> {
    let (tx, rx) = futures::channel::mpsc::channel::<Snapshot>(2);
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let mut tx = tx;
            let mut sampler = Sampler::new();
            sampler.sample(); // baseline for deltas
            // Immediate snapshot (rates read 0) so the first frame has rows,
            // then a short-delta snapshot for real rates, then 1s cadence.
            let _ = tx.try_send(sampler.sample());
            std::thread::sleep(Duration::from_millis(150));
            let _ = tx.try_send(sampler.sample());
            loop {
                std::thread::sleep(Duration::from_millis(1000));
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
        let table = cx.new(|cx| TableState::new(ProcessTableDelegate::new(), window, cx));

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
