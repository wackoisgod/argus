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
    fn D3DKMTQueryAdapterInfo(info: *mut QueryAdapterInfo) -> i32;
}

const TYPE_ADAPTER: u32 = 0;
const TYPE_SEGMENT: u32 = 3;
const TYPE_NODE: u32 = 5;
const TYPE_PROCESS_NODE: u32 = 6;
const TYPE_PHYSICAL_ADAPTER: u32 = 10;
const KMTQAITYPE_ADAPTERREGISTRYINFO: u32 = 8;
const KMTQAITYPE_NODEMETADATA: u32 = 25;

#[repr(C)]
struct QueryAdapterInfo {
    h_adapter: u32,
    ty: u32,
    data: *mut std::ffi::c_void,
    size: u32,
    _pad: u32,
}

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

struct Adapter {
    h_adapter: u32,
    luid_low: u32,
    luid_high: u32,
    node_count: u32,
    segment_count: u32,
    name: std::sync::Arc<str>,
    engine_names: std::sync::Arc<Vec<String>>,
    driver_version: std::sync::Arc<str>,
    driver_date: std::sync::Arc<str>,
}

/// One GPU adapter's system-wide state for the Performance tab.
#[derive(Clone, Default)]
pub struct GpuAdapterPerf {
    pub name: std::sync::Arc<str>,
    pub engine_names: std::sync::Arc<Vec<String>>,
    pub engine_pcts: Vec<f32>,
    /// Busiest engine — Task Manager's "GPU %".
    pub utilization: f32,
    pub vram_used: u64,
    pub vram_total: u64,
    pub shared_used: u64,
    pub shared_total: u64,
    pub temperature_c: Option<f32>,
    pub luid_low: u32,
    pub luid_high: u32,
    pub driver_version: std::sync::Arc<str>,
    pub driver_date: std::sync::Arc<str>,
}

pub struct GpuMonitor {
    adapters: Vec<Adapter>,
    /// Cumulative running time (100ns units) and probe timestamp per
    /// (pid, create_time). Deltas divide by the span since *that process's*
    /// last probe — bucket-cadence probes cover many ticks.
    prev: FxHashMap<(u32, i64), (u64, std::time::Instant)>,
    /// Pids that showed GPU time recently — polled every tick.
    active: FxHashSet<u32>,
    /// Global per-(adapter, node) running time + timestamp + recently-active
    /// flag, for adapter utilization (busiest engine). Every QueryStatistics
    /// call can stall in the driver, so idle engines poll at reduced rate.
    node_prev: Vec<Vec<(u64, std::time::Instant, bool)>>,
    /// Nodes that have ever shown global running-time activity. Per-process
    /// probes only query these: a node whose global delta is zero cannot have
    /// accumulated time for any process, and every skipped node is one fewer
    /// driver call that can stall the CPU. Grows monotonically.
    ever_active: Vec<Vec<bool>>,
    /// Set when `ever_active` gained a node last perf tick; per-process
    /// baselines are then rebuilt so cumulative totals stay consistent.
    nodes_changed: bool,
    /// Cached (vram_used, vram_total, shared_used, shared_total, temp) per
    /// adapter. The segment and physical-adapter queries make the AMD
    /// driver stall the CPU polling hardware (~9ms), so they refresh every
    /// few ticks instead of every second.
    mem_temp_cache: Vec<(u64, u64, u64, u64, Option<f32>)>,
    perf_tick: u32,
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
            .map(|a| vec![(0u64, std::time::Instant::now(), true); a.node_count as usize])
            .collect();
        // Seed node 0 (the 3D engine) on hardware adapters so the first
        // ticks have a probe set before global activity is known; software
        // adapters start fully skipped.
        let ever_active = adapters
            .iter()
            .map(|a| {
                let hw = !a.name.contains("Basic Render");
                (0..a.node_count).map(|n| hw && n == 0).collect()
            })
            .collect();
        let adapter_count = adapters.len();
        GpuMonitor {
            adapters,
            prev: FxHashMap::default(),
            active: FxHashSet::default(),
            node_prev,
            ever_active,
            nodes_changed: false,
            mem_temp_cache: vec![(0, 0, 0, 0, None); adapter_count],
            perf_tick: 0,
            tick: 0,
        }
    }

    /// System-wide per-adapter state: per-engine utilization (from NODE
    /// global running-time deltas), VRAM segment usage, and temperature.
    pub fn adapters_perf(&mut self) -> Vec<GpuAdapterPerf> {
        let now = std::time::Instant::now();
        self.perf_tick = self.perf_tick.wrapping_add(1);
        // Hardware-polling queries (segments, temperature) every 5th tick.
        let refresh_hw = self.perf_tick % 5 == 1;
        let mut out = Vec::with_capacity(self.adapters.len());
        for (ai, adapter) in self.adapters.iter().enumerate() {
            let mut perf = GpuAdapterPerf {
                name: adapter.name.clone(),
                engine_names: adapter.engine_names.clone(),
                engine_pcts: vec![0.0; adapter.node_count as usize],
                luid_low: adapter.luid_low,
                luid_high: adapter.luid_high,
                driver_version: adapter.driver_version.clone(),
                driver_date: adapter.driver_date.clone(),
                ..Default::default()
            };
            for node in 0..adapter.node_count {
                let (prev_total, prev_at, was_active) = self.node_prev[ai][node as usize];
                // Idle engines poll every 5th tick to limit driver stalls.
                let probe = was_active || (self.perf_tick + node) % 5 == 0;
                if !probe {
                    continue;
                }
                let mut q = zeroed_stats();
                q.ty = TYPE_NODE;
                q.luid_low = adapter.luid_low;
                q.luid_high = adapter.luid_high;
                q.query_in[0] = node;
                if unsafe { D3DKMTQueryStatistics(&mut q) } != 0 {
                    continue;
                }
                let total = q.result[0]; // GlobalInformation.RunningTime
                let span = now.duration_since(prev_at).as_secs_f64();
                let mut active = false;
                if span > 0.05 && prev_total > 0 {
                    let pct =
                        (total.saturating_sub(prev_total) as f64 / (span * 10_000_000.0)
                            * 100.0)
                            .min(100.0) as f32;
                    perf.engine_pcts[node as usize] = pct;
                    perf.utilization = perf.utilization.max(pct);
                    active = pct > 0.1;
                }
                if active && !self.ever_active[ai][node as usize] {
                    self.ever_active[ai][node as usize] = true;
                    self.nodes_changed = true;
                }
                self.node_prev[ai][node as usize] = (total, now, active);
            }
            if refresh_hw {
                let mut cache = (0u64, 0u64, 0u64, 0u64, None);
                // VRAM: local segments = dedicated, aperture segments =
                // shared.
                for seg in 0..adapter.segment_count {
                    let mut q = zeroed_stats();
                    q.ty = TYPE_SEGMENT;
                    q.luid_low = adapter.luid_low;
                    q.luid_high = adapter.luid_high;
                    q.query_in[0] = seg;
                    if unsafe { D3DKMTQueryStatistics(&mut q) } != 0 {
                        continue;
                    }
                    // SEGMENT_INFORMATION: CommitLimit@0, BytesResident@16,
                    // Aperture@40 (u32) — offsets from the SDK header probe.
                    // "Unlimited" shared segments report huge sentinel
                    // limits; leave those out of the total.
                    const SANE: u64 = 1 << 50;
                    let commit_limit = q.result[0];
                    let resident = q.result[2].min(SANE);
                    let aperture = (q.result[5] & 0xFFFF_FFFF) as u32;
                    let (used, total) = if aperture != 0 {
                        (&mut cache.2, &mut cache.3)
                    } else {
                        (&mut cache.0, &mut cache.1)
                    };
                    *used = used.saturating_add(resident);
                    if commit_limit < SANE {
                        *total = total.saturating_add(commit_limit);
                    }
                }
                // Temperature via physical-adapter perf data (deci-Celsius).
                let mut q = zeroed_stats();
                q.ty = TYPE_PHYSICAL_ADAPTER;
                q.luid_low = adapter.luid_low;
                q.luid_high = adapter.luid_high;
                q.query_in[0] = 0;
                if unsafe { D3DKMTQueryStatistics(&mut q) } == 0 {
                    // ADAPTER_PERFDATA at result offset 0; Temperature @56.
                    let temp_deci = (q.result[7] & 0xFFFF_FFFF) as u32;
                    if temp_deci > 0 {
                        let t = temp_deci as f32 / 10.0;
                        cache.4 = Some(if t > 200.0 { t / 100.0 } else { t });
                    }
                }
                self.mem_temp_cache[ai] = cache;
            }
            let (vu, vt, su, st, temp) = self.mem_temp_cache[ai];
            perf.vram_used = vu;
            perf.vram_total = vt;
            perf.shared_used = su;
            perf.shared_total = st;
            perf.temperature_c = temp;
            out.push(perf);
        }
        out
    }

    /// Total cumulative GPU running time for one process, summed across all
    /// adapters' *ever-active* engine nodes (see `ever_active` — skipped
    /// nodes provably contribute zero). `None` if the process can't be
    /// opened.
    fn process_running_time(&self, pid: u32) -> Option<u64> {
        if !self
            .ever_active
            .iter()
            .any(|nodes| nodes.iter().any(|&a| a))
        {
            return None;
        }
        // NB: dxgkrnl rejects PROCESS_QUERY_LIMITED_INFORMATION handles with
        // STATUS_INVALID_PARAMETER; full QUERY_INFORMATION is required.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut total = 0u64;
        for (ai, adapter) in self.adapters.iter().enumerate() {
            for node in 0..adapter.node_count {
                if !self.ever_active[ai][node as usize] {
                    continue;
                }
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
        // The probe set grew: cached totals cover a smaller node set than the
        // next probe will, which would read as a burst of activity. Rebuild
        // baselines instead (one tick of missing per-process GPU data).
        if self.nodes_changed {
            self.nodes_changed = false;
            self.prev.clear();
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
                    let pct = (delta as f64 / (span * 10_000_000.0) * 100.0).min(100.0) as f32;
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
    fn dxgi_map_debug() {
        for ((low, high), name) in dxgi_adapter_names() {
            eprintln!("dxgi: luid {high:08x}:{low:08x} = '{name}'");
        }
        let m = GpuMonitor::new();
        for a in &m.adapters {
            eprintln!("kmt:  luid {:08x}:{:08x} nodes {}", a.luid_high, a.luid_low, a.node_count);
        }
    }

    #[test]
    fn adapter_names_debug() {
        let m = GpuMonitor::new();
        for (i, a) in m.adapters.iter().enumerate() {
            let mut buf = vec![0u16; 1040];
            let mut q = QueryAdapterInfo {
                h_adapter: a.h_adapter,
                ty: KMTQAITYPE_ADAPTERREGISTRYINFO,
                data: buf.as_mut_ptr().cast(),
                size: 2080,
                _pad: 0,
            };
            let status = unsafe { D3DKMTQueryAdapterInfo(&mut q) };
            let end = buf[..260].iter().position(|&c| c == 0).unwrap_or(0);
            eprintln!(
                "adapter {i}: h={:#x} nodes={} status={status:#010x} name='{}' (resolved '{}')",
                a.h_adapter,
                a.node_count,
                String::from_utf16_lossy(&buf[..end]),
                a.name
            );
        }
    }

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
    #[ignore = "needs an active desktop GPU workload; run locally with --ignored"]
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
        let dxgi_names = dxgi_adapter_names();
        for info in infos.iter().take(e.num_adapters as usize) {
            // Ask the adapter how many engine nodes / memory segments it has.
            let mut q = zeroed_stats();
            q.ty = TYPE_ADAPTER;
            q.luid_low = info.luid_low;
            q.luid_high = info.luid_high;
            if D3DKMTQueryStatistics(&mut q) == 0 {
                let segment_count = (q.result[0] & 0xFFFF_FFFF) as u32; // NbSegments
                let node_count = (q.result[0] >> 32) as u32; // NodeCount
                if node_count > 0 && node_count < 256 {
                    let name = adapter_name(info.h_adapter)
                        .or_else(|| {
                            dxgi_names
                                .get(&(info.luid_low, info.luid_high))
                                .map(|n| std::sync::Arc::from(n.as_str()))
                        })
                        .unwrap_or_else(|| std::sync::Arc::from("GPU"));
                    // Multiple identical WARP software adapters add nothing.
                    if name.contains("Basic Render")
                        && result.iter().any(|a: &Adapter| a.name == name)
                    {
                        continue;
                    }
                    let engine_names: Vec<String> =
                        (0..node_count).map(|n| node_name(info.h_adapter, n)).collect();
                    let (driver_version, driver_date) = driver_info(&name);
                    // Handle stays open for QueryAdapterInfo calls.
                    result.push(Adapter {
                        h_adapter: info.h_adapter,
                        luid_low: info.luid_low,
                        luid_high: info.luid_high,
                        node_count,
                        segment_count: segment_count.min(64),
                        name,
                        engine_names: std::sync::Arc::new(engine_names),
                        driver_version,
                        driver_date,
                    });
                }
            }
        }
    }
    result
}

/// Display driver version and date from the display-class registry key,
/// matched by DriverDesc. Empty strings when no instance matches (software
/// adapters). One-time cost at adapter enumeration.
fn driver_info(adapter_name: &str) -> (std::sync::Arc<str>, std::sync::Arc<str>) {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_ANY};
    let read = |subkey: &str, value: &str| -> Option<String> {
        let key: Vec<u16> = subkey.encode_utf16().chain([0]).collect();
        let val: Vec<u16> = value.encode_utf16().chain([0]).collect();
        let mut buf = [0u16; 256];
        let mut size = (buf.len() * 2) as u32;
        let ok = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                val.as_ptr(),
                RRF_RT_ANY,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut std::ffi::c_void,
                &mut size,
            )
        } == 0;
        if !ok {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
        Some(String::from_utf16_lossy(&buf[..end]).trim().to_string())
    };
    const CLASS: &str =
        "SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}";
    for i in 0..16 {
        let subkey = format!("{CLASS}\\{i:04}");
        match read(&subkey, "DriverDesc") {
            Some(desc) if desc == adapter_name => {
                let version = read(&subkey, "DriverVersion").unwrap_or_default();
                let date = read(&subkey, "DriverDate").unwrap_or_default();
                return (version.into(), date.into());
            }
            Some(_) => continue,
            None => break,
        }
    }
    (std::sync::Arc::from(""), std::sync::Arc::from(""))
}

/// Adapter names by LUID from DXGI — covers software adapters that have no
/// registry info (Basic Render Driver etc.). Minimal raw-COM: factory →
/// EnumAdapters1 → GetDesc1.
fn dxgi_adapter_names() -> FxHashMap<(u32, u32), String> {
    #[link(name = "dxgi")]
    extern "system" {
        fn CreateDXGIFactory1(riid: *const [u8; 16], out: *mut *mut std::ffi::c_void) -> i32;
    }
    // IID_IDXGIFactory1 {770aae78-f26f-4dba-a829-253c83d1b387}
    const IID_FACTORY1: [u8; 16] = [
        0x78, 0xae, 0x0a, 0x77, 0x6f, 0xf2, 0xba, 0x4d, 0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1,
        0xb3, 0x87,
    ];
    let mut out = FxHashMap::default();
    unsafe {
        type Unknown = *mut std::ffi::c_void;
        let mut factory: Unknown = std::ptr::null_mut();
        let hr = CreateDXGIFactory1(&IID_FACTORY1, &mut factory);
        if hr < 0 || factory.is_null() {
            #[cfg(test)]
            eprintln!("CreateDXGIFactory1 failed: {hr:#010x}");
            return out;
        }
        let vtbl = |obj: Unknown, index: usize| -> usize {
            *(*(obj as *const *const usize)).add(index)
        };
        let release = |obj: Unknown| {
            let f: extern "system" fn(Unknown) -> u32 = std::mem::transmute(vtbl(obj, 2));
            f(obj);
        };
        // IDXGIFactory1::EnumAdapters1 is vtable slot 12 (3 IUnknown +
        // 4 IDXGIObject + 5 IDXGIFactory).
        let enum_adapters1: extern "system" fn(Unknown, u32, *mut Unknown) -> i32 =
            std::mem::transmute(vtbl(factory, 12));
        for i in 0..16u32 {
            let mut adapter: Unknown = std::ptr::null_mut();
            if enum_adapters1(factory, i, &mut adapter) < 0 || adapter.is_null() {
                break;
            }
            // IDXGIAdapter1::GetDesc1 is vtable slot 10; DXGI_ADAPTER_DESC1:
            // Description WCHAR[128] @0, LUID @296.
            let get_desc1: extern "system" fn(Unknown, *mut [u8; 312]) -> i32 =
                std::mem::transmute(vtbl(adapter, 10));
            let mut desc = [0u8; 312];
            if get_desc1(adapter, &mut desc) >= 0 {
                let name_u16: Vec<u16> = desc[..256]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                let low = u32::from_le_bytes(desc[296..300].try_into().unwrap());
                let high = u32::from_le_bytes(desc[300..304].try_into().unwrap());
                let name = String::from_utf16_lossy(&name_u16).trim().to_string();
                if !name.is_empty() {
                    out.insert((low, high), name);
                }
            }
            release(adapter);
        }
        release(factory);
    }
    out
}

fn adapter_name(h_adapter: u32) -> Option<std::sync::Arc<str>> {
    // D3DKMT_ADAPTERREGISTRYINFO: 4 × WCHAR[MAX_PATH]; AdapterString first.
    let mut buf = vec![0u16; 1040];
    let mut q = QueryAdapterInfo {
        h_adapter,
        ty: KMTQAITYPE_ADAPTERREGISTRYINFO,
        data: buf.as_mut_ptr().cast(),
        size: 2080,
        _pad: 0,
    };
    if unsafe { D3DKMTQueryAdapterInfo(&mut q) } != 0 {
        return None;
    }
    let end = buf[..260].iter().position(|&c| c == 0).unwrap_or(0);
    if end == 0 {
        return None;
    }
    Some(std::sync::Arc::from(
        String::from_utf16_lossy(&buf[..end]).trim(),
    ))
}

fn node_name(h_adapter: u32, node: u32) -> String {
    // D3DKMT_NODEMETADATA (78 bytes): NodeOrdinalAndAdapterIndex u32 @0,
    // EngineType i32 @4, FriendlyName WCHAR[32] @8.
    let mut buf = [0u8; 78];
    buf[..4].copy_from_slice(&node.to_le_bytes());
    let mut q = QueryAdapterInfo {
        h_adapter,
        ty: KMTQAITYPE_NODEMETADATA,
        data: buf.as_mut_ptr().cast(),
        size: 78,
        _pad: 0,
    };
    if unsafe { D3DKMTQueryAdapterInfo(&mut q) } == 0 {
        let friendly: Vec<u16> = buf[8..72]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        if !friendly.is_empty() {
            return String::from_utf16_lossy(&friendly);
        }
        let engine_type = i32::from_le_bytes(buf[4..8].try_into().unwrap());
        let name = match engine_type {
            1 => "3D",
            2 => "Video Decode",
            3 => "Video Encode",
            4 => "Video Processing",
            5 => "Scene Assembly",
            6 => "Copy",
            7 => "Overlay",
            8 => "Crypto",
            _ => return format!("Engine {node}"),
        };
        return name.to_string();
    }
    format!("Engine {node}")
}
