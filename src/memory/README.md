# Fuchsia Memory (`//src/memory`)

`//src/memory` contains Fuchsia's userspace memory monitoring, attribution,
profiling, diagnostics components, shared libraries, and host analysis tools.

## Directory Layout

### Memory Attribution & System Monitoring

- **`attribution/`** — Fuchsia's modern memory attribution framework and
  **Memory Monitor 2** (`memory_monitor2`):
  - `monitor/` — `memory_monitor2` system component collecting Zircon kernel
    statistics, attribution trees, memory stalls, and page refault telemetry for
    Inspect, Cobalt metrics, and trace recording.
  - `processing/` — Rust library aggregating Principal/Resource attribution
    claims, Zircon VMO/mapping snapshots, and memory bucket definitions into
    digests and summaries.
  - `server/` — Rust helper library serving `fuchsia.memory.attribution.Provider`
    for runner and component implementations (such as ELF runner, Starnix, and
    driver manager).
  - `refaults_vmo/` — Shared-memory bitvector and counter primitives
    (`PageRefaultCounter`, `AtomicBitVec`) used by user pagers (Fxfs, Erofs,
    Starnix) to report page refaults to `fuchsia.memory.attribution.PageRefaultSink`.
  - `testing/` — Integration test utilities for verifying component memory
    attribution trees.
- **`buckets/`** — JSON5 memory bucket definitions (`buckets.json5` and
  subsystem-specific bucket configs) that classify processes and VMOs into named
  subsystems for Inspect reporting, Cobalt telemetry, and memory triage.
- **`pressure_signaler/`** — `memory_pressure_signaler` component monitoring
  Zircon kernel memory pressure events, serving `fuchsia.memorypressure.Provider`
  to system clients, and optionally filing critical crash reports.
- **`legacy/`** — Legacy C++ **Memory Monitor 1** implementation, superseded by
  `attribution/monitor/`:
  - `legacy/monitor/` — Legacy C++ `memory_monitor` component.
  - `legacy/metrics/` — Legacy C++ library capturing Zircon process/VMO state and
    computing memory summaries, digests, and bucket matches for
    `legacy/monitor/`.

### Profiling & Diagnostics Components

- **`heapdump/`** — Live userspace and kernel heap profiler backing
  `ffx profile heapdump`:
  - `collector/` — On-device component collecting live heap snapshots from
    instrumented processes and serving symbolizable `pprof` profiles.
  - `instrumentation/` — C/C++ Scudo allocator hooks and Rust bindings recording
    live allocations into shared VMOs.
  - `kernel-collector/` — Collector exporting Zircon kernel heap profiles.
  - `lib/` — Shared snapshot streaming (`heapdump_snapshot`) and VMO layout
    (`heapdump_vmo`) crates.
  - `example/` — Example component demonstrating `heapdump` instrumentation.
- **`sampler/`** — Statistical heap profiler (`memory_sampler` component and
  `instrumentation/` Poisson sampling library) recording sampled allocations and
  exporting `pprof` profiles via crash reports.
- **`stacktrack/`** — Live thread call-stack tracker backing
  `ffx profile stacktrack`:
  - `collector/` — On-device collector snapshotting live thread stacks from
    instrumented processes.
  - `instrumentation/` — Runtime hooks and async frame tracking recording live
    call stacks into shared VMOs.
  - `lib/` — Shared snapshot (`stacktrack_snapshot`) and VMO table
    (`stacktrack_vmo`) crates.
  - `example/` — Example component demonstrating `stacktrack` integration.
- **`thrasher/`** — Diagnostic component and tool for generating controlled
  page/memory thrashing workloads.

### Shared Libraries & Allocator Utilities (`lib/`)

- **`lib/index_table_builder/`** — Deduplicating string and index table builder
  crate used during profile and trace serialization.
- **`lib/memory_mapped_vmo/`** — Safe Rust wrapper over memory-mapped Zircon VMOs
  shared by `attribution/refaults_vmo`, `heapdump`, and `stacktrack`.
- **`lib/performance/`** — Host Python library (`memory`: `profile.py`,
  `resource_usage.py`) used by end-to-end performance tests to capture device
  memory profiles and publish metrics.
- **`lib/pprof/`** — Rust crate constructing `pprof` protobuf profiles (shared by
  `ffx profile heapdump` and `tools/dupefinder`).
- **`lib/scudo/`** — C header (`mallopt.h`) and Rust bindings (`scudo`) for
  controlling the Scudo memory allocator (such as purging thread caches).

### Host Analysis Tools & Scripts

- **`tools/`** — Offline host analysis scripts and Python libraries (`smaps/`,
  `snapshots/`, `debug_json/`, `images_json/`, `multidict/`,
  `dataclasses_json_lite/`, `components_sql.py`, `mem_digest.py`, `mem_tree.py`,
  `treemap.py`) for inspecting snapshots, computing digests, and visualizing
  memory usage.
  - **`tools/dupefinder/`** — Host Rust CLI (`fx dupefinder`) that analyzes
    `heapdump` snapshots and heap content dumps to blame source locations
    allocating duplicate buffers and render HTML/JSON reports.

## Testing

Run unit and integration tests across `//src/memory`:

```sh
fx test //src/memory:tests
```

Host-driven emulator E2E tests (such as `dupefinder`'s smoke test) live under:

```sh
fx test //src/memory:host_tests
```

## Related Directories Outside `//src/memory`

- **FIDL & SDK Libraries**:
  - `//sdk/fidl/fuchsia.memory*` (`fuchsia.memory`,
    `fuchsia.memory.attribution`, `fuchsia.memory.attribution.plugin`,
    `fuchsia.memory.debug`, `fuchsia.memory.heapdump.*`,
    `fuchsia.memory.inspection`, `fuchsia.memory.sampler`,
    `fuchsia.memory.stacktrack.*`)
  - `//sdk/fidl/fuchsia.memorypressure`
  - `//sdk/lib/heapdump_instrumentation`
- **FFX Developer CLI Plugins**:
  - `//src/developer/ffx/plugins/profile/memory`
  - `//src/developer/ffx/plugins/profile/heapdump`
  - `//src/developer/ffx/plugins/profile/stacktrack`
- **Assembly Inputs & Boot Budget Tests**:
  - `//bundles/assembly/bazel_inputs/src/memory`
  - `//src/tests/memory_budget`
