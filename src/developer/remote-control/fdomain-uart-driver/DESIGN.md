# `fdomain-uart-driver` Target Transport Bridge Design

## 1. Overview

`fdomain-uart-driver` is the target-side counterpart to the host `ffx` UART
transport daemon (`//src/developer/ffx/tools/uart_driver`). It runs on the
Fuchsia target device and bridges a physical or virtual serial port
(`/dev/class/serial`) to the Remote Control Service
(`fuchsia.developer.remotecontrol.connector/Connector`).

The wire framing, CRC-8 header and CRC-32 payload checksum verification,
handshake state machine, and sliding-window Go-Back-N (`ProtocolId::ResendSP`)
reliability layer are shared with the host via `//src/developer/lib/uart_fpl`
and documented in [`//src/developer/ffx/tools/uart_driver/DESIGN.md`][host-design].
This document describes the target component's lifecycle, task pipeline, and
target-specific architectural choices.

---

## 2. Lifecycle & Task Architecture

### 2.1 Component Startup & Session Loop

1. **Boot Argument Gate**: On startup, `run_driver` queries
   `fuchsia.boot/Arguments` for `dev.fdomain.uart` (default `false`). When
   disabled, the component parks forever (`futures::future::pending()`) with
   zero CPU or serial resource usage. When enabled, it reads
   `dev.fdomain.uart.baud` (default `1000000`).
2. **Serial Discovery**: `open_serial_device` scans `/dev/class/serial` in
   lexicographical order, selects a host-facing UART, and configures `8N1` with
   `FlowControl::None` at the target baud rate.
3. **Cold-Start Reset Notification**: On its very first session negotiation
   after component start (`is_initial_start`), `negotiate_session` transmits an
   unsolicited `FrameType::Reset` frame (`session_id = 0`) before waiting for a
   handshake. If the target rebooted while the host daemon had an active
   session open, this immediately notifies the host to abandon stale sequence
   state and initiate a fresh handshake.
4. **Handshake & Bridge Loop**: Once `run_target_handshake_with_initial_data`
   negotiates `ProtocolId::ResendSP` and records the host-chosen `session_id`,
   `run_bridge_session` runs four cooperating asynchronous tasks until the
   session resets or the serial device errors.

### 2.2 Task Pipeline & Data Flow

```text
  fuchsia.hardware.serial/Device (TX)      fuchsia.hardware.serial/Device (RX)
                 ▲                                          │
                 │                                          ▼ (SerialReader)
      ┌──────────────────────┐                   ┌──────────────────────┐
      │     writer_task      │◄── AckTracker ────│    receiver_task     │
      │  (4 KiB batching)    │ (latest-ACK slot) │   (ResendReceiver)   │
      └──────────────────────┘                   └──────────────────────┘
                 ▲                                   │              │
                 │ data_tx (bounded: 64)             │              │
      ┌──────────────────────┐                       │              │
      │     sender_task      │◄─── ack_tx (64) ──────┘              │
      │    (ResendSender)    │   (Cumulative ACKs)                  │
      └──────────────────────┘                                      │
                 ▲                                                  │
                 │ sender_tx (bounded: 64)                          │
      ┌──────────────────────┐                                      │
      │   coordinator_task   │◄──────── serial_tx (bounded: 64) ────┘
      │  (CoordinatorState)  │          (In-order DATA / CLOSE)
      └──────────────────────┘
        │        ▲         ▲
        │        │         └── internal_tx (bounded: 64)
        │        │             (Registered / RegistrationFailed / WriterDone)
        │        │
        │        └── client_tx (bounded: 64, gated by total_queued < 64)
        │
        ▼ writer_tx (bounded: 256)
  channel_writer_task / channel_reader_task (per active channel_id)
        │                     ▲
        ▼                     │
  Zircon Stream Socket (RCS Connector.FdomainToolboxSocket)
```

The crate separates low-level serial I/O (`src/serial.rs`) from the Go-Back-N
`ResendSP` state machines (`src/receiver.rs` and `src/sender.rs`):
* **Why there is a `writer_task` on TX, but a `SerialReader` struct (not a
  `reader_task`) on RX**:
  * **RX (`SerialReader` $\rightarrow$ `receiver_task`)**: `receiver_task` is
    the sole consumer of incoming serial bytes. Rather than spawning a separate
    `reader_task` connected by an extra MPSC channel, `receiver_task` borrows
    `&mut SerialReader` directly and calls `reader.next_frame().await`. When a
    session terminates (for example, on `TargetDriverError::SessionIdChanged`),
    `run_bridge_session` immediately calls `reader.take_unconsumed()` on that
    `SerialReader` to recover any trailing bytes buffered in its `FrameParser`
    for the next session's handshake.
  * **TX (`sender_task` + `AckTracker` $\rightarrow$ `writer_task`)**: Multiple
    producers emit frames to the serial device concurrently (`sender_task`
    sends data and close frames, `receiver_task` sends duplicate
    `NegotiateResp` frames via `data_tx`, and `receiver_task` signals priority
    cumulative ACKs via `AckTracker`). A dedicated `writer_task` is required to
    multiplex `AckTracker` ahead of `data_rx`, coalesce up to 4 KiB of frames
    per `Device.Write` call, and retry transient write errors.

---

## 3. Non-Obvious Architectural Choices

### 3.1 Priority ACK Coalescing via `AckTracker`

In a half-automated or full-duplex Go-Back-N link over UART, enqueueing
outgoing `FrameType::Ack` frames into the same FIFO MPSC channel (`data_tx`) as
outgoing `FrameType::Data` frames causes two problems under load:
1. **Head-of-line ACK delay**: Up to 64 outgoing data frames
   (`MAX_PAYLOAD_SIZE` each) could sit ahead of an ACK, stalling the host's
   transmit window or triggering spurious host Go-Back-N timeouts.
2. **Redundant ACK traffic**: Because `ResendSP` ACKs are cumulative, sending
   intermediate ACKs (`seq = 1`, `seq = 2`, ..., `seq = 8`) when `seq = 8` is
   already known wastes UART bandwidth.

Instead, `receiver_task` and `writer_task` share an `AckTracker`
(`//src/developer/lib/uart_fpl`):
* `AckTracker` is a single-slot latest-value register (`Option<(u32, u8)>`)
  paired with a `futures::task::AtomicWaker`. Calling
  `ack_tracker.set_ack(session_id, seq)` overwrites any unsent older ACK in
  $O(1)$ space and wakes `writer_task`.
* At the top of every iteration in `writer_task`, `ack_tracker.take_ack()` is
  checked **before** polling `data_rx`, and `futures::select_biased!` polls
  `ack_tracker.wait_ack()` ahead of `data_rx`.
* When `writer_task` does pull a frame from `data_rx`, `flush_frame_batch`
  non-blockingly drains ready frames via `data_rx.try_recv()` up to
  `WRITE_BATCH_CAPACITY` (4 KiB) into a single `Device.Write` FIDL call, then
  immediately returns to the top of the loop to check `ack_tracker` before
  flushing the next batch.

### 3.2 Two-Phase Asynchronous RCS Registration & Per-Channel Generations

When the host opens a new multiplexed channel, it does not send a separate
"channel open" handshake; it immediately sends the first `FrameType::Data`
frame carrying the new `channel_id`. On the target, connecting that channel to
RCS requires an asynchronous FIDL call (`Connector.FdomainToolboxSocket`).

* **Why registration is spawned off-loop (`ChannelState::Pending`)**: Awaiting
  `fdomain_toolbox_socket` directly inside `coordinator_task` would stall the
  entire multiplexer (blocking all other active channels and incoming serial
  frames) while RCS spawns or binds the toolbox connection. Instead,
  `open_pending_channel` spawns `spawn_channel_registration` in the background
  and places the channel in `ChannelState::Pending`, staging any subsequent
  `SerialData` frames for that `channel_id` in an in-memory buffer (capped at
  `MAX_CHANNEL_STAGING_BYTES = 2 MiB`).
* **Graceful close-before-connect draining (`closing_writers`)**: Short-lived
  channels may receive `SerialData` followed immediately by `SerialClose`
  before `fdomain_toolbox_socket` finishes registering, or while staged bytes
  are still waiting to be written to the Zircon socket. Rather than dropping
  the buffered payload on `SerialClose`, `CoordinatorState` marks the channel
  `closing: true`, flushes all staged buffers through `channel_writer_task`,
  and holds the writer task in `closing_writers` until
  `InternalEvent::WriterDone` confirms the socket has drained.
* **Generation-tagged events (`generation: u64`)**: Because registration and
  socket teardown happen asynchronously, a host could close `channel_id = K`
  and immediately reuse `channel_id = K` for a new connection while tasks from
  the previous instance are still winding down. Every channel instance is
  assigned a monotonically increasing `generation: u64`, and all
  `InternalEvent` and `ClientEvent` messages carry `(channel_id, generation)`
  so `CoordinatorState` ignores stale completions from superseded channel
  instances.

### 3.3 Wait-Free Ingress Dispatch & Non-Blocking Channel Staging

To guarantee that a single stalled or slow RCS socket cannot deadlock the UART
link or block control frames (`Ack`, `Reset`, `NegotiateReq`):
1. **`receiver_task` never `.await`s on downstream MPSC channels**: Both
   `serial_tx.try_send(event)` and `ack_tx.try_send(seq)` are synchronous. If
   `serial_tx` is full because `coordinator_task` is busy, `receiver_task`
   drops the incoming `DATA`/`CLOSE` frame **without** advancing
   `ResendReceiver` and re-asserts `receiver.current_ack_seq()` on
   `ack_tracker`. The host's Go-Back-N window naturally pauses and retransmits
   the frame once `serial_tx` drains.
2. **`coordinator_task` never `.await`s on per-channel socket writers**:
   `stage_or_send_connected` uses `sender.try_send(data)` on the per-channel
   `writer_tx` queue (`CHANNEL_WRITER_QUEUE_CAPACITY = 256`). If a specific RCS
   socket is slow and its queue fills, overflow chunks are staged in that
   channel's `pending_incoming` queue up to `MAX_CHANNEL_STAGING_BYTES`
   (2 MiB) and drained opportunistically via `drain_pending_incoming()` on
   every coordinator turn. If a runaway channel exceeds
   `MAX_CHANNEL_STAGING_BYTES`, only that channel is dropped and sent a
   `SenderMessage::Close`, isolating healthy channels.

### 3.4 Round-Robin Egress Scheduling & Backpressure Gating

* **Fair Egress Multiplexing (`select_next_message`)**: Each active channel's
  `channel_reader_task` chunks RCS socket reads into `MAX_PAYLOAD_SIZE` slices
  and sends `ClientEvent::Data` to `coordinator_task`, which queues them per
  `channel_id` in `CoordinatorState::outgoing`. When `sender_tx` has capacity
  (`poll_send_next_queued`), `select_next_message` pops one frame at a time
  across sorted `channel_id`s in round-robin order (`last_channel_id`). This
  prevents a bulk transfer channel (such as `ffx target snapshot` or `ffx log`)
  from monopolizing the Go-Back-N send window.
* **Gated `client_rx` Polling (`next_client_event`)**: When the UART link is
  congested or unacknowledged frames fill `ResendSender`'s sliding window,
  `sender_task` stops pulling from `sender_rx`. Once the total number of queued
  frames across `CoordinatorState::outgoing` reaches
  `DEFAULT_CLIENT_DATA_CAPACITY` (64), `next_client_event` returns
  `futures::future::pending()`, suspending reads from `client_rx`. This
  backpressures all `channel_reader_task` instances, which stop reading from
  their Zircon sockets and propagate kernel socket buffer backpressure directly
  to the RCS producer components.

### 3.5 Session Renegotiation, Duplicate Handshakes & Byte Carry-Over

* **Preserving Trailing Bytes (`take_unconsumed` & `requeue_frame`)**:
  `fuchsia.hardware.serial/Device.Read` returns arbitrary byte chunks from the
  driver ring buffer. A single read during handshake may contain the
  `NegotiateReq` frame followed immediately by the first `Data` frames of the
  new session. `run_target_handshake_with_initial_data` extracts any trailing
  bytes from `FrameParser::take_unconsumed()` and seeds them into
  `SerialReader::with_initial_data` so no post-handshake frames are lost.
  Conversely, if `receiver_task` encounters a `NegotiateReq` with a new
  `session_id` (`TargetDriverError::SessionIdChanged`), it puts that frame back
  at the front of `SerialReader` via `requeue_frame` before returning to
  `run_device_sessions`, allowing `negotiate_session` to process the new
  handshake immediately without waiting for the host to time out and
  retransmit.
* **Duplicate `NegotiateReq` Handling**: If the target's `NegotiateResp` frame
  is corrupted on the wire during handshake, the target enters
  `run_bridge_session` while the host times out and retransmits `NegotiateReq`
  with the **same** `session_id`. When `receiver_task` sees a `NegotiateReq`
  matching `current_session_id`, `handle_duplicate_negotiate_req` re-transmits
  `NegotiateResp` (`NEGOTIATE_RESP_SEQ = 1`) through `data_tx` without tearing
  down the active session.

### 3.6 Serial Device Discovery & Virtual UART Quirks

* **Device Class Filtering (`discover_serial_port`)**: A Fuchsia board may
  expose multiple `/dev/class/serial` nodes, including internal board UARTs
  (`Class::BluetoothHci`, `Class::KernelDebug`, `Class::Mcu`).
  `discover_serial_port` probes `GetClass()` on each sorted path, immediately
  selecting the first `Class::Generic` port, holding `Class::Console` as a
  fallback (though no current Fuchsia serial driver reports `Class::Console`),
  and ignoring internal peripheral UARTs.
* **Non-Fatal `SetConfig` Status (`open_serial_device`)**: Emulated UART
  drivers such as `uart16550` on QEMU return an error status from `SetConfig`
  if the port is already enabled or if `baud_rate > 115200`, even though the
  underlying emulated byte pipe works at any speed. `open_serial_device` logs a
  warning on non-zero `SetConfig` status rather than failing device
  initialization.
* **Bounded Write Retries (`write_frame_to_device`)**: Transient driver buffer
  saturation on `Device.Write` is retried up to `MAX_WRITE_ATTEMPTS`
  (5 attempts with `50ms` delay) before tearing down the session and reopening
  the device.

[host-design]: ../../ffx/tools/uart_driver/DESIGN.md
