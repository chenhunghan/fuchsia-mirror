# Fuchsia Universal Flash Storage (UFS) Drivers

This directory contains two Fuchsia Driver Framework v2 (DFv2) drivers for
Universal Flash Storage (UFS) host controllers (JEDEC UFS 3.1/4.0, JESD220, and
UFSHCI 3.0/4.0, JESD223):

- **C++ driver** (`*.cc`, packages `ufs-pci` and `ufs-pdev`): the production
  driver enabled in products.
- **Rust driver** (`src/`, packages `ufs-rust-pci` and `ufs-rust-pdev`): built
  side by side. It shares the same bind rules (`ufs-pci-bind`, `ufs-pdev-bind`)
  as the C++ driver, so a product must never include both packages for the same
  bus.

The rest of this document describes the Rust driver. For the JEDEC UFS and
UFSHCI protocol, UPIU wire format, SCSI command set, and UniPro DME attributes,
see [UFS_SPECIFICATION.md](UFS_SPECIFICATION.md).

---

## Table of Contents

1. [Overview & Module Map](#1-overview--module-map)
2. [Hardware Bring-Up & Initialization Sequence](#2-hardware-bring-up--initialization-sequence)
3. [I/O Path & Threading Model](#3-io-path--threading-model)
4. [DMA & Timeout Safety](#4-dma--timeout-safety)
5. [Building & Testing](#5-building--testing)

---

## 1. Overview & Module Map

| Module | Single Responsibility |
|---|---|
| `src/lib.rs` | DFv2 entry point (`UfsDriver`), bus binding, bring-up, IRQ workers, and teardown. |
| `src/constants.rs` | JEDEC UFS, UFSHCI, MIPI UniPro, and SCSI constants, bitmasks, and timeouts. |
| `src/controller.rs` | Controller reset (`HCE`), `IS`, Auto-Hibernate (`AHIT`), and UIC commands. |
| `src/device_manager.rs` | `fDeviceInit`, `bRefClkFreq`, HS gear switch, descriptors, features. |
| `src/transfer_request_processor.rs` | 32-slot UTRL engine, slot pool, BTI pinning, and ISR. |
| `src/request_list.rs` | UTRD ring (`RequestList`), per-slot UCDs (`RequestSlot`), `SlotState`. |
| `src/dma_buffer.rs` | Contiguous uncached DMA buffer (`ContiguousDmaBuffer`) with volatile I/O. |
| `src/transfer.rs` | UCD layout (`CommandDescriptorLayout`) and `PrdtEntry` coalescing. |
| `src/upiu.rs` | Big-endian UPIU headers and `NOP OUT/IN`, Command, and Response UPIUs. |
| `src/query.rs` | Query Request and Response UPIUs (`QueryUpiu`) for descriptors and flags. |
| `src/descriptors.rs` | Zero-copy big-endian Device, Geometry, Unit, and RPMB descriptors. |
| `src/scsi.rs` | SCSI CDB builders/parsers (`READ`, `WRITE`, `UNMAP`, `SYNCHRONIZE_CACHE`, etc.). |
| `src/uic.rs` | MIPI UniPro DME command builders (`DME_*`) and `poll_until` timeout helpers. |
| `src/registers.rs` | Typed UFSHCI MMIO registers (`CAP`, `IS`, `IE`, `HCS`, `HCE`, `AHIT`). |
| `src/block_device.rs` | `block_server` callback interface (`UfsBlockDevice`) for block LUNs. |
| `src/rpmb.rs` | `fuchsia.hardware.rpmb/Rpmb` server (`UfsRpmbServer`) for Well-Known LUN `0xC4`. |
| `src/server.rs` | `fuchsia.hardware.ufs/Ufs` admin server (`UfsServer`) for descriptors and UIC. |
| `src/testing.rs`, `fake_ufs.rs` | Unit test harness and `FakeUfs` hardware and UTP emulator. |

### DFv2 Node Topology & PHY Handshake

A single driver binary backs both `ufs-rust-pdev` (composite with `pdev`, `phy`,
and optional `ufs-interconnect` parents) and `ufs-rust-pci` (`pci` parent):

```
Platform Device Node (UFSHC) or PCI Device Node
      |
      +---> ufs-phy Node (Platform M-PHY Driver, on SoC platforms)
      |         | serves fuchsia.hardware.ufs.phy.Service
      |         v
      +---> ufs-rust-pdev / ufs-rust-pci (src/devices/block/drivers/ufs)
                +---> scsi-block-device-0-<lun> (fuchsia.hardware.block.volume.Service)
                |         +---> block (devfs class "block")
                +---> ufs.rpmb (fuchsia.hardware.rpmb.Service)
                +---> fuchsia.hardware.ufs.Service (Outgoing Directory)
```

On SoC platforms, `UfsPhy::Init` and `PrePowerModeChange` issue `Ufshci::DmeSet`
and `DmeGet` callbacks back into the host controller over a `Ufshci` channel
passed to `Init`. Both the `UfsPhy` call and the `Ufshci` callback stream
(`handle_ufshci_stream`) run asynchronously on `UfsDriver::scope`, avoiding
synchronous FIDL deadlocks without extra dispatcher threads.

---

## 2. Hardware Bring-Up & Initialization Sequence

`UfsDriver::start` initializes the controller and storage device in 7 steps:

1. **Bus Binding**: Acquire resources from `pdev` (MMIO, BTI, IRQ, optional
   `ufs-phy`, and optional `ufs-interconnect` bandwidth vote with tag `'UFS '`)
   or fall back to `pci` (BAR 0, MSI-X/MSI/legacy IRQ, BTI, bus mastering, and
   PCI vendor/device ID quirks).
2. **Controller Enable & Link Startup**: Reset and enable the controller
   (`HCE = 1`), run `UfsPhy::Init` (if attached), and issue `DME_LINKSTARTUP`
   (`0x16`) to bring up the UniPro link in PWM mode.
3. **Queue & Interrupt Setup**: Program the 32-slot UTRL base address and set
   `UTRLRSR = 1` (`processor.init()`), enable controller interrupts (`IE`), and
   spawn the dedicated interrupt thread (`ufs-irq-0`) and command-timeout task
   (`run_io_timeout_task`).
4. **Device Initialization**: Verify UTP transport with `NOP OUT` / `NOP IN`,
   set `fDeviceInit` and poll until the device clears it, and write
   `bRefClkFreq` (`19.2 MHz` on Intel PCI hosts, `38.4 MHz` elsewhere).
5. **High-Speed Gear Switch**: Query local and peer UniPro link capabilities
   (`PA_AvailTxDataLanes`, `PA_AvailRxDataLanes`, `PA_MaxRxHSGear`), negotiate a
   symmetric HS gear (`min(local, peer)` for both `PA_TxGear` and `PA_RxGear`),
   run `UfsPhy::PrePowerModeChange`, configure HS Series B and DME timeouts, and
   switch to High-Speed `Fast_Mode` via `PA_PWRMode` (continuing in PWM mode
   with a warning if the switch fails). Verify `bCurrentPowerMode == Active`
   and set `bActiveICCLevel` to `0x0F`.
6. **Descriptors & Features**: Read the Device and Geometry descriptors (using
   `bMaxNumberLU` to bound LUN scanning to `0..8` or `0..32`), scan Unit
   Descriptors, enable background operations (`fBackgroundOpsEn`) and
   WriteBooster (`fWriteBoosterEn`, `fWBBufferFlushDuringHibernate`), and
   configure hardware Auto-Hibernate (`AHIT`) from structured configuration
   (`enable_auto_hibernate`, `auto_hibernate_timer_us`).
7. **Child Node Registration**: Issue `TEST UNIT READY` to each enabled LUN,
   register `scsi-block-device-0-<lun>` and `block` child nodes, register
   `ufs.rpmb` (`0xC4`) if present, and serve `fuchsia.hardware.ufs.Service` and
   Inspect metrics.

### Host Quirks

- **Intel PCI**: Uses a `19.2 MHz` reference clock, sets the device's
  `PA_TActivate` (`0x15a8`) to `host_t_activate + 2` before switching power
  modes, and after switching to `Fast_Mode` waits `1250 us` and probes the link
  with `DME_PEER_GET(PA_GRANULARITY)` (`0x15aa`) to settle the link and clear
  pending DME errors.
- **QEMU PCI (`1b36:0013`)**: Skips UniPro attribute setup, the High-Speed power
  mode switch, and Auto-Hibernate configuration.

---

## 3. I/O Path & Threading Model

The driver separates the synchronous block data plane from the asynchronous
control plane so block I/O never hops through the async executor:

```
+-----------------------------------------------------------------------------+
|  CONTROL PLANE (fuchsia_async Executor on UfsDriver::scope)                 |
|  - Bring-up, UfsPhy callbacks, fuchsia.hardware.ufs/Ufs, and RPMB           |
|  - Async UPIUs (execute) and I/O stall watchdog (run_io_timeout_task)       |
+-----------------------------------------------------------------------------+
|  DATA PLANE (Session & Mapper Worker Threads + Dedicated IRQ Thread)        |
|  - "ufs-lun<N>-session" and mapper threads call submit_io() synchronously   |
|  - Slot reservation (Condvar pushback) -> BTI pin -> PRDT -> UTRLDBR        |
|  - "ufs-irq-0" waits on zx::Interrupt and calls BlockServerCompleter        |
+-----------------------------------------------------------------------------+
```

- **Slot Partitioning & Pre-Pinned UCDs**:
  - Of the 32 UTRL slots, **slot 31 (`ADMIN_COMMAND_SLOT_NUMBER`)** is reserved
    for Query and `NOP OUT` UPIUs (serialized by `admin_lock`), while **slots
    `0..30`** serve concurrent block I/O, RPMB, and management SCSI commands.
  - Each `RequestSlot` owns a pre-pinned 8 KiB `ContiguousDmaBuffer` UTP Command
    Descriptor (UCD) containing the request UPIU (offset `0`), response UPIU,
    up to 256 `PrdtEntry` scatter-gather items (coalescing physically contiguous
    pages up to 256 KiB per entry), and a 64-byte inline write region at offset
    `8128` for small payloads such as `UNMAP` parameter lists.
- **Synchronous Block Submission (`submit_io`)**:
  - Block session FIFO loops (`ufs-lun<N>-session`, scheduled under role
    `fuchsia.devices.storage.ufs.worker`) and `block_server` mapper threads
    submit requests directly in `on_requests`.
  - `submit_io` validates arguments before reserving a slot (blocking on a
    `Condvar` when all 31 I/O slots are busy so client pages are not pinned
    while waiting), pins the client VMO via `zx::Bti::pin` outside
    `request_list.lock()` using stack scratch arrays, cleans/invalidates CPU
    caches, populates the UCD and UTRD, issues an MMIO write barrier, sets
    `1 << slot` in `UTRLDBR` (`0x58`), and returns.
- **Direct Interrupt Completion (`service_interrupt`)**:
  - `ufs-irq-0` blocks on `zx::Interrupt::wait()`. When `IS.UTRCS` fires, it
    reads `UTRLDBR` once, completes every `Scheduled` slot whose doorbell bit
    cleared, invalidates read caches and unpins the `zx::Pmt` outside
    `request_list.lock()`, and invokes `BlockServerCompleter::complete`
    directly with zero per-request futures or channel allocations.
- **Async Control & UIC Commands**:
  - Admin, RPMB, and bring-up commands run on the async executor via `execute`,
    receiving a copied response UPIU over a per-slot `oneshot` channel before
    the slot is recycled.
  - `UfsController` UIC methods hold the controller mutex only across individual
    MMIO accesses, hold `AHIT = 0` during DME sequences, and poll `IS.UCCS` /
    `IS.UPMS` with async timers.

---

## 4. DMA & Timeout Safety

- **Slot Lifecycle & `zx::Pmt` Pinning Contract**:
  - Every slot follows `Free -> Reserved -> Scheduled -> Free`.
  - Client `zx::Pmt` handles are never unpinned in `Drop`. They are unpinned at
    most once through explicit `unsafe` calls (`unpin_pmt` / `Deferred`) only
    when hardware is proven done with the buffer:
    1. Submission failed before ringing `UTRLDBR`.
    2. `UTRLDBR` read back with the slot's bit cleared (`0`).
    3. The host controller was disabled (`HCE == 0`).
  - If `HCE` fails to clear on quiesce or a guard is dropped without proof of
    hardware completion, the `zx::Pmt` handle is dropped without calling
    `unpin()` (and `quarantined_pmt_count` is incremented), leaving its pages
    quarantined by Zircon's BTI quarantine.
- **Timeout & Stall Handling**:
  - `run_io_timeout_task` logs in-flight synchronous and async UTP slots that
    exceed `COMMAND_TIMEOUT` (10 s) once per slot while keeping their DMA
    buffers pinned until `UTRLDBR` clears or controller quiesce disables `HCE`.
- **Shutdown Sequence (`UfsDriver::stop`)**:
  1. Close new submissions (`processor.close_submissions()`), setting
     `list.closing = true` and waking blocked reservers so new requests fail
     fast while in-flight completions continue.
  2. Terminate all per-LUN block sessions (`session_manager().terminate()`
     off-thread, bounded by the shared drain deadline before quiesce and joined
     after quiesce if a hardware slot was wedged) so no new FIFO requests are
     read.
  3. Drain remaining in-flight requests (`processor.drain_inflight`).
  4. Quiesce the transfer processor (`processor.quiesce()`): stop the UTRL
     (`UTRLRSR = 0`), disable the controller (`HCE = 0`), fail any remaining
     slots with `ZX_ERR_IO`, and unpin all UTRL, UCD, and client DMA buffers.
  5. Stop interrupt workers (`interrupt_workers.stop()`), destroying the
     `zx::Interrupt` handles and joining `ufs-irq-0`.
  6. Cancel `scope` (`scope.to_handle().cancel().await`), stopping the stall
     watchdog and FIDL/devfs servers.
  7. Release the parent bus (`release_bus`): clear the PDev interconnect
     bandwidth vote to `(0, 0)` or disable PCI bus mastering.

---

## 5. Building & Testing

Build both C++ and Rust UFS driver packages (included in `group("ufs")`):

```bash
fx build //src/devices/block/drivers/ufs:ufs
```

Run the Rust unit and `FakeUfs` hardware emulation tests:

```bash
fx test ufs_rust_tests
```

Run the C++ unit tests and QEMU end-to-end tests:

```bash
fx test ufs-unit-tests
fx test qemu_ufs_test
```

Run `clippy` and format check:

```bash
fx clippy //src/devices/block/drivers/ufs:ufs_rust //src/devices/block/drivers/ufs:ufs_rust_test
fx format-code
```
