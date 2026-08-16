//! Per-process GPU utilization via D3DKMTQueryStatistics — the direct
//! kernel-graphics route (Process Explorer / System Informer style), not the
//! costly PDH "GPU Engine" counter set. Works unelevated for any process we
//! can open a query handle to.
//!
//! Struct layout facts were compiled-and-printed from the installed Windows
//! SDK's d3dkmthk.h (10.0.26100.0, x64): sizeof(D3DKMT_QUERYSTATISTICS) =
//! 808, QueryResult at offset 24 (776 bytes), input union at 800,
//! ADAPTER_INFORMATION.NodeCount at result+4, PROCESS_NODE_INFORMATION
//! .RunningTime at result+0.
//!
//! Cost control: querying is (adapters × nodes) syscalls per process. GPU-
//! active processes are polled every tick; inactive ones are probed in
//! rotating pid buckets (one tenth per tick) plus on first sight, keeping the
//! steady-state cost around a millisecond.

use rustc_hash::{FxHashMap, FxHashSet};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION};

#[link(name = "gdi32")]
extern "system" {
    fn D3DKMTQueryStatistics(stats: *mut QueryStats) -> i32;
    fn D3DKMTEnumAdapters2(enum_adapters: *mut EnumAdapters2) -> i32;
    fn D3DKMTCloseAdapter(close: *const CloseAdapter) -> i32;
}

const TYPE_ADAPTER: u32 = 0;
const TYPE_NODE: u32 = 5;
const TYPE_PROCESS_NODE: u32 = 6;

#[repr(C)]
struct QueryStats {
    ty: u32,
    luid_low: u32,
    luid_high: u32,
    _pad: u32,
    h_process: usize,
    /// D3DKMT_QUERYSTATISTICS_RESULT (776 bytes, 8-aligned).
    result: [u64; 97],
    /// Input union (QueryNode.NodeId etc.).
    query_in: [u32; 2],
}

#[repr(C)]
struct AdapterInfo {
    h_adapter: u32,
    luid_low: u32,
    luid_high: u32,
    num_sources: u32,
    precise_present: u32,
}

#[repr(C)]
struct EnumAdapters2 {
    num_adapters: u32,
    _pad: u32,
    adapters: *mut AdapterInfo,
}

#[repr(C)]
struct CloseAdapter {
    h_adapter: u32,
}

struct Adapter {
    luid_low: u32,
    luid_high: u32,
    node_count: u32,
}

pub struct GpuMonitor {
    adapters: Vec<Adapter>,
    /// Cumulative running time (100ns units) and probe timestamp per
    /// (pid, create_time). Deltas divide by the span since *that process's*
    /// last probe — bucket-cadence probes cover many ticks.
    prev: FxHashMap<(u32, i64), (u64, std::time::Instant)>,
    /// Pids that showed GPU time recently — polled every tick.
    active: FxHashSet<u32>,
    /// Global per-(adapter, node) running time + timestamp, for adapter
    /// utilization (busiest engine).
    node_prev: Vec<Vec<(u64, std::time::Instant)>>,
    tick: u32,
}

fn zeroed_stats() -> QueryStats {
    // SAFETY: all-zero is a valid representation for this plain-data struct.
    unsafe { std::mem::zeroed() }
}

impl GpuMonitor {
    pub fn new() -> Self {
        let adapters = enum_adapters();
        let node_prev = adapters
            .iter()
            .map(|a| vec![(0u64, std::time::Instant::now()); a.node_count as usize])
            .collect();
        GpuMonitor {
            adapters,
            prev: FxHashMap::default(),
            active: FxHashSet::default(),
            node_prev,
            tick: 0,
        }
    }

    /// System-wide utilization per adapter: the busiest engine's percent,
    /// Task Manager's definition of "GPU %".
    pub fn adapter_utilization(&mut self) -> Vec<f32> {
        let now = std::time::Instant::now();
        let mut out = Vec::with_capacity(self.adapters.len());
        for (ai, adapter) in self.adapters.iter().enumerate() {
            let mut busiest = 0.0f32;
            for node in 0..adapter.node_count {
                let mut q = zeroed_stats();
                q.ty = TYPE_NODE;
                q.luid_low = adapter.luid_low;
                q.luid_high = adapter.luid_high;
                q.query_in[0] = node;
                if unsafe { D3DKMTQueryStatistics(&mut q) } != 0 {
                    continue;
                }
                let total = q.result[0]; // GlobalInformation.RunningTime
                let (prev_total, prev_at) = self.node_prev[ai][node as usize];
                let span = now.duration_since(prev_at).as_secs_f64();
                if span > 0.05 && prev_total > 0 {
                    let pct =
                        (total.saturating_sub(prev_total) as f64 / (span * 10_000_000.0)
                            * 100.0) as f32;
                    busiest = busiest.max(pct.min(100.0));
                }
                self.node_prev[ai][node as usize] = (total, now);
            }
            out.push(busiest);
        }
        out
    }

    /// Total cumulative GPU running time for one process, summed across all
    /// adapters and engine nodes. `None` if the process can't be opened.
    fn process_running_time(&self, pid: u32) -> Option<u64> {
        // NB: dxgkrnl rejects PROCESS_QUERY_LIMITED_INFORMATION handles with
        // STATUS_INVALID_PARAMETER; full QUERY_INFORMATION is required.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut total = 0u64;
        for adapter in &self.adapters {
            for node in 0..adapter.node_count {
                let mut q = zeroed_stats();
                q.ty = TYPE_PROCESS_NODE;
                q.luid_low = adapter.luid_low;
                q.luid_high = adapter.luid_high;
                q.h_process = handle as usize;
                q.query_in[0] = node;
                if unsafe { D3DKMTQueryStatistics(&mut q) } == 0 {
                    total += q.result[0]; // RunningTime at result offset 0
                }
            }
        }
        unsafe { CloseHandle(handle) };
        Some(total)
    }

    /// Per-pid GPU percent (of one GPU engine, all cores=100 like CPU) for
    /// this tick. Call once per sampler tick.
    pub fn sample(&mut self, procs: &[crate::RawProcess]) -> FxHashMap<u32, f32> {
        self.tick = self.tick.wrapping_add(1);
        let now = std::time::Instant::now();
        let mut out = FxHashMap::default();
        if self.adapters.is_empty() {
            return out;
        }
        let mut next_prev = FxHashMap::default();
        for raw in procs {
            if raw.pid == 0 || raw.pid == 4 {
                continue;
            }
            let key = (raw.pid, raw.create_time);
            let known = self.prev.contains_key(&key);
            let probe = self.active.contains(&raw.pid)
                || !known
                || (raw.pid / 4) % 10 == self.tick % 10;
            if !probe {
                // Carry the stale total (and its timestamp) forward so the
                // next real probe computes its rate over the right span.
                if let Some(prev) = self.prev.get(&key) {
                    next_prev.insert(key, *prev);
                }
                continue;
            }
            let Some(total) = self.process_running_time(raw.pid) else {
                continue;
            };
            if let Some((prev_total, prev_at)) = self.prev.get(&key) {
                let span = now.duration_since(*prev_at).as_secs_f64();
                if span > 0.05 {
                    let delta = total.saturating_sub(*prev_total);
                    // RunningTime is in 100ns units (per System Informer,
                    // despite the header comment claiming microseconds).
                    let pct = (delta as f64 / (span * 10_000_000.0) * 100.0) as f32;
                    if pct > 0.05 {
                        out.insert(raw.pid, pct);
                    }
                    if delta > 0 {
                        self.active.insert(raw.pid);
                    } else {
                        self.active.remove(&raw.pid);
                    }
                }
            } else if total > 0 {
                self.active.insert(raw.pid);
            }
            next_prev.insert(key, (total, now));
        }
        self.prev = next_prev;
        out
    }
}

impl Default for GpuMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_enumerate() {
        let m = GpuMonitor::new();
        assert!(!m.adapters.is_empty(), "no adapters found");
        // The ADAPTER query must agree with enumeration: struct layout check.
        for a in &m.adapters {
            assert!(a.node_count > 0 && a.node_count < 256);
        }
    }

    #[test]
    fn some_process_has_gpu_time() {
        // At least one process on a desktop system (usually a browser or
        // compositor we can open) should report cumulative GPU running time.
        let m = GpuMonitor::new();
        let mut q = crate::nt::ProcessQuery::new();
        let mut raw = Vec::new();
        q.query(&mut raw).unwrap();
        let found = raw
            .iter()
            .filter(|p| p.pid > 4)
            .filter_map(|p| m.process_running_time(p.pid))
            .any(|total| total > 0);
        assert!(found, "no process reported GPU running time");
    }
}

fn enum_adapters() -> Vec<Adapter> {
    let mut result = Vec::new();
    unsafe {
        let mut e = EnumAdapters2 {
            num_adapters: 0,
            _pad: 0,
            adapters: std::ptr::null_mut(),
        };
        if D3DKMTEnumAdapters2(&mut e) != 0 || e.num_adapters == 0 {
            return result;
        }
        let mut infos: Vec<AdapterInfo> = (0..e.num_adapters)
            .map(|_| std::mem::zeroed())
            .collect();
        e.adapters = infos.as_mut_ptr();
        if D3DKMTEnumAdapters2(&mut e) != 0 {
            return result;
        }
        for info in infos.iter().take(e.num_adapters as usize) {
            // Ask the adapter how many engine nodes it has.
            let mut q = zeroed_stats();
            q.ty = TYPE_ADAPTER;
            q.luid_low = info.luid_low;
            q.luid_high = info.luid_high;
            if D3DKMTQueryStatistics(&mut q) == 0 {
                let node_count = (q.result[0] >> 32) as u32; // NodeCount at result+4
                if node_count > 0 && node_count < 256 {
                    result.push(Adapter {
                        luid_low: info.luid_low,
                        luid_high: info.luid_high,
                        node_count,
                    });
                }
            }
            let close = CloseAdapter {
                h_adapter: info.h_adapter,
            };
            let _ = D3DKMTCloseAdapter(&close);
        }
    }
    result
}
