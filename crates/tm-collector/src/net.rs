//! Unelevated connection-table query: which pids currently own TCP/UDP
//! sockets. Used to gate the counter-based network approximation when the
//! ETW session is unavailable, so processes doing heavy non-network ioctls
//! (audio, GPU) don't show phantom network traffic.

use std::collections::HashSet;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};

const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

pub struct ConnQuery {
    buf: Vec<u8>,
}

impl ConnQuery {
    pub fn new() -> Self {
        ConnQuery {
            buf: Vec::with_capacity(128 * 1024),
        }
    }

    /// Every pid that owns at least one TCP or UDP endpoint.
    pub fn pids_with_connections(&mut self) -> HashSet<u32> {
        let mut pids = HashSet::new();
        // (row size, pid offset) per table layout; see MIB_*ROW_OWNER_PID.
        self.collect(true, AF_INET, 24, 20, &mut pids);
        self.collect(true, AF_INET6, 56, 52, &mut pids);
        self.collect(false, AF_INET, 12, 8, &mut pids);
        self.collect(false, AF_INET6, 28, 24, &mut pids);
        pids
    }

    fn collect(
        &mut self,
        tcp: bool,
        af: u32,
        row_size: usize,
        pid_offset: usize,
        out: &mut HashSet<u32>,
    ) {
        loop {
            let mut size = self.buf.capacity() as u32;
            let ret = unsafe {
                if tcp {
                    GetExtendedTcpTable(
                        self.buf.as_mut_ptr().cast(),
                        &mut size,
                        0,
                        af,
                        TCP_TABLE_OWNER_PID_ALL,
                        0,
                    )
                } else {
                    GetExtendedUdpTable(
                        self.buf.as_mut_ptr().cast(),
                        &mut size,
                        0,
                        af,
                        UDP_TABLE_OWNER_PID,
                        0,
                    )
                }
            };
            if ret == ERROR_INSUFFICIENT_BUFFER {
                let want = size as usize + 16 * 1024;
                if want > self.buf.capacity() {
                    self.buf.reserve(want - self.buf.capacity());
                }
                continue;
            }
            if ret != 0 {
                return;
            }
            break;
        }
        // Both table types start with a u32 entry count followed by rows.
        let base = self.buf.as_ptr();
        let count = unsafe { *(base as *const u32) } as usize;
        for i in 0..count {
            let pid = unsafe { *(base.add(4 + i * row_size + pid_offset) as *const u32) };
            if pid != 0 {
                out.insert(pid);
            }
        }
    }
}

impl Default for ConnQuery {
    fn default() -> Self {
        Self::new()
    }
}
