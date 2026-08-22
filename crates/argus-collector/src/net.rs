//! Unelevated connection-table query: which pids currently own TCP/UDP
//! sockets. Used to gate the counter-based network approximation when the
//! ETW session is unavailable, so processes doing heavy non-network ioctls
//! (audio, GPU) don't show phantom network traffic.

use rustc_hash::FxHashSet;
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
    pub fn pids_with_connections(&mut self) -> FxHashSet<u32> {
        let mut pids = FxHashSet::default();
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
        out: &mut FxHashSet<u32>,
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

/// One endpoint row for the Connections tab.
#[derive(Clone, Debug)]
pub struct ConnRow {
    pub proto: &'static str,
    pub local: String,
    pub remote: String,
    pub state: &'static str,
    /// MIB_TCP_STATE for sorting (0 for UDP).
    pub state_ord: u32,
    pub pid: u32,
}

fn tcp_state(state: u32) -> &'static str {
    match state {
        1 => "CLOSED",
        2 => "LISTENING",
        3 => "SYN_SENT",
        4 => "SYN_RCVD",
        5 => "ESTABLISHED",
        6 => "FIN_WAIT1",
        7 => "FIN_WAIT2",
        8 => "CLOSE_WAIT",
        9 => "CLOSING",
        10 => "LAST_ACK",
        11 => "TIME_WAIT",
        12 => "DELETE_TCB",
        _ => "",
    }
}

fn v4(addr_be: u32, port_be: u32) -> String {
    let ip = std::net::Ipv4Addr::from(u32::from_be(addr_be));
    format!("{ip}:{}", u16::from_be(port_be as u16))
}

fn v6(bytes: &[u8], port_be: u32) -> String {
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&bytes[..16]);
    let ip = std::net::Ipv6Addr::from(octets);
    format!("[{ip}]:{}", u16::from_be(port_be as u16))
}

impl ConnQuery {
    /// Full connection table (TCP+UDP, v4+v6) — one pass, unelevated.
    pub fn connections(&mut self) -> Vec<ConnRow> {
        let mut out = Vec::with_capacity(512);
        self.rows_tcp4(&mut out);
        self.rows_tcp6(&mut out);
        self.rows_udp4(&mut out);
        self.rows_udp6(&mut out);
        out
    }

    /// Fills self.buf for (tcp, af); returns entry count or 0.
    fn fill(&mut self, tcp: bool, af: u32) -> usize {
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
                return 0;
            }
            break;
        }
        unsafe { *(self.buf.as_ptr() as *const u32) as usize }
    }

    fn rows_tcp4(&mut self, out: &mut Vec<ConnRow>) {
        let count = self.fill(true, AF_INET);
        let base = unsafe { self.buf.as_ptr().add(4) };
        for i in 0..count {
            // MIB_TCPROW_OWNER_PID: state@0 local@4 lport@8 rem@12 rport@16 pid@20
            let row = unsafe { base.add(i * 24) };
            let read = |off: usize| unsafe { *(row.add(off) as *const u32) };
            let state = read(0);
            let listening = state == 2;
            out.push(ConnRow {
                proto: "TCPv4",
                local: v4(read(4), read(8)),
                remote: if listening {
                    "*:*".into()
                } else {
                    v4(read(12), read(16))
                },
                state: tcp_state(state),
                state_ord: state,
                pid: read(20),
            });
        }
    }

    fn rows_tcp6(&mut self, out: &mut Vec<ConnRow>) {
        let count = self.fill(true, AF_INET6);
        let base = unsafe { self.buf.as_ptr().add(4) };
        for i in 0..count {
            // MIB_TCP6ROW_OWNER_PID: laddr[16]@0 lscope@16 lport@20
            // raddr[16]@24 rscope@40 rport@44 state@48 pid@52
            let row = unsafe { base.add(i * 56) };
            let read = |off: usize| unsafe { *(row.add(off) as *const u32) };
            let bytes = |off: usize| unsafe { std::slice::from_raw_parts(row.add(off), 16) };
            let state = read(48);
            let listening = state == 2;
            out.push(ConnRow {
                proto: "TCPv6",
                local: v6(bytes(0), read(20)),
                remote: if listening {
                    "*:*".into()
                } else {
                    v6(bytes(24), read(44))
                },
                state: tcp_state(state),
                state_ord: state,
                pid: read(52),
            });
        }
    }

    fn rows_udp4(&mut self, out: &mut Vec<ConnRow>) {
        let count = self.fill(false, AF_INET);
        let base = unsafe { self.buf.as_ptr().add(4) };
        for i in 0..count {
            // MIB_UDPROW_OWNER_PID: addr@0 port@4 pid@8
            let row = unsafe { base.add(i * 12) };
            let read = |off: usize| unsafe { *(row.add(off) as *const u32) };
            out.push(ConnRow {
                proto: "UDPv4",
                local: v4(read(0), read(4)),
                remote: "*:*".into(),
                state: "",
                state_ord: 0,
                pid: read(8),
            });
        }
    }

    fn rows_udp6(&mut self, out: &mut Vec<ConnRow>) {
        let count = self.fill(false, AF_INET6);
        let base = unsafe { self.buf.as_ptr().add(4) };
        for i in 0..count {
            // MIB_UDP6ROW_OWNER_PID: addr[16]@0 scope@16 port@20 pid@24
            let row = unsafe { base.add(i * 28) };
            let read = |off: usize| unsafe { *(row.add(off) as *const u32) };
            let bytes = |off: usize| unsafe { std::slice::from_raw_parts(row.add(off), 16) };
            out.push(ConnRow {
                proto: "UDPv6",
                local: v6(bytes(0), read(20)),
                remote: "*:*".into(),
                state: "",
                state_ord: 0,
                pid: read(24),
            });
        }
    }
}

#[cfg(test)]
mod conn_tests {
    use super::*;

    #[test]
    fn connections_enumerate() {
        let mut q = ConnQuery::new();
        let rows = q.connections();
        assert!(rows.len() > 10, "expected live endpoints, got {}", rows.len());
        assert!(rows.iter().any(|r| r.state == "LISTENING"));
        assert!(rows.iter().any(|r| r.proto.starts_with("UDP")));
    }
}
