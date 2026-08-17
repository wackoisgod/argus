# Argus

A fast, native Windows task manager. Rust + [GPUI](https://www.gpui.rs/) (Zed's GPU-accelerated UI framework).

Named for the hundred-eyed watchman: Argus sees everything running on the machine while staying nearly invisible in its own process list.

## Highlights

- **~220 ms cold start** with a fully populated process table on the first frame — collection warms up in parallel with window creation.
- **~1.5% of one core idle** with the window visible, **~0.5% minimized** (adaptive sampling and an input-aware render cadence), versus ~5% for comparable tools.
- **Processes tab**: grouped Apps / Background / Windows sections with process-tree and same-name aggregation, icons, friendly names, live filter, click-to-sort on every column, drag-reorderable columns, End Task context menu.
- **Real data, no elevation required**: process metrics from a single `NtQuerySystemInformation` call per tick; per-process GPU via `D3DKMTQueryStatistics`; disk/network approximated from counters unelevated and silently upgraded to kernel ETW truth when elevated.
- **Performance tab**: smooth-scrolling 60-second history charts with hover inspection — per-core CPU grid with kernel overlay, memory with composition bar, per-engine GPU utilization with VRAM and temperature, physical disks, and per-adapter network.

## Building

```
cargo build --release
```

The binary lands at `target/release/argus.exe`. Requires stable Rust on Windows (MSVC toolchain). No elevation needed to build or run.

## Architecture

- `crates/argus-collector` — all data collection: NT process snapshots, ETW sessions, D3DKMT GPU queries, disk ioctls, adapter tables, and a background enrichment pool for icons/descriptions/users. UI-agnostic.
- `crates/argus` — the GPUI app: virtualized process table (via `gpui-component`), performance charts drawn with GPUI canvas paths.
- `vendor/gpui` — gpui 0.2.2 with three small idle-efficiency patches to the Windows vsync loop (search `TaskManager patch`).

A `procdump` binary in the collector crate prints the same data to a console for validation against Task Manager.

## Status

Personal project under active development; expect rough edges. No license has been chosen yet.
