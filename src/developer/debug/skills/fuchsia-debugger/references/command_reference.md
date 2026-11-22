# Command and JSON protocol reference

`fx debug cli` supports both POSIX subcommands and raw `--json` request strings.
Pass either a subcommand or `--json '<payload>'`, never both:

```posix-terminal
# Subcommand mode and --json mode validate the same protocol models
fx debug cli stack-trace -t 1
fx debug cli --json '{"command": "stack-trace", "thread_id": 1}'

# Print the full JSON schema for all request and response models locally
fx debug cli schema
```

Account for these output and path differences across commands:

- **Daemon commands:** Print a single-line JSON response (`success`, `message`,
  `body`, `events`) on `stdout`.
- **`start` and `stop`:** Run locally, print
  `{"success": <bool>, "message": "<str>"}` (to `stderr` when `success` is
  `false`), and ignore `--ack-seq`.
- **`schema`:** Runs locally in CLI mode only and prints multi-line indented
  JSON Schema on `stdout`.
- **CLI and socket errors:** Argument validation errors, missing daemon sockets,
  and socket timeouts print plain text to `stderr` and exit with a non-zero code
  (1 or 2).
- **Source file paths (`break` and `tracepoint`):** CLI mode resolves
  workspace-relative, CWD-relative, and absolute paths to absolute host paths.
  `--json` mode passes `"file"` verbatim and requires an absolute host path.

## Global fields and event history

Include `--ack-seq <seq>` (before the subcommand) or `"ack_seq": <seq>` (in any
daemon `--json` request) to prune buffered event history up to that sequence
number:

```posix-terminal
fx debug cli --ack-seq 5 get-state
fx debug cli --json '{"command": "get-state", "ack_seq": 5}'
```

Any daemon `--json` request can also include `"last_seen_seq": <seq>` to return
buffered events in `events`. The daemon keeps up to 100 buffered events and
evicts older entries automatically. If `wait-for-event` passes a `last_seen_seq`
older than pruned or evicted events, `message` reports
`"Warning: Some events were pruned from history"`.

## Identifiers and auto-pause timeouts

When you construct requests, apply these protocol rules:

- **Thread ID and process KOID:** Pass the sequential DAP `"thread_id"` (from
  `wait-for-event`, `get-state`, or `threads`) as `-t <tid>` or
  `"thread_id": <tid>`. Pass Zircon process KOIDs (from `get-state`
  `"processes"` or `driver-list` `"driver_hosts"[*]."host_koid"`) as `-p <pid>`
  or `"pid": <pid>`.
- **Auto-pause timeout and command list:** Commands that auto-pause active
  threads wait up to 10 seconds for the `"stopped"` event. For the list of
  thread-pausing, auto-resuming, and non-pausing commands, see [Transport and
  watchdog
  hazards](live_processes_and_drivers.md#transport-and-watchdog-hazards).

Note: Ignore `--start` and `--count` on `evaluate --help`. In `schema` output,
ignore the internal `hello` handshake request and the unimplemented
`single_thread`, `granularity`, and `target_id` fields on `step-in`, `next`, and
`finish` (`--single-thread` applies only to `continue`).

## Subcommand and JSON payload catalog

The following tables list all 27 subcommands by category. In CLI syntax, `-t`,
`-p`, `-c`, `-a`, `-s`, `-k`, and `-w` also accept `--thread-id`, `--pid`,
`--condition`, `--access-type`, `--size`, `--koid`, and `--watch`. On
`tracepoint`, `-e` also accepts `--expr` or `--expression` and `--max-frames`
also accepts `--max-matching-frames`. In `--json` requests, you can omit
`<opt_*>` fields to use their default or null values.

### Session, discovery, and attachment

| Subcommand (aliases) | CLI syntax | `--json` payload |
| --- | --- | --- |
| `start` | `start [--port <port>] [--connect]` | `{"command": "start", "port": <opt_int>, "connect": <opt_bool>}` |
| `stop` | `stop` | `{"command": "stop"}` |
| `schema` | `schema [--indent <n>]` | *(Local CLI subcommand only)* |
| `get-state` (`get_state`, `getState`) | `get-state` | `{"command": "get-state"}` *(Returns `threads`, `processes`, `breakpoints`, `watchpoints`, `function_breakpoints`, `tracepoints`)* |
| `threads` | `threads` | `{"command": "threads"}` |
| `driver-list` (`driver-hosts`, `driver_list`, `driverList`) | `driver-list [<filter> \| -f/--filter <filter>]` | `{"command": "driver-list", "filter": "<opt_substr>"}` |
| `attach` | `attach <filter> [--exact] [--weak] [-r/--recursive] [--job-only]` or `attach (-d/--driver <drv> \| -n/--node <moniker>) [--weak] [--wait] [-r/--recursive] [--job-only]` | `{"command": "attach", "filter": "<name>" \| <koid>, "exact": <opt_bool>, "weak": <opt_bool>, "recursive": <opt_bool>, "job_only": <opt_bool>}` or with `"driver": "<drv>"` or `"node": "<moniker>"` plus optional `"weak"`, `"wait"`, `"recursive"`, `"job_only"` |
| `detach` | `detach (<pid> \| --all)` | `{"command": "detach", "pid": <pid>}` or `{"command": "detach", "all": true}` |

### Execution control and events

| Subcommand (aliases) | CLI syntax | `--json` payload |
| --- | --- | --- |
| `wait-for-event` (`wait_for_event`, `waitForEvent`) | `wait-for-event [--last-seen-seq <seq>] [--timeout <secs>]` | `{"command": "wait-for-event", "last_seen_seq": <seq>, "timeout": <opt_secs>}` |
| `pause` | `pause (-t <tid> \| -p <pid>)` | `{"command": "pause", "thread_id": <tid>}` or `{"command": "pause", "pid": <pid>}` |
| `continue` | `continue <tid> [--single-thread]` | `{"command": "continue", "thread_id": <tid>, "single_thread": <opt_bool>}` |
| `step-in` (`step`, `s`, `step-into`, `step_in`, `stepIn`, `stepin`, `step_into`, `stepinto`) | `step-in <tid>` | `{"command": "step-in", "thread_id": <tid>}` |
| `next` (`n`, `step-over`, `step_over`, `stepOver`, `stepover`) | `next <tid>` | `{"command": "next", "thread_id": <tid>}` |
| `finish` (`step-out`, `step_out`, `stepOut`, `stepout`) | `finish <tid>` | `{"command": "finish", "thread_id": <tid>}` |

### Stack, frame, concurrency, and console inspection

| Subcommand (aliases) | CLI syntax | `--json` payload |
| --- | --- | --- |
| `stack-trace` (`bt`, `backtrace`, `stack_trace`, `stackTrace`) | `stack-trace (-t <tid> \| -p <pid>) [-r/--raw] [--force-remote-unwind] [--unwinder <name>]` | `{"command": "stack-trace", "thread_id": <tid>}` or `{"command": "stack-trace", "pid": <pid>}` with optional `"raw": <bool>`, `"force_remote_unwind": <bool>`, `"unwinder": "cfi" \| "fp" \| "scs" \| "plt" \| "armehabi" \| "sigreturn"` |
| `variables` (`locals`) | `variables -t <tid> [--frame-index <idx>]` | `{"command": "variables", "thread_id": <tid>, "frame_index": <opt_idx>}` |
| `evaluate` (`print`) | `evaluate -t <tid> [--frame-index <idx>] <expr>` | `{"command": "evaluate", "thread_id": <tid>, "frame_index": <opt_idx>, "expression": "<expr>"}` |
| `console` (`cmd`, `console-command`, `console_command`) | `console [-t <tid>] [--frame-index <idx>] [--] <cmd>` | `{"command": "console", "input": "<cmd>", "thread_id": <opt_tid>, "frame_index": <opt_idx>}` |
| `async-backtrace` (`abt`, `tasks`, `async-tasks`, `async_tasks`, `asyncTasks`, `async_backtrace`, `asyncBacktrace`) | `async-backtrace [-p <pid>]` | `{"command": "async-backtrace", "pid": <opt_pid>}` |
| `diagnose-hang` (`hang`, `hang-analyze`, `diagnose_hang`, `diagnoseHang`) | `diagnose-hang [-p <pid>] [--auto-resume \| --no-auto-resume]` | `{"command": "diagnose-hang", "pid": <opt_pid>, "auto_resume": <opt_bool>}` |

### Process modules, address space, handles, and memory

| Subcommand (aliases) | CLI syntax | `--json` payload |
| --- | --- | --- |
| `modules` (`libs`, `sym-stat`, `sym_stat`) | `modules [-p <pid>] [-f/--filter <filter>] [-r/--reload]` | `{"command": "modules", "pid": <opt_pid>, "filter": "<opt_substr>", "force_reload": <opt_bool>}` |
| `aspace` (`as`) | `aspace [<addr>] [-p <pid>] [-f/--filter <filter>]` | `{"command": "aspace", "pid": <opt_pid>, "address": "<opt_addr>", "filter": "<opt_substr>"}` |
| `handles` (`handle`) | `handles [-p <pid>] [--type <type>] [-k <koid>] [--detail]` | `{"command": "handles", "pid": <opt_pid>, "type": "<opt_type>", "koid": <opt_koid>, "detail": <opt_bool>, "hex": <opt_bool>}` |
| `mem-read` (`x`, `mem_read`, `memRead`) | `mem-read <addr_or_expr> [-s <bytes>] [-p <pid>] [-t <tid> [--frame-index <idx>]]` | `{"command": "mem-read", "address": "<addr_or_expr>", "size": <opt_bytes>, "pid": <opt_pid>, "thread_id": <opt_tid>, "frame_index": <opt_idx>}` |

### Breakpoints, watchpoints, and tracepoints

| Subcommand (aliases) | CLI syntax | `--json` payload |
| --- | --- | --- |
| `break` (`b`, `breakpoint`, `set-breakpoints`, `set_breakpoints`, `setBreakpoints`) | `break (<file>:<line> \| --file <f> --line <n> \| -f/--function <sym>) [-c <expr>] [-d/--delete]` | `{"command": "break", "file": "<abs_host_path>", "line": <n>, "condition": "<opt_expr>", "delete": <opt_bool>}` or `{"command": "break", "function": "<sym>", "condition": "<opt_expr>", "delete": <opt_bool>}` |
| `watch` (`watchpoint`, `data-breakpoint`, `set-data-breakpoints`, `set_data_breakpoints`, `setDataBreakpoints`) | `watch -t <tid> -a <write\|readWrite> [--frame-index <idx>] [-c <expr>] [--hit-condition <expr>] <expr>` or `watch (-d/--delete <data_id> \| --clear)` | `{"command": "watch", "name": "<expr>", "thread_id": <tid>, "frame_index": <opt_idx>, "access_type": "write" \| "readWrite", "condition": "<opt_expr>", "hit_condition": "<opt_expr>"}` or `{"command": "watch", "name": "<data_id>", "delete": true}` or `{"command": "watch", "clear": true}` |
| `tracepoint` (`tp`, `trace`) | `tracepoint (<file>:<line> \| --file <f> --line <n> \| <sym> \| -f/--function <sym> \| -w <expr> [-t <tid> [--frame-index <idx>]] [-a <write\|readWrite>] [--size <bytes>]) [--id <id>] [-c <expr>] [-e <expr>] [--locals] [--registers] [--backtrace] [-r/--raw-backtrace] [--max-frames <n>] [--max-hits <n>]` or `tracepoint (--list \| --clear \| (<id> \| --id <id>) (--enable \| --disable \| -d/--delete) \| <target> -d/--delete)` | Target: `{"command": "tracepoint", "file": "<abs_host_path>", "line": <n>}` or `{"command": "tracepoint", "function": "<sym>"}` or `{"command": "tracepoint", "watch": "<expr>", "thread_id": <opt_tid>, "frame_index": <opt_idx>, "size": <opt_bytes>, "access_type": "write" \| "readWrite"}`, plus optional `"id": <int>`, `"condition": "<expr>"`, `"expressions": ["<expr>"]`, `"capture": ["frame", "backtrace", "locals", "registers"]`, `"raw_backtrace": <bool>`, `"max_matching_frames": <int>`, `"max_hits": <int>`. Manage: `{"command": "tracepoint", "list_all": true}`, `{"command": "tracepoint", "clear": true}`, or `{"command": "tracepoint", "tracepoint_id": <id>, "enabled": <bool> \| "delete": true}` |
