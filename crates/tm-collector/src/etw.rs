//! Real-time ETW consumption of the kernel disk and network providers — the
//! same sources Windows Task Manager uses for its Disk and Network columns,
//! and strictly better data than I/O transfer counters (which lump in pipes
//! and device I/O).
//!
//! Starting a real-time trace session requires elevation (or membership in
//! the Performance Log Users group). When that fails, [`EtwMonitor::active`]
//! is false and callers fall back gracefully.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ferrisetw::parser::Parser;
use ferrisetw::provider::Provider;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::UserTrace;
use ferrisetw::EventRecord;

const KERNEL_DISK: &str = "c7bde69a-e1e0-4177-b6ef-283ad1525271"; // Microsoft-Windows-Kernel-Disk
const KERNEL_NETWORK: &str = "7dd42a49-5329-4832-8dfd-43d979153a88"; // Microsoft-Windows-Kernel-Network

/// Cumulative per-process byte counters since the session started.
#[derive(Default, Clone, Copy)]
pub struct IoTotals {
    pub disk_read: u64,
    pub disk_write: u64,
    pub net_send: u64,
    pub net_recv: u64,
}

#[derive(Default)]
struct EtwState {
    totals: Mutex<HashMap<u32, IoTotals>>,
}

pub struct EtwMonitor {
    state: Arc<EtwState>,
    /// False when the trace session could not be started (not elevated).
    pub active: bool,
    // Dropping the trace stops the session; keep it alive with the monitor.
    _trace: Option<UserTrace>,
}

impl EtwMonitor {
    pub fn start() -> Self {
        let state = Arc::new(EtwState::default());

        let disk_state = Arc::clone(&state);
        let disk = Provider::by_guid(KERNEL_DISK)
            // Keyword mask 0 (the default) does not match Kernel-Disk's
            // keyword-tagged events; ask for everything.
            .any(u64::MAX)
            .add_callback(move |record: &EventRecord, sl: &SchemaLocator| {
                let id = record.event_id();
                // 10 = Read completed, 11 = Write completed.
                if id != 10 && id != 11 {
                    return;
                }
                // This build's manifest has no IssuingProcessId field; the
                // event header pid is the issuer for direct I/O, and System
                // (4) for cached lazy-writes — same attribution Task Manager
                // shows.
                let pid = record.process_id();
                if pid == u32::MAX {
                    return;
                }
                let Ok(schema) = sl.event_schema(record) else {
                    return;
                };
                let parser = Parser::create(record, &schema);
                let size: u32 = match parser.try_parse("TransferSize") {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let mut totals = disk_state.totals.lock().unwrap();
                let t = totals.entry(pid).or_default();
                if id == 10 {
                    t.disk_read += size as u64;
                } else {
                    t.disk_write += size as u64;
                }
            })
            .build();

        let net_state = Arc::clone(&state);
        let network = Provider::by_guid(KERNEL_NETWORK)
            .any(u64::MAX)
            .add_callback(move |record: &EventRecord, sl: &SchemaLocator| {
                // TCPv4/v6 send/recv: 10/11, 26/27. UDPv4/v6: 42/43, 58/59.
                let id = record.event_id();
                let send = matches!(id, 10 | 26 | 42 | 58);
                let recv = matches!(id, 11 | 27 | 43 | 59);
                if !send && !recv {
                    return;
                }
                let Ok(schema) = sl.event_schema(record) else {
                    return;
                };
                let parser = Parser::create(record, &schema);
                let pid: u32 = match parser.try_parse("PID") {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let size: u32 = match parser.try_parse("size") {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let mut totals = net_state.totals.lock().unwrap();
                let t = totals.entry(pid).or_default();
                if send {
                    t.net_send += size as u64;
                } else {
                    t.net_recv += size as u64;
                }
            })
            .build();

        // A previous instance that died without stopping its session leaves
        // the name claimed (ETW sessions outlive their creating process).
        let _ = ferrisetw::trace::stop_trace_by_name("tm-app-io");

        match UserTrace::new()
            .named("tm-app-io".to_string())
            .enable(disk)
            .enable(network)
            .start_and_process()
        {
            Ok(trace) => EtwMonitor {
                state,
                active: true,
                _trace: Some(trace),
            },
            Err(err) => {
                eprintln!("ETW session unavailable: {err:?}");
                EtwMonitor {
                    state,
                    active: false,
                    _trace: None,
                }
            }
        }
    }

    /// Copy out current totals (small map: only pids that did I/O).
    pub fn totals(&self) -> HashMap<u32, IoTotals> {
        self.state.totals.lock().unwrap().clone()
    }

    /// Drop counters for pids that no longer exist so the map stays bounded.
    pub fn retain(&self, live: impl Fn(u32) -> bool) {
        self.state.totals.lock().unwrap().retain(|pid, _| live(*pid));
    }
}
