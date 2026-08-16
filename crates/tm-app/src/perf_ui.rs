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
const KERNEL_FILL: u32 = 0xf38ba866;
const MEM_FILL: u32 = 0xcba6f755;
const GPU_FILL: u32 = 0xa6e3a155;
const NET_FILL: u32 = 0x94e2d555;
const NET_TX_FILL: u32 = 0x89b4fa66;
const CHART_BG: u32 = 0x14141f;

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
    Net(u64),
}

#[derive(Default)]
pub struct PerfHistory {
    pub cpu_total: Series,
    pub cpu_kernel: Series,
    pub cores: Vec<(Series, Series)>,
    pub mem_pct: Series,
    pub gpus: Vec<Series>,
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
        for (i, pct) in perf.gpus.iter().enumerate() {
            self.gpus[i].push(*pct);
        }
        for adapter in &perf.adapters {
            let entry = self.net.entry(adapter.luid).or_default();
            entry.0.push(adapter.rx_bps as f32);
            entry.1.push(adapter.tx_bps as f32);
        }
        self.latest = perf.clone();
    }
}

/// Filled-area history chart. Each series is (values, fill color); values are
/// normalized against `max`. Newest sample hugs the right edge.
fn chart(series: Vec<(Vec<f32>, u32)>, max: f32, height: f32) -> AnyElement {
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
                    for (values, color) in &series {
                        if values.len() < 2 {
                            continue;
                        }
                        let n = values.len();
                        let step = w / (HISTORY.max(2) - 1) as f32;
                        let start_x = x0 + w - step * (n - 1) as f32;
                        let mut b = gpui::PathBuilder::fill();
                        b.move_to(point(px(start_x), px(y0 + h)));
                        for (i, v) in values.iter().enumerate() {
                            let x = start_x + step * i as f32;
                            let y = y0 + h * (1.0 - (v / max).clamp(0.0, 1.0));
                            b.line_to(point(px(x), px(y)));
                        }
                        b.line_to(point(px(x0 + w), px(y0 + h)));
                        b.close();
                        if let Ok(path) = b.build() {
                            window.paint_path(path, rgba(*color));
                        }
                    }
                },
            )
            .size_full(),
        )
        .into_any_element()
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
    fn perf_card(
        &self,
        cx: &mut Context<Self>,
        pane: Pane,
        title: String,
        line1: String,
        line2: String,
        mini: Vec<(Vec<f32>, u32)>,
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
            vec![(series_vec(&h.cpu_total), CPU_FILL)],
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
            vec![(series_vec(&h.mem_pct), MEM_FILL)],
            100.0,
        ));
        for (i, gpu) in h.gpus.iter().enumerate() {
            cards.push(self.perf_card(
                cx,
                Pane::Gpu(i),
                format!("GPU {i}"),
                format!("{:.1}%", gpu.latest()),
                String::new(),
                vec![(series_vec(gpu), GPU_FILL)],
                100.0,
            ));
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
                        (series_vec(rx), NET_FILL),
                        (series_vec(tx), NET_TX_FILL),
                    ],
                    peak,
                ));
            }
        }

        // ---- Detail pane ----
        let detail: AnyElement = match self.pane {
            Pane::Cpu => self.render_cpu_pane(),
            Pane::Memory => self.render_memory_pane(),
            Pane::Gpu(i) => self.render_gpu_pane(i),
            Pane::Net(luid) => self.render_net_pane(luid),
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

    fn render_cpu_pane(&self) -> AnyElement {
        let h = &self.history;
        let perf = &h.latest;
        let sys = &self.sys;
        let core_count = h.cores.len();
        let mut core_cells: Vec<AnyElement> = Vec::new();
        for (i, (total, kernel)) in h.cores.iter().enumerate() {
            core_cells.push(
                div()
                    .flex()
                    .flex_col()
                    .w(px(150.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(px(11.))
                            .text_color(rgb(TEXT_DIM))
                            .child(format!("CPU {i}"))
                            .child(format!("{:.0}%", total.latest())),
                    )
                    .child(chart(
                        vec![
                            (series_vec(total), CPU_FILL),
                            (series_vec(kernel), KERNEL_FILL),
                        ],
                        100.0,
                        64.,
                    ))
                    .into_any_element(),
            );
        }
        div()
            .id("cpu-pane")
            .flex()
            .flex_col()
            .gap(px(12.))
            .overflow_y_scroll()
            .child(div().text_color(rgb(TEXT)).text_size(px(16.)).child(perf.cpu_name.clone()))
            .child(chart(
                vec![
                    (series_vec(&h.cpu_total), CPU_FILL),
                    (series_vec(&h.cpu_kernel), KERNEL_FILL),
                ],
                100.0,
                180.,
            ))
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

    fn render_memory_pane(&self) -> AnyElement {
        let h = &self.history;
        let m = &h.latest.mem;
        div()
            .id("mem-pane")
            .flex()
            .flex_col()
            .gap(px(12.))
            .overflow_y_scroll()
            .child(div().text_color(rgb(TEXT)).text_size(px(16.)).child("Memory"))
            .child(chart(vec![(series_vec(&h.mem_pct), MEM_FILL)], 100.0, 260.))
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

    fn render_gpu_pane(&self, index: usize) -> AnyElement {
        let h = &self.history;
        let series = h.gpus.get(index).cloned().unwrap_or_default();
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .text_color(rgb(TEXT))
                    .text_size(px(16.))
                    .child(format!("GPU {index}")),
            )
            .child(chart(vec![(series_vec(&series), GPU_FILL)], 100.0, 260.))
            .child(div().flex().flex_wrap().children(vec![stat(
                "Utilization (busiest engine)",
                format!("{:.1}%", series.latest()),
            )]))
            .into_any_element()
    }

    fn render_net_pane(&self, luid: u64) -> AnyElement {
        let h = &self.history;
        let adapter = h
            .latest
            .adapters
            .iter()
            .find(|a| a.luid == luid)
            .cloned()
            .unwrap_or_default();
        let (rx, tx) = h.net.get(&luid).cloned().unwrap_or_default();
        let peak = rx.max().max(tx.max()).max(1.0);
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
            .child(chart(
                vec![
                    (series_vec(&rx), NET_FILL),
                    (series_vec(&tx), NET_TX_FILL),
                ],
                peak,
                260.,
            ))
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
                    ]),
            )
            .into_any_element()
    }
}
