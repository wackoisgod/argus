//! tm-collector: fast Windows process/system data collection.
//!
//! Design: a [`Sampler`] is called on a fixed tick (e.g. 1s). Each call makes
//! a single `NtQuerySystemInformation` round-trip plus two cheap Win32 calls,
//! then computes rates (CPU %, I/O per second) against the previous tick.
//! Buffers are reused, so steady-state cost is one kernel copy and the
//! per-process name strings.

mod disk;
mod enrich;
mod etw;
mod gpu;
mod icon;
mod net;
mod nt;
mod perf;
mod windows_q;

pub use disk::DiskStats;
pub use enrich::{Enriched, Enricher};
pub use etw::{EtwMonitor, IoTotals};
pub use gpu::GpuAdapterPerf;
pub use nt::RawProcess;
pub use perf::{CoreLoad, MemDetail, NetAdapterStats, PerfInfo};

use std::sync::Arc;

use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;

/// One process with derived per-tick rates.
#[derive(Debug, Clone)]
pub struct ProcessStats {
    pub raw: RawProcess,
    /// Percent of total system CPU (all cores = 100).
    pub cpu_percent: f32,
    /// Kernel-mode share of `cpu_percent`.
    pub kernel_percent: f32,
    /// GPU engine utilization percent across all adapters/nodes.
    pub gpu_percent: f32,
    pub read_bytes_per_sec: u64,
    pub write_bytes_per_sec: u64,
    /// Disk bytes/sec. Kernel-ETW truth when the session is available,
    /// otherwise approximated from read+write transfer counters.
    pub disk_bytes_per_sec: u64,
    /// Network bytes/sec. Kernel-ETW truth when available; otherwise
    /// approximated from "other" transfer counters (socket I/O flows through
    /// AFD ioctls), gated to processes that own TCP/UDP endpoints.
    pub net_bytes_per_sec: u64,
    /// User + description, resolved asynchronously on the enrichment pool;
    /// `None` until the first resolution for this process completes.
    pub enriched: Option<Arc<Enriched>>,
    /// Owns a visible, titled top-level window (Task Manager's "Apps" test).
    pub has_window: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SystemStats {
    pub cpu_percent: f32,
    pub mem_total: u64,
    pub mem_available: u64,
    pub process_count: u32,
    pub thread_count: u32,
    pub handle_count: u32,
    pub logical_cpus: u32,
    /// Whether the ETW session is running (needs elevation); when false the
    /// disk/network per-process rates are unavailable.
    pub etw_active: bool,
}

impl SystemStats {
    pub fn mem_used(&self) -> u64 {
        self.mem_total.saturating_sub(self.mem_available)
    }
    pub fn mem_percent(&self) -> f32 {
        if self.mem_total == 0 {
            0.0
        } else {
            self.mem_used() as f32 / self.mem_total as f32 * 100.0
        }
    }
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub system: SystemStats,
    pub processes: Vec<ProcessStats>,
    /// System-wide performance data; empty (`cores.is_empty()`) on light
    /// ticks taken while the window is minimized.
    pub perf: PerfInfo,
}

/// Per-process counters remembered from the previous tick.
struct PrevProc {
    cpu_100ns: i64,
    kernel_100ns: i64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
    etw: IoTotals,
}

pub struct Sampler {
    query: nt::ProcessQuery,
    enricher: Enricher,
    etw: EtwMonitor,
    gpu: gpu::GpuMonitor,
    conn: net::ConnQuery,
    perf: perf::PerfSampler,
    raw: Vec<RawProcess>,
    // Keyed by (pid, create_time) so a reused pid doesn't inherit deltas.
    prev: FxHashMap<(u32, i64), PrevProc>,
    prev_idle: u64,
    prev_busy_total: u64, // kernel(incl. idle) + user across all cores
    prev_tick: Option<Instant>,
    logical_cpus: u32,
}

fn filetime_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

impl Sampler {
    pub fn new() -> Self {
        Sampler {
            query: nt::ProcessQuery::new(),
            enricher: Enricher::new(),
            etw: EtwMonitor::start(),
            gpu: gpu::GpuMonitor::new(),
            conn: net::ConnQuery::new(),
            perf: perf::PerfSampler::new(),
            raw: Vec::new(),
            prev: FxHashMap::default(),
            prev_idle: 0,
            prev_busy_total: 0,
            prev_tick: None,
            logical_cpus: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1),
        }
    }

    /// Take a snapshot. The first call has no baseline, so all rates are 0;
    /// call once at startup and then on every tick.
    pub fn sample(&mut self) -> Snapshot {
        self.sample_with(false)
    }

    /// `light` skips the optional per-process extras (GPU probing, the
    /// connection-table walk) — for ticks taken while the window is
    /// minimized, where nobody sees those columns. Rate math stays correct
    /// across the gap because GPU deltas carry their own timestamps.
    pub fn sample_with(&mut self, light: bool) -> Snapshot {
        self.sample_with_opts(light, false)
    }

    /// `gpu_relaxed` reuses the previous tick's adapter-wide GPU data
    /// (engines, VRAM, temperature) instead of re-probing the driver — every
    /// D3DKMT query can stall the CPU inside the display driver, and while
    /// the Performance tab isn't visible nobody needs 1s-fresh engine data.
    /// Per-process GPU attribution still samples every tick.
    pub fn sample_with_opts(&mut self, light: bool, gpu_relaxed: bool) -> Snapshot {
        let now = Instant::now();
        let elapsed = self
            .prev_tick
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        self.prev_tick = Some(now);

        // System-wide CPU: GetSystemTimes sums across all cores; kernel time
        // includes idle time.
        let (mut idle, mut kernel, mut user) = (
            FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 },
            FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 },
            FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 },
        );
        unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
        let idle = filetime_u64(idle);
        let total = filetime_u64(kernel) + filetime_u64(user);
        let d_total = total.saturating_sub(self.prev_busy_total);
        let d_idle = idle.saturating_sub(self.prev_idle);
        let cpu_percent = if self.prev_busy_total != 0 && d_total > 0 {
            (d_total.saturating_sub(d_idle)) as f32 / d_total as f32 * 100.0
        } else {
            0.0
        };
        self.prev_busy_total = total;
        self.prev_idle = idle;

        let mut mem = unsafe { std::mem::zeroed::<MEMORYSTATUSEX>() };
        mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        unsafe { GlobalMemoryStatusEx(&mut mem) };

        if let Err(status) = self.query.query(&mut self.raw) {
            // Nothing sane to do per-tick; return system stats with an empty
            // process list rather than panicking the sampler thread.
            eprintln!("NtQuerySystemInformation failed: {status:#x}");
            self.raw.clear();
        }

        let mut thread_count = 0u32;
        let mut handle_count = 0u32;
        let mut next_prev: FxHashMap<(u32, i64), PrevProc> =
            FxHashMap::with_capacity_and_hasher(self.raw.len(), Default::default());
        let mut processes = Vec::with_capacity(self.raw.len());
        let etw_totals = self.etw.totals();
        let gpu_pcts = if light {
            FxHashMap::default()
        } else {
            self.gpu.sample(&self.raw)
        };
        let window_pids = windows_q::pids_with_visible_windows();
        // Without ETW, network is approximated from AFD-ioctl counters;
        // only processes actually owning sockets get attributed.
        let conn_pids = if self.etw.active || light {
            FxHashSet::default()
        } else {
            self.conn.pids_with_connections()
        };

        for raw in &self.raw {
            thread_count += raw.threads;
            handle_count += raw.handles;
            let key = (raw.pid, raw.create_time);
            let cpu_100ns = raw.user_time_100ns + raw.kernel_time_100ns;
            let etw = etw_totals.get(&raw.pid).copied().unwrap_or_default();
            let (mut cpu_pct, mut read_bps, mut write_bps) = (0.0f32, 0u64, 0u64);
            let (mut disk_bps, mut net_bps) = (0u64, 0u64);
            let mut kernel_pct = 0.0f32;
            if let Some(prev) = self.prev.get(&key) {
                if d_total > 0 {
                    let d = (cpu_100ns - prev.cpu_100ns).max(0) as u64;
                    cpu_pct = d as f32 / d_total as f32 * 100.0;
                    let dk = (raw.kernel_time_100ns - prev.kernel_100ns).max(0) as u64;
                    kernel_pct = dk as f32 / d_total as f32 * 100.0;
                }
                if elapsed > 0.05 {
                    read_bps = ((raw.read_bytes.saturating_sub(prev.read_bytes)) as f64
                        / elapsed) as u64;
                    write_bps = ((raw.write_bytes.saturating_sub(prev.write_bytes)) as f64
                        / elapsed) as u64;
                    if self.etw.active {
                        let d_disk = (etw.disk_read + etw.disk_write)
                            .saturating_sub(prev.etw.disk_read + prev.etw.disk_write);
                        let d_net = (etw.net_send + etw.net_recv)
                            .saturating_sub(prev.etw.net_send + prev.etw.net_recv);
                        disk_bps = (d_disk as f64 / elapsed) as u64;
                        net_bps = (d_net as f64 / elapsed) as u64;
                    } else {
                        disk_bps = read_bps + write_bps;
                        if conn_pids.contains(&raw.pid) {
                            let d_other =
                                raw.other_bytes.saturating_sub(prev.other_bytes);
                            net_bps = (d_other as f64 / elapsed) as u64;
                        }
                    }
                }
            }
            next_prev.insert(
                key,
                PrevProc {
                    cpu_100ns,
                    kernel_100ns: raw.kernel_time_100ns,
                    read_bytes: raw.read_bytes,
                    write_bytes: raw.write_bytes,
                    other_bytes: raw.other_bytes,
                    etw,
                },
            );
            processes.push(ProcessStats {
                raw: raw.clone(),
                cpu_percent: cpu_pct,
                kernel_percent: kernel_pct,
                gpu_percent: gpu_pcts.get(&raw.pid).copied().unwrap_or(0.0),
                read_bytes_per_sec: read_bps,
                write_bytes_per_sec: write_bps,
                disk_bytes_per_sec: disk_bps,
                net_bytes_per_sec: net_bps,
                enriched: self.enricher.get_or_schedule(key),
                has_window: window_pids.contains(&raw.pid),
            });
        }
        self.prev = next_prev;
        self.enricher.retain(|k| self.prev.contains_key(k));
        let live_pids: FxHashSet<u32> = self.raw.iter().map(|r| r.pid).collect();
        self.etw.retain(|pid| live_pids.contains(&pid));

        let perf = if light {
            PerfInfo::default()
        } else {
            let gpus = if gpu_relaxed {
                self.gpu.last_perf()
            } else {
                self.gpu.adapters_perf()
            };
            self.perf.sample(gpus)
        };

        Snapshot {
            perf,
            system: SystemStats {
                cpu_percent,
                mem_total: mem.ullTotalPhys,
                mem_available: mem.ullAvailPhys,
                process_count: processes.len() as u32,
                thread_count,
                handle_count,
                logical_cpus: self.logical_cpus,
                etw_active: self.etw.active,
            },
            processes,
        }
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Terminate a process. Fails with the Win32 error for protected/elevated
/// processes (access denied = error 5).
pub fn kill_process(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, PROCESS_TERMINATE,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            let err = GetLastError();
            return Err(if err == 5 {
                "access denied (elevated process)".to_string()
            } else {
                format!("open failed (error {err})")
            });
        }
        let ok = TerminateProcess(handle, 1);
        let err = GetLastError();
        CloseHandle(handle);
        if ok == 0 {
            Err(format!("terminate failed (error {err})"))
        } else {
            Ok(())
        }
    }
}

/// Human-readable byte size, Task Manager style.
pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
