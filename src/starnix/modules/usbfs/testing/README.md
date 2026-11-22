# Starnix `usbfs` Golden-Trace Testing

This directory contains the data-driven Linux golden-trace testing framework for
verifying Starnix `usbfs` behavior against real Linux (`drivers/usb/core/devio.c`
and `urb.c`).

## Layout

*   `schema.rs` (`usbfs_trace_schema`): Shared `serde`/`serde_json` schema types
    for declarative scenarios (`Scenario`, `DeviceSpec`, `Step`), recorded golden
    traces (`RecordedTrace`, `RecordedStep`, `StepOutcome`), gadget control RPCs
    (`DeviceCommand`, `DeviceResponse`), and divergence rules
    (`DivergenceManifest`).
*   `linux_ffs_daemon.rs` (`usbfs_linux_trace_exec`): Dynamic Linux `configfs` +
    `functionfs` + `dummy_hcd` gadget controller serving `DeviceCommand` over
    `127.0.0.1:28765`.
*   `linux_exec.rs` (`usbfs_linux_trace_exec`): Executes scenarios in Record mode
    (producing `RecordedTrace`) or replays golden traces in Verify mode via
    `libc` syscalls and `linux_uapi`.
*   `recorder_main.rs` (`usbfs_trace_recorder`): Guest CLI binary invoked inside
    QEMU/KVM to record scenarios into `generated/*.json`.
*   `starnix_replayer.rs`: Fast in-process trace replayer for
    `starnix-usbfs-tests` driving `UsbDeviceManager`, `UsbDevice`, and
    `MockUsbServer`.
*   `trace_generator.py`: Host utility that boots the in-tree `debian_guest`
    kernel under QEMU/KVM to record `generated/*.json`, or runs with
    `--generate-rs` during GN builds to emit `scenarios.rs`.
*   `scenario_matrix_gen.py`: Generator for systematic validation and errno
    precedence matrix scenarios in `scenarios/`.
*   `scenarios_list.json`: Ordered list of scenario names.
*   `scenarios/`: Declarative input scenario JSON files.
*   `generated/`: Checked-in golden trace JSON files recorded from real Linux.
*   `divergences.json`: Explicit allowlist of intentional Starnix vs. Linux
    differences.

## Generating Traces

To record golden trace files from the hermetic Linux guest:

```bash
python3 src/starnix/modules/usbfs/testing/trace_generator.py
```

## Adding New Tests

1.  Add a scenario JSON file to `scenarios/` (or update
    `scenario_matrix_gen.py`).
2.  Add the scenario name to `scenarios_list.json`.
3.  Run `trace_generator.py` to produce the golden JSON trace in `generated/`.
4.  Run `fx test starnix-usbfs-tests`,
    `fx test starnix_syscalls_rust_tests --test-filter "*usb*"`, and
    `fx test linux_syscalls_rust_tests --test-filter "*usb*"`.
