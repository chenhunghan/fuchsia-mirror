---
name: zpo
description: >
  Builds and runs zpo, the Zircon Profile Optimizer (`zircon/skills/zpo`), to
  find out why a task on a Fuchsia device is slow from a trace (own code, IPC
  hops, hardware waits or the scheduler) and, when it is the scheduler, to
  derive deadline-profile parameters (capacity, period, affinity) for the
  board's `.profiles` file and verify them on the device with an A/B gate.
---

# zpo: Zircon Profile Optimizer

Use zpo when someone asks why a task (touch latency, a FIDL request, a driver
thread) is slow on a device, or what deadline profile a thread should have. zpo
answers from a trace, not from guesses, and only proposes profile changes when
the time is spent runnable-but-not-running.

## Build

```sh
fx set <product> --with-host //zircon/skills/zpo
fx build --host //zircon/skills/zpo
ZPO=$(fx get-build-dir)/host-tools/zpo
```

zpo needs a board file (`boards/<name>.json`) from an extension directory; see
[README.md](README.md#extensions).

## Analyze an existing trace

The trace must include `kernel:meta,kernel:sched,kernel:ipc,kernel:irq` (add
`kernel:power` for exact processing rates).

```sh
$ZPO analyze --trace t.fxt --task "events:irq:716->ipc-in:fuchsia.input.report/InputReportsReaderV2.OnInputReports" \
    [--board <name>] [--window 850us] [--json out.json] [--dump-job -1]
```

Task forms: `events:<start>-><end>` (events `irq:<n>`, `ipc:<Method>`,
`ipc-in:<Method>`, `slice:<Name>`, `slice-end:<Name>`),
`fidl:<lib/Protocol.Method>`, or `thread:<process>/<thread>` (globs).

Read the verdict first. `run` dominating means the code itself; `hw_wait` means
the hardware or protocol; `preempted`, `wake_latency` or `throttled` mean the
scheduler. `--dump-job -1` prints the slowest job's critical path segment by
segment. Quote zpo's numbers; do not recommend profile changes when the verdict
is code or hardware.

## Measure on a device

```sh
$ZPO session-init --session ~/zpo/touch --task "…" --load-mode manual --instruction "Swipe for 15s"
$ZPO measure --session ~/zpo/touch --name baseline
$ZPO profile-apply --session ~/zpo/touch --name baseline --dry-run   # the recommended proposals
$ZPO profile-set --role <role> --capacity 150us --period 850us --dry-run
$ZPO ota
$ZPO measure --session ~/zpo/touch --name candidate-1
$ZPO gate --session ~/zpo/touch --candidate candidate-1
```

Manual loads need a person at the device: say so instead of measuring without
one. `zpo ui` serves the same flow in a browser.
