//! Raw NT API layer: one `NtQuerySystemInformation(SystemProcessInformation)`
//! call yields every process's timing, memory, and I/O counters in a single
//! kernel round-trip. This is the same source Task Manager and Process
//! Explorer use, and it avoids opening a handle per process.

use std::ffi::c_void;
use std::sync::Arc;

use rustc_hash::FxHashMap;

const SYSTEM_PROCESS_INFORMATION_CLASS: u32 = 5;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;

#[link(name = "ntdll")]
extern "system" {
    pub(crate) fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
}

const SYSTEM_PROCESS_ID_INFORMATION_CLASS: u32 = 88;

/// Query a process's NT image path (\Device\HarddiskVolumeN\...) without
/// opening a handle — works unelevated for every process, including
/// protected ones and services.
pub(crate) fn image_nt_path(pid: u32) -> Option<Vec<u16>> {
    #[repr(C)]
    struct SystemProcessIdInformation {
        process_id: usize,
        image_name: UnicodeString,
    }
    let mut buf = vec![0u16; 600];
    let mut info = SystemProcessIdInformation {
        process_id: pid as usize,
        image_name: UnicodeString {
            length: 0,
            maximum_length: (buf.len() * 2) as u16,
            buffer: buf.as_mut_ptr(),
        },
    };
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_PROCESS_ID_INFORMATION_CLASS,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<SystemProcessIdInformation>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status < 0 || info.image_name.length == 0 {
        return None;
    }
    let chars = info.image_name.length as usize / 2;
    Some(buf[..chars].to_vec())
}

#[repr(C)]
struct UnicodeString {
    length: u16,       // bytes, not chars
    maximum_length: u16,
    buffer: *const u16,
}

/// x64 layout of SYSTEM_PROCESS_INFORMATION (per phnt). Windows 7+ fields
/// included; we only ever read the fixed-size head of each entry, and
/// `next_entry_offset` walks us to the next one past the thread array.
#[repr(C)]
struct SystemProcessInformation {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,   // 100ns units
    kernel_time: i64, // 100ns units
    image_name: UnicodeString,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_page_count: usize,
    read_operation_count: i64,
    write_operation_count: i64,
    other_operation_count: i64,
    read_transfer_count: i64,
    write_transfer_count: i64,
    other_transfer_count: i64,
    // SYSTEM_THREAD_INFORMATION Threads[] follows.
}

/// One process's raw counters, copied out of the kernel buffer.
#[derive(Debug, Clone)]
pub struct RawProcess {
    pub pid: u32,
    pub parent_pid: u32,
    /// Interned: decoded from UTF-16 once per process lifetime, then shared
    /// by refcount across every subsequent tick and snapshot clone.
    pub name: Arc<str>,
    pub threads: u32,
    pub handles: u32,
    pub session_id: u32,
    pub base_priority: i32,
    /// FILETIME-style creation timestamp; combined with pid it uniquely
    /// identifies a process across pid reuse.
    pub create_time: i64,
    pub cycle_time: u64,
    pub user_time_100ns: i64,
    pub kernel_time_100ns: i64,
    pub working_set: u64,
    pub private_working_set: u64,
    pub private_bytes: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub other_bytes: u64,
    pub read_ops: u64,
    pub write_ops: u64,
    pub hard_faults: u32,
}

/// Reusable query state: the kernel buffer and the process-name intern table.
/// Steady-state cost per query is one kernel copy and zero heap allocation
/// (names only allocate when a new process appears).
pub struct ProcessQuery {
    buf: Vec<u8>,
    names: FxHashMap<(u32, i64), Arc<str>>,
    names_next: FxHashMap<(u32, i64), Arc<str>>,
}

impl ProcessQuery {
    pub fn new() -> Self {
        ProcessQuery {
            buf: Vec::with_capacity(512 * 1024),
            names: FxHashMap::default(),
            names_next: FxHashMap::default(),
        }
    }

    /// Snapshot every process on the system into `out`.
    pub fn query(&mut self, out: &mut Vec<RawProcess>) -> Result<(), i32> {
        let buf = &mut self.buf;
    loop {
        let mut needed: u32 = 0;
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_PROCESS_INFORMATION_CLASS,
                buf.as_mut_ptr() as *mut c_void,
                buf.capacity() as u32,
                &mut needed,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            // Kernel told us the size it wanted; pad for processes spawned
            // between the two calls.
            let want = (needed as usize).max(buf.capacity() * 2) + 64 * 1024;
            buf.reserve(want - buf.capacity());
            continue;
        }
        if status < 0 {
            return Err(status);
        }
        break;
    }

    out.clear();
    let base = buf.as_ptr();
    let mut offset = 0usize;
    loop {
        // SAFETY: the kernel guarantees each entry's fixed head fits within
        // the returned buffer and next_entry_offset chains are in-bounds.
        let info = unsafe { &*(base.add(offset) as *const SystemProcessInformation) };

        let key = (info.unique_process_id as u32, info.create_time);
        let name: Arc<str> = if let Some(n) = self.names.get(&key) {
            n.clone()
        } else if info.image_name.buffer.is_null() || info.image_name.length == 0 {
            if info.unique_process_id == 0 {
                Arc::from("System Idle Process")
            } else {
                Arc::from("")
            }
        } else {
            let chars = info.image_name.length as usize / 2;
            let slice = unsafe { std::slice::from_raw_parts(info.image_name.buffer, chars) };
            Arc::from(String::from_utf16_lossy(slice))
        };
        self.names_next.insert(key, name.clone());

        out.push(RawProcess {
            pid: info.unique_process_id as u32,
            parent_pid: info.inherited_from_unique_process_id as u32,
            name,
            threads: info.number_of_threads,
            handles: info.handle_count,
            session_id: info.session_id,
            base_priority: info.base_priority,
            create_time: info.create_time,
            cycle_time: info.cycle_time,
            user_time_100ns: info.user_time,
            kernel_time_100ns: info.kernel_time,
            working_set: info.working_set_size as u64,
            private_working_set: info.working_set_private_size.max(0) as u64,
            private_bytes: info.pagefile_usage as u64,
            read_bytes: info.read_transfer_count.max(0) as u64,
            write_bytes: info.write_transfer_count.max(0) as u64,
            other_bytes: info.other_transfer_count.max(0) as u64,
            read_ops: info.read_operation_count.max(0) as u64,
            write_ops: info.write_operation_count.max(0) as u64,
            hard_faults: info.hard_fault_count,
        });

        if info.next_entry_offset == 0 {
            break;
        }
        offset += info.next_entry_offset as usize;
    }

        // Swap intern tables so entries for exited processes are dropped.
        std::mem::swap(&mut self.names, &mut self.names_next);
        self.names_next.clear();
        Ok(())
    }
}

impl Default for ProcessQuery {
    fn default() -> Self {
        Self::new()
    }
}
