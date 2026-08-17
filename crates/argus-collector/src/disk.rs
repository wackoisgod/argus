//! Physical disk statistics via IOCTL_DISK_PERFORMANCE — works unelevated
//! with a zero-access handle. Handles open once at startup.

use std::ffi::c_void;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    DISK_PERFORMANCE, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_PERFORMANCE,
    IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

#[derive(Clone, Default)]
pub struct DiskStats {
    pub index: u32,
    pub model: String,
    pub size_bytes: u64,
    pub active_pct: f32,
    pub read_bps: u64,
    pub write_bps: u64,
}

struct Disk {
    index: u32,
    handle: HANDLE,
    model: String,
    size_bytes: u64,
    prev: Option<(i64, i64, i64, Instant)>, // read bytes, write bytes, idle 100ns
}

pub(crate) struct DiskMonitor {
    disks: Vec<Disk>,
}

// SAFETY: raw handles are only used from the sampler thread.
unsafe impl Send for DiskMonitor {}

impl DiskMonitor {
    pub fn new() -> Self {
        let mut disks = Vec::new();
        for index in 0..16u32 {
            let path: Vec<u16> = format!("\\\\.\\PhysicalDrive{index}")
                .encode_utf16()
                .chain([0])
                .collect();
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let model = query_model(handle).unwrap_or_else(|| format!("Disk {index}"));
            let size_bytes = query_size(handle).unwrap_or(0);
            disks.push(Disk {
                index,
                handle,
                model,
                size_bytes,
                prev: None,
            });
        }
        DiskMonitor { disks }
    }

    pub fn sample(&mut self) -> Vec<DiskStats> {
        let now = Instant::now();
        let mut out = Vec::with_capacity(self.disks.len());
        for disk in &mut self.disks {
            let mut perf: DISK_PERFORMANCE = unsafe { std::mem::zeroed() };
            let mut ret = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    disk.handle,
                    IOCTL_DISK_PERFORMANCE,
                    std::ptr::null(),
                    0,
                    &mut perf as *mut _ as *mut c_void,
                    std::mem::size_of::<DISK_PERFORMANCE>() as u32,
                    &mut ret,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                continue;
            }
            let mut stats = DiskStats {
                index: disk.index,
                model: disk.model.clone(),
                size_bytes: disk.size_bytes,
                ..Default::default()
            };
            if let Some((p_read, p_write, p_idle, p_at)) = disk.prev {
                let span = now.duration_since(p_at).as_secs_f64();
                if span > 0.05 {
                    stats.read_bps =
                        ((perf.BytesRead - p_read).max(0) as f64 / span) as u64;
                    stats.write_bps =
                        ((perf.BytesWritten - p_write).max(0) as f64 / span) as u64;
                    let idle_frac =
                        (perf.IdleTime - p_idle).max(0) as f64 / (span * 10_000_000.0);
                    stats.active_pct = ((1.0 - idle_frac).clamp(0.0, 1.0) * 100.0) as f32;
                }
            }
            disk.prev = Some((perf.BytesRead, perf.BytesWritten, perf.IdleTime, now));
            out.push(stats);
        }
        out
    }
}

impl Drop for DiskMonitor {
    fn drop(&mut self) {
        for disk in &self.disks {
            unsafe { CloseHandle(disk.handle) };
        }
    }
}

fn query_model(handle: HANDLE) -> Option<String> {
    unsafe {
        let mut query: STORAGE_PROPERTY_QUERY = std::mem::zeroed();
        query.PropertyId = 0; // StorageDeviceProperty
        query.QueryType = 0; // PropertyStandardQuery
        let mut buf = [0u8; 1024];
        let mut ret = 0u32;
        if DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        ) == 0
        {
            return None;
        }
        let desc = &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR);
        let off = desc.ProductIdOffset as usize;
        if off == 0 || off >= buf.len() {
            return None;
        }
        let end = buf[off..]
            .iter()
            .position(|&c| c == 0)
            .map(|e| off + e)
            .unwrap_or(buf.len());
        let s = String::from_utf8_lossy(&buf[off..end]).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

fn query_size(handle: HANDLE) -> Option<u64> {
    unsafe {
        // DISK_GEOMETRY_EX: DISK_GEOMETRY (24 bytes) then DiskSize i64 @24.
        let mut buf = [0u8; 256];
        let mut ret = 0u32;
        if DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            std::ptr::null(),
            0,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        ) == 0
        {
            return None;
        }
        Some(u64::from_le_bytes(buf[24..32].try_into().ok()?))
    }
}
