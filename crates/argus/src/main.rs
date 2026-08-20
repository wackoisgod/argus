// GUI subsystem: no console window on launch. Startup tlog output still
// reaches stderr when one is attached (e.g. `tm-app 2>log` from a shell).
#![windows_subsystem = "windows"]

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    actions, div, px, rgb, size, App, Application, Bounds, ClipboardItem, Context, Entity,
    SharedString, Timer, Window, WindowBounds, WindowOptions,
};
use gpui::prelude::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_component::table::{Column, Table, TableDelegate, TableState};
use gpui_component::theme::{Theme, ThemeMode};
use gpui_component::Root;
use argus_collector::{fmt_bytes, Sampler, Snapshot, SystemStats};

mod perf_ui;

actions!(tm, [EndTask, CopyPid, CopyName]);

static START: OnceLock<Instant> = OnceLock::new();

/// Which tab is visible (0 = Processes, 1 = Performance), for the sampler
/// thread: adapter-wide GPU probing relaxes to every other tick while the
/// Performance tab isn't showing, since each D3DKMT query can stall inside
/// the display driver.
static CURRENT_TAB: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

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
/// Color of the vertical line between columns (body and header).
const COL_SEPARATOR: u32 = 0x252539;

/// Full-height cell wrapper: owns the padding (columns are declared p_0) and
/// draws the column separator on its right edge.
fn td_cell(content: impl gpui::IntoElement, right: bool) -> gpui::AnyElement {
    div()
        .w_full()
        .h_full()
        .px(px(8.))
        .flex()
        .items_center()
        .when(right, |d| d.justify_end())
        .border_r_1()
        .border_color(rgb(COL_SEPARATOR))
        .overflow_hidden()
        .child(content)
        .into_any_element()
}

fn fmt_mibs(bytes_per_sec: u64) -> String {
    format!("{:.2} MiB/s", bytes_per_sec as f64 / (1024.0 * 1024.0))
}

/// Network rates in fixed megabits/sec, Task Manager style.
fn fmt_mbps(bytes_per_sec: u64) -> String {
    format!("{:.2} Mbps", bytes_per_sec as f64 * 8.0 / 1_000_000.0)
}

/// Everything a column needs: menu category, identity, layout, and whether
/// its values are numeric (right-aligned, sorted descending first).
struct ColSpec {
    category: &'static str,
    key: &'static str,
    name: &'static str,
    width: f32,
    right: bool,
    /// In the default set (see DEFAULT_COLUMNS for default *order*).
    #[allow(dead_code)]
    default_on: bool,
}

const fn col(
    category: &'static str,
    key: &'static str,
    name: &'static str,
    width: f32,
    right: bool,
    default_on: bool,
) -> ColSpec {
    ColSpec {
        category,
        key,
        name,
        width,
        right,
        default_on,
    }
}

/// Every column the table can show, in canonical order. The header context
/// menu groups them by category, TaskSlinger style.
const COLUMN_CATALOG: &[ColSpec] = &[
    col("Process", "name", "Name", 240., false, true),
    col("Process", "pid", "PID", 80., true, true),
    col("Process", "parent", "PID parent", 90., true, false),
    col("Process", "session", "Session", 80., true, false),
    col("Process", "start", "Start time", 150., false, false),
    col("Process", "user", "User", 110., false, true),
    col("Process", "cmdline", "Command line", 400., false, false),
    col("Image", "company", "Company", 170., false, false),
    col("Image", "path", "Image path", 400., false, false),
    col("Image", "exe", "Process name", 280., false, true),
    col("CPU", "cpu", "CPU", 80., true, true),
    col("CPU", "cpuk", "CPU (kernel)", 100., true, false),
    col("CPU", "cpuu", "CPU (user)", 100., true, false),
    col("CPU", "cputime", "CPU time", 110., true, false),
    col("CPU", "priority", "Priority", 80., true, false),
    col("Memory", "mem", "Memory", 110., true, true),
    col("Memory", "commit", "Commit size", 110., true, false),
    col("Memory", "ws", "Working set", 110., true, false),
    col("Memory", "wspeak", "Working set peak", 130., true, false),
    col("Memory", "virt", "Virtual size", 110., true, false),
    col("Memory", "faults", "Page faults", 110., true, false),
    col("Memory", "pagedpool", "Paged pool", 100., true, false),
    col("Memory", "npool", "Non-paged pool", 120., true, false),
    col("I/O", "disk", "Disk", 110., true, true),
    col("I/O", "ioread", "I/O read rate", 110., true, false),
    col("I/O", "iowrite", "I/O write rate", 110., true, false),
    col("Network", "net", "Network", 110., true, true),
    col("GPU", "gpu", "GPU", 80., true, true),
    col("Objects", "threads", "Threads", 80., true, true),
    col("Objects", "handles", "Handles", 80., true, true),
];

/// Default layout (order matters — this is the classic arrangement, not
/// catalog order).
const DEFAULT_COLUMNS: &[&str] = &[
    "name", "pid", "user", "cpu", "gpu", "mem", "disk", "net", "threads", "handles", "exe",
];

fn col_spec(key: &str) -> Option<&'static ColSpec> {
    COLUMN_CATALOG.iter().find(|s| s.key == key)
}

fn build_column(spec: &ColSpec) -> Column {
    let c = Column::new(spec.key, spec.name).width(px(spec.width)).p_0();
    if spec.right {
        c.text_right()
    } else {
        c
    }
}

/// Where the chosen column set persists across runs.
fn columns_config_path() -> Option<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(std::path::PathBuf::from(appdata).join("argus").join("columns.cfg"))
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
    // Optional-column data (hidden by default; toggled via the header menu).
    parent_s: SharedString,
    session_s: SharedString,
    start_s: SharedString,
    cmdline_s: SharedString,
    company_s: SharedString,
    path_s: SharedString,
    cpuk: f32,
    cpuk_s: SharedString,
    cpuu: f32,
    cpuu_s: SharedString,
    cputime: i64,
    cputime_s: SharedString,
    priority: i32,
    priority_s: SharedString,
    commit: u64,
    commit_s: SharedString,
    ws: u64,
    ws_s: SharedString,
    wspeak: u64,
    wspeak_s: SharedString,
    virt: u64,
    virt_s: SharedString,
    faults: u64,
    faults_s: SharedString,
    pagedpool: u64,
    pagedpool_s: SharedString,
    npool: u64,
    npool_s: SharedString,
    ioread: u64,
    ioread_s: SharedString,
    iowrite: u64,
    iowrite_s: SharedString,
}

/// FILETIME (100ns since 1601 UTC) → "HH:MM:SS YYYY-MM-DD" local time.
fn fmt_start_time(create: i64) -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    if create <= 0 {
        return String::new();
    }
    unsafe {
        let ft = FILETIME {
            dwLowDateTime: create as u32,
            dwHighDateTime: (create >> 32) as u32,
        };
        let mut st = std::mem::zeroed::<SYSTEMTIME>();
        if FileTimeToSystemTime(&ft, &mut st) == 0 {
            return String::new();
        }
        let mut lt = std::mem::zeroed::<SYSTEMTIME>();
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &st, &mut lt) == 0 {
            return String::new();
        }
        format!(
            "{:02}:{:02}:{:02} {:04}-{:02}-{:02}",
            lt.wHour, lt.wMinute, lt.wSecond, lt.wYear, lt.wMonth, lt.wDay
        )
    }
}

/// Cumulative CPU time (100ns) → "h:mm:ss".
fn fmt_cpu_time(total_100ns: i64) -> String {
    let secs = (total_100ns.max(0) / 10_000_000) as u64;
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
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
    /// PNG-bytes identity → decoded image. Keyed by the shared PNG Arc's
    /// address (icons are deduped per exe in the collector), so gpui sees
    /// one stable image per unique icon no matter how many processes use it.
    icon_cache: rustc_hash::FxHashMap<usize, std::sync::Arc<gpui::Image>>,
    /// Every process from the latest snapshot, unfiltered and unsorted.
    all_rows: Vec<ProcRow>,
    /// The visible view: sections + filtered, sorted processes.
    rows: Vec<Row>,
    /// (column key, ascending); keyed by name, not index, so column
    /// drag-reordering can't desync sorting from data.
    sort: Option<(SharedString, bool)>,
    /// Lowercased needle matched against name/user/description/PID.
    filter: String,
    /// Rows the open context menu refers to: (pid, name) per target.
    menu_rows: Vec<(u32, SharedString)>,
    /// Multi-selected pids (plain/ctrl/shift click), pruned to live
    /// processes each snapshot.
    selected: rustc_hash::FxHashSet<u32>,
    /// Anchor pid for shift-range selection.
    sel_anchor: Option<u32>,
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
            columns: Self::load_columns(),
            all_rows: Vec::new(),
            totals: HeaderTotals::default(),
            icon_cache: rustc_hash::FxHashMap::default(),
            rows: Vec::new(),
            sort: Some(("pid".into(), true)),
            filter: String::new(),
            menu_rows: Vec::new(),
            selected: rustc_hash::FxHashSet::default(),
            sel_anchor: None,
            collapsed: [false; 3],
            expanded: rustc_hash::FxHashSet::default(),
        }
    }

    /// Column set from disk (one key per line, display order) or defaults.
    fn load_columns() -> Vec<Column> {
        let saved: Vec<String> = columns_config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.lines().map(|l| l.trim().to_string()).collect())
            .unwrap_or_default();
        let keys: Vec<&str> = if saved.is_empty() {
            DEFAULT_COLUMNS.to_vec()
        } else {
            saved
                .iter()
                .map(|k| k.as_str())
                .filter(|k| col_spec(k).is_some())
                .collect()
        };
        let mut columns: Vec<Column> = keys
            .iter()
            .filter_map(|k| col_spec(k))
            .map(build_column)
            .collect();
        if columns.is_empty() {
            columns = DEFAULT_COLUMNS
                .iter()
                .filter_map(|k| col_spec(k))
                .map(build_column)
                .collect();
        }
        columns
    }

    fn save_columns(&self) {
        if let Some(path) = columns_config_path() {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let keys: Vec<&str> = self.columns.iter().map(|c| c.key.as_ref()).collect();
            let _ = std::fs::write(path, keys.join("\n"));
        }
    }

    fn has_column(&self, key: &str) -> bool {
        self.columns.iter().any(|c| c.key.as_ref() == key)
    }

    /// Show/hide a column. New columns appear at their catalog position
    /// relative to the currently visible set.
    fn toggle_column(&mut self, key: &str) {
        if let Some(ix) = self.columns.iter().position(|c| c.key.as_ref() == key) {
            if self.columns.len() > 1 {
                self.columns.remove(ix);
            }
        } else if let Some(spec) = col_spec(key) {
            let catalog_ix = COLUMN_CATALOG.iter().position(|s| s.key == key).unwrap();
            let insert_at = self
                .columns
                .iter()
                .take_while(|c| {
                    COLUMN_CATALOG
                        .iter()
                        .position(|s| s.key == c.key.as_ref())
                        .map(|p| p < catalog_ix)
                        .unwrap_or(false)
                })
                .count();
            self.columns.insert(insert_at, build_column(spec));
        }
        self.save_columns();
    }

    fn reset_columns(&mut self) {
        self.columns = DEFAULT_COLUMNS
            .iter()
            .filter_map(|k| col_spec(k))
            .map(build_column)
            .collect();
        self.save_columns();
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
        // Optional-column strings are only worth formatting when the column
        // is visible — this loop runs every second for every process.
        let want = |k: &str| self.columns.iter().any(|c| c.key.as_ref() == k);
        let (w_parent, w_session, w_start) = (want("parent"), want("session"), want("start"));
        let (w_cmdline, w_company, w_path) = (want("cmdline"), want("company"), want("path"));
        let (w_cpuk, w_cpuu, w_cputime, w_priority) =
            (want("cpuk"), want("cpuu"), want("cputime"), want("priority"));
        let (w_commit, w_ws, w_wspeak, w_virt) =
            (want("commit"), want("ws"), want("wspeak"), want("virt"));
        let (w_faults, w_pagedpool, w_npool) =
            (want("faults"), want("pagedpool"), want("npool"));
        let (w_ioread, w_iowrite) = (want("ioread"), want("iowrite"));
        let s = |on: bool, f: &dyn Fn() -> String| -> SharedString {
            if on {
                f().into()
            } else {
                SharedString::default()
            }
        };
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
            let (company, path, cmdline) = p
                .enriched
                .as_ref()
                .filter(|_| w_company || w_path || w_cmdline)
                .map(|e| {
                    (
                        e.company.to_string(),
                        e.image_path.to_string(),
                        e.command_line.to_string(),
                    )
                })
                .unwrap_or_default();
            let cputime = p.raw.user_time_100ns + p.raw.kernel_time_100ns;
            let user_pct = (p.cpu_percent - p.kernel_percent).max(0.0);
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
                parent_s: s(w_parent, &|| p.raw.parent_pid.to_string()),
                session_s: s(w_session, &|| p.raw.session_id.to_string()),
                start_s: s(w_start, &|| fmt_start_time(p.raw.create_time)),
                cmdline_s: if w_cmdline {
                    cmdline.into()
                } else {
                    SharedString::default()
                },
                company_s: if w_company {
                    company.into()
                } else {
                    SharedString::default()
                },
                path_s: if w_path {
                    path.into()
                } else {
                    SharedString::default()
                },
                cpuk: p.kernel_percent,
                cpuk_s: s(w_cpuk, &|| format!("{:.1}%", p.kernel_percent)),
                cpuu: user_pct,
                cpuu_s: s(w_cpuu, &|| format!("{user_pct:.1}%")),
                cputime,
                cputime_s: s(w_cputime, &|| fmt_cpu_time(cputime)),
                priority: p.raw.base_priority,
                priority_s: s(w_priority, &|| p.raw.base_priority.to_string()),
                commit: p.raw.private_bytes,
                commit_s: s(w_commit, &|| fmt_bytes(p.raw.private_bytes)),
                ws: p.raw.working_set,
                ws_s: s(w_ws, &|| fmt_bytes(p.raw.working_set)),
                wspeak: p.raw.peak_working_set,
                wspeak_s: s(w_wspeak, &|| fmt_bytes(p.raw.peak_working_set)),
                virt: p.raw.virtual_size,
                virt_s: s(w_virt, &|| fmt_bytes(p.raw.virtual_size)),
                faults: p.raw.page_faults as u64,
                faults_s: s(w_faults, &|| p.raw.page_faults.to_string()),
                pagedpool: p.raw.paged_pool,
                pagedpool_s: s(w_pagedpool, &|| fmt_bytes(p.raw.paged_pool)),
                npool: p.raw.nonpaged_pool,
                npool_s: s(w_npool, &|| fmt_bytes(p.raw.nonpaged_pool)),
                ioread: p.read_bytes_per_sec,
                ioread_s: s(w_ioread, &|| fmt_mibs(p.read_bytes_per_sec)),
                iowrite: p.write_bytes_per_sec,
                iowrite_s: s(w_iowrite, &|| fmt_mibs(p.write_bytes_per_sec)),
            });
        }
        let live: rustc_hash::FxHashSet<usize> = self
            .all_rows
            .iter()
            .filter_map(|r| r.icon.as_ref().map(|b| std::sync::Arc::as_ptr(b) as usize))
            .collect();
        self.icon_cache.retain(|identity, _| live.contains(identity));
        let live_pids: rustc_hash::FxHashSet<u32> =
            self.all_rows.iter().map(|r| r.pid).collect();
        self.selected.retain(|pid| live_pids.contains(pid));
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
        col_spec(key).map(|s| !s.right).unwrap_or(false)
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
    fn icon_image(&mut self, bytes: &std::sync::Arc<Vec<u8>>) -> std::sync::Arc<gpui::Image> {
        let identity = std::sync::Arc::as_ptr(bytes) as usize;
        if let Some(image) = self.icon_cache.get(&identity) {
            return image.clone();
        }
        let image = std::sync::Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            (**bytes).clone(),
        ));
        self.icon_cache.insert(identity, image.clone());
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
            agg.cpuk += c.cpuk;
            agg.cpuu += c.cpuu;
            agg.cputime += c.cputime;
            agg.commit += c.commit;
            agg.ws += c.ws;
            agg.wspeak = agg.wspeak.max(c.wspeak);
            agg.virt += c.virt;
            agg.faults += c.faults;
            agg.pagedpool += c.pagedpool;
            agg.npool += c.npool;
            agg.ioread += c.ioread;
            agg.iowrite += c.iowrite;
        }
        agg.cpu_s = format!("{:.1}%", agg.cpu).into();
        agg.gpu_s = format!("{:.1}%", agg.gpu).into();
        agg.mem_s = fmt_bytes(agg.mem).into();
        agg.disk_s = fmt_mibs(agg.disk).into();
        agg.net_s = fmt_mbps(agg.net).into();
        agg.threads_s = agg.threads.to_string().into();
        agg.handles_s = agg.handles.to_string().into();
        agg.cpuk_s = format!("{:.1}%", agg.cpuk).into();
        agg.cpuu_s = format!("{:.1}%", agg.cpuu).into();
        agg.cputime_s = fmt_cpu_time(agg.cputime).into();
        agg.commit_s = fmt_bytes(agg.commit).into();
        agg.ws_s = fmt_bytes(agg.ws).into();
        agg.wspeak_s = fmt_bytes(agg.wspeak).into();
        agg.virt_s = fmt_bytes(agg.virt).into();
        agg.faults_s = agg.faults.to_string().into();
        agg.pagedpool_s = fmt_bytes(agg.pagedpool).into();
        agg.npool_s = fmt_bytes(agg.npool).into();
        agg.ioread_s = fmt_mibs(agg.ioread).into();
        agg.iowrite_s = fmt_mibs(agg.iowrite).into();
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

    fn row_pid(&self, row_ix: usize) -> Option<u32> {
        match self.rows.get(row_ix) {
            Some(Row::Proc { row, .. }) => Some(row.pid),
            _ => None,
        }
    }

    fn select_only(&mut self, pid: u32) {
        self.selected.clear();
        self.selected.insert(pid);
        self.sel_anchor = Some(pid);
    }

    fn toggle_select(&mut self, pid: u32) {
        if !self.selected.insert(pid) {
            self.selected.remove(&pid);
        }
        self.sel_anchor = Some(pid);
    }

    /// Shift-click: select every process row between the anchor and the
    /// clicked row in the current visible order. The anchor stays put so
    /// repeated shift-clicks re-extend from the same origin.
    fn select_range_to(&mut self, row_ix: usize) {
        let Some(clicked) = self.row_pid(row_ix) else {
            return;
        };
        let anchor_ix = self
            .sel_anchor
            .and_then(|a| {
                self.rows.iter().position(
                    |r| matches!(r, Row::Proc { row, .. } if row.pid == a),
                )
            })
            .unwrap_or(row_ix);
        let (lo, hi) = (anchor_ix.min(row_ix), anchor_ix.max(row_ix));
        self.selected.clear();
        for r in &self.rows[lo..=hi] {
            if let Row::Proc { row, .. } = r {
                self.selected.insert(row.pid);
            }
        }
        if self.sel_anchor.is_none() {
            self.sel_anchor = Some(clicked);
        }
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
                "parent" => a.parent.cmp(&b.parent),
                "session" => a.session.cmp(&b.session),
                "start" => a.create.cmp(&b.create),
                "cpuk" => a.cpuk.total_cmp(&b.cpuk),
                "cpuu" => a.cpuu.total_cmp(&b.cpuu),
                "cputime" => a.cputime.cmp(&b.cputime),
                "priority" => a.priority.cmp(&b.priority),
                "commit" => a.commit.cmp(&b.commit),
                "ws" => a.ws.cmp(&b.ws),
                "wspeak" => a.wspeak.cmp(&b.wspeak),
                "virt" => a.virt.cmp(&b.virt),
                "faults" => a.faults.cmp(&b.faults),
                "pagedpool" => a.pagedpool.cmp(&b.pagedpool),
                "npool" => a.npool.cmp(&b.npool),
                "ioread" => a.ioread.cmp(&b.ioread),
                "iowrite" => a.iowrite.cmp(&b.iowrite),
                "company" => a
                    .company_s
                    .as_ref()
                    .to_ascii_lowercase()
                    .cmp(&b.company_s.as_ref().to_ascii_lowercase()),
                "path" => a
                    .path_s
                    .as_ref()
                    .to_ascii_lowercase()
                    .cmp(&b.path_s.as_ref().to_ascii_lowercase()),
                "cmdline" => a
                    .cmdline_s
                    .as_ref()
                    .to_ascii_lowercase()
                    .cmp(&b.cmdline_s.as_ref().to_ascii_lowercase()),
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
                    .h_full()
                    .pl(px(8.))
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
                            .map(|bytes| self.icon_image(bytes));
                        let content = div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .overflow_hidden()
                            .whitespace_nowrap()
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
                            .child(row.name.clone());
                        td_cell(content, false)
                    }
                    // Numeric cells right-align to match their headers.
                    "pid" => td_cell(row.pid_s.clone(), true),
                    "user" => td_cell(row.user_s.clone(), false),
                    "cpu" => td_cell(row.cpu_s.clone(), true),
                    "gpu" => td_cell(row.gpu_s.clone(), true),
                    "mem" => td_cell(row.mem_s.clone(), true),
                    "disk" => td_cell(row.disk_s.clone(), true),
                    "net" => td_cell(row.net_s.clone(), true),
                    "threads" => td_cell(row.threads_s.clone(), true),
                    "handles" => td_cell(row.handles_s.clone(), true),
                    "parent" => td_cell(row.parent_s.clone(), true),
                    "session" => td_cell(row.session_s.clone(), true),
                    "start" => td_cell(row.start_s.clone(), false),
                    "cmdline" => td_cell(row.cmdline_s.clone(), false),
                    "company" => td_cell(row.company_s.clone(), false),
                    "path" => td_cell(row.path_s.clone(), false),
                    "cpuk" => td_cell(row.cpuk_s.clone(), true),
                    "cpuu" => td_cell(row.cpuu_s.clone(), true),
                    "cputime" => td_cell(row.cputime_s.clone(), true),
                    "priority" => td_cell(row.priority_s.clone(), true),
                    "commit" => td_cell(row.commit_s.clone(), true),
                    "ws" => td_cell(row.ws_s.clone(), true),
                    "wspeak" => td_cell(row.wspeak_s.clone(), true),
                    "virt" => td_cell(row.virt_s.clone(), true),
                    "faults" => td_cell(row.faults_s.clone(), true),
                    "pagedpool" => td_cell(row.pagedpool_s.clone(), true),
                    "npool" => td_cell(row.npool_s.clone(), true),
                    "ioread" => td_cell(row.ioread_s.clone(), true),
                    "iowrite" => td_cell(row.iowrite_s.clone(), true),
                    _ => td_cell(row.exe_s.clone(), false),
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
        self.save_columns();
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
        let table = cx.entity();
        div()
            .id(("proc-th", col_ix))
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .pl(px(8.))
            .border_r_1()
            .border_color(rgb(COL_SEPARATOR))
            .when(right_aligned, |d| d.items_end())
            .cursor_pointer()
            .on_click(cx.listener(move |state, _, _, cx| {
                state.delegate_mut().toggle_sort(col_ix);
                cx.notify();
            }))
            .child(format!("{name}{indicator}"))
            .when_some(total, |d, total| d.child(total))
            // Right-click: the column picker, grouped by category.
            .context_menu(move |menu, _window, _cx| {
                let mut menu = menu.scrollable(true).max_h(px(700.));
                {
                    let table = table.clone();
                    menu = menu.item(PopupMenuItem::new("Reset to default").on_click(
                        move |_, _, cx| {
                            table.update(cx, |state, cx| {
                                state.delegate_mut().reset_columns();
                                state.refresh(cx);
                                cx.notify();
                            });
                        },
                    ));
                }
                let mut last_category = "";
                for spec in COLUMN_CATALOG {
                    if spec.category != last_category {
                        last_category = spec.category;
                        menu = menu.separator().label(spec.category);
                    }
                    let checked = table.read_with(_cx, |state, _| {
                        state.delegate().has_column(spec.key)
                    });
                    let table = table.clone();
                    let key = spec.key;
                    menu = menu.item(
                        PopupMenuItem::new(spec.name)
                            .checked(checked)
                            .disabled(key == "name")
                            .on_click(move |_, _, cx| {
                                table.update(cx, |state, cx| {
                                    state.delegate_mut().toggle_column(key);
                                    state.refresh(cx);
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            })
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        let mut tr = div().id(("row", row_ix));
        let Some(pid) = self.row_pid(row_ix) else {
            return tr;
        };
        if self.selected.contains(&pid) {
            tr = tr.bg(rgb(0x2d3050));
        }
        tr.on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |state, e: &gpui::MouseDownEvent, _, cx| {
                let d = state.delegate_mut();
                if e.modifiers.shift {
                    d.select_range_to(row_ix);
                } else if e.modifiers.control {
                    d.toggle_select(pid);
                } else {
                    d.select_only(pid);
                }
                cx.notify();
            }),
        )
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let (pid, name) = match self.rows.get(row_ix) {
            Some(Row::Proc { row, .. }) => (row.pid, row.name.clone()),
            _ => return menu,
        };
        // Right-click outside the current selection retargets it to the
        // clicked row (standard multi-select behavior); inside it, the
        // whole selection becomes the menu's target.
        if !self.selected.contains(&pid) {
            self.select_only(pid);
            cx.notify();
        }
        // Targets: the selection in visible order (deduped — aggregated
        // group rows share the root pid), or just the clicked row.
        let mut seen = rustc_hash::FxHashSet::default();
        let mut targets: Vec<(u32, SharedString)> = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Proc { row, .. }
                    if self.selected.contains(&row.pid) && seen.insert(row.pid) =>
                {
                    Some((row.pid, row.name.clone()))
                }
                _ => None,
            })
            .collect();
        if targets.is_empty() {
            targets = vec![(pid, name)];
        }
        let n = targets.len();
        let title: SharedString = if n == 1 {
            format!("{}  ({})", targets[0].1, targets[0].0).into()
        } else {
            format!("{n} processes selected").into()
        };
        let end_label: SharedString = if n == 1 {
            "End Task".into()
        } else {
            format!("End {n} Tasks").into()
        };
        self.menu_rows = targets;
        menu.label(title)
            .separator()
            .menu(end_label, Box::new(EndTask))
            .separator()
            .menu(if n == 1 { "Copy PID" } else { "Copy PIDs" }, Box::new(CopyPid))
            .menu(
                if n == 1 { "Copy Name" } else { "Copy Names" },
                Box::new(CopyName),
            )
    }

}

struct TaskManagerApp {
    table: Entity<TableState<ProcessTableDelegate>>,
    filter_input: Entity<InputState>,
    sys: SystemStats,
    status: Option<SharedString>,
    first_snapshot: bool,
    /// 0 = Processes, 1 = Performance.
    tab: u8,
    pane: perf_ui::Pane,
    history: perf_ui::PerfHistory,
    /// Chart the mouse is over and the 0..1 x-fraction, for tooltips.
    chart_hover: Option<(SharedString, f32)>,
    /// CPU pane: per-core grid instead of the overall chart.
    cpu_per_core: bool,
    /// Draw the kernel-time line on CPU charts.
    kernel_on: bool,
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
            // then a short-delta snapshot for real rates, then two pickup
            // ticks that surface the async enrichment wave (names, users,
            // icons) as it lands instead of waiting for the 1s cadence.
            let _ = tx.try_send(sampler.sample());
            for wait in [150, 350, 500] {
                std::thread::sleep(Duration::from_millis(wait));
                let _ = tx.try_send(sampler.sample());
            }
            let mut hwnd: isize = 0;
            let mut was_minimized = false;
            let mut tick: u64 = 0;
            loop {
                tick += 1;
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
                // block the sampler. Minimized ticks skip the GPU probe and
                // connection walk nobody can see.
                let still_minimized = minimized && window_minimized(&mut hwnd);
                // On the transition into the tray, hand the working set back
                // to the OS — Task Manager does the same. Pages fault back
                // in lazily on restore.
                if still_minimized && !was_minimized {
                    use windows_sys::Win32::System::ProcessStatus::K32EmptyWorkingSet;
                    use windows_sys::Win32::System::Threading::GetCurrentProcess;
                    unsafe { K32EmptyWorkingSet(GetCurrentProcess()) };
                }
                was_minimized = still_minimized;
                let gpu_relaxed = CURRENT_TAB.load(std::sync::atomic::Ordering::Relaxed) == 0
                    && tick % 2 == 1;
                match tx.try_send(sampler.sample_with_opts(still_minimized, gpu_relaxed)) {
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
            InputState::new(window, cx)
                .placeholder("Filter by name, user, description, or PID")
                .clean_on_escape()
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

        // Chart scroll animation: ~30fps notifications while the Performance
        // tab is visible (actual paint rate is governed by the adaptive
        // vsync layer); near-dormant on the Processes tab or while the
        // window is minimized — nobody sees those frames.
        cx.spawn(async move |this, cx| {
            let mut hwnd: isize = 0;
            loop {
                let mut animate = false;
                if this
                    .update(cx, |this, cx| {
                        animate = this.tab == 1 && !window_minimized(&mut hwnd);
                        if animate {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                Timer::after(Duration::from_millis(if animate { 33 } else { 250 })).await;
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
                    this.history.update(&snap);
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
            tab: 0,
            pane: perf_ui::Pane::Cpu,
            history: perf_ui::PerfHistory::default(),
            chart_hover: None,
            cpu_per_core: false,
            kernel_on: true,
        }
    }

    fn menu_rows(&self, cx: &App) -> Vec<(u32, SharedString)> {
        self.table.read(cx).delegate().menu_rows.clone()
    }

    fn end_task(&mut self, cx: &mut Context<Self>) {
        let targets = self.menu_rows(cx);
        if targets.is_empty() {
            return;
        }
        let mut ended = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for (pid, name) in &targets {
            match argus_collector::kill_process(*pid) {
                Ok(()) => ended += 1,
                Err(e) => failures.push(format!("{name} ({pid}): {e}")),
            }
        }
        self.status = Some(match (&targets[..], &failures[..]) {
            ([(pid, name)], []) => format!("Ended {name} ({pid})").into(),
            (_, []) => format!("Ended {ended} tasks").into(),
            ([(pid, name)], [err]) => {
                let _ = (pid, name);
                format!("Could not end {err}").into()
            }
            _ => format!("Ended {ended} tasks; failed: {}", failures.join(", ")).into(),
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
            .child(
                div()
                    .w(px(340.))
                    .child(Input::new(&self.filter_input).cleanable(true)),
            )
            .when_some(self.status.clone(), |this, status| {
                this.child(div().text_color(rgb(TEXT_DIM)).child(status))
            });

        let tab = |this: &Self,
                   cx: &mut Context<Self>,
                   id: u8,
                   label: &'static str|
         -> gpui::Stateful<gpui::Div> {
            let active = this.tab == id;
            div()
                .id(("tab", id as usize))
                .px(px(14.))
                .py(px(6.))
                .cursor_pointer()
                .text_color(if active { rgb(TEXT) } else { rgb(TEXT_DIM) })
                .border_b_2()
                .border_color(if active {
                    rgb(ACCENT)
                } else {
                    rgb(BG_HEADER)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab = id;
                    CURRENT_TAB.store(id, std::sync::atomic::Ordering::Relaxed);
                    cx.notify();
                }))
                .child(label)
        };
        let tab_bar = div()
            .flex()
            .flex_none()
            .gap(px(4.))
            .px(px(8.))
            .bg(rgb(BG_HEADER))
            .child(tab(self, cx, 0, "Processes"))
            .child(tab(self, cx, 1, "Performance"));

        let body: gpui::AnyElement = if self.tab == 0 {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .overflow_hidden()
                .child(toolbar)
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .child(Table::new(&self.table).stripe(true)),
                )
                .into_any_element()
        } else {
            self.render_performance(cx)
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .text_size(px(13.))
            .on_action(cx.listener(|this, _: &EndTask, _, cx| this.end_task(cx)))
            .on_action(cx.listener(|this, _: &CopyPid, _, cx| {
                let rows = this.menu_rows(cx);
                if !rows.is_empty() {
                    let pids: Vec<String> =
                        rows.iter().map(|(pid, _)| pid.to_string()).collect();
                    this.copy_to_clipboard(pids.join(", "), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CopyName, _, cx| {
                let rows = this.menu_rows(cx);
                if !rows.is_empty() {
                    let names: Vec<String> =
                        rows.iter().map(|(_, name)| name.to_string()).collect();
                    this.copy_to_clipboard(names.join(", "), cx);
                }
            }))
            .child(tab_bar)
            .child(stats_bar)
            .child(body)
    }
}

/// gpui-component's icons resolve through the app's AssetSource; the
/// published crate doesn't bundle them, so serve the few we use ourselves.
struct ArgusAssets;

impl gpui::AssetSource for ArgusAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        // Lucide icons, matching the icon set gpui-component targets.
        const CHECK_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>"##;
        const CIRCLE_X_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="m15 9-6 6"/><path d="m9 9 6 6"/></svg>"##;
        Ok(match path {
            "icons/check.svg" => Some(std::borrow::Cow::Borrowed(CHECK_SVG)),
            "icons/circle-x.svg" => Some(std::borrow::Cow::Borrowed(CIRCLE_X_SVG)),
            _ => None,
        })
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

fn main() {
    tlog("main entry");
    let rx = spawn_sampler();
    Application::new().with_assets(ArgusAssets).run(move |cx: &mut App| {
        tlog("gpui run callback");
        gpui_component::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        tlog("gpui_component init done");
        let bounds = Bounds::centered(None, size(px(1400.), px(1200.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Argus".into()),
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
