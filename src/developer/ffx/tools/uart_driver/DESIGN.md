# `ffx-uart-driver` Async Multiplexer & Transport Design

## 1. Overview & Design Goals

`ffx-uart-driver` bridges multiple concurrent host-side `ffx` UNIX domain socket connections (`FDomain` / RCS channels) over a single physical or virtual UART byte stream using the Fuchsia Packet Layer (`uart_fpl`) framing and sliding-window Go-Back-N protocol.

Unlike TCP or USB, a UART link has **no hardware-enforced link-layer flow control (`FlowControl::None`)** and is subject to byte drops, line noise, baud-rate mismatches, and target reboots. At the same time, the daemon must run on a single-threaded asynchronous executor (`fuchsia_async::LocalExecutor`) and satisfy five core requirements:

1. **Lossless Go-Back-N Delivery & Glitch Recovery**: Recover transparently from corrupted frames (Fletcher-16 checksum failures), dropped bytes, and out-of-order arrivals without corrupting or stalling upper-layer `FDomain` streams.
2. **End-to-End Bidirectional Backpressure**: Prevent fast target producers (e.g., `ffx log` or `ffx target snapshot` at 1,000,000 baud) from overwhelming slow host UNIX socket consumers, and prevent fast host producers from overflowing the UART transmit window-without forcefully disconnecting legitimate slow clients.
3. **Multi-Baud-Rate Operation (115,200 bps to 1,000,000+ bps)**: Scale efficiently from low-speed debug serial links (~11.5 KB/s) up to 1 Mbps+ (~100 KB/s) high-speed UART links via adaptive read/write batching and cooperative executor yielding.
4. **Software Rate-Limiting for Virtual & Non-Baud-Paced Links**: Emulate physical baud-rate line pacing (`baud / 10` bytes/sec) when communicating over links where the direct host connection does not naturally impose a physical baud-rate limitation (such as UNIX domain sockets, TCP endpoints, or PTYs). This prevents host writes from overwhelming downstream physical UART buffers or triggering hypervisor VM-exit / interrupt storms in emulated environments, while guaranteeing control ACKs are never starved.
5. **Deadlock & Starvation Freedom**: Guarantee that no combination of saturated channels, slow or disconnecting UNIX sockets, retransmission bursts, or high-rate ingress traffic can cause a cyclic task wait (deadlock) or starve the opposite direction of traffic.

---

## 2. Task Architecture & Data Flow

The driver decomposes each active UART session into four cooperating tasks on the `LocalExecutor`, plus a pair of per-client socket tasks (`client_reader_task` and `client_writer_task`):

```text
  Host UNIX Sockets (Client N)
       │                ▲
       ▼                │
client_reader_task   client_writer_task
       │                ▲
       │ client_tx      │ state.writer_tx (bounded: 256)
       ▼                │
    ┌──────────────────────┐
    │   coordinator_task   │◄─── serial_rx (bounded: 64) ───┐
    └──────────────────────┘                                │
       │                                                    │
       │ sender_tx (bounded, gated by outgoing_queue)       │
       ▼                                                    │
    ┌──────────────────────┐                                │
    │     sender_task      │◄─── incoming_event_rx (16) ────┤
    │   (ResendSender)     │     (Cumulative ACKs)          │
    └──────────────────────┘                                │
       │                                                    │
       │ data_tx (bounded: 16)                              │
       ▼                                                    │
    ┌──────────────────────┐                       ┌──────────────────────┐
    │     writer_task      │◄── AckTracker ────────│    receiver_task     │
    │   (DeviceWriter)     │   (O(1) watch slot)   │   (ResendReceiver)   │
    └──────────────────────┘                       └──────────────────────┘
       │                                                    ▲
       ▼                                                    │
  UART TX Half                                         UART RX Half
```

---

## 3. How Key Requirements Are Met

### 3.1 Go-Back-N Delivery & Wire Glitch Recovery

* **Checksum Verification & Framing**: `UartReader` feeds raw UART bytes into `FrameParser`, which synchronizes on COBS/frame boundaries and validates each frame's Fletcher-16 checksum. Corrupted frames are discarded immediately and counted in `DaemonMetrics::checksum_errors`.
* **Sequence Gap Detection (`dispatch_ordered_event`)**: When a frame is discarded due to line noise, subsequent frames in the remote sender's window arrive with `seq != expected_seq` (`FrameStatus::OutOfOrder`). `receiver_task` discards these out-of-order frames **without advancing `ResendReceiver`** and immediately updates `AckTracker` (`ack_tracker.set_ack(session_id, ack_seq)`) with the last contiguous sequence number received prior to the gap.
* **Priority ACK Transmission (`writer_task` & `AckTracker`)**: `AckTracker` is a single-slot latest-value register (`Option<(u32, u8)>`) rather than a FIFO queue. Because FPL ACKs are cumulative, overwriting an older unsent ACK with a newer `ack_seq` is always valid and takes $O(1)$ space. `writer_task` checks `ack_tracker.take_ack()` at the top of every loop iteration and prioritizes `ack_tracker.wait_ack()` in `select_biased!` ahead of `data_rx`, ensuring cumulative ACKs (and duplicate ACKs signaling a glitch) jump to the front of the wire ahead of bulk data frames.
* **Sliding-Window Retransmission (`sender_task`)**: `sender_task` maintains a single `active_timer` (`DEFAULT_RETRANSMISSION_TIMEOUT`). When an ACK advances the window (`AckOutcome::Advanced`), RTT estimates are updated and the timer is either cleared (`all_acked`) or reset. On timeout (`handle_sender_timeout`), `ResendSender` yields all unacknowledged frames in `[base, next_seq)` for Go-Back-N retransmission.

### 3.2 End-to-End Bidirectional Backpressure

* **Target-to-Host Backpressure (Lossless Flow Control at 1,000,000 Baud)**:
  * At 1,000,000 baud (~100 KB/s), a host client (`ffx log`, `ffx target snapshot`, or a paused terminal) may consume data more slowly than the UART delivers it.
  * When a client socket slows down, `client_writer_task` blocks in `writer.write_all(&batch).await`, causing that channel's `state.writer_tx` (`CLIENT_WRITER_CHANNEL_CAPACITY = 256`) to fill up.
  * In `coordinator_task`, `handle_uart_event` awaits `state.writer_tx.send(data)` rather than dropping the payload or closing the client channel. Waiting on `state.writer_tx` intentionally pauses `coordinator_task` from draining `serial_rx`.
  * Once `serial_rx` fills up, `receiver_task` hits `Err(e) if e.is_full()` on `serial_tx.try_send(event)` in `handle_in_order_event`. Crucially, `receiver_task` **drops the `DATA` frame without advancing `ResendReceiver`** and re-transmits `current_ack_seq()`.
  * This withholds window advancement from the target's `ResendSender`, causing the target's 64-frame sliding window to fill and pause transmission until the host client drains its UNIX socket-achieving lossless end-to-end flow control without dropping the client connection.
* **Host-to-Target Backpressure**:
  * In `sender_task`, `sender_rx` is only polled when `sender.can_send()` is true (the Go-Back-N window has free sequence slots).
  * When the UART TX window is full, `sender_tx` exerts backpressure on `coordinator_task`'s `outgoing_queue`.
  * Once `outgoing_queue.len() >= DEFAULT_CLIENT_DATA_CAPACITY`, `coordinator_task` disables polling `client_rx`. This causes `client_reader_task` instances to suspend on `coordinator_tx.send(ClientEvent::ChannelData)`, which stops reading from the client UNIX sockets and propagates standard OS `UnixStream` backpressure to the `ffx` client processes.

### 3.3 Scaling Across Baud Rates & Software Rate-Limiting

* **Batching at High Baud Rates (1,000,000+ baud)**:
  * `UartReader::drain_available` non-blockingly drains all immediately available kernel buffer bytes into `FrameParser` before yielding.
  * `writer_task` (`collect_data_batch`) coalesces pending frames from `data_rx` up to `MAX_UART_WRITE_BATCH_BYTES` per syscall.
  * `client_writer_task` coalesces up to 64 KB of channel chunks from `writer_rx` per UNIX socket write.
  * `receiver_task` executes `fuchsia_async::yield_now().await` after each processed frame so that when a single large UART read delivers dozens of frames in memory, `coordinator_task` and `writer_task` get scheduled on the single-threaded runtime before `serial_tx` saturates.
* **Software Rate-Limiting for Non-Baud-Paced Links**:
  * When connected directly to a physical UART character device (such as `/dev/ttyUSB0`), the hardware serial transceiver naturally paces byte transmission according to the configured baud rate.
  * However, when communicating over non-character devices-such as UNIX domain sockets (e.g., Pontis remote serial tunnels), TCP serial endpoints, or PTYs-the direct transport link imposes no physical baud-rate limitation. The host can transmit into the socket at memory speeds.
  * Without pacing, these bursts overwhelm downstream physical UART buffers on the remote bridge, or in emulated environments (such as QEMU serial sockets), trigger hypervisor VM-exit and interrupt storms that consume 100% guest CPU in the ISR, starve userspace, and cause guest lockups.
  * To prevent this, the driver automatically determines when the direct connection lacks a physical baud-rate limitation (`!is_char_device(target_path) || is_pty`) and enables software rate limiting capped at `baud / 10` bytes per second (e.g., 100 KB/s at 1 Mbaud, or 11.5 KB/s at 115,200 baud).
  * In `writer_task`, `DeviceWriter::write_frame` computes the transmission duration (`frame.len() / rate_limit`) and sleeps until `next_allowed_time` before writing.
  * Crucially, because `writer_task` checks `ack_tracker` before `data_rx`, cumulative ACKs are interleaved ahead of pending data batches, and because `sender_task` drains incoming ACKs concurrently while awaiting `data_tx` (see §3.4), rate-limiting pacing on `writer_task` cannot stall ACK processing or deadlock the Go-Back-N window.

### 3.4 Deadlock & Starvation Avoidance

To prevent cyclic wait dependencies across bounded channels on the single-threaded runtime, the tasks enforce four structural invariants:

1. **Wait-Free Frame Dispatch in `receiver_task`**:
   * `receiver_task` **never `.await`s** when dispatching received frames. Both `serial_tx.try_send(event)` (`DATA`/`CLOSE`) and `incoming_event_tx.try_send(seq)` (`ACK`) are synchronous and non-blocking.
   * Thus, `receiver_task` can never be blocked by downstream congestion in `coordinator_task` or `sender_task`; it always remains free to drain the UART RX buffer and process incoming `FrameType::Ack` and `FrameType::Reset` control frames.
2. **Concurrent ACK Draining in `sender_task` (`send_data_frame`)**:
   * When `writer_task` is rate-limited or backpressured, `data_tx` (capacity 16) fills up and `sender_task` blocks inside `send_data_frame` (especially during a Go-Back-N timeout retransmission burst of up to 64 frames).
   * `send_data_frame` uses `futures::select_biased!` to continuously poll `incoming_event_rx.next()` alongside `data_tx.send(frame)`. Incoming ACKs are processed immediately even while `data_tx` is full, and `handle_sender_timeout` checks `if sender.is_idle() { break; }` on every iteration so an ACK arriving mid-burst aborts any remaining redundant retransmissions.
3. **Concurrent Egress Draining & Fair Scheduling in `coordinator_task`**:
   * `coordinator_task` uses fair `futures::select!` across `serial_rx`, `sender_ready_fut`, and `client_rx_fut` so high-baudrate UART ingress cannot starve host-to-target egress.
   * While `handle_uart_event` awaits `state.writer_tx.send(data)` for a congested client socket, it concurrently polls `poll_send_outgoing` in a `select_biased!` loop so messages already in `outgoing_queue` continue flowing to `sender_task`.
4. **Drop-Before-Notify on Client Disconnect (`client_writer_task` / `client_reader_task`)**:
   * If a client socket disconnects (`EPIPE` / `EOF`) at the exact moment when both `state.writer_tx` (`coordinator_task` $\rightarrow$ `client_writer_task`) and `coordinator_tx` (`client_writer_task` $\rightarrow$ `coordinator_task`) are 100% full, awaiting `coordinator_tx.send(ClientEvent::ChannelClose)` while still holding `receiver` (`writer_rx`) would deadlock both tasks.
   * `client_writer_task` explicitly executes `drop(receiver); drop(writer);` (and `client_reader_task` executes `drop(reader);`) **before** awaiting `coordinator_tx.send(ClientEvent::ChannelClose)`. Dropping `receiver` immediately causes `state.writer_tx.send(data)` in `coordinator_task` to return `Err(disconnected)`, breaking the cycle.
