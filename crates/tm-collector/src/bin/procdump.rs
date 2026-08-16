//! Console validation harness for the collector: prints system totals and the
//! top processes by CPU for a few ticks, so the data layer can be sanity
//! checked against Task Manager side by side.

use std::time::Duration;
use tm_collector::{fmt_bytes, Sampler};

fn main() {
    let ticks: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(3);

    let mut sampler = Sampler::new();
    sampler.sample(); // baseline; rates need two snapshots

    for tick in 0..ticks {
        std::thread::sleep(Duration::from_secs(1));
        let t0 = std::time::Instant::now();
        let snap = sampler.sample();
        let cost = t0.elapsed();
        let s = &snap.system;
        println!(
            "\n== tick {} [sample cost: {:?}] | CPU {:5.1}% | Mem {}/{} ({:.0}%) | {} procs, {} threads, {} handles ==",
            tick + 1,
            cost,
            s.cpu_percent,
            fmt_bytes(s.mem_used()),
            fmt_bytes(s.mem_total),
            s.mem_percent(),
            s.process_count,
            s.thread_count,
            s.handle_count,
        );

        let mut procs: Vec<_> = snap
            .processes
            .iter()
            .filter(|p| p.raw.pid != 0) // hide Idle
            .collect();
        procs.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent));

        println!(
            "{:<28} {:>7} {:>6} {:>12} {:>12} {:>12} {:>7} {:>7}",
            "Name", "PID", "CPU%", "WorkingSet", "Read/s", "Write/s", "Thrds", "Hndls"
        );
        for p in procs.iter().take(15) {
            println!(
                "{:<28} {:>7} {:>6.1} {:>12} {:>12} {:>12} {:>7} {:>7}",
                truncate(&p.raw.name, 28),
                p.raw.pid,
                p.cpu_percent,
                fmt_bytes(p.raw.working_set),
                fmt_bytes(p.read_bytes_per_sec),
                fmt_bytes(p.write_bytes_per_sec),
                p.raw.threads,
                p.raw.handles,
            );
        }
    }
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
