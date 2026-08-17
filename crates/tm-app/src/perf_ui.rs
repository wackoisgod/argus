//! Performance tab: sidebar of resource cards with sparklines and a detail
//! pane with 60-second history charts, Task Manager style. Charts are drawn
//! with gpui's canvas + path API — filled area per series, newest at the
//! right edge.

use std::collections::VecDeque;

use gpui::{
    canvas, div, point, prelude::*, px, rgb, rgba, AnyElement, Context, SharedString, Window,
};
use rustc_hash::FxHashMap;
use tm_collector::{fmt_bytes, PerfInfo, Snapshot};

use crate::{fmt_mbps, TaskManagerApp, ACCENT, BG_HEADER, TEXT, TEXT_DIM};

pub const HISTORY: usize = 60;

const CPU_FILL: u32 = 0x89b4fa55;
const KERNEL_LINE: u32 = 0xf38ba8dd;
const MEM_FILL: u32 = 0xcba6f755;
const GPU_FILL: u32 = 0xa6e3a155;
const NET_FILL: u32 = 0x94e2d555;
const NET_TX_FILL: u32 = 0x89b4fa66;
const DISK_FILL: u32 = 0xf9e2af55;
const CHART_BG: u32 = 0x14141f;
const COMP_IN_USE: u32 = 0x89b4fa;
const COMP_MODIFIED: u32 = 0xf9e2af;
const COMP_STANDBY: u32 = 0xa6e3a1;
const COMP_FREE: u32 = 0x45475a;

#[derive(Default, Clone)]
pub struct Series(pub VecDeque<f32>);

impl Series {
    fn push(&mut self, v: f32) {
        if self.0.len() >= HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(v);
    }
    fn latest(&self) -> f32 {
        self.0.back().copied().unwrap_or(0.0)
    }
    fn max(&self) -> f32 {
        self.0.iter().copied().fold(0.0, f32::max)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Cpu,
    Memory,
    Gpu(usize),
    Disk(u32),
    Net(u64),
}

#[derive(Default)]
pub struct PerfHistory {
    /// When the newest sample landed — drives sub-second scroll animation.
    pub last_push: Option<std::time::Instant>,
    pub cpu_total: Series,
    pub cpu_kernel: Series,
    pub cores: Vec<(Series, Series)>,
    pub mem_pct: Series,
    pub gpus: Vec<Series>,
    pub gpu_engines: Vec<Vec<Series>>,
    pub disks: FxHashMap<u32, (Series, Series, Series)>, // active%, read, write
    pub net: FxHashMap<u64, (Series, Series)>,
    pub latest: PerfInfo,
}

impl PerfHistory {
    pub fn update(&mut self, snap: &Snapshot) {
        let perf = &snap.perf;
        if perf.cores.is_empty() {
            return; // light tick — window minimized, keep history frozen
        }
        self.cpu_total.push(snap.system.cpu_percent);
        let kernel_avg =
            perf.cores.iter().map(|c| c.kernel).sum::<f32>() / perf.cores.len() as f32;
        self.cpu_kernel.push(kernel_avg);
        self.cores
            .resize_with(perf.cores.len(), Default::default);
        for (i, core) in perf.cores.iter().enumerate() {
            self.cores[i].0.push(core.total);
            self.cores[i].1.push(core.kernel);
        }
        if perf.mem.total > 0 {
            self.mem_pct
                .push(perf.mem.in_use as f32 / perf.mem.total as f32 * 100.0);
        }
        self.gpus.resize_with(perf.gpus.len(), Default::default);
        self.gpu_engines
            .resize_with(perf.gpus.len(), Default::default);
        for (i, gpu) in perf.gpus.iter().enumerate() {
            self.gpus[i].push(gpu.utilization);
            self.gpu_engines[i].resize_with(gpu.engine_pcts.len(), Default::default);
            for (e, pct) in gpu.engine_pcts.iter().enumerate() {
                self.gpu_engines[i][e].push(*pct);
            }
        }
        for disk in &perf.disks {
            let entry = self.disks.entry(disk.index).or_default();
            entry.0.push(disk.active_pct);
            entry.1.push(disk.read_bps as f32);
            entry.2.push(disk.write_bps as f32);
        }
        for adapter in &perf.adapters {
            let entry = self.net.entry(adapter.luid).or_default();
            entry.0.push(adapter.rx_bps as f32);
            entry.1.push(adapter.tx_bps as f32);
        }
        self.latest = perf.clone();
        self.last_push = Some(std::time::Instant::now());
    }

    /// Fraction of one sample step elapsed since the newest sample: charts
    /// shift left by this much for continuous scrolling.
    pub fn anim_offset(&self) -> f32 {
        self.last_push
            .map(|t| t.elapsed().as_secs_f32().min(1.0))
            .unwrap_or(0.0)
    }
}

/// One chart series: history values, color, and how to draw it.
#[derive(Clone)]
pub struct ChartSeries {
    values: Vec<f32>,
    color: u32,
    /// Stroked line instead of a filled area (e.g. the kernel overlay).
    stroke: bool,
}

fn fill(values: Vec<f32>, color: u32) -> ChartSeries {
    ChartSeries {
        values,
        color,
        stroke: false,
    }
}

fn line(values: Vec<f32>, color: u32) -> ChartSeries {
    ChartSeries {
        values,
        color,
        stroke: true,
    }
}

/// History chart. Values normalize against `max`; newest sample hugs the
/// right edge; `offset` (0..1 of one sample step) slides everything left for
/// continuous scroll, with the newest value held to the right edge so no gap
/// appears.
fn chart_with(series: Vec<ChartSeries>, max: f32, height: f32, offset: f32) -> gpui::Div {
    let max = max.max(1e-6);
    div()
        .w_full()
        .h(px(height))
        .bg(rgb(CHART_BG))
        .border_1()
        .border_color(rgb(0x2a2a3d))
        .rounded(px(4.))
        .overflow_hidden()
        .child(
            canvas(
                move |_, _, _| {},
                move |bounds, _, window, _| {
                    let w = f32::from(bounds.size.width);
                    let h = f32::from(bounds.size.height);
                    let x0 = f32::from(bounds.origin.x);
                    let y0 = f32::from(bounds.origin.y);
                    for s in &series {
                        if s.values.len() < 2 {
                            continue;
                        }
                        let n = s.values.len();
                        let step = w / (HISTORY.max(2) - 1) as f32;
                        let start_x = x0 + w - step * (n - 1) as f32 - step * offset;
                        let mut b = if s.stroke {
                            gpui::PathBuilder::stroke(px(1.5))
                        } else {
                            gpui::PathBuilder::fill()
                        };
                        let mut last_y = y0 + h;
                        for (i, v) in s.values.iter().enumerate() {
                            let x = start_x + step * i as f32;
                            let y = y0 + h * (1.0 - (v / max).clamp(0.0, 1.0));
                            if i == 0 && !s.stroke {
                                b.move_to(point(px(start_x), px(y0 + h)));
                            }
                            if i == 0 && s.stroke {
                                b.move_to(point(px(x), px(y)));
                            } else {
                                b.line_to(point(px(x), px(y)));
                            }
                            last_y = y;
                        }
                        // Hold the newest value to the right edge.
                        b.line_to(point(px(x0 + w), px(last_y)));
                        if !s.stroke {
                            b.line_to(point(px(x0 + w), px(y0 + h)));
                            b.close();
                        }
                        if let Ok(path) = b.build() {
                            window.paint_path(path, rgba(s.color));
                        }
                    }
                },
            )
            .size_full(),
        )
}

fn chart(series: Vec<ChartSeries>, max: f32, height: f32) -> AnyElement {
    chart_with(series, max, height, 0.0).into_any_element()
}

/// Value formatting for hover tooltips.
#[derive(Clone, Copy)]
pub enum Unit {
    Percent,
    Mibs,
    Mbps,
}

impl Unit {
    fn format(self, v: f32) -> String {
        match self {
            Unit::Percent => format!("{v:.1}%"),
            Unit::Mibs => crate::fmt_mibs(v as u64),
            Unit::Mbps => crate::fmt_mbps(v as u64),
        }
    }
}

fn series_vec(s: &Series) -> Vec<f32> {
    s.0.iter().copied().collect()
}

fn stat(label: &str, value: String) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .w(px(220.))
        .mb(px(10.))
        .child(div().text_color(rgb(TEXT_DIM)).child(label.to_string()))
        .child(div().text_color(rgb(TEXT)).child(value))
        .into_any_element()
}

fn fmt_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

impl TaskManagerApp {
    /// A pane chart with hover: moving the mouse across it shows the value
    /// at that point in history, Task Manager style.
    fn hover_chart(
        &self,
        cx: &mut Context<Self>,
        key: &'static str,
        series: Vec<ChartSeries>,
        max: f32,
        height: f32,
        offset: f32,
        unit: Unit,
    ) -> AnyElement {
        use std::cell::Cell;
        use std::rc::Rc;
        let geom: Rc<Cell<(f32, f32)>> = Rc::new(Cell::new((0.0, 0.0)));
        let geom_paint = Rc::clone(&geom);
        let primary: Vec<f32> = series
            .first()
            .map(|s| s.values.clone())
            .unwrap_or_default();
        let hovered_frac = match &self.chart_hover {
            Some((k, frac)) if k == key => Some(*frac),
            _ => None,
        };
        let mut container = div()
            .id(key)
            .relative()
            .w_full()
            .h(px(height))
            .child(
                chart_with(series, max, height, offset)
                    .absolute()
                    .top_0()
                    .left_0(),
            )
            .child(
                // Invisible geometry probe: records where the chart landed.
                div().absolute().top_0().left_0().size_full().child(
                    canvas(
                        move |bounds, _, _| {
                            geom_paint.set((
                                f32::from(bounds.origin.x),
                                f32::from(bounds.size.width),
                            ));
                        },
                        |_, _, _, _| {},
                    )
                    .size_full(),
                ),
            )
            .on_mouse_move(cx.listener(move |this, e: &gpui::MouseMoveEvent, _, cx| {
                let (x0, w) = geom.get();
                if w > 0.0 {
                    let frac = (f32::from(e.position.x) - x0) / w;
                    if (0.0..=1.0).contains(&frac) {
                        this.chart_hover = Some((key.into(), frac));
                        cx.notify();
                    } else if matches!(&this.chart_hover, Some((k, _)) if k == key) {
                        this.chart_hover = None;
                        cx.notify();
                    }
                }
            }))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if !hovered && matches!(&this.chart_hover, Some((k, _)) if k == key) {
                    this.chart_hover = None;
                    cx.notify();
                }
            }));
        if let Some(frac) = hovered_frac {
            // Map fraction of the window to an index in the (possibly
            // shorter) series, anchored at the right edge.
            let slot = (frac * (HISTORY - 1) as f32).round() as usize;
            let missing = HISTORY.saturating_sub(primary.len());
            let label = if slot >= missing && !primary.is_empty() {
                let idx = (slot - missing).min(primary.len() - 1);
                let ago = HISTORY - 1 - slot;
                Some(format!(
                    "{}  ·  {}s ago",
                    unit.format(primary[idx]),
                    ago
                ))
            } else {
                None
            };
            container = container.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(gpui::relative(frac))
                    .w(px(1.))
                    .bg(rgb(0x6c7086)),
            );
            if let Some(label) = label {
                container = container.child(
                    div()
                        .absolute()
                        .top(px(6.))
                        .left(gpui::relative(frac.min(0.75)))
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(px(4.))
                        .bg(rgb(0x2a2a3d))
                        .text_size(px(11.))
                        .text_color(rgb(TEXT))
                        .child(label),
                );
            }
        }
        container.into_any_element()
    }

    fn perf_card(
        &self,
        cx: &mut Context<Self>,
        pane: Pane,
        title: String,
        line1: String,
        line2: String,
        mini: Vec<ChartSeries>,
        mini_max: f32,
    ) -> AnyElement {
        let selected = self.pane == pane;
        let id: SharedString = format!("card-{title}-{line1}").into();
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(8.))
            .p(px(8.))
            .mb(px(6.))
            .rounded(px(6.))
            .border_1()
            .border_color(if selected {
                rgb(ACCENT)
            } else {
                rgb(0x2a2a3d)
            })
            .bg(rgb(if selected { 0x1c1c2e } else { CHART_BG }))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.pane = pane;
                cx.notify();
            }))
            .child(div().w(px(64.)).flex_none().child(chart(mini, mini_max, 36.)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(div().text_color(rgb(TEXT)).child(title))
                    .child(
                        div()
                            .text_color(rgb(TEXT_DIM))
                            .text_size(px(11.))
                            .whitespace_nowrap()
                            .child(line1),
                    )
                    .child(
                        div()
                            .text_color(rgb(TEXT_DIM))
                            .text_size(px(11.))
                            .whitespace_nowrap()
                            .child(line2),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_performance(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let h = &self.history;
        let perf = h.latest.clone();

        // ---- Sidebar cards ----
        let mut cards: Vec<AnyElement> = Vec::new();
        cards.push(self.perf_card(
            cx,
            Pane::Cpu,
            "CPU".into(),
            perf.cpu_name.clone(),
            format!("{:.1}%  ·  {} MHz base", h.cpu_total.latest(), perf.cpu_mhz),
            vec![fill(series_vec(&h.cpu_total), CPU_FILL)],
            100.0,
        ));
        cards.push(self.perf_card(
            cx,
            Pane::Memory,
            "Memory".into(),
            format!(
                "{} / {}",
                fmt_bytes(perf.mem.in_use),
                fmt_bytes(perf.mem.total)
            ),
            format!("{:.1}%", h.mem_pct.latest()),
            vec![fill(series_vec(&h.mem_pct), MEM_FILL)],
            100.0,
        ));
        for (i, gpu) in h.gpus.iter().enumerate() {
            let info = perf.gpus.get(i);
            let name = info
                .map(|g| g.name.to_string())
                .unwrap_or_else(|| format!("GPU {i}"));
            let temp = info
                .and_then(|g| g.temperature_c)
                .map(|t| format!("  ·  {t:.0} °C"))
                .unwrap_or_default();
            cards.push(self.perf_card(
                cx,
                Pane::Gpu(i),
                format!("GPU {i}"),
                name,
                format!("{:.1}%{temp}", gpu.latest()),
                vec![fill(series_vec(gpu), GPU_FILL)],
                100.0,
            ));
        }
        for disk in &perf.disks {
            if let Some((active, _, _)) = h.disks.get(&disk.index) {
                cards.push(self.perf_card(
                    cx,
                    Pane::Disk(disk.index),
                    format!("Disk {}", disk.index),
                    disk.model.clone(),
                    format!("{:.1}%  ·  {}", active.latest(), fmt_bytes(disk.size_bytes)),
                    vec![fill(series_vec(active), DISK_FILL)],
                    100.0,
                ));
            }
        }
        for adapter in &perf.adapters {
            if let Some((rx, tx)) = h.net.get(&adapter.luid) {
                let peak = (rx.max().max(tx.max())).max(1.0);
                cards.push(self.perf_card(
                    cx,
                    Pane::Net(adapter.luid),
                    adapter.name.clone(),
                    fmt_mbps((adapter.rx_bps + adapter.tx_bps) as u64),
                    if adapter.connected {
                        "Connected".into()
                    } else {
                        String::new()
                    },
                    vec![
                        fill(series_vec(rx), NET_FILL),
                        fill(series_vec(tx), NET_TX_FILL),
                    ],
                    peak,
                ));
            }
        }

        // ---- Detail pane ----
        let detail: AnyElement = match self.pane {
            Pane::Cpu => self.render_cpu_pane(cx),
            Pane::Memory => self.render_memory_pane(cx),
            Pane::Gpu(i) => self.render_gpu_pane(cx, i),
            Pane::Disk(i) => self.render_disk_pane(cx, i),
            Pane::Net(luid) => self.render_net_pane(cx, luid),
        };

        div()
            .flex()
            .flex_1()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(280.))
                    .flex_none()
                    .p(px(8.))
                    .bg(rgb(BG_HEADER))
                    .overflow_hidden()
                    .children(cards),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .p(px(16.))
                    .overflow_hidden()
                    .child(detail),
            )
            .into_any_element()
    }

    fn render_cpu_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let h = &self.history;
        let offset = h.anim_offset();
        let perf = h.latest.clone();
        let sys = self.sys.clone();
        let core_count = h.cores.len();
        let mut core_cells: Vec<AnyElement> = Vec::new();
        for (i, (total, kernel)) in h.cores.iter().enumerate() {
            // Auto-scale each cell to its own recent peak (15% floor) so
            // lightly-loaded cores still show a readable waveform instead of
            // a sliver pinned to the bottom.
            let cell_max = total.max().max(15.0);
            core_cells.push(
                div()
                    .flex()
                    .flex_col()
                    .w(px(170.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(px(11.))
                            .text_color(rgb(TEXT_DIM))
                            .child(format!("CPU {i}"))
                            .child(format!("{:.0}%", total.latest())),
                    )
                    .child(
                        chart_with(
                            vec![
                                fill(series_vec(total), CPU_FILL),
                                line(series_vec(kernel), KERNEL_LINE),
                            ],
                            cell_max,
                            80.,
                            offset,
                        )
                        .into_any_element(),
                    )
                    .into_any_element(),
            );
        }
        let main_chart = self.hover_chart(
            cx,
            "cpu-main",
            vec![
                fill(series_vec(&self.history.cpu_total), CPU_FILL),
                line(series_vec(&self.history.cpu_kernel), KERNEL_LINE),
            ],
            100.0,
            180.,
            offset,
            Unit::Percent,
        );
        div()
            .id("cpu-pane")
            .flex()
            .flex_col()
            .gap(px(12.))
            .overflow_y_scroll()
            .child(div().text_color(rgb(TEXT)).text_size(px(16.)).child(perf.cpu_name.clone()))
            .child(main_chart)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .children(core_cells),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .children(vec![
                        stat("Utilization", format!("{:.1}%", h.cpu_total.latest())),
                        stat("Kernel time", format!("{:.1}%", h.cpu_kernel.latest())),
                        stat("Base speed", format!("{} MHz", perf.cpu_mhz)),
                        stat("Logical processors", core_count.to_string()),
                        stat("Processes", sys.process_count.to_string()),
                        stat("Threads", sys.thread_count.to_string()),
                        stat("Handles", sys.handle_count.to_string()),
                        stat("Up time", fmt_uptime(perf.uptime_secs)),
                    ]),
            )
            .into_any_element()
    }

    fn render_memory_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let mem_chart = self.hover_chart(
            cx,
            "mem-main",
            vec![fill(series_vec(&self.history.mem_pct), MEM_FILL)],
            100.0,
            220.,
            self.history.anim_offset(),
            Unit::Percent,
        );
        let h = &self.history;
        let m = &h.latest.mem;
        // Composition bar: in-use | modified | standby | free, spanning
        // physical RAM.
        let composition: AnyElement = if m.total > 0 && m.standby + m.free > 0 {
            let in_use = m
                .total
                .saturating_sub(m.standby + m.modified + m.free);
            let seg = |bytes: u64, color: u32| {
                div()
                    .h_full()
                    .w(gpui::relative((bytes as f32 / m.total as f32).min(1.0)))
                    .bg(rgb(color))
                    .into_any_element()
            };
            let legend = |label: &str, bytes: u64, color: u32| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().w(px(10.)).h(px(10.)).rounded(px(2.)).bg(rgb(color)))
                    .child(
                        div()
                            .text_color(rgb(TEXT_DIM))
                            .text_size(px(11.))
                            .child(format!("{label} {}", fmt_bytes(bytes))),
                    )
                    .into_any_element()
            };
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(div().text_color(rgb(TEXT_DIM)).child("Memory composition"))
                .child(
                    div()
                        .flex()
                        .h(px(24.))
                        .w_full()
                        .rounded(px(4.))
                        .overflow_hidden()
                        .child(seg(in_use, COMP_IN_USE))
                        .child(seg(m.modified, COMP_MODIFIED))
                        .child(seg(m.standby, COMP_STANDBY))
                        .child(seg(m.free, COMP_FREE)),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(16.))
                        .child(legend("In use", in_use, COMP_IN_USE))
                        .child(legend("Modified", m.modified, COMP_MODIFIED))
                        .child(legend("Standby", m.standby, COMP_STANDBY))
                        .child(legend("Free", m.free, COMP_FREE)),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };
        div()
            .id("mem-pane")
            .flex()
            .flex_col()
            .gap(px(12.))
            .overflow_y_scroll()
            .child(div().text_color(rgb(TEXT)).text_size(px(16.)).child("Memory"))
            .child(mem_chart)
            .child(composition)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .children(vec![
                        stat("In use", fmt_bytes(m.in_use)),
                        stat("Available", fmt_bytes(m.available)),
                        stat(
                            "Committed",
                            format!(
                                "{} / {}",
                                fmt_bytes(m.commit_used),
                                fmt_bytes(m.commit_limit)
                            ),
                        ),
                        stat("Cached", fmt_bytes(m.cached)),
                        stat("Paged pool", fmt_bytes(m.paged_pool)),
                        stat("Non-paged pool", fmt_bytes(m.nonpaged_pool)),
                    ]),
            )
            .into_any_element()
    }

    fn render_disk_pane(&self, cx: &mut Context<Self>, index: u32) -> AnyElement {
        let h = &self.history;
        let offset = h.anim_offset();
        let disk = h
            .latest
            .disks
            .iter()
            .find(|d| d.index == index)
            .cloned()
            .unwrap_or_default();
        let (active, read, write) = h.disks.get(&index).cloned().unwrap_or_default();
        let peak = read.max().max(write.max()).max(1.0);
        let active_chart = self.hover_chart(
            cx,
            "disk-active",
            vec![fill(series_vec(&active), DISK_FILL)],
            100.0,
            160.,
            offset,
            Unit::Percent,
        );
        let rate_chart = self.hover_chart(
            cx,
            "disk-rate",
            vec![
                fill(series_vec(&write), DISK_FILL),
                fill(series_vec(&read), NET_TX_FILL),
            ],
            peak,
            120.,
            offset,
            Unit::Mibs,
        );
        div()
            .id("disk-pane")
            .flex()
            .flex_col()
            .gap(px(12.))
            .overflow_y_scroll()
            .child(
                div()
                    .text_color(rgb(TEXT))
                    .text_size(px(16.))
                    .child(format!("Disk {} — {}", disk.index, disk.model)),
            )
            .child(div().text_color(rgb(TEXT_DIM)).child("Active time"))
            .child(active_chart)
            .child(div().text_color(rgb(TEXT_DIM)).child("Disk transfer rate"))
            .child(rate_chart)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .children(vec![
                        stat("Active time", format!("{:.1}%", active.latest())),
                        stat("Read speed", crate::fmt_mibs(disk.read_bps)),
                        stat("Write speed", crate::fmt_mibs(disk.write_bps)),
                        stat("Capacity", fmt_bytes(disk.size_bytes)),
                    ]),
            )
            .into_any_element()
    }

    fn render_gpu_pane(&self, cx: &mut Context<Self>, index: usize) -> AnyElement {
        let offset = self.history.anim_offset();
        let main_chart = self.hover_chart(
            cx,
            "gpu-main",
            vec![fill(series_vec(&self.history.gpus.get(index).cloned().unwrap_or_default()), GPU_FILL)],
            100.0,
            180.,
            offset,
            Unit::Percent,
        );
        let h = &self.history;
        let series = h.gpus.get(index).cloned().unwrap_or_default();
        let info = h.latest.gpus.get(index).cloned().unwrap_or_default();
        // Engine grid: engines with any recent activity, plus the always-
        // interesting ones by name; capped so 33-node adapters stay sane.
        let mut engine_cells: Vec<AnyElement> = Vec::new();
        if let Some(engines) = h.gpu_engines.get(index) {
            let mut shown = 0;
            for (e, hist) in engines.iter().enumerate() {
                let name = info
                    .engine_names
                    .get(e)
                    .cloned()
                    .unwrap_or_else(|| format!("Engine {e}"));
                let interesting = hist.max() > 0.5
                    || name.contains("3D")
                    || name.contains("Copy")
                    || name.contains("Video");
                if !interesting || shown >= 10 {
                    continue;
                }
                shown += 1;
                engine_cells.push(
                    div()
                        .flex()
                        .flex_col()
                        .w(px(220.))
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .text_size(px(11.))
                                .text_color(rgb(TEXT_DIM))
                                .child(name)
                                .child(format!("{:.1}%", hist.latest())),
                        )
                        .child(
                            chart_with(vec![fill(series_vec(hist), GPU_FILL)], 100.0, 70., offset)
                                .into_any_element(),
                        )
                        .into_any_element(),
                );
            }
        }
        let vram: AnyElement = if info.vram_total > 0 {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().text_color(rgb(TEXT_DIM)).child(format!(
                    "VRAM  {} / {}",
                    fmt_bytes(info.vram_used),
                    fmt_bytes(info.vram_total)
                )))
                .child(
                    div()
                        .h(px(14.))
                        .w_full()
                        .rounded(px(3.))
                        .bg(rgb(CHART_BG))
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(
                                    (info.vram_used as f32 / info.vram_total as f32)
                                        .min(1.0),
                                ))
                                .rounded(px(3.))
                                .bg(rgb(0x94e2d5)),
                        ),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };
        let mut stats: Vec<AnyElement> = vec![stat(
            "Utilization (busiest engine)",
            format!("{:.1}%", series.latest()),
        )];
        if info.vram_total > 0 {
            stats.push(stat(
                "Dedicated memory",
                format!(
                    "{} / {}",
                    fmt_bytes(info.vram_used),
                    fmt_bytes(info.vram_total)
                ),
            ));
        }
        if info.shared_used > 0 {
            let value = if info.shared_total > 0 {
                format!(
                    "{} / {}",
                    fmt_bytes(info.shared_used),
                    fmt_bytes(info.shared_total)
                )
            } else {
                fmt_bytes(info.shared_used)
            };
            stats.push(stat("Shared memory", value));
        }
        if let Some(t) = info.temperature_c {
            stats.push(stat("Temperature", format!("{t:.0} °C")));
        }
        div()
            .id("gpu-pane")
            .flex()
            .flex_col()
            .gap(px(12.))
            .overflow_y_scroll()
            .child(
                div()
                    .text_color(rgb(TEXT))
                    .text_size(px(16.))
                    .child(info.name.to_string()),
            )
            .child(main_chart)
            .child(div().flex().flex_wrap().gap(px(8.)).children(engine_cells))
            .child(vram)
            .child(div().flex().flex_wrap().children(stats))
            .into_any_element()
    }

    fn render_net_pane(&self, cx: &mut Context<Self>, luid: u64) -> AnyElement {
        let h = &self.history;
        let offset = h.anim_offset();
        let adapter = h
            .latest
            .adapters
            .iter()
            .find(|a| a.luid == luid)
            .cloned()
            .unwrap_or_default();
        let (rx, tx) = h.net.get(&luid).cloned().unwrap_or_default();
        let peak = rx.max().max(tx.max()).max(1.0);
        let main_chart = self.hover_chart(
            cx,
            "net-main",
            vec![
                fill(series_vec(&rx), NET_FILL),
                fill(series_vec(&tx), NET_TX_FILL),
            ],
            peak,
            260.,
            offset,
            Unit::Mbps,
        );
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(
                        div()
                            .text_color(rgb(TEXT))
                            .text_size(px(16.))
                            .child(adapter.name.clone()),
                    )
                    .child(
                        div()
                            .text_color(rgb(TEXT_DIM))
                            .child(format!("peak {}", fmt_mbps(peak as u64))),
                    ),
            )
            .child(main_chart)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .children(vec![
                        stat("Receive", fmt_mbps(adapter.rx_bps)),
                        stat("Send", fmt_mbps(adapter.tx_bps)),
                        stat(
                            "Link speed",
                            format!("{:.0} Mbps", adapter.link_speed as f64 / 1e6),
                        ),
                        stat(
                            "Status",
                            if adapter.connected {
                                "Connected".into()
                            } else {
                                "Disconnected".into()
                            },
                        ),
                        stat("MAC address", adapter.mac.clone()),
                        stat("IPv4 address", adapter.ipv4.clone()),
                        stat("IPv6 address", adapter.ipv6.clone()),
                    ]),
            )
            .into_any_element()
    }
}
