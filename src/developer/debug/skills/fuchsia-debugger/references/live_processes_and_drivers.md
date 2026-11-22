# Live processes and drivers

`fx debug cli` attaches directly to active components, processes, and DFv2
driver hosts on a Fuchsia target. For full flag and JSON schemas, see [Command
and JSON protocol reference](command_reference.md).

## Start and stop a standalone session

When you do not use `fx test --agent-debugging-mode`, manage the daemon and
target attachment explicitly:

1.  Start the background daemon (which also starts the Fuchsia package server if
    none is active):

    ```posix-terminal
    fx debug cli start
    ```

2.  Attach by process name substring, exact name (`--exact`), or Zircon KOID:

    ```posix-terminal
    fx debug cli attach cobalt.cm
    ```

3.  When you finish, detach from individual processes or stop the daemon:

    ```posix-terminal
    fx debug cli detach --all
    fx debug cli stop
    ```

## Discover and attach to a DFv2 driver host

When you pause a driver host, you pause every driver in that process. If a
transport or watchdog driver shares that host, a pause can sever USB, CDC-NCM,
ADB, ethernet, or serial transport, or trip a hardware watchdog.

Before you attach, run `driver-list` to inspect colocated drivers and
`transport_hazard` flags. Then attach by driver name or URL (`--driver` or `-d`)
or by device topology moniker (`--node` or `-n`):

```posix-terminal
# List active driver_hosts, colocated drivers, and transport hazards
fx debug cli driver-list --filter xhci

# Attach by driver name/URL or device node moniker (warns on transport hazards)
fx debug cli attach --driver xhci
fx debug cli attach --node dev.sys.platform.xhci
```

### Transport and watchdog hazards

Caution: Inspect `transport_hazard` in `driver-list` or `attach` output for
colocated USB, CDC-NCM, ethernet, serial, or watchdog drivers. If
`transport_hazard` is `true`, do not run thread-pausing commands or set
breakpoints (`break`) on that host:

- **Auto-pause without auto-resume:** `pause`, `stack-trace`, `variables`,
  `evaluate`, `async-backtrace`, `watch` (when you set a watchpoint),
  `console -t`, `mem-read -t`, and `tracepoint -w <expr> -t <tid>` pause active
  targets and leave them paused.
- **Auto-pause with auto-resume:** `diagnose-hang` pauses active threads briefly
  and resumes them by default (unless you pass `--no-auto-resume` or a thread
  was already paused). Even a brief pause can drop transport links or trigger
  watchdogs.

On transport-sensitive driver hosts, use these non-pausing workflows:

- Attach weakly (`--weak`) to catch crashes without thread pauses:

  ```posix-terminal
  fx debug cli attach --driver xhci --weak
  ```

- Query session and process state with non-pausing commands: `driver-list`,
  `get-state`, `threads`, `modules`, `aspace`, `handles`, `console` without
  `-t`, and address-only `mem-read` without `-t`. Note that `modules` switches a
  weak attach to a strong attach.
- Sample variables, expressions, or backtraces with `tracepoint` probes at a
  code location or a `-w 0x...` hex address (see [Breakpoints, watchpoints, and
  tracepoints](breakpoints_watchpoints_and_tracepoints.md)).

### Unbound drivers and deferred filters

If a driver is not active or bound yet, pass `--wait` with `--driver` or
`--node`. When no active driver host matches, `fx debug cli` installs a `zxdb`
process filter on `"driver_host"` and attaches to every new driver host. `zxdb`
filters match process names rather than driver URLs or device monikers.

`fx debug cli` forwards `--weak`, `--recursive`, and `--job-only` to this
filter. The response returns `requested_target` and `filter_warning` because all
new driver hosts will match. When new driver hosts can load transport drivers,
combine `--wait` with `--weak`:

```posix-terminal
fx debug cli attach --driver my_driver --wait --weak
```

## Inspect a live process without thread pauses

`modules`, `aspace`, `handles`, and address-only `mem-read` return structured
JSON and do not pause target threads. When you attach to multiple processes,
pass `-p <pid>` (`--pid`) to select the target process KOID:

```posix-terminal
# Inspect loaded modules/DSOs, virtual memory mappings, and Zircon handles
fx debug cli modules --filter xhci
fx debug cli aspace --filter mmio
fx debug cli handles --type interrupt --detail

# Read raw memory at a hex address without thread pauses
fx debug cli mem-read 0x7ffee1234000 -s 32
```

Account for these side effects and performance behaviors:

- `modules` (aliases `libs`, `sym-stat`) requests the module list from
  `debug_agent`, which switches a weak attach (`--weak`) to a strong attach.
  Pass `-r` (`--reload`) to reload symbols.
- `handles -k <koid>` queries a single handle directly in 1 IPC call and always
  returns full detail. Without `-k`, `handles --detail` makes N + 1 sequential
  IPC calls to load handle rights, related KOIDs, and VMO metadata. On processes
  with large handle tables, combine `--detail` with `--type <type>`.
- `mem-read` pauses the target thread only when you pass `-t <tid>` (and
  optional `--frame-index`) to evaluate a frame-relative expression such as
  `"&dev->regs"`.

## Diagnose concurrency stalls and async tasks

If a process can pause briefly, use `diagnose-hang` or `async-backtrace` to
inspect stalls, locks, IPC waits, and async executors:

- **Single-shot stall snapshot (`diagnose-hang`, aliases `hang`,
  `hang-analyze`):** `diagnose-hang` samples open interrupt, channel, and port
  handles, pauses the process briefly, and reports `dwell_ms`. It classifies
  every thread (`blocked_lock`, `blocked_ipc`, `port_wait`, `interrupt_wait`,
  `sleeping`, `active`, or `unknown`) and captures `async_tasks`. If all threads
  were active beforehand, `diagnose-hang` resumes them by default unless you
  pass `--no-auto-resume`. If any thread was already paused beforehand,
  `diagnose-hang` leaves the process paused (`auto_resumed: false`):

  ```posix-terminal
  fx debug cli diagnose-hang -p <pid>
  ```

- **Async executor task tree (`async-backtrace`, aliases `abt`, `tasks`):**
  `async-backtrace` pauses all threads in the process, queries the active async
  task tree, and leaves the process paused:

  ```posix-terminal
  fx debug cli async-backtrace -p <pid>
  ```

## Recover incomplete stack unwinds

When `stack-trace` stops early or elides runtime frames, pass unwind flags:

- `-r` (`--raw`): Expand collapsed `"subtle"` runtime frames.
- `--force-remote-unwind`: Unwind frames on the target device instead of the
  host.

```posix-terminal
fx debug cli stack-trace -p <pid> --force-remote-unwind
```

Note: Only pass `--unwinder <name>` (`cfi`, `fp`, `scs`, `plt`, `armehabi`, or
`sigreturn`) to force a specific unwinder when explicitly asked for.
