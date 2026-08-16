//! System-wide performance data for the Performance tab: per-core CPU
//! times, memory details, and per-adapter network throughput. All
//! unelevated.

use std::ffi::c_void;
use std::time::Instant;

use rustc_hash::FxHashMap;
use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows_sys::Win32::System::ProcessStatus::{K32GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_ANY};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use crate::nt::NtQuerySystemInformation;

const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION_CLASS: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessorPerf {
    idle: i64,
    kernel: i64, // includes idle
    user: i64,
    reserved1: [i64; 2],
    reserved2: u32,
    _pad: u32,
}

/// One logical core's utilization for the last tick, percent.
#[derive(Clone, Copy, Default)]
pub struct CoreLoad {
    pub total: f32,
    pub kernel: f32,
}

#[derive(Clone, Default)]
pub struct MemDetail {
    pub in_use: u64,
    pub available: u64,
    pub total: u64,
    pub commit_used: u64,
    pub commit_limit: u64,
    pub cached: u64,
    pub paged_pool: u64,
    pub nonpaged_pool: u64,
}

#[derive(Clone, Default)]
pub struct NetAdapterStats {
    pub name: String,
    pub luid: u64,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub link_speed: u64,
    pub connected: bool,
}

#[derive(Clone, Default)]
pub struct PerfInfo {
    pub cores: Vec<CoreLoad>,
    pub mem: MemDetail,
    pub adapters: Vec<NetAdapterStats>,
    /// Per GPU adapter: busiest-engine utilization percent (Task Manager
    /// semantics).
    pub gpus: Vec<f32>,
    pub uptime_secs: u64,
    pub cpu_name: String,
    pub cpu_mhz: u32,
}

pub(crate) struct PerfSampler {
    prev_cores: Vec<ProcessorPerf>,
    prev_net: FxHashMap<u64, (u64, u64, Instant)>,
    cpu_name: String,
    cpu_mhz: u32,
}

impl PerfSampler {
    pub fn new() -> Self {
        let (cpu_name, cpu_mhz) = read_cpu_identity();
        PerfSampler {
            prev_cores: Vec::new(),
            prev_net: FxHashMap::default(),
            cpu_name,
            cpu_mhz,
        }
    }

    pub fn sample(&mut self, gpus: Vec<f32>) -> PerfInfo {
        PerfInfo {
            cores: self.sample_cores(),
            mem: sample_memory(),
            adapters: self.sample_net(),
            gpus,
            uptime_secs: unsafe { GetTickCount64() } / 1000,
            cpu_name: self.cpu_name.clone(),
            cpu_mhz: self.cpu_mhz,
        }
    }

    fn sample_cores(&mut self) -> Vec<CoreLoad> {
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let mut now: Vec<ProcessorPerf> = vec![ProcessorPerf::default(); n];
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION_CLASS,
                now.as_mut_ptr() as *mut c_void,
                (n * std::mem::size_of::<ProcessorPerf>()) as u32,
                std::ptr::null_mut(),
            )
        };
        if status < 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(n);
        if self.prev_cores.len() == n {
            for (cur, prev) in now.iter().zip(self.prev_cores.iter()) {
                let d_idle = (cur.idle - prev.idle).max(0) as f64;
                let d_kernel = (cur.kernel - prev.kernel).max(0) as f64;
                let d_user = (cur.user - prev.user).max(0) as f64;
                let d_total = d_kernel + d_user; // kernel includes idle
                if d_total > 0.0 {
                    out.push(CoreLoad {
                        total: (((d_total - d_idle) / d_total) * 100.0) as f32,
                        kernel: (((d_kernel - d_idle).max(0.0) / d_total) * 100.0) as f32,
                    });
                } else {
                    out.push(CoreLoad::default());
                }
            }
        } else {
            out.resize(n, CoreLoad::default());
        }
        self.prev_cores = now;
        out
    }

    fn sample_net(&mut self) -> Vec<NetAdapterStats> {
        let mut out = Vec::new();
        unsafe {
            let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
            if GetIfTable2(&mut table) != 0 || table.is_null() {
                return out;
            }
            let now = Instant::now();
            let count = (*table).NumEntries as usize;
            let rows = (*table).Table.as_ptr();
            for i in 0..count {
                let row = &*rows.add(i);
                // Skip loopback (24) and software filter/hidden interfaces
                // (FilterInterface is bit 1 of the flags bitfield).
                if row.Type == 24 || row.InterfaceAndOperStatusFlags._bitfield & 0x02 != 0 {
                    continue;
                }
                // Only interfaces that are operationally up.
                if row.OperStatus != 1 {
                    continue;
                }
                let luid = row.InterfaceLuid.Value;
                let name_end = row
                    .Alias
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(row.Alias.len());
                let name = String::from_utf16_lossy(&row.Alias[..name_end]);
                let (mut rx_bps, mut tx_bps) = (0u64, 0u64);
                if let Some((prev_in, prev_out, prev_t)) = self.prev_net.get(&luid) {
                    let span = now.duration_since(*prev_t).as_secs_f64();
                    if span > 0.05 {
                        rx_bps =
                            ((row.InOctets.saturating_sub(*prev_in)) as f64 / span) as u64;
                        tx_bps =
                            ((row.OutOctets.saturating_sub(*prev_out)) as f64 / span) as u64;
                    }
                }
                self.prev_net
                    .insert(luid, (row.InOctets, row.OutOctets, now));
                out.push(NetAdapterStats {
                    name,
                    luid,
                    rx_bps,
                    tx_bps,
                    link_speed: row.ReceiveLinkSpeed,
                    connected: row.MediaConnectState == 1,
                });
            }
            FreeMibTable(table as *const c_void);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

fn sample_memory() -> MemDetail {
    let mut info: PERFORMANCE_INFORMATION = unsafe { std::mem::zeroed() };
    info.cb = std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32;
    if unsafe { K32GetPerformanceInfo(&mut info, info.cb) } == 0 {
        return MemDetail::default();
    }
    let page = info.PageSize as u64;
    MemDetail {
        in_use: (info.PhysicalTotal - info.PhysicalAvailable) as u64 * page,
        available: info.PhysicalAvailable as u64 * page,
        total: info.PhysicalTotal as u64 * page,
        commit_used: info.CommitTotal as u64 * page,
        commit_limit: info.CommitLimit as u64 * page,
        cached: info.SystemCache as u64 * page,
        paged_pool: info.KernelPaged as u64 * page,
        nonpaged_pool: info.KernelNonpaged as u64 * page,
    }
}

fn read_cpu_identity() -> (String, u32) {
    unsafe {
        let key: Vec<u16> = "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0"
            .encode_utf16()
            .chain([0])
            .collect();
        let name_v: Vec<u16> = "ProcessorNameString".encode_utf16().chain([0]).collect();
        let mut buf = [0u16; 256];
        let mut size = (buf.len() * 2) as u32;
        let mut name = String::new();
        if RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name_v.as_ptr(),
            RRF_RT_ANY,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
        ) == 0
        {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
            name = String::from_utf16_lossy(&buf[..end]).trim().to_string();
        }
        let mhz_v: Vec<u16> = "~MHz".encode_utf16().chain([0]).collect();
        let mut mhz: u32 = 0;
        let mut size = 4u32;
        let _ = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            mhz_v.as_ptr(),
            RRF_RT_ANY,
            std::ptr::null_mut(),
            &mut mhz as *mut u32 as *mut c_void,
            &mut size,
        );
        (name, mhz)
    }
}
