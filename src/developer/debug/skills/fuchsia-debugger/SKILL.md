---
name: fuchsia-debugger
description: >
  Debugs Fuchsia target tests, live components, processes, and DFv2 driver
  hosts with fx debug cli, a turn-based zxdb interface over the Debug Adapter
  Protocol. Use when asked to debug with fx debug or zxdb, analyze runtime
  state of target components, processes, and threads, diagnose failed,
  crashed, or unresponsive target tests, investigate hung, deadlocked, or
  stalled processes and drivers, or set breakpoints, watchpoints, and
  tracepoints. Use in conjunction with crash-triage-pathfinder, ffx-log, and
  ffx-inspect for complete debugging information. Don't use for host-only
  Linux tests or for changing the zxdb codebase itself (use editing-zxdb).
---

# Fuchsia debugger

`fx debug cli` provides a stateless, turn-based interface to `zxdb` over the
Debug Adapter Protocol (DAP). A background daemon maintains the target
connection, manages the Fuchsia package server, and buffers events across turns.
Daemon commands return a single-line JSON response (`success`, `message`,
`body`, `events`) on standard output.

## Bundled references

Before you run commands for a workflow, read the matching reference guide:

| Reference | When to read |
| --- | --- |
| [Target test workflow](references/test_debugging.md) | Run `fx test` in agent debug mode, inspect test crashes, handle clean exits, or set proactive test breakpoints. |
| [Live processes and drivers](references/live_processes_and_drivers.md) | Start a standalone session, inspect live memory or handles, diagnose concurrency stalls, or attach safely to DFv2 driver hosts. |
| [Breakpoints, watchpoints, and tracepoints](references/breakpoints_watchpoints_and_tracepoints.md) | Set source or function breakpoints, configure hardware watchpoints, or sample runtime state with non-stopping tracepoints. |
| [Command and JSON protocol reference](references/command_reference.md) | Look up all 27 subcommands, aliases, `--json` request fields, and `--ack-seq` event pruning. |

## Core rules

Obey these rules during every debug session:

- **Use `fx debug cli` only.** Do not run interactive `fx debug` (without `cli`)
  or `ffx debug connect`. Interactive consoles wait for terminal input and stall
  non-interactive agents.
- **Match the daemon lifecycle.** For target tests, pass
  `--agent-debugging-mode` to `fx test` so it manages the daemon. For live
  processes or drivers, run `fx debug cli start` first and `fx debug cli stop`
  when done.
- **Prefer CLI subcommands.** Pass either `<subcommand>` or `--json '<payload>'`
  to `fx debug cli`, never both. CLI `break` and `tracepoint` resolve
  workspace-relative source paths automatically, whereas `--json` `"file"`
  requires an absolute host path.
- **Separate thread IDs from process KOIDs.** Pass sequential DAP thread IDs to
  `-t` or `"thread_id"`. Pass Zircon process KOIDs to `-p` or `"pid"`.
- **Protect transport-sensitive driver hosts.** Before you attach to a DFv2
  driver, run `fx debug cli driver-list` to inspect `transport_hazard`. Do not
  run thread-pausing commands or set breakpoints (`break`) on a driver host with
  USB, CDC-NCM, ethernet, serial, or watchdog drivers. Those commands sever
  `ffx` communication (see [Transport and watchdog
  hazards](references/live_processes_and_drivers.md#transport-and-watchdog-hazards)
  for the thread-pausing commands and non-pausing alternatives).
- **Stop after two command failures.** If a command fails twice in a row, do not
  run it again. Run `fx debug cli <subcommand> --help`, `fx debug cli schema`,
  or `fx debug cli get-state` first.

## Quick start

Choose the entry workflow that matches your target:

1.  **Target test failure or proactive test breakpoint:** Read [Target test
    workflow](references/test_debugging.md). Start `fx test` in the background.
    After compilation finishes, poll `wait-for-event`:

    ```posix-terminal
    fx test <test_suite_name> --agent-debugging-mode
    fx debug cli wait-for-event --last-seen-seq 0 --timeout 10
    ```

2.  **Live component, stalled process, or DFv2 driver:** Read [Live processes
    and drivers](references/live_processes_and_drivers.md). Start the daemon.
    Inspect driver hosts. Then attach to the target:

    ```posix-terminal
    fx debug cli start
    fx debug cli driver-list --filter <driver_substr>
    fx debug cli attach <process_name_or_koid>
    ```
