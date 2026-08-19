# Changelog

All notable changes to Argus are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Performance
- Adapter-wide GPU probing (engines, VRAM, temperature) relaxes to every
  other tick while the Performance tab isn't visible — each D3DKMT query can
  stall inside the display driver. Per-process GPU attribution still samples
  every second.

### Added
- Multi-select in the process table: click selects, Ctrl+click toggles,
  Shift+click extends a range. Right-click shows "End N Tasks" / "Copy
  PIDs" / "Copy Names" for the whole selection (right-clicking outside the
  selection retargets it, matching standard multi-select behavior).

## [0.2.0] - 2026-08-18

### Added
- Column picker: right-click any header for a categorized, checkable menu
  (Process / Image / CPU / Memory / I/O / Network / GPU / Objects) with
  "Reset to default". Chosen columns and drag order persist across runs.
- 19 optional process columns: PID parent, Session, Start time, Command line,
  Company, Image path, CPU (kernel), CPU (user), CPU time, Priority,
  Commit size, Working set, Working set peak, Virtual size, Page faults,
  Paged pool, Non-paged pool, I/O read rate, and I/O write rate.
- GPU pane rebuilt around a two-column grid of equal per-engine charts (idle
  engines included), full-width VRAM and GPU-temperature history charts, and
  driver version/date plus adapter LUID in the stats.
- CPU pane "Per core" / "Overall" and "Kernel on/off" toggles; hover any
  chart (including individual core cells) for the value and wall-clock time
  at that point in history.

### Changed
- Charts draw with natural-curve (Catmull-Rom) smoothing, gradient fills that
  fade toward the baseline, and crisp stroked top edges.
- Performance-pane charts flex to fill the pane with the stats grid pinned
  below, instead of fixed heights over empty space.
- Numeric process-table cells right-align under their right-aligned headers,
  and a full-height separator line runs between columns.
- Display names, users, and icons now fill in within the first half-second
  of launch: the enrichment pool sizes to the machine for the startup burst,
  and early sampler ticks surface results as they resolve.

### Performance
- Launch: ~300 ms to first visible window down to ~173 ms median (beating
  TaskSlinger's 218 ms on the same benchmark) by creating the D3D11 device
  concurrently with DirectWrite setup and serving the cached system font
  collection instead of revalidating it.
- CPU: per-process GPU probes only query engine nodes that have ever shown
  global activity, cutting AMD driver stalls roughly in half (Processes tab
  total CPU −22%, Performance tab −13%); optional-column strings only format
  while their column is visible; the chart-animation loop idles while
  minimized.
- Memory: gpui's full-window path-rendering textures (~41 MB incl. the 4×
  MSAA target) are now created on demand and released when no paths render —
  fresh commit 113 → 78 MB, and leaving the Performance tab returns ~72 MB
  that was previously held forever. Icons are extracted once per unique exe
  and decoded once per unique icon. The working set is handed back to the OS
  on minimize (~7 MB in the tray).

### Fixed
- Column-picker checkmarks render (the published gpui-component crate ships
  no icon assets; an embedded check icon is now served via an AssetSource).
- Flat charts no longer show a bulge at the left edge from the area fill's
  baseline anchor being fed into curve smoothing.
- Per-process GPU percent is clamped to 100.

## [0.1.0] - 2026-08-17

Initial release.

- Processes tab: live process table with Apps / Background processes /
  Windows processes sections, child-process and same-exe aggregation with
  expandable groups, app icons and friendly display names, aggregated totals
  in a two-line header, whole-header click-to-sort, drag-to-reorder columns,
  live filtering, and End Task / Copy PID / Copy Name context menu.
- Columns: PID, User, CPU, GPU, Memory, Disk, Network, Threads, Handles,
  Process name — fixed units (Disk MiB/s, Network Mbps), no elevation
  required for any of them.
- Performance tab: sidebar resource cards with sparklines; CPU, Memory
  (composition bar), per-adapter GPU (engines, VRAM, temperature), per-disk,
  and per-NIC detail panes with 60-second smooth-scrolling history charts.
- Collector: direct NT process queries, kernel-ETW disk/network attribution
  with unelevated fallbacks, D3DKMT GPU statistics, IOCTL disk performance,
  and parallel cached enrichment — ~1.5% of one core while visible.
- Fast startup with data on the first frame; adaptive cadence and reduced
  work while minimized.
- Windows GUI binary with embedded Argus icon, GitHub Actions CI, and
  tag-driven releases.

[Unreleased]: https://github.com/wackoisgod/argus/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/wackoisgod/argus/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/wackoisgod/argus/releases/tag/v0.1.0
