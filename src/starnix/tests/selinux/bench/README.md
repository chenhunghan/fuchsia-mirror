# SEStarnix Microbenchmarks

This directory defines two performance test components for Starnix's SELinux
implementation (`//src/starnix/lib/selinux` and
`//src/starnix/kernel/core/security`):

* **`selinux_bench`** (`fuchsia.sestarnix`): Criterion latency microbenchmarks
  covering policy loading, `SidTable` lookups, uncached policy calculations,
  shared cache lookups, multi-threaded contention, and Starnix LSM security
  hooks against `aosp_sepolicy`.
* **`sestarnix_memory_bench`** (`fuchsia.sestarnix_memory`): Heap memory
  measurement for loading `aosp_sepolicy` into a `SecurityServer`.

## Real-World Performance vs. Diagnostic Signals

Performance optimization work should ultimately improve real-world workloads.
Most benchmarks in `selinux_bench` intentionally bypass production fast-paths
and caches, to isolate the cost of relatively infrequent operations as signals
for regression diagnosis; optimizing those metrics in isolation rarely improves
real-world performance, and can regress memory footprint or code simplicity.

### Production Fast-Path Hierarchy

In production, SEStarnix resolves checks through five tiers, from hottest to
coldest:

1. **LSM hook fast-paths:** Kernel-private anonymous nodes skip `FsNode` checks,
   same-domain file operations skip `fd { use }`, and `file_permission()` skips
   cache and policy lookups when the task SID, node SID, and policy version
   still match the state validated at `file_open()`.
2. **Per-task cache (`PerThreadCache`):** Small per-`CurrentTask` LRU caches
   satisfy most steady-state permission checks without locks or cross-core
   synchronization.
3. **Shared cache (`AccessVectorCache`):** Lock-free caches shared across tasks
   absorb per-task misses for access, extended-permission, and unnamed
   create-SID queries.
4. **Uncached policy and `SidTable` (RCU read-side):** Reached only on shared
   cache misses, named object creation, or security context string parsing and
   serialization.
5. **Writer-locked state:** Reached only when minting a new dynamic SID,
   resolving filesystem mount labels, or loading or modifying policy.

### Interpreting Results

Each benchmark serves one of four roles:

* **Optimization goal (`load_policy_memory`):** Memory retained by
  `SecurityServer` is pinned for the lifetime of the container, directly
  reducing RAM available to user workloads.
* **Hot-path regression signals:** Guard the LSM hook fast-paths,
  `PerThreadCache`, and `AccessVectorCache` hit paths. Even on hot paths, weigh
  nanosecond changes against full syscall or IPC cost in real workload traces
  before adding complexity.
* **Diagnostic signals:** Measure uncached policy evaluation, transition
  calculation, `SidTable` conversion, and policy load in isolation to help
  [attribute regressions][use-cases] seen in end-to-end workloads. Do not add
  caching or complexity solely to improve these metrics without trace or cache
  miss evidence from a real workload.
* **Scalability canaries (`*_contended`):** Verify under synthetic multi-core
  load that RCU readers and lock-free caches scale without cross-core contention
  or blocking on writer locks; they are not standalone optimization targets when
  contended latency tracks uncontended latency.

Intentional trade-offs across metrics are expected -- for example, trading a
small increase in boot-once `load_policy` time or uncached query cost for lower
`load_policy_memory` or simpler policy structures when real workloads are
unaffected.

---

## Benchmark Catalog

### `fuchsia.sestarnix_memory` (`sestarnix_memory_bench`)

| Metric | What It Measures | Frequency | Role |
| :--- | :--- | :--- | :--- |
| `load_policy_memory` | Net heap bytes retained by `SecurityServer` after loading `aosp_sepolicy` (indexed policy, initial `SidTable`, and pre-allocated `AccessVectorCache`). | Pinned for container lifetime | **Optimization goal** |

### `fuchsia.sestarnix` (`selinux_bench`)

All post-load `SecurityServer` benchmarks pre-populate the `SidTable` with
dynamic SIDs so lookups operate on a realistically sized table.

#### Policy Loading & `SidTable` Conversions

| Metric(s) | What It Measures | Frequency | Role |
| :--- | :--- | :--- | :--- |
| `load_policy` | Parsing and validating `aosp_sepolicy` and building the indexed policy and initial `SidTable`. | Once per container boot | **Diagnostic signal** (boot latency; trades off against `load_policy_memory`) |
| `security_context_to_sid_{simple, c0_c255}` | Parsing a context string and looking up an already-allocated SID in `SidTable` (`c0_c255` adds MLS category parsing and hashing; excludes minting new SIDs). | Cold inode labeling and explicit context writes | **Diagnostic signal** |
| `sid_to_security_context_{simple, c0_c255}` | Looking up a SID in `SidTable` and serializing a newly allocated context string (`c0_c255` adds MLS category formatting). | Context serialization queries and audit logging | **Diagnostic signal** |

#### Policy Computations & Object Transitions

| Metric(s) | What It Measures | Frequency | Role |
| :--- | :--- | :--- | :--- |
| `compute_access_decision_{simple, c0_c255, constraints}` | Uncached `compute_access_decision_raw` evaluation, separating baseline rule lookup (`simple`), MLS category comparison (`c0_c255`), and constraint evaluation (`constraints`). | Shared `AccessVectorCache` misses only | **Diagnostic signal** |
| `compute_create_sid_{simple, c0_c255}` | Uncached nameless `process` transition calculation and `SidTable` lookup (`c0_c255` adds MLS range handling). | Shared `AccessVectorCache` misses only | **Diagnostic signal** |
| `compute_create_sid_filename_{match, no_match, no_target_rules, nameless}` | Uncached `compute_create_sid_raw` across filename-transition rule match (`match`), hash miss (`no_match`), target-type bitmap rejection (`no_target_rules`), and empty name (`nameless`). | Named object creation (`nameless`: shared `AccessVectorCache` misses only) | **Diagnostic signal** |
| `cached_create_sid_filename_nameless` | `PermissionCheck::compute_create_sid` with an empty name, hitting `AccessVectorCache` (paired with `compute_create_sid_filename_nameless`). | Process transitions and unnamed object creation | **Hot-path regression signal** |

#### Shared Cache & Multi-Threaded Contention

| Metric(s) | What It Measures | Frequency | Role |
| :--- | :--- | :--- | :--- |
| `concurrent_access_cache_get` | Total latency for a batch of 1,000 single-threaded `ConcurrentAccessCache` hits. | Tasks whose working set exceeds `PerThreadCache` | **Hot-path regression signal** |
| `security_context_to_sid_{read,write}_contended`, `compute_access_decision_{read,write}_contended`, `compute_create_sid_{read,write}_contended`, `compute_create_sid_filename_read_contended` | Uncached `SecurityServer` queries under concurrent background RCU read load (`read_contended`), plus periodic writer-lock acquisitions (`write_contended`). | Synthetic multi-core stress | **Scalability canary** |
| `concurrent_access_cache_get_contended` | Batch of 1,000 `ConcurrentAccessCache` hits while background threads drive concurrent cache misses and evictions. | Synthetic cache-eviction stress | **Scalability canary** |

#### Starnix LSM Security Hooks

All LSM hook benchmarks run inside an in-process Starnix kernel instance with
`aosp_sepolicy` loaded and enforced.

| Metric(s) | What It Measures | Frequency | Role |
| :--- | :--- | :--- | :--- |
| `file_permission_tmpfs` | `security::file_permission` on an opened `tmpfs` file after `file_open()`, exercising the steady-state open-state fast path. | Opened-file reads and writes | **Hot-path regression signal** |
| `fs_node_permission_tmpfs` | `security::fs_node_permission` on a policy-labeled `tmpfs` file, hitting `PerThreadCache`. | Path resolution and inode access checks | **Hot-path regression signal** |
| `file_open_tmpfs` | `security::file_open` on a newly opened `tmpfs` file, hitting `PerThreadCache` and initializing cached open state. | File opens | **Hot-path regression signal** |
| `check_file_ioctl_access_tmpfs` | `security::check_file_ioctl_access` on an opened `tmpfs` file, exercising extended-permission (`allowxperm`) checks via `PerThreadCache`. | Ioctls on labeled files and devices | **Hot-path regression signal** |
| `binder_transaction` | `security::binder_transaction` for a single peer domain pair, hitting `PerThreadCache`. | Binder transactions (single-pair best case) | **Hot-path regression signal** |
| `file_permission`, `fs_node_permission`, `check_file_ioctl_access` | Hook dispatch on a kernel-private anonymous file owned by `current_task`, returning before `FsNode`, extended-permission, or cache lookups. | Kernel-private anonymous fds | **Hot-path regression signal** (hook-entry baseline) |

---

## Running Benchmarks

### Locally

Run both suites and write `*.fuchsiaperf.json` artifacts to `<output_dir>` (see
[Running performance tests][running-perf-tests]):

```sh
fx test --e2e --outdir <output_dir> selinux_bench sestarnix_memory_bench
```

### CI and Perfcompare

By default, these benchmarks run only in CI and upload results to the
performance dashboards (Googlers can view trends at
[go/sestarnix-perf][sestarnix-perf]).

To run them on CQ before submitting a CL, add a [perfcompare][perfcompare]
tryjob such as `terminal.x64-release-perfcompare` (in Gerrit: "Choose Tryjobs" >
search "perfcompare" > "Add"). Once the tryjob finishes, search for `selinux` in
the **Test Results** tab and open the `report.txt` artifact.

[perfcompare]: /docs/development/performance/perfcompare.md
[running-perf-tests]: /docs/development/performance/running_performance_tests.md
[sestarnix-perf]: https://goto.google.com/sestarnix-perf
[use-cases]: /docs/development/performance/use_cases_for_tests.md
