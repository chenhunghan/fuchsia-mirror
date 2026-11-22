# Fuchsia UFS drivers

This directory contains C++ (`ufs-pci`, `ufs-pdev`) and Rust (`ufs-rust-pci`,
`ufs-rust-pdev`) Fuchsia Driver Framework v2 (DFv2) drivers for Universal Flash
Storage (UFS) host controllers. Because the C++ and Rust drivers share bind
rules, do not include both packages for the same bus in a product image.

This document covers the Rust driver. For the JEDEC UFS and UFSHCI protocol,
packet formats, and command sets, see [UFS_SPECIFICATION.md][ufs-spec].
For the driver's module layout, locking rules, and power/DMA safety design,
see the module comment in `src/lib.rs`.

* [Architecture](#architecture)
* [Power states](#power-states)
* [Configuration](#configuration)
* [Building and testing](#building-and-testing)

## Architecture {:#architecture}

* [Component topology](#component-topology)
* [Source layout](#source-layout)
* [Bring-up sequence](#bring-up-sequence)
* [Threading and data path](#threading-and-data-path)
* [Host quirks](#host-quirks)

### Component topology {:#component-topology}

A single binary backs both `ufs-rust-pdev` on SoC platform devices and
`ufs-rust-pci` on PCI. Upstream, `pdev` or `pci` supplies the host controller's
UFSHCI register block, DMA bus transaction initiator (BTI), and interrupt. On
SoC boards, the controller binds alongside two additional parent nodes:

* `phy` (`fuchsia.hardware.ufs.phy.Service`): Separate platform driver for the
  SoC's MIPI M-PHY transceiver, clocks, and power rails, which live outside the
  UFSHCI register block.
* `ufs-interconnect` (`fuchsia.hardware.interconnect.PathService`, optional):
  Votes for memory-bus bandwidth between the controller and DRAM.

Downstream, the driver publishes block and Replay Protected Memory Block (RPMB)
child nodes, serves the `fuchsia.hardware.ufs` management protocol, and exports
controller, link, and IO counters through Inspect:

```none
  pdev (or pci)                phy (SoC)           ufs-interconnect (opt.)
    |                            |                           |
    | fuchsia.hardware.          | fuchsia.hardware.         | fuchsia.hardware.
    |   platform.device.Service  |   ufs.phy.Service         |   interconnect.
    | (or fuchsia.hardware.      |                           |   PathService
    |   pci.Service)             |                           |
    +----------------------------+---------------------------+
                                 |
                                 v
                 +-------------------------------+
                 | ufs-rust-pdev / ufs-rust-pci  |
                 +-------------------------------+
                   |                     |     |
fuchsia.hardware.  |   fuchsia.hardware. |     | fuchsia.hardware.
  block.volume.    |     rpmb.Service    |     |   ufs.Service
  Service          |     (when present)  |     |   (outgoing svc/)
                   v                     v     v
      scsi-block-device-0-<lun>      ufs.rpmb  (FIDL clients)
                   |
                   | devfs class "block" (fuchsia.storage.block/Block)
                   v
                 block
```

On SoC platforms, `UfsDriver::init_phy` passes a
`fuchsia.hardware.ufs.phy/Ufshci` client end to `UfsPhy.Init`, and the `phy`
driver keeps that channel for `PrePowerModeChange` to issue `DmeSet` and
`DmeGet` callbacks through the host controller during M-PHY calibration and gear
switches. Both the outgoing `UfsPhy` call and the incoming `Ufshci` stream
(`handle_ufshci_stream`) run asynchronously on `UfsDriver::scope` so they do not
deadlock without extra dispatcher threads.

### Source layout {:#source-layout}

| File | Responsibility |
| --- | --- |
| `src/lib.rs` | DFv2 driver entry, bus binding, bring-up, and child nodes. |
| `src/constants.rs` | UFS, UFSHCI, UniPro, and SCSI constants and timeouts. |
| `src/registers.rs` | Typed UFSHCI MMIO register definitions (`CAP`..`UTRL`). |
| `src/controller.rs` | Controller reset (`HCE`), `IS`, auto-hibernate, UIC. |
| `src/uic.rs` | UniPro DME command builders and register polling helpers. |
| `src/device_manager.rs` | `fDeviceInit`, gear switch, descriptors, features. |
| `src/power.rs` | `PowerControl`, `PowerGuard`, and `PowerState` transitions. |
| `src/power_worker.rs` | Serialized `PowerWorker` for system suspend and resume. |
| `src/inspect.rs` | Inspect tree (`DeviceInspect`, `DeviceFlags`, I/O stats). |
| `src/transfer_request_processor.rs` | 32-slot UTP engine, BTI pin, and ISR. |
| `src/request_list.rs` | UTRD ring (`RequestList`), UCD slots, `SlotState`. |
| `src/dma_buffer.rs` | Uncached DMA buffer (`ContiguousDmaBuffer`). |
| `src/transfer.rs` | UTP Command Descriptor layout and `PrdtEntry` builders. |
| `src/upiu.rs` | Big-endian UPIU headers and NOP, Command, Response packets. |
| `src/query.rs` | Query Request and Response UPIUs (`QueryUpiu`). |
| `src/descriptors.rs` | Zero-copy Device, Geometry, Unit, RPMB descriptors. |
| `src/scsi.rs` | SCSI CDB builders and sense parsers (`READ`, `WRITE`, etc.). |
| `src/block_device.rs` | `block_server` interface (`UfsBlockDevice`) per LUN. |
| `src/rpmb.rs` | `fuchsia.hardware.rpmb/Rpmb` server (`UfsRpmbServer`). |
| `src/server.rs` | `fuchsia.hardware.ufs/Ufs` query and SCSI buffer server. |
| `src/testing.rs` | Unit test fixtures for MMIO, interrupts, and UTP/UIC. |
| `src/testing/fake_ufs.rs` | Stateful UFSHCI, UIC, and UTP test emulator. |

### Bring-up sequence {:#bring-up-sequence}

`UfsDriver::start` brings up the controller and device in seven steps:

1. Acquire MMIO, BTI, and IRQ from `pdev` (plus `phy` and optional
   `ufs-interconnect` bandwidth) or `pci` (BAR 0, MSI-X/MSI/legacy IRQ, bus
   mastering, and PCI ID quirks).
2. Reset and enable the controller (`HCE`), run `UfsPhy.Init` if attached, and
   issue `DME_LINKSTARTUP` to bring up the UniPro link in PWM mode.
3. Program and start the 32-slot UTRL (`UTRLRSR`), enable interrupts (`IE`),
   and start the `ufs-irq-0` thread and `run_io_timeout_task`.
4. Publish Inspect, verify transport with `NOP OUT` / `NOP IN`, set
   `fDeviceInit` and poll until cleared, and write `bRefClkFreq`.
5. Read UniPro capabilities (`PA_TActivate`, `PA_Granularity`,
   `PA_MaxRxHSGear`), run `UfsPhy.PrePowerModeChange`, switch to high-speed
   `Fast_Mode` via `PA_PWRMode` (continuing in PWM mode on failure), verify
   `bCurrentPowerMode` is `Active`, and set `bActiveICCLevel`.
6. Read `bBootLunEn`, Device, Geometry (`bMaxNumberLU`), and Unit descriptors,
   enable `fBackgroundOpsEn` and WriteBooster (`fWriteBoosterEn`,
   `fWBBufferFlushDuringHibernate`), and configure auto-hibernate (`AHIT`).
7. Send `TEST UNIT READY` to each enabled LUN, add `scsi-block-device-0-<lun>`,
   `block`, and `ufs.rpmb` child nodes, and serve
   `fuchsia.hardware.ufs.Service`.

For the reverse teardown order, see `Driver::stop` in `src/lib.rs`.

### Threading and data path {:#threading-and-data-path}

```none
+-----------------------------------------------------------------------------+
| Control plane (fuchsia_async executor on UfsDriver::scope)                  |
| - Bring-up, Ufshci callbacks, power transitions, Ufs, and RPMB              |
| - Async UPIUs (admin and SCSI) and timeout task (run_io_timeout_task)       |
+-----------------------------------------------------------------------------+
| Data plane (block worker threads + dedicated interrupt thread)              |
| - ufs-lun<lun>-session and mapper threads call submit_io synchronously      |
| - Slot 0..30 reservation (Condvar) -> BTI pin -> PRDT -> UTRLDBR            |
| - ufs-irq-0 waits on zx::Interrupt and calls BlockServerCompleter::complete |
+-----------------------------------------------------------------------------+
```

Each of the 32 slots owns a pre-pinned command descriptor buffer. Only commands
on the admin slot (slot 31) time out after `command_timeout_sec`, keeping
bring-up and control commands from hanging while quarantining slot 31 and its
descriptor buffer until `UTRLDBR[31]` or `HCE` clears. Data slots 0 through 30
instead log controller and UIC error registers without aborting, and wait
indefinitely for requests to complete. See the
`src/transfer_request_processor.rs` module docs for why. Client `zx::Pmt`
handles are never unpinned on drop, and are unpinned only after hardware clears
the slot, submission fails before ringing `UTRLDBR`, or `HCE` clears to stop
DMA (otherwise leaving pages in Zircon's BTI quarantine).

### Host quirks {:#host-quirks}

* **Intel PCI**: Uses a 19.2 MHz `bRefClkFreq` and applies extra `PA_TActivate`
  margin and a post-switch `PA_Granularity` settle probe around `Fast_Mode`.
* **QEMU PCI**: Skips UniPro attribute setup, the high-speed gear switch, UFS
  power-mode setup, and auto-hibernate because the emulated controller does not
  implement them.

## Power states {:#power-states}

When power management is enabled, a serialized worker coordinates three
mutually exclusive states across the device, UniPro link, PHY, and host
controller:

* `Active`: Device, high-speed link, PHY, and controller are active with full
  interconnect bandwidth and open client IO.
* `SystemSleep`: System-suspend fallback where the device enters sleep, the
  link hibernates, and interconnect bandwidth votes drop to zero while parking
  client IO.
* `PowerDown`: Deep system suspend where the device enters power-down, the link
  hibernates, and the controller is disabled, parking client IO.

Because `PowerDown` clears volatile controller state, waking resets and
re-enables the controller, re-initializes the PHY, UniPro link, and pre-pinned
transfer ring, and clears logical unit attention before reopening client IO.

If a suspend step fails, the driver does not undo earlier steps. It keeps client
IO parked and resets the hardware on the next resume. If a hardware reset during
resume fails, the driver panics rather than hanging client IO.

## Configuration {:#configuration}

`meta/ufs-rust.shard.cml` defines the structured configuration shared by
`ufs-rust-pdev` and `ufs-rust-pci`:

* `suspend_enabled` and `storage_power_management_enabled`: `suspend_enabled`
  creates the Power Broker element; both must be true for CPU dependency
  registration and hardware suspend.
* `enable_auto_hibernate` and `auto_hibernate_timer_us`: Enable hardware
  auto-hibernate and set its idle delay in microseconds.
* `command_timeout_sec`: Timeout in seconds for admin commands and diagnostic
  logging interval for overdue data slots.

## Building and testing {:#building-and-testing}

Build the C++ and Rust UFS driver packages:

```posix-terminal
fx build //src/devices/block/drivers/ufs:ufs
```

Run the Rust unit and hardware-emulation tests, C++ unit tests, and QEMU
end-to-end tests:

```posix-terminal
fx test ufs_rust_test
fx test ufs-unit-tests
fx test qemu_ufs_test
```

Run Clippy on the Rust driver and test targets:

```posix-terminal
fx clippy //src/devices/block/drivers/ufs:ufs_rust
fx clippy //src/devices/block/drivers/ufs:ufs_rust_test
```

[ufs-spec]: UFS_SPECIFICATION.md
