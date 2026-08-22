//! Services, Startup Apps, Connections, and Information tabs. Each data
//! source is only queried while its tab is visible (or once, for the static
//! Information data), and rows precompute their display strings on refresh
//! so render only clones refcounted SharedStrings.

use gpui::{div, prelude::*, px, rgb, AnyElement, App, Context, SharedString, Window};
use gpui_component::table::{Column, TableDelegate, TableState};
use rustc_hash::FxHashMap;

use argus_collector::{ConnRow, Section, ServiceInfo, StartupApp, SystemInformation};

use crate::{td_cell, ACCENT, COL_SEPARATOR, TEXT, TEXT_DIM};

fn simple_column(key: &'static str, name: &'static str, width: f32, right: bool) -> Column {
    let c = Column::new(key, name).width(px(width)).p_0();
    if right {
        c.text_right()
    } else {
        c
    }
}

/// Shared whole-header click-to-sort th, matching the processes table.
fn sort_th<D: TableDelegate + 'static>(
    name: SharedString,
    col_ix: usize,
    sort: Option<(usize, bool)>,
    right: bool,
    cx: &mut Context<TableState<D>>,
    toggle: impl Fn(&mut D) + 'static,
) -> impl gpui::IntoElement {
    let indicator = match sort {
        Some((ix, asc)) if ix == col_ix => {
            if asc {
                " ▲"
            } else {
                " ▼"
            }
        }
        _ => "",
    };
    div()
        .id(("th", col_ix))
        .size_full()
        .flex()
        .items_center()
        .pl(px(8.))
        .border_r_1()
        .border_color(rgb(COL_SEPARATOR))
        .when(right, |d| d.justify_end())
        .cursor_pointer()
        .on_click(cx.listener(move |state, _, _, cx| {
            toggle(state.delegate_mut());
            cx.notify();
        }))
        .child(format!("{name}{indicator}"))
}

fn flip_sort(sort: &mut Option<(usize, bool)>, col_ix: usize, text_col: bool) {
    *sort = match *sort {
        Some((ix, asc)) if ix == col_ix => Some((col_ix, !asc)),
        _ => Some((col_ix, text_col)),
    };
}

fn ordered<T: Ord>(a: T, b: T, asc: bool) -> std::cmp::Ordering {
    if asc {
        a.cmp(&b)
    } else {
        b.cmp(&a)
    }
}

// ---------------------------------------------------------------- Services

struct ServiceRow {
    name: SharedString,
    display: SharedString,
    status: &'static str,
    startup: &'static str,
    pid: u32,
    pid_s: SharedString,
    user: SharedString,
    path: SharedString,
}

pub struct ServicesDelegate {
    columns: Vec<Column>,
    rows: Vec<ServiceRow>,
    sort: Option<(usize, bool)>,
    pub running: usize,
    pub stopped: usize,
}

impl ServicesDelegate {
    pub fn new() -> Self {
        ServicesDelegate {
            columns: vec![
                simple_column("name", "Name", 200., false),
                simple_column("display", "Display name", 280., false),
                simple_column("status", "Status", 90., false),
                simple_column("startup", "Startup type", 110., false),
                simple_column("pid", "PID", 70., true),
                simple_column("user", "User", 200., false),
                simple_column("path", "Path", 500., false),
            ],
            rows: Vec::new(),
            sort: Some((0, true)),
            running: 0,
            stopped: 0,
        }
    }

    pub fn set_services(&mut self, services: &[ServiceInfo]) {
        self.running = services.iter().filter(|s| s.running).count();
        self.stopped = services.len() - self.running;
        self.rows = services
            .iter()
            .map(|s| ServiceRow {
                name: s.name.to_string().into(),
                display: s.display.to_string().into(),
                status: if s.running { "Running" } else { "Stopped" },
                startup: s.startup_label(),
                pid: s.pid,
                pid_s: if s.pid != 0 {
                    s.pid.to_string().into()
                } else {
                    SharedString::default()
                },
                user: s.user.to_string().into(),
                path: s.path.to_string().into(),
            })
            .collect();
        self.resort();
    }

    fn resort(&mut self) {
        let Some((col, asc)) = self.sort else { return };
        self.rows.sort_by(|a, b| match col {
            0 => ordered(a.name.to_ascii_lowercase(), b.name.to_ascii_lowercase(), asc),
            1 => ordered(
                a.display.to_ascii_lowercase(),
                b.display.to_ascii_lowercase(),
                asc,
            ),
            2 => ordered(a.status, b.status, asc),
            3 => ordered(a.startup, b.startup, asc),
            4 => ordered(a.pid, b.pid, asc),
            5 => ordered(a.user.to_ascii_lowercase(), b.user.to_ascii_lowercase(), asc),
            _ => ordered(a.path.to_ascii_lowercase(), b.path.to_ascii_lowercase(), asc),
        });
    }

    fn toggle_sort(&mut self, col_ix: usize) {
        flip_sort(&mut self.sort, col_ix, col_ix != 4);
        self.resort();
    }
}

impl TableDelegate for ServicesDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl gpui::IntoElement {
        sort_th(
            self.columns[col_ix].name.clone(),
            col_ix,
            self.sort,
            col_ix == 4,
            cx,
            move |d: &mut Self| d.toggle_sort(col_ix),
        )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl gpui::IntoElement {
        let Some(row) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };
        match col_ix {
            0 => td_cell(row.name.clone(), false),
            1 => td_cell(row.display.clone(), false),
            2 => td_cell(
                div()
                    .text_color(rgb(if row.status == "Running" { TEXT } else { TEXT_DIM }))
                    .child(row.status),
                false,
            ),
            3 => td_cell(row.startup, false),
            4 => td_cell(row.pid_s.clone(), true),
            5 => td_cell(row.user.clone(), false),
            _ => td_cell(row.path.clone(), false),
        }
    }
}

// ------------------------------------------------------------ Startup apps

struct StartupRow {
    name: SharedString,
    publisher: SharedString,
    enabled: bool,
    kind: &'static str,
    location: SharedString,
    command: SharedString,
}

pub struct StartupDelegate {
    columns: Vec<Column>,
    rows: Vec<StartupRow>,
    sort: Option<(usize, bool)>,
}

impl StartupDelegate {
    pub fn new() -> Self {
        StartupDelegate {
            columns: vec![
                simple_column("name", "Name", 220., false),
                simple_column("publisher", "Publisher", 180., false),
                simple_column("status", "Status", 90., false),
                simple_column("type", "Type", 110., false),
                simple_column("location", "Location", 220., false),
                simple_column("command", "Command", 560., false),
            ],
            rows: Vec::new(),
            sort: Some((0, true)),
        }
    }

    pub fn set_apps(&mut self, apps: &[StartupApp]) {
        self.rows = apps
            .iter()
            .map(|a| StartupRow {
                name: a.name.to_string().into(),
                publisher: a.publisher.to_string().into(),
                enabled: a.enabled,
                kind: a.kind,
                location: a.location.to_string().into(),
                command: a.command.to_string().into(),
            })
            .collect();
        self.resort();
    }

    fn resort(&mut self) {
        let Some((col, asc)) = self.sort else { return };
        self.rows.sort_by(|a, b| match col {
            0 => ordered(a.name.to_ascii_lowercase(), b.name.to_ascii_lowercase(), asc),
            1 => ordered(
                a.publisher.to_ascii_lowercase(),
                b.publisher.to_ascii_lowercase(),
                asc,
            ),
            2 => ordered(a.enabled, b.enabled, asc),
            3 => ordered(a.kind, b.kind, asc),
            4 => ordered(
                a.location.to_ascii_lowercase(),
                b.location.to_ascii_lowercase(),
                asc,
            ),
            _ => ordered(
                a.command.to_ascii_lowercase(),
                b.command.to_ascii_lowercase(),
                asc,
            ),
        });
    }

    fn toggle_sort(&mut self, col_ix: usize) {
        flip_sort(&mut self.sort, col_ix, true);
        self.resort();
    }
}

impl TableDelegate for StartupDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl gpui::IntoElement {
        sort_th(
            self.columns[col_ix].name.clone(),
            col_ix,
            self.sort,
            false,
            cx,
            move |d: &mut Self| d.toggle_sort(col_ix),
        )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl gpui::IntoElement {
        let Some(row) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };
        match col_ix {
            0 => td_cell(row.name.clone(), false),
            1 => td_cell(row.publisher.clone(), false),
            2 => td_cell(
                div()
                    .text_color(rgb(if row.enabled { TEXT } else { TEXT_DIM }))
                    .child(if row.enabled { "Enabled" } else { "Disabled" }),
                false,
            ),
            3 => td_cell(row.kind, false),
            4 => td_cell(row.location.clone(), false),
            _ => td_cell(row.command.clone(), false),
        }
    }
}

// ------------------------------------------------------------- Connections

struct ConnDisplayRow {
    proto: &'static str,
    local: SharedString,
    remote: SharedString,
    state: &'static str,
    state_ord: u32,
    pid: u32,
    pid_s: SharedString,
    process: SharedString,
}

pub struct ConnectionsDelegate {
    columns: Vec<Column>,
    rows: Vec<ConnDisplayRow>,
    sort: Option<(usize, bool)>,
    pub total: usize,
    pub tcp: usize,
    pub udp: usize,
    pub established: usize,
    pub listening: usize,
}

impl ConnectionsDelegate {
    pub fn new() -> Self {
        ConnectionsDelegate {
            columns: vec![
                simple_column("proto", "Proto", 80., false),
                simple_column("local", "Address local", 240., false),
                simple_column("remote", "Address foreign", 240., false),
                simple_column("state", "State", 120., false),
                simple_column("pid", "PID", 80., true),
                simple_column("process", "Process", 240., false),
            ],
            rows: Vec::new(),
            sort: Some((0, true)),
            total: 0,
            tcp: 0,
            udp: 0,
            established: 0,
            listening: 0,
        }
    }

    pub fn set_connections(
        &mut self,
        conns: &[ConnRow],
        names: &FxHashMap<u32, SharedString>,
    ) {
        self.total = conns.len();
        self.tcp = conns.iter().filter(|c| c.proto.starts_with("TCP")).count();
        self.udp = self.total - self.tcp;
        self.established = conns.iter().filter(|c| c.state_ord == 5).count();
        self.listening = conns.iter().filter(|c| c.state_ord == 2).count();
        self.rows = conns
            .iter()
            .map(|c| ConnDisplayRow {
                proto: c.proto,
                local: c.local.clone().into(),
                remote: c.remote.clone().into(),
                state: c.state,
                state_ord: c.state_ord,
                pid: c.pid,
                pid_s: c.pid.to_string().into(),
                process: names.get(&c.pid).cloned().unwrap_or_default(),
            })
            .collect();
        self.resort();
    }

    fn resort(&mut self) {
        let Some((col, asc)) = self.sort else { return };
        self.rows.sort_by(|a, b| match col {
            0 => ordered(a.proto, b.proto, asc).then(a.local.cmp(&b.local)),
            1 => ordered(a.local.clone(), b.local.clone(), asc),
            2 => ordered(a.remote.clone(), b.remote.clone(), asc),
            3 => ordered(a.state_ord, b.state_ord, asc),
            4 => ordered(a.pid, b.pid, asc),
            _ => ordered(
                a.process.to_ascii_lowercase(),
                b.process.to_ascii_lowercase(),
                asc,
            ),
        });
    }

    fn toggle_sort(&mut self, col_ix: usize) {
        flip_sort(&mut self.sort, col_ix, col_ix != 4);
        self.resort();
    }
}

impl TableDelegate for ConnectionsDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl gpui::IntoElement {
        sort_th(
            self.columns[col_ix].name.clone(),
            col_ix,
            self.sort,
            col_ix == 4,
            cx,
            move |d: &mut Self| d.toggle_sort(col_ix),
        )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl gpui::IntoElement {
        let Some(row) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };
        match col_ix {
            0 => td_cell(row.proto, false),
            1 => td_cell(row.local.clone(), false),
            2 => td_cell(row.remote.clone(), false),
            3 => td_cell(row.state, false),
            4 => td_cell(row.pid_s.clone(), true),
            _ => td_cell(row.process.clone(), false),
        }
    }
}

// -------------------------------------------------------------- Information

fn info_row(key: &'static str, value: &str) -> AnyElement {
    div()
        .flex()
        .gap(px(8.))
        .py(px(2.))
        .child(
            div()
                .w(px(150.))
                .flex_none()
                .text_color(rgb(TEXT_DIM))
                .child(format!("{key}:")),
        )
        .child(div().text_color(rgb(TEXT)).child(value.to_string()))
        .into_any_element()
}

fn info_section(title: &'static str, rows: &Section) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .mb(px(16.))
        .child(
            div()
                .px(px(8.))
                .py(px(4.))
                .mb(px(6.))
                .rounded(px(4.))
                .bg(rgb(0x1c1c2e))
                .text_color(rgb(ACCENT))
                .child(title),
        )
        .children(rows.iter().map(|(k, v)| info_row(k, v)))
        .into_any_element()
}

fn info_section_group(title: &'static str, groups: &[Section]) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .mb(px(16.))
        .child(
            div()
                .px(px(8.))
                .py(px(4.))
                .mb(px(6.))
                .rounded(px(4.))
                .bg(rgb(0x1c1c2e))
                .text_color(rgb(ACCENT))
                .child(title),
        )
        .children(groups.iter().enumerate().flat_map(|(i, rows)| {
            let mut items: Vec<AnyElement> = Vec::with_capacity(rows.len() + 1);
            if i > 0 {
                items.push(div().h(px(8.)).into_any_element());
            }
            items.extend(rows.iter().map(|(k, v)| info_row(k, v)));
            items
        }))
        .into_any_element()
}

/// The three-column Information pane. `live` carries the values that change
/// while running (uptime, counts, memory in use, adapters).
pub struct LiveInfo {
    pub uptime: String,
    pub processes: u32,
    pub threads: u32,
    pub handles: u32,
    pub mem_in_use: String,
    pub commit: String,
    pub adapters: Vec<Section>,
}

pub fn render_information(info: &SystemInformation, live: &LiveInfo) -> AnyElement {
    let mut system = info.system.clone();
    system.push(("System Uptime", live.uptime.clone()));
    system.push(("Process Count", live.processes.to_string()));
    system.push(("Thread Count", live.threads.to_string()));
    system.push(("Handle Count", live.handles.to_string()));
    let mut memory = info.memory.clone();
    memory.push(("In Use", live.mem_in_use.clone()));
    memory.push(("Committed", live.commit.clone()));

    div()
        .id("info-pane")
        .flex_1()
        .min_h(px(0.))
        .flex()
        .gap(px(24.))
        .p(px(16.))
        .overflow_y_scroll()
        .text_size(px(13.))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(info_section("Operating System", &info.os))
                .child(info_section("Processor", &info.cpu))
                .child(info_section("System", &system))
                .child(info_section_group("Disk Drives", &info.disks)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(info_section("Memory", &memory))
                .child(info_section_group("Memory Modules", &info.modules)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(info_section_group("Network Adapters", &live.adapters)),
        )
        .into_any_element()
}
