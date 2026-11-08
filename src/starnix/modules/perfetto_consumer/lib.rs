// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]

use crate::blob_sink::TraceBufferSink;
use crate::packet_forwarder::{MonotonicClock, PacketForwarder, forward_slices};
use anyhow::{Context as _, anyhow, bail};
use fuchsia_trace::{BufferingMode, ProlongedContext, TraceState, category_enabled, trace_state};
use fuchsia_trace_observer::TraceObserver;
use futures::{SinkExt, StreamExt};
use perfetto_protos::perfetto::protos::trace_config::buffer_config::FillPolicy;
use perfetto_protos::perfetto::protos::trace_config::{BufferConfig, DataSource};
use perfetto_protos::perfetto::protos::{
    DataSourceConfig, DisableTracingRequest, EnableTracingRequest, FlushRequest,
    FreeBuffersRequest, FtraceConfig, IpcFrame, ReadBuffersRequest, TraceConfig, ipc_frame,
};
use perfetto_trace_protos::perfetto::protos::frame_timeline_event::{
    ActualDisplayFrameStart, ActualSurfaceFrameStart, Event, ExpectedDisplayFrameStart,
    ExpectedSurfaceFrameStart,
};
use perfetto_trace_protos::perfetto::protos::ftrace_event::Event::Print;
use perfetto_trace_protos::perfetto::protos::trace_packet;
use starnix_core::security;
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_core::task::tracing::PidKoidSession;
use starnix_core::task::{ArgNameAndValue, CurrentTask, Kernel};
use starnix_core::vfs::FsString;
use starnix_logging::{CATEGORY_ATRACE, log_debug, log_error, log_info, log_warn};
use starnix_perfetto_trace_decoder::{
    decode_enable_tracing_response, decode_read_buffers_response, decode_trace, encode_trace,
};

use starnix_uapi::errors::Errno;

mod atrace;
mod blob_sink;
mod packet_forwarder;
#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod testing;

const PERFETTO_BUFFER_SIZE_KB: u32 = 63488;

/// How long a stop drain may keep writing Perfetto data to the trace buffer, measured from the
/// start of the drain.
///
/// The drain holds a prolonged trace context. trace-engine can't finish stopping while that
/// context is held, so trace_manager has to wait for the drain. trace_manager gives up on a
/// stopping session after 60 s (`kStopTimeout` in trace_manager.cc). The drain stops writing
/// after half of that, which leaves the other half for the rest of the drain and for
/// trace-engine to finish stopping.
///
/// The drain checks the deadline before each slice of the ReadBuffers response, before each
/// packet, and while it waits for buffer space. It can't interrupt a blocked read from the
/// Perfetto consumer socket, so a read that blocks past the deadline delays the check. Once
/// the drain has stopped writing, it no longer copies, frames or rewrites the remaining
/// packets, but it still holds the prolonged context while it reads the rest of the
/// ReadBuffers response, up to the size of the Perfetto buffer, and then sends FreeBuffers
/// and drains its reply. That part isn't bounded, which is a known limitation: the context is
/// released only once the FreeBuffers reply has been drained.
// TODO(b/571694521): Release the context once the drain stops writing (fxr/1881111).
const DRAIN_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(30);

/// `FlushFlags(FlushFlags::Initiator::kConsumerSdk, FlushFlags::Reason::kTraceStop)` from
/// Perfetto's `include/perfetto/ext/tracing/core/flush_flags.h` (`kConsumerSdk = 3 << 4`,
/// `kTraceStop = 2`).
const FLUSH_FLAGS_TRACE_STOP: u64 = (3 << 4) | 2;

/// Timeout in milliseconds for the pre-stop `Flush` request (`kDefaultFlushTimeoutMs` in
/// Perfetto's `tracing_service_impl.cc`).
const FLUSH_TIMEOUT_MS: u32 = 5000;

/// Returns `true` if `frame` replies to the request with ID `request_id`. A `None` ID matches no
/// frame, including frames that carry no request ID.
fn is_reply_to(frame: &IpcFrame, request_id: Option<u64>) -> bool {
    request_id.is_some_and(|id| frame.request_id == Some(id))
}

/// Checks an `EnableTracingResponse` IPC frame and returns `true` if traced rejected the request,
/// the reply could not be decoded, or the reply reported a non-empty error.
///
/// traced rejects the deferred reply (`success: false`, no reply proto) when, for example, a later
/// `EnableTracing` on the same connection replaces it. Perfetto's consumer client treats both a
/// rejection and a failed decode as errors (`ClientImpl::OnInvokeMethodReply`,
/// `ConsumerIPCClientImpl::OnEnableTracingResponse`).
fn check_enable_tracing_reply(frame: &IpcFrame) -> bool {
    let Some(ipc_frame::Msg::MsgInvokeMethodReply(reply)) = &frame.msg else {
        return false;
    };
    if reply.success != Some(true) {
        log_warn!("Perfetto EnableTracing request rejected: {reply:?}");
        return true;
    }
    match decode_enable_tracing_response(reply.reply_proto.as_deref().unwrap_or(&[])) {
        Ok(resp) => {
            if let Some(err) = resp.error.as_deref()
                && !err.is_empty()
            {
                log_warn!("Perfetto EnableTracingResponse reported error: {err}");
                return true;
            }
        }
        Err(e) => {
            log_error!("Cannot decode Perfetto EnableTracingResponse: {e:?}");
            return true;
        }
    }
    false
}

/// Tracks the traced replies that a Stopping waits for before it reads the trace buffers.
///
/// Stopping sends a `Flush`, waits for its reply, then sends `DisableTracing` and waits for its
/// reply. traced also sends the deferred reply to this session's `EnableTracing`
/// (`OnTracingDisabled`) once tracing is disabled, and it can arrive during either wait.
/// `ReadBuffers` waits for both, so that asynchronous producers (e.g. SurfaceFlinger) have
/// committed their last events to the shared memory buffer before the buffers are read.
struct StopReplies {
    /// ID of this session's `EnableTracing` request, or `None` if no reply to it is expected.
    enable_request_id: Option<u64>,
    /// Whether the reply to `EnableTracing` has arrived, or none is expected.
    got_on_disabled: bool,
    /// Whether the reply to `DisableTracing` has arrived.
    got_disable_reply: bool,
    /// Whether the `Flush` or `EnableTracing` reply reported a failure. Records may then be
    /// missing from the trace without being counted as dropped.
    perfetto_stop_error: bool,
}

impl StopReplies {
    fn new(enable_request_id: Option<u64>) -> Self {
        Self {
            enable_request_id,
            got_on_disabled: enable_request_id.is_none(),
            got_disable_reply: false,
            perfetto_stop_error: false,
        }
    }

    /// Handles `frame` while waiting for the reply to the `Flush` request `flush_request`.
    /// Returns `true` once that reply has arrived.
    fn on_frame_before_flush_reply(&mut self, frame: &IpcFrame, flush_request: u64) -> bool {
        if frame.request_id == Some(flush_request) {
            if let Some(ipc_frame::Msg::MsgInvokeMethodReply(reply)) = &frame.msg
                && reply.success != Some(true)
            {
                log_warn!("Perfetto Flush before stop failed or timed out: {reply:?}");
                self.perfetto_stop_error = true;
            }
            return true;
        }
        if !self.on_enable_tracing_frame(frame) {
            log_debug!("Ignoring frame while waiting for FlushResponse: {frame:?}");
        }
        false
    }

    /// Handles `frame` while waiting for the reply to the `DisableTracing` request
    /// `disable_request` and the deferred `EnableTracing` reply. Returns `true` once both have
    /// arrived.
    fn on_frame_before_disable_replies(&mut self, frame: &IpcFrame, disable_request: u64) -> bool {
        if frame.request_id == Some(disable_request) {
            self.got_disable_reply = true;
        } else if !self.on_enable_tracing_frame(frame) {
            log_debug!(
                "Ignoring frame while waiting for DisableTracingResponse / \
                 EnableTracingResponse: {frame:?}"
            );
        }
        self.got_disable_reply && self.got_on_disabled
    }

    /// Handles `frame` if it replies to `EnableTracing`, and returns whether it did.
    fn on_enable_tracing_frame(&mut self, frame: &IpcFrame) -> bool {
        if !is_reply_to(frame, self.enable_request_id) {
            return false;
        }
        self.got_on_disabled = true;
        if check_enable_tracing_reply(frame) {
            self.perfetto_stop_error = true;
        }
        true
    }
}

/// Supported Android Atrace categories and their corresponding Fuchsia trace category strings.
/// Each entry is `(android_atrace_category_name, fuchsia_trace_category)`.
const ALL_ATRACE_CATEGORIES: &[(&str, &str)] = &[
    ("adb", "starnix:atrace:adb"),
    ("aidl", "starnix:atrace:aidl"),
    ("am", "starnix:atrace:am"),
    ("audio", "starnix:atrace:audio"),
    ("binder_driver", "starnix:atrace:binder_driver"),
    ("binder_lock", "starnix:atrace:binder_lock"),
    ("bionic", "starnix:atrace:bionic"),
    ("camera", "starnix:atrace:camera"),
    ("dalvik", "starnix:atrace:dalvik"),
    ("database", "starnix:atrace:database"),
    ("gfx", "starnix:atrace:gfx"),
    ("hal", "starnix:atrace:hal"),
    ("input", "starnix:atrace:input"),
    ("network", "starnix:atrace:network"),
    ("nnapi", "starnix:atrace:nnapi"),
    ("pm", "starnix:atrace:pm"),
    ("power", "starnix:atrace:power"),
    ("res", "starnix:atrace:res"),
    ("rro", "starnix:atrace:rro"),
    ("rs", "starnix:atrace:rs"),
    ("sched", "starnix:atrace:sched"),
    ("sm", "starnix:atrace:sm"),
    ("ss", "starnix:atrace:ss"),
    ("vibrator", "starnix:atrace:vibrator"),
    ("video", "starnix:atrace:video"),
    ("view", "starnix:atrace:view"),
    ("webview", "starnix:atrace:webview"),
    ("wm", "starnix:atrace:wm"),
];

/// Trace category that enables application-level tracing (`atrace_apps: ["*"]` and atrace category `"app"`).
const CATEGORY_ATRACE_APPS: &str = "starnix:atrace:apps";

/// Builds the `FtraceConfig` protobuf for Perfetto based on enabled Fuchsia trace categories.
///
/// Returns `None` if no atrace categories or app tracing are enabled.
fn build_ftrace_config<F>(mut is_category_enabled: F) -> Option<FtraceConfig>
where
    F: FnMut(&'static str) -> bool,
{
    let blanket_enabled = is_category_enabled(CATEGORY_ATRACE);
    let apps_enabled = blanket_enabled || is_category_enabled(CATEGORY_ATRACE_APPS);

    let mut atrace_categories =
        Vec::with_capacity(if blanket_enabled { ALL_ATRACE_CATEGORIES.len() + 1 } else { 0 });
    for &(atrace_name, fuchsia_cat) in ALL_ATRACE_CATEGORIES {
        if blanket_enabled || is_category_enabled(fuchsia_cat) {
            atrace_categories.push(atrace_name.to_string());
        }
    }
    if apps_enabled {
        atrace_categories.push("app".to_string());
    }

    let atrace_apps = if apps_enabled { vec!["*".to_string()] } else { Vec::new() };

    if atrace_categories.is_empty() && atrace_apps.is_empty() {
        None
    } else {
        Some(FtraceConfig {
            ftrace_events: vec!["ftrace/print".to_string()],
            buffer_size_kb: Some(2048),
            atrace_categories,
            atrace_apps,
            ..Default::default()
        })
    }
}

/// State needed to act upon trace state changes.
struct CallbackState {
    /// The previously observed trace state.
    prev_state: TraceState,
    /// Path to the Perfetto consumer socket.
    socket_path: FsString,
    /// Connection to the consumer socket, if it has been initialized. This gets initialized the
    /// first time it is needed.
    connection: Option<perfetto::Consumer>,
    /// Prolonged trace context to prevent the Fuchsia trace session from terminating while reading
    /// data from Perfetto. `TraceState::Started` stores it only once it has sent EnableTracing,
    /// and only `TraceState::Stopping` takes it, to disable Perfetto tracing and read the data.
    /// So it is `Some` exactly when Started has requested Perfetto tracing and Stopping hasn't run
    /// since. Holding it keeps trace-engine from reaching Stopped, so the session it belongs to
    /// can't end, and no other session can start, until Stopping releases it.
    prolonged_context: Option<ProlongedContext>,
    /// Request ID of the active session's `EnableTracing` IPC, taken in `TraceState::Stopping` to
    /// wait for its deferred `EnableTracingResponse` (`OnTracingDisabled`), which `traced` sends
    /// once all data sources have acknowledged `StopDataSource` (or `data_source_stop_timeout_ms`
    /// expires).
    enable_request: Option<u64>,
    /// Partial trace packet returned from Perfetto but not yet written to Fuchsia.
    packet_data: Vec<u8>,

    /// Recording session for pid/koid mappings, held while tracing is started. Dropping
    /// it (on stop) releases this client's interest in the shared manager.
    session: Option<PidKoidSession>,
}

impl CallbackState {
    /// Returns the state for a consumer that hasn't seen any trace state change yet.
    fn new(socket_path: FsString) -> Self {
        Self {
            prev_state: TraceState::Stopped,
            socket_path,
            connection: None,
            prolonged_context: None,
            enable_request: None,
            packet_data: Vec::new(),
            session: None,
        }
    }

    fn connection(
        &mut self,
        current_task: &CurrentTask,
    ) -> Result<&mut perfetto::Consumer, anyhow::Error> {
        match self.connection {
            None => {
                self.connection =
                    Some(perfetto::Consumer::new(current_task, self.socket_path.as_ref())?);
                Ok(self.connection.as_mut().unwrap())
            }
            Some(ref mut conn) => Ok(conn),
        }
    }

    /// Clears the state kept for a trace session that has ended. Leaves `prolonged_context`
    /// alone: only `TraceState::Stopping` releases it (see `prolonged_context`).
    fn handle_stopped(&mut self) {
        self.enable_request = None;
        self.packet_data.clear();
        self.session = None;
    }

    fn on_state_change(
        &mut self,
        new_state: TraceState,
        current_task: &CurrentTask,
    ) -> Result<(), anyhow::Error> {
        let prev_state = self.prev_state;
        self.prev_state = new_state;
        log_debug!(
            "Perfetto consumer state change. new_state: {new_state:?}, prev_state: {prev_state:?}"
        );
        match new_state {
            TraceState::Started => {
                if self.prolonged_context.is_some() {
                    // Perfetto tracing has already been requested for the session whose context
                    // we hold, and that session hasn't ended (see `prolonged_context`). So this
                    // Started is a repeat for that session (see
                    // `start_perfetto_consumer_thread()` for how that happens), or a stale one
                    // for a session that ended before we acquired the context. Either way there
                    // is nothing to do.
                    return Ok(());
                }
                if prev_state == TraceState::Stopping {
                    // The observer reads the current state when it is woken, so it misses the
                    // previous session's Stopped if the next session has already started by then.
                    // A stale Stopping that disabled Perfetto tracing early (see Stopping below)
                    // can also be followed by Started. Either way the pid/koid session and packet
                    // data are left over from before.
                    log_warn!(
                        "Started received in {prev_state:?} state! Cleaning up then starting."
                    );
                    self.handle_stopped();
                }
                // After Stopped, or after a Started that didn't enable Perfetto tracing (see
                // below), there is nothing to clean up.

                // `ProlongedContext::acquire()` fails only while trace-engine's context count is
                // zero. With Started reported, that means one of:
                // - The session has already ended, for example because its Stopping and Stopped
                //   states were queued behind this one.
                // - The session is still starting. `trace_engine_start()` stores Started before
                //   it makes the count nonzero, and notifies observers only after that, so a
                //   `trace_state()` read in between returns Started while the count is zero.
                // Either way another state change follows: the end of the session, or its start
                // notification (see `start_perfetto_consumer_thread()` for why neither is
                // missed). Skip Perfetto until then. `prolonged_context` stays `None`, so a
                // later Stopping skips Perfetto and a later Started tries again.
                let Some(prolonged_context) = ProlongedContext::acquire() else {
                    log_info!("No trace context for Started; not enabling Perfetto tracing yet");
                    return Ok(());
                };
                let connection = self.connection(current_task)?;
                // A fixed set of data sources that may be of interest. As demand for other sources
                // is found, add them here, and it may become worthwhile to allow this set to be
                // configurable per trace session.
                let mut data_sources = vec![
                    DataSource {
                        config: Some(DataSourceConfig {
                            name: Some("track_event".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    DataSource {
                        config: Some(DataSourceConfig {
                            name: Some("android.surfaceflinger.frame".to_string()),
                            target_buffer: Some(0),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    DataSource {
                        config: Some(DataSourceConfig {
                            name: Some("android.surfaceflinger.frametimeline".to_string()),
                            target_buffer: Some(0),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ];
                if let Some(ftrace_config) = build_ftrace_config(category_enabled) {
                    data_sources.push(DataSource {
                        config: Some(DataSourceConfig {
                            name: Some("linux.ftrace".to_string()),
                            ftrace_config: Some(ftrace_config),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                }
                let enable_request = connection.enable_tracing(
                    current_task,
                    EnableTracingRequest {
                        trace_config: Some(TraceConfig {
                            buffers: vec![BufferConfig {
                                size_kb: Some(PERFETTO_BUFFER_SIZE_KB),
                                fill_policy: Some(FillPolicy::Discard.into()),
                                ..Default::default()
                            }],
                            data_sources,
                            ..Default::default()
                        }),
                        attach_notification_only: None,
                    },
                )?;
                // EnableTracing has been sent now, so hold the context until Stopping tears
                // Perfetto tracing down. If an error above returned early, EnableTracing wasn't
                // sent and the context was dropped without being stored.
                self.enable_request = Some(enable_request);
                self.prolonged_context = Some(prolonged_context);
                // Once tracing has started, open a recording session so pid/koid
                // mappings are tracked for the duration of the trace.
                self.session = Some(current_task.kernel().trace_event_manager.open());
            }
            TraceState::Stopping => {
                // Hold the prolonged context so that trace-engine can't finish stopping while
                // we forward Perfetto data. Taking it out of `self` also drops it if we bail
                // below.
                let Some(_prolonged_context) = self.prolonged_context.take() else {
                    // Without a prolonged context Perfetto tracing isn't enabled (see
                    // `prolonged_context`), so there is no Perfetto session to disable or read.
                    if prev_state != TraceState::Started {
                        // If we receive a stop request and we don't think we're actually
                        // tracing, our local state likely desynced from the global trace state.
                        // Clean up our state and ensure we're stopped so we re-synchronize.
                        log_error!("Stopping received in {prev_state:?} state! Cleaning up.");
                        self.handle_stopped();
                    }
                    return Ok(());
                };
                // With the context held, this Stopping is for the session it belongs to, or a
                // stale one for a session that ended before we acquired the context. In the
                // stale case Perfetto tracing pauses for the current session until its own
                // Started, which is queued after this Stopping, enables it again, unless that
                // session is already stopping by then. Either way it is not left enabled.
                let start_time = zx::MonotonicInstant::get();
                let mut stop_replies = StopReplies::new(self.enable_request.take());

                let connection = self.connection(current_task)?;
                let flush_request = connection.flush(
                    current_task,
                    FlushRequest {
                        timeout_ms: Some(FLUSH_TIMEOUT_MS),
                        flags: Some(FLUSH_FLAGS_TRACE_STOP),
                    },
                )?;
                loop {
                    let frame = connection.next_frame_blocking(current_task)?;
                    if stop_replies.on_frame_before_flush_reply(&frame, flush_request) {
                        break;
                    }
                }

                let disable_request =
                    connection.disable_tracing(current_task, DisableTracingRequest {})?;
                loop {
                    let frame = connection.next_frame_blocking(current_task)?;
                    if stop_replies.on_frame_before_disable_replies(&frame, disable_request) {
                        break;
                    }
                }
                let perfetto_stop_error = stop_replies.perfetto_stop_error;

                let read_buffers_request =
                    connection.read_buffers(current_task, ReadBuffersRequest {})?;

                // The buffering mode is fixed for the whole trace session.
                let retry_on_full = {
                    // A held prolonged context keeps trace-engine's context count above zero,
                    // so this can't fail.
                    let Some(context) = fuchsia_trace::Context::acquire() else {
                        bail!("Tracing stopped despite holding prolonged context");
                    };
                    context.buffering_mode() == BufferingMode::Streaming
                };
                let mut forwarder = PacketForwarder::new(
                    TraceBufferSink,
                    MonotonicClock,
                    retry_on_full,
                    start_time + DRAIN_TIMEOUT,
                );

                // IPC responses may be spread across multiple frames, so loop until we get a
                // message that indicates it is the last one. Additionally, if there are
                // unrelated messages on the socket (e.g. leftover from a previous trace
                // session), the loop will read past and ignore them.
                //
                // The loop reads the whole response even after the forwarder has stopped:
                // skipped slices cost little, and if the consumer stops reading, traced blocks
                // on the full socket and then shuts the connection down
                // (`kDefaultIpcTxTimeoutMs` in Perfetto's ipc/host_impl.h). `self.connection`
                // would keep that dead connection, and the next session's EnableTracing would
                // fail on it, because `connection()` only reconnects when it is `None`.
                loop {
                    let frame = self.connection(current_task)?.next_frame_blocking(current_task)?;
                    if frame.request_id != Some(read_buffers_request) {
                        log_debug!(
                            "perfetto_consumer ignoring frame while looking for ReadBuffersRequest {read_buffers_request}: {frame:?}"
                        );
                        continue;
                    }
                    if let Some(ipc_frame::Msg::MsgInvokeMethodReply(reply)) = &frame.msg {
                        if let Ok(response) = decode_read_buffers_response(
                            reply.reply_proto.as_deref().unwrap_or(&[]),
                        ) {
                            // `rewrite_pids()` borrows all of `self`, so `packet_data` is moved
                            // out while the slices are forwarded.
                            let mut packet_data = std::mem::take(&mut self.packet_data);
                            forward_slices(
                                &response.slices,
                                &mut packet_data,
                                &mut forwarder,
                                |blob_data| self.rewrite_pids(&blob_data).unwrap_or(blob_data),
                            );
                            self.packet_data = packet_data;
                        } else {
                            log_error!("perfetto_consumer cannot decode protobuf from {reply:?}");
                        }
                        if reply.has_more != Some(true) {
                            break;
                        }
                    } else {
                        log_error!(
                            "perfetto_consumer ignoring non-MsgInvokeMethodReply message: {frame:?}"
                        );
                    }
                }
                // FreeBuffers comes only after the last ReadBuffers reply: traced reads the
                // buffers in several tasks, and a FreeBuffers handled in between frees them, so
                // the response would never get its last reply.
                // Drain the FreeBuffers reply so it is not left in the consumer socket buffer for
                // the next trace session.
                let connection = self.connection(current_task)?;
                let free_buffers_request = connection
                    .free_buffers(current_task, FreeBuffersRequest { buffer_ids: vec![0] })?;
                loop {
                    let frame = connection.next_frame_blocking(current_task)?;
                    if frame.request_id == Some(free_buffers_request) {
                        break;
                    }
                    log_debug!("Ignoring frame while waiting for FreeBuffersResponse: {frame:?}");
                }
                let elapsed = (zx::MonotonicInstant::get() - start_time).into_millis();
                let PacketForwarder {
                    packets_written,
                    packets_dropped,
                    records_dropped_uncounted,
                    bytes_dropped,
                    stop_reason,
                    ..
                } = forwarder;
                let uncounted_drops = records_dropped_uncounted + usize::from(perfetto_stop_error);
                if uncounted_drops > 0 {
                    if let Some(context) = fuchsia_trace::Context::acquire() {
                        context.mark_records_dropped(uncounted_drops);
                    }
                }
                if packets_dropped > 0 || stop_reason.is_some() || perfetto_stop_error {
                    log_warn!(
                        "Perfetto frames copied with drops or stop errors, dropping prolonged \
                         trace context. Processing took {elapsed} ms. Packets written: \
                         {packets_written}, dropped: {packets_dropped} ({bytes_dropped} bytes), \
                         stop reason: {stop_reason:?}, perfetto_stop_error: {perfetto_stop_error}"
                    );
                } else {
                    log_info!(
                        "Perfetto frames copied, dropping prolonged trace context. Processing took \
                         {elapsed} ms. Packets written: {packets_written}, dropped: \
                         {packets_dropped} ({bytes_dropped} bytes), stop reason: {stop_reason:?}"
                    );
                }
            }
            TraceState::Stopped => {
                if self.prolonged_context.is_some() {
                    // The session whose context we hold can't reach Stopped until Stopping
                    // releases the context, so this Stopped is stale: it is for a session that
                    // ended before we acquired the context. The held session's Stopping is still
                    // to come, so keep its context and pid/koid session until then.
                    return Ok(());
                }
                self.handle_stopped();
            }
        }
        Ok(())
    }

    fn rewrite_pids(&mut self, protobuf_blob: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut proto = decode_trace(protobuf_blob)?;
        for p in &mut proto.packet {
            if let Some(ref mut data) = p.data {
                match data {
                    trace_packet::Data::FrameTimelineEvent(frame_timeline_event) => {
                        if let Some(evt) = &mut frame_timeline_event.event {
                            // Update the linux pid to the Fuchsia pid. Each event has its own
                            // match arm since the variant data is of a different type for each event.
                            match evt {
                                Event::ExpectedDisplayFrameStart(ExpectedDisplayFrameStart {
                                    pid,
                                    ..
                                })
                                | Event::ActualDisplayFrameStart(ActualDisplayFrameStart {
                                    pid,
                                    ..
                                })
                                | Event::ExpectedSurfaceFrameStart(ExpectedSurfaceFrameStart {
                                    pid,
                                    ..
                                })
                                | Event::ActualSurfaceFrameStart(ActualSurfaceFrameStart {
                                    pid,
                                    ..
                                }) => {
                                    pid.as_mut().map(|pid| {
                                        *pid = self.map_to_koid_val(*pid);
                                    });
                                }
                                Event::FrameEnd(_frame_end) => {}
                            }
                        }
                    }
                    trace_packet::Data::FtraceEvents(ftrace_bundle) => {
                        for evt in &mut ftrace_bundle.event {
                            if let Some(ref mut pid) = evt.pid {
                                *pid = self.map_thread_to_koid_val(*pid as i32) as u32;
                            }
                            if let Some(ref mut event_data) = evt.event {
                                match event_data {
                                    Print(print) => {
                                        if let Some(ref mut data) = print.buf {
                                            *data = self.map_print_event(data)
                                        }
                                    }
                                    _ => (),
                                }
                            }
                        }
                    }
                    // No need to process other data; we only fixup data that references the pid.
                    _ => (),
                }
            }
        }
        Ok(encode_trace(&proto))
    }

    fn map_print_event(&mut self, data: &String) -> String {
        if let Some(mut event) = atrace::ATraceEvent::parse(&data) {
            match event {
                atrace::ATraceEvent::Begin { ref mut pid, .. }
                | atrace::ATraceEvent::End { ref mut pid }
                | atrace::ATraceEvent::Instant { ref mut pid, .. }
                | atrace::ATraceEvent::AsyncBegin { ref mut pid, .. }
                | atrace::ATraceEvent::AsyncEnd { ref mut pid, .. }
                | atrace::ATraceEvent::Counter { ref mut pid, .. }
                | atrace::ATraceEvent::AsyncTrackBegin { ref mut pid, .. }
                | atrace::ATraceEvent::AsyncTrackEnd { ref mut pid, .. }
                | atrace::ATraceEvent::Track { ref mut pid, .. } => {
                    *pid = self.map_to_koid_val(*pid as i32) as u64
                }
            }
            event.data()
        } else {
            data.to_string()
        }
    }

    fn map_thread_to_koid_val(&mut self, pid: i32) -> i32 {
        self.session
            .as_ref()
            .and_then(|s| s.resolve_tid_to_koid(pid))
            .map(|k| k.raw_koid() as i32)
            .unwrap_or(pid)
    }

    fn map_to_koid_val(&mut self, pid: i32) -> i32 {
        self.session
            .as_ref()
            .and_then(|s| s.resolve_pid_to_koid(pid))
            .map(|k| k.raw_koid() as i32)
            .unwrap_or(pid)
    }
}

fn start_perfetto_consumer_thread(kernel: &Kernel, socket_path: FsString) -> Result<(), Errno> {
    let (mut tx, mut rx) = futures::channel::mpsc::channel::<TraceState>(32);

    // Register the observer here, before the worker below reads `trace_state()` to find a session
    // that started earlier (for example on boot). trace-engine's observer protocol is to register
    // first and then read the state (see `trace_register_observer()` in instrumentation.h), so
    // that every state change after that read is reported. This holds because registration takes
    // trace-engine's engine mutex, which `trace_engine_start()` holds from storing Started until
    // it has made the context count nonzero and notified observers. So if a start begins after
    // registration, this observer gets its start notification. If it finished before
    // registration, the worker sees the nonzero count, and then `ProlongedContext::acquire()`
    // fails only for a session that has since ended, which this observer reports. The observer
    // clears its event before it reads the state, so if it reads Started before the count is
    // nonzero, the start notification signals it again.
    //
    // One session's Started can also be reported twice: by the worker's read and by the start
    // notification, if the start falls between registration and that read, or by two observer
    // reads, if the observer was woken earlier and reads Started before the notification comes.
    // `CallbackState::on_state_change()` ignores a Started while it holds the prolonged context.
    let observer = TraceObserver::new();

    // Listens for trace state changes and sends them to Perfetto consumer thread.
    // Unlike the perfetto thread, it won't block, so we can spawn it on the main async executor.
    kernel.kthreads.spawn_future(
        move || async move {
            while let Ok(state) = observer.on_state_changed().await {
                if let Err(e) = tx.send(state).await {
                    log_error!("perfetto-trace-observer failed to send trace state change: {:?}. Receiver dropped.", e);
                    return;
                }
            }
        },
        "perfetto-trace-observer",
    );

    // Perfetto consumer task: reads state changes from the channel and handles them.
    // This task can block, so we spawn it on a dedicated thread to not block the observer or the
    // main async executor.
    let worker_closure = async move |current_task: &CurrentTask| {
        let mut callback_state = CallbackState::new(socket_path);

        fn handle_state_change(
            callback_state: &mut CallbackState,
            current_task: &&CurrentTask,
            state: TraceState,
        ) -> Result<(), anyhow::Error> {
            let current_task = current_task;
            // TODO: https://fxbug.dev/457381697 - Revise how this kernel-internal work is security-
            // checked.
            let creds = security::creds_start_internal_operation(current_task);
            current_task
                .override_creds(creds, || callback_state.on_state_change(state, current_task))
        }

        // Check for tracing already started before we began observing.
        // This happens when tracing is started on boot.
        let mut state = trace_state();
        if trace_state() == TraceState::Started {
            const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
            // When we do boot tracing, it is possible (even likely), that starnix has started but
            // perfetto may not be ready to be connected to yet.
            // In that case poll until it has started.
            loop {
                match handle_state_change(&mut callback_state, &current_task, state) {
                    Ok(_) => break, // Success, exit loop.
                    Err(e) => {
                        if let Some(errno) = e.downcast_ref::<Errno>() {
                            if errno == &starnix_uapi::errors::ENOENT
                                || errno == &starnix_uapi::errors::ECONNREFUSED
                            {
                                log_warn!(
                                    "perfetto_consumer initial start tracing failed because perfetto socket connection not established: {e:?} retrying in 5 seconds..."
                                );
                                std::thread::sleep(RETRY_DELAY);
                                // `prev_state` stays at the failed Started, so if the session
                                // is stopping by now, the retry handles its Stopping quietly.
                                callback_state.connection = None;
                                state = trace_state();
                                continue; // Retry
                            }
                        }
                        // For any other error, log and exit loop.
                        log_error!(
                            "perfetto_consumer initial start tracing failed with error: {e:?}"
                        );
                        break;
                    }
                }
            }
        }

        while let Some(state) = rx.next().await {
            handle_state_change(&mut callback_state, &current_task, state).unwrap_or_else(|e| {
                log_error!("perfetto_consumer state change callback error: {:?}", e);
            })
        }
    };
    let worker_req = SpawnRequestBuilder::new()
        .with_debug_name("perfetto-consumer")
        .with_async_closure(worker_closure)
        .build();
    kernel.kthreads.spawner().spawn_from_request(worker_req);

    Ok(())
}

fn parse_args(args: &[&str]) -> Result<FsString, anyhow::Error> {
    let mut socket_path: Option<&str> = None;
    for &arg in args {
        match ArgNameAndValue::from(arg) {
            ArgNameAndValue { name: "socket_path", value: Some(path) } if !path.is_empty() => {
                socket_path = Some(path);
            }
            _ => bail!("invalid perfetto argument: {arg}"),
        }
    }
    let socket_path =
        socket_path.ok_or_else(|| anyhow!("missing required perfetto socket_path=<path>"))?;
    Ok(socket_path.into())
}

/// Validates `perfetto` module configuration `args`.
pub fn validate_args(args: &[&str]) -> Result<(), anyhow::Error> {
    parse_args(args).map(|_| ())
}

/// Initializes the Perfetto consumer thread from module configuration `args`.
pub fn module_init(kernel: &Kernel, args: &[&str]) -> Result<(), anyhow::Error> {
    let socket_path = parse_args(args)?;
    start_perfetto_consumer_thread(kernel, socket_path)
        .context("Failed to start perfetto consumer thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TraceSession, lock_trace_engine};
    use perfetto_protos::perfetto::protos::IpcFrame;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_core::vfs::socket::{SocketDomain, SocketType, UnixSocket};
    use starnix_uapi::open_flags::OpenFlags;
    use starnix_uapi::vfs::FdEvents;

    // The first five tests below run `CallbackState::on_state_change()` with `/nonexistent/...`
    // so every attempt to connect to Perfetto fails, exercising the skip, retry, and cleanup paths
    // without a Perfetto endpoint. `stopping_drains_deferred_enable_and_free_buffers_replies` then
    // injects a `UnixSocket` pair via `Consumer::from_file` to cover `Started` (enabling Perfetto
    // tracing and holding the prolonged context) and `Stopping` (flushing, disabling tracing and
    // waiting for the deferred `EnableTracing` reply before reading buffers, reading and freeing
    // buffers, draining the `FreeBuffers` reply, and releasing the context) end-to-end.

    #[fuchsia::test]
    async fn started_without_trace_context_skips_perfetto() {
        spawn_kernel_and_run(async |current_task| {
            let _lock = lock_trace_engine();
            // No trace session runs in this test, so `ProlongedContext::acquire()` fails as it
            // does for a session that has already ended or hasn't finished starting. The second
            // Started is a repeat that finds no context either.
            // `started_without_trace_context_tries_again_on_next_started` covers a later Started
            // that does find one. This test doesn't cover the observer registration order in
            // `start_perfetto_consumer_thread()`.
            assert!(ProlongedContext::acquire().is_none(), "a trace session is running");
            // Nothing listens on this path, so any attempt to connect to Perfetto fails.
            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            for state in [
                TraceState::Started,
                TraceState::Started,
                TraceState::Stopping,
                TraceState::Stopped,
            ] {
                if let Err(e) = callback_state.on_state_change(state, current_task) {
                    panic!("{state:?}: {e:?}");
                }
                assert_eq!(callback_state.prev_state, state);
                assert!(callback_state.connection.is_none(), "{state:?}: connected to Perfetto");
                assert!(callback_state.prolonged_context.is_none(), "{state:?}: holds a context");
                assert!(callback_state.session.is_none(), "{state:?}: opened a pid/koid session");
            }
        })
        .await;
    }

    #[fuchsia::test]
    async fn started_without_trace_context_tries_again_on_next_started() {
        spawn_kernel_and_run(async |current_task| {
            // Declared before `callback_state` so that it is dropped after it, also when a
            // failed assertion unwinds: dropping a session waits until no context is held.
            let _session;
            // Nothing listens on this path, so any attempt to connect to Perfetto fails.
            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            {
                let _lock = lock_trace_engine();
                assert!(ProlongedContext::acquire().is_none(), "a trace session is running");
                // A Started read while the session was still starting, before trace-engine made
                // the context count nonzero.
                if let Err(e) = callback_state.on_state_change(TraceState::Started, current_task) {
                    panic!("first Started: {e:?}");
                }
                assert!(
                    callback_state.prolonged_context.is_none(),
                    "first Started: holds a context"
                );
                assert!(
                    callback_state.connection.is_none(),
                    "first Started: connected to Perfetto"
                );
            }
            // `TraceSession::start()` takes `TRACE_ENGINE_LOCK` itself, so it is released above.
            // Other tests may run in between, but they don't share `callback_state`.
            _session = TraceSession::start();
            // The session's start notification. Started now acquires a context and tries to
            // enable Perfetto tracing, which fails to connect. Only that attempt can return an
            // error, so the error shows that this Started got past `ProlongedContext::acquire()`.
            assert!(
                callback_state.on_state_change(TraceState::Started, current_task).is_err(),
                "second Started didn't try to enable Perfetto tracing"
            );
            assert!(callback_state.prolonged_context.is_none(), "second Started kept the context");
        })
        .await;
    }

    #[fuchsia::test]
    async fn trace_context_is_held_until_stopping() {
        spawn_kernel_and_run(async |current_task| {
            let _session = TraceSession::start();
            // Nothing listens on this path, so any attempt to connect to Perfetto fails.
            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            // Stand in for a Started that enabled Perfetto tracing and opened a pid/koid session.
            // Enabling it for real needs a Perfetto service, which this test doesn't have.
            callback_state.prev_state = TraceState::Started;
            callback_state.prolonged_context = ProlongedContext::acquire();
            assert!(callback_state.prolonged_context.is_some(), "no trace session is running");
            callback_state.session = Some(current_task.kernel().trace_event_manager.open());
            // A repeated Started, and stale Stopped and Started states of an earlier session,
            // keep the context and the pid/koid session, and don't touch Perfetto. The test feeds
            // these states directly: the trace-engine timing that produces them (see
            // `start_perfetto_consumer_thread()`) isn't reproduced here.
            for state in
                [TraceState::Started, TraceState::Stopped, TraceState::Started, TraceState::Stopped]
            {
                if let Err(e) = callback_state.on_state_change(state, current_task) {
                    panic!("{state:?}: {e:?}");
                }
                assert!(callback_state.prolonged_context.is_some(), "{state:?}: dropped context");
                assert!(callback_state.session.is_some(), "{state:?}: dropped pid/koid session");
                assert!(callback_state.connection.is_none(), "{state:?}: connected to Perfetto");
            }
            // Stopping takes the context to disable Perfetto tracing, even after a Stopped. Here
            // it fails to connect to Perfetto, and drops the context anyway, so the trace session
            // can stop when `_session` is dropped.
            assert!(
                callback_state.on_state_change(TraceState::Stopping, current_task).is_err(),
                "Stopping didn't try to disable Perfetto tracing"
            );
            assert!(callback_state.prolonged_context.is_none(), "Stopping kept the context");
        })
        .await;
    }

    #[fuchsia::test]
    async fn started_after_stopping_cleans_up_before_starting() {
        spawn_kernel_and_run(async |current_task| {
            let _session = TraceSession::start();
            // Nothing listens on this path, so any attempt to connect to Perfetto fails.
            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            // Stand in for a previous session whose Stopped the observer missed, which left a
            // pid/koid session and a partial packet behind, and whose Stopping released the
            // context.
            callback_state.prev_state = TraceState::Stopping;
            callback_state.enable_request = Some(42);
            callback_state.packet_data = vec![0; 8];
            callback_state.session = Some(current_task.kernel().trace_event_manager.open());
            // Started clears what is left over, then acquires a context and tries to enable
            // Perfetto tracing, which fails to connect. Only that attempt can return an error,
            // so the error shows that this Started got past `ProlongedContext::acquire()`.
            assert!(
                callback_state.on_state_change(TraceState::Started, current_task).is_err(),
                "Started after Stopping didn't try to enable Perfetto tracing"
            );
            assert!(
                callback_state.enable_request.is_none(),
                "Started kept the old EnableTracing request ID"
            );
            assert!(callback_state.packet_data.is_empty(), "Started kept the old packet data");
            assert!(callback_state.session.is_none(), "Started kept the old pid/koid session");
            assert!(callback_state.prolonged_context.is_none(), "Started kept the context");
        })
        .await;
    }

    #[fuchsia::test]
    async fn started_that_fails_to_enable_perfetto_keeps_no_context() {
        spawn_kernel_and_run(async |current_task| {
            let _session = TraceSession::start();
            // Nothing listens on this path, so any attempt to connect to Perfetto fails.
            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            // Started acquires a context, then fails to connect, so Perfetto tracing isn't
            // enabled and the context must not be kept.
            assert!(
                callback_state.on_state_change(TraceState::Started, current_task).is_err(),
                "Started connected to Perfetto"
            );
            assert!(callback_state.prolonged_context.is_none(), "Started kept the context");
            assert!(callback_state.session.is_none(), "Started opened a pid/koid session");
            // So Stopping has no Perfetto session to disable and doesn't try to connect.
            if let Err(e) = callback_state.on_state_change(TraceState::Stopping, current_task) {
                panic!("Stopping: {e:?}");
            }
            assert!(callback_state.connection.is_none(), "Stopping connected to Perfetto");
        })
        .await;
    }

    #[fuchsia::test]
    async fn stopping_drains_deferred_enable_and_free_buffers_replies() {
        spawn_kernel_and_run(async |current_task| {
            let _session = TraceSession::start();
            let (client_file, server_file) = UnixSocket::new_pair(
                current_task,
                SocketDomain::Unix,
                SocketType::Stream,
                OpenFlags::RDWR,
            )
            .expect("new_pair");

            let mut server_conn = perfetto::IpcConnection::new(server_file.clone());
            let mut server_reader = perfetto::FrameReader::new(server_file);

            // Queue BindServiceReply (`request_id = 1`) before `Consumer::from_file` reads it.
            server_conn
                .write_frame(
                    IpcFrame {
                        request_id: Some(1),
                        data_for_testing: Vec::new(),
                        msg: Some(ipc_frame::Msg::MsgBindServiceReply(
                            ipc_frame::BindServiceReply {
                                success: Some(true),
                                service_id: Some(1),
                                methods: vec![
                                    ipc_frame::bind_service_reply::MethodInfo {
                                        id: Some(1),
                                        name: Some("EnableTracing".to_string()),
                                    },
                                    ipc_frame::bind_service_reply::MethodInfo {
                                        id: Some(2),
                                        name: Some("DisableTracing".to_string()),
                                    },
                                    ipc_frame::bind_service_reply::MethodInfo {
                                        id: Some(3),
                                        name: Some("ReadBuffers".to_string()),
                                    },
                                    ipc_frame::bind_service_reply::MethodInfo {
                                        id: Some(4),
                                        name: Some("FreeBuffers".to_string()),
                                    },
                                    ipc_frame::bind_service_reply::MethodInfo {
                                        id: Some(5),
                                        name: Some("Flush".to_string()),
                                    },
                                ],
                            },
                        )),
                    },
                    current_task,
                )
                .expect("write BindServiceReply");

            let consumer = perfetto::Consumer::from_file(current_task, client_file.clone())
                .expect("Consumer::from_file");
            let bind_req = server_reader.next_frame_blocking(current_task).expect("read bind");
            assert_eq!(bind_req.request_id, Some(1));

            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            callback_state.connection = Some(consumer);

            // Session 1: deferred `EnableTracing` reply arrives during the `Flush` wait, before
            // `DisableTracing` is sent.
            callback_state
                .on_state_change(TraceState::Started, current_task)
                .expect("first Started should succeed");
            assert!(callback_state.prolonged_context.is_some());
            assert_eq!(callback_state.enable_request, Some(2));
            let enable_req =
                server_reader.next_frame_blocking(current_task).expect("read first EnableTracing");
            assert_eq!(enable_req.request_id, Some(2));

            // Queue replies in the order Perfetto's `traced` emits them across session teardown
            // when tracing disables during `Flush` (interleaved with unrelated frames to exercise
            // the skip branches of all four wait loops):
            // - `request_id = 97` (unrelated frame before Flush reply)
            // - `request_id = 2` (deferred EnableTracing reply arriving during Flush wait)
            // - `request_id = 3` (Flush reply)
            // - `request_id = 98` (unrelated frame before DisableTracing reply)
            // - `request_id = 4` (DisableTracing reply)
            // - `request_id = 99` (unrelated frame before ReadBuffers reply)
            // - `request_id = 5` (ReadBuffers reply with `has_more = false`)
            // - `request_id = 100` (unrelated frame before FreeBuffers reply)
            // - `request_id = 6` (FreeBuffers reply)
            for (req_id, has_more) in [
                (97, false),
                (2, false),
                (3, false),
                (98, false),
                (4, false),
                (99, false),
                (5, false),
                (100, false),
                (6, false),
            ] {
                server_conn
                    .write_frame(
                        IpcFrame {
                            request_id: Some(req_id),
                            data_for_testing: Vec::new(),
                            msg: Some(ipc_frame::Msg::MsgInvokeMethodReply(
                                ipc_frame::InvokeMethodReply {
                                    success: Some(true),
                                    has_more: Some(has_more),
                                    reply_proto: Some(Vec::new()),
                                },
                            )),
                        },
                        current_task,
                    )
                    .expect("write reply");
            }

            callback_state
                .on_state_change(TraceState::Stopping, current_task)
                .expect("first Stopping should drain replies cleanly");
            assert!(callback_state.prolonged_context.is_none());
            assert!(callback_state.enable_request.is_none());

            // Verify all four requests (`Flush` = 3, `DisableTracing` = 4, `ReadBuffers` = 5,
            // `FreeBuffers` = 6) were sent in order and all queued replies on `client_file` were
            // consumed.
            for (expected_id, expected_method_id) in [(3, 5), (4, 2), (5, 3), (6, 4)] {
                let req = server_reader.next_frame_blocking(current_task).expect("read request");
                assert_eq!(req.request_id, Some(expected_id));
                let Some(ipc_frame::Msg::MsgInvokeMethod(invoke)) = req.msg else {
                    panic!("expected MsgInvokeMethod for request {expected_id}, got {req:?}");
                };
                assert_eq!(invoke.method_id, Some(expected_method_id));
            }
            let events = client_file.query_events(current_task).expect("query_events");
            assert!(
                !events.contains(FdEvents::POLLIN),
                "client socket still has unread frames after first Stopping: {events:?}"
            );

            callback_state
                .on_state_change(TraceState::Stopped, current_task)
                .expect("Stopped should succeed");

            // Session 2 (reusing the same consumer connection): `will_notify_on_stop` data sources
            // finish stopping after `DisableTracingResponse`, so the deferred `EnableTracing`
            // reply arrives after `DisableTracingResponse` and unblocks `ReadBuffers`.
            callback_state
                .on_state_change(TraceState::Started, current_task)
                .expect("second Started should succeed");
            assert!(callback_state.prolonged_context.is_some());
            assert_eq!(callback_state.enable_request, Some(7));
            let enable_req =
                server_reader.next_frame_blocking(current_task).expect("read second EnableTracing");
            assert_eq!(enable_req.request_id, Some(7));

            // Queue replies in the order Perfetto's `traced` emits them across session teardown
            // when `DisableTracing` resolves immediately while `OnTracingDisabled` waits for data
            // source stop acks:
            // - `request_id = 8` (Flush reply)
            // - `request_id = 9` (DisableTracing reply arriving before deferred EnableTracing)
            // - `request_id = 101` (unrelated frame before deferred EnableTracing reply)
            // - `request_id = 7` (deferred EnableTracing reply, unblocking ReadBuffers)
            // - `request_id = 10` (ReadBuffers reply with `has_more = false`)
            // - `request_id = 11` (FreeBuffers reply)
            for (req_id, has_more) in
                [(8, false), (9, false), (101, false), (7, false), (10, false), (11, false)]
            {
                server_conn
                    .write_frame(
                        IpcFrame {
                            request_id: Some(req_id),
                            data_for_testing: Vec::new(),
                            msg: Some(ipc_frame::Msg::MsgInvokeMethodReply(
                                ipc_frame::InvokeMethodReply {
                                    success: Some(true),
                                    has_more: Some(has_more),
                                    reply_proto: Some(Vec::new()),
                                },
                            )),
                        },
                        current_task,
                    )
                    .expect("write reply");
            }

            callback_state
                .on_state_change(TraceState::Stopping, current_task)
                .expect("second Stopping should wait for EnableTracing and FreeBuffers replies");
            assert!(callback_state.prolonged_context.is_none());
            assert!(callback_state.enable_request.is_none());

            // Verify all four requests (`Flush` = 8, `DisableTracing` = 9, `ReadBuffers` = 10,
            // `FreeBuffers` = 11) were sent in order and all queued replies on `client_file` were
            // consumed.
            for (expected_id, expected_method_id) in [(8, 5), (9, 2), (10, 3), (11, 4)] {
                let req = server_reader.next_frame_blocking(current_task).expect("read request");
                assert_eq!(req.request_id, Some(expected_id));
                let Some(ipc_frame::Msg::MsgInvokeMethod(invoke)) = req.msg else {
                    panic!("expected MsgInvokeMethod for request {expected_id}, got {req:?}");
                };
                assert_eq!(invoke.method_id, Some(expected_method_id));
            }
            let events = client_file.query_events(current_task).expect("query_events");
            assert!(
                !events.contains(FdEvents::POLLIN),
                "client socket still has unread frames after second Stopping: {events:?}"
            );
        })
        .await;
    }

    #[fuchsia::test]
    fn test_blanket_category_enables_all() {
        let ftrace_config = build_ftrace_config(|cat| cat == CATEGORY_ATRACE)
            .expect("expected ftrace_config to be Some");
        assert_eq!(ftrace_config.ftrace_events, vec!["ftrace/print".to_string()]);
        assert_eq!(ftrace_config.atrace_categories.len(), ALL_ATRACE_CATEGORIES.len() + 1);
        for &(atrace_name, _) in ALL_ATRACE_CATEGORIES {
            assert!(
                ftrace_config.atrace_categories.iter().any(|c| c == atrace_name),
                "missing category: {atrace_name}"
            );
        }
        assert!(ftrace_config.atrace_categories.iter().any(|c| c == "app"));
        assert_eq!(ftrace_config.atrace_apps, vec!["*".to_string()]);
    }

    #[fuchsia::test]
    fn test_blanket_and_granular_combined_no_duplicates() {
        let enabled: std::collections::HashSet<&str> =
            [CATEGORY_ATRACE, "starnix:atrace:am", "starnix:atrace:gfx", CATEGORY_ATRACE_APPS]
                .into();
        let ftrace_config = build_ftrace_config(|cat| enabled.contains(cat))
            .expect("expected ftrace_config to be Some");
        assert_eq!(ftrace_config.atrace_categories.len(), ALL_ATRACE_CATEGORIES.len() + 1);
        for &(atrace_name, _) in ALL_ATRACE_CATEGORIES {
            assert_eq!(
                ftrace_config.atrace_categories.iter().filter(|&c| c == atrace_name).count(),
                1,
                "category {atrace_name} should appear exactly once"
            );
        }
        assert_eq!(
            ftrace_config.atrace_categories.iter().filter(|&c| c == "app").count(),
            1,
            "category app should appear exactly once"
        );
        assert_eq!(ftrace_config.atrace_apps, vec!["*".to_string()]);
    }

    #[fuchsia::test]
    fn test_granular_categories_without_apps() {
        let enabled: std::collections::HashSet<&str> =
            ["starnix:atrace:am", "starnix:atrace:gfx"].into();
        let ftrace_config = build_ftrace_config(|cat| enabled.contains(cat))
            .expect("expected ftrace_config to be Some");
        assert_eq!(ftrace_config.atrace_categories, vec!["am".to_string(), "gfx".to_string()]);
        assert!(ftrace_config.atrace_apps.is_empty());
    }

    #[fuchsia::test]
    fn test_apps_category_enables_both_tag_and_apps() {
        let ftrace_config = build_ftrace_config(|cat| cat == CATEGORY_ATRACE_APPS)
            .expect("expected ftrace_config to be Some");
        assert_eq!(ftrace_config.atrace_categories, vec!["app".to_string()]);
        assert_eq!(ftrace_config.atrace_apps, vec!["*".to_string()]);
    }

    #[fuchsia::test]
    fn test_granular_categories_with_apps() {
        let enabled: std::collections::HashSet<&str> =
            ["starnix:atrace:view", CATEGORY_ATRACE_APPS].into();
        let ftrace_config = build_ftrace_config(|cat| enabled.contains(cat))
            .expect("expected ftrace_config to be Some");
        assert_eq!(ftrace_config.atrace_categories, vec!["view".to_string(), "app".to_string()]);
        assert_eq!(ftrace_config.atrace_apps, vec!["*".to_string()]);
    }

    #[fuchsia::test]
    fn test_no_categories_enabled() {
        assert!(build_ftrace_config(|_| false).is_none());
    }

    #[fuchsia::test]
    fn check_enable_tracing_reply_detects_non_empty_error() {
        let make_frame = |error: Option<&str>| {
            // Protobuf encoding of `EnableTracingResponse`:
            // - field 1 (`disabled = true`, varint): `0x08, 0x01`
            // - field 3 (`error`, length-delimited): `0x1a, len, bytes...`
            let mut reply_proto = vec![0x08, 0x01];
            if let Some(err) = error {
                reply_proto.push(0x1a);
                reply_proto.push(u8::try_from(err.len()).expect("short test error string"));
                reply_proto.extend_from_slice(err.as_bytes());
            }
            IpcFrame {
                request_id: Some(1),
                msg: Some(ipc_frame::Msg::MsgInvokeMethodReply(ipc_frame::InvokeMethodReply {
                    success: Some(true),
                    has_more: Some(false),
                    reply_proto: Some(reply_proto),
                })),
                data_for_testing: Vec::new(),
            }
        };

        assert!(!check_enable_tracing_reply(&make_frame(None)));
        assert!(!check_enable_tracing_reply(&make_frame(Some(""))));
        assert!(check_enable_tracing_reply(&make_frame(Some("Data source stop timed out"))));
    }

    #[fuchsia::test]
    fn check_enable_tracing_reply_detects_rejection() {
        // traced rejects a deferred reply with `success: false` and no reply proto.
        let make_frame = |success: Option<bool>| IpcFrame {
            request_id: Some(1),
            msg: Some(ipc_frame::Msg::MsgInvokeMethodReply(ipc_frame::InvokeMethodReply {
                success,
                has_more: Some(false),
                reply_proto: None,
            })),
            data_for_testing: Vec::new(),
        };

        assert!(check_enable_tracing_reply(&make_frame(Some(false))));
        assert!(check_enable_tracing_reply(&make_frame(None)));
        assert!(!check_enable_tracing_reply(&make_frame(Some(true))));
    }

    // `logging = false` skips `diagnostics_log` initialization in this test's child process so
    // the expected `log_error!` on malformed protobuf bytes doesn't trip the package's default
    // `WARN` log-severity bound.
    #[fuchsia::test(logging = false)]
    fn check_enable_tracing_reply_detects_decode_error() {
        // Field 3 (`error`, length-delimited) declaring 5 bytes with none following.
        let malformed_enable_reply = IpcFrame {
            request_id: Some(ENABLE_ID),
            msg: Some(ipc_frame::Msg::MsgInvokeMethodReply(ipc_frame::InvokeMethodReply {
                success: Some(true),
                has_more: Some(false),
                reply_proto: Some(vec![0x1a, 0x05]),
            })),
            data_for_testing: Vec::new(),
        };

        assert!(check_enable_tracing_reply(&malformed_enable_reply));

        let replies = run_stop_replies(
            Some(ENABLE_ID),
            &[reply_frame(Some(FLUSH_ID), true, None)],
            &[reply_frame(Some(DISABLE_ID), true, None), malformed_enable_reply],
        );
        assert!(replies.perfetto_stop_error);
    }

    #[fuchsia::test]
    fn is_reply_to_requires_matching_request_id() {
        let make_frame = |request_id: Option<u64>| IpcFrame {
            request_id,
            msg: None,
            data_for_testing: Vec::new(),
        };

        assert!(is_reply_to(&make_frame(Some(7)), Some(7)));
        assert!(!is_reply_to(&make_frame(Some(8)), Some(7)));
        assert!(!is_reply_to(&make_frame(None), Some(7)));
        // With no outstanding request, nothing matches, including frames without a request ID.
        assert!(!is_reply_to(&make_frame(None), None));
        assert!(!is_reply_to(&make_frame(Some(7)), None));
    }

    const FLUSH_ID: u64 = 10;
    const DISABLE_ID: u64 = 11;
    const ENABLE_ID: u64 = 12;

    /// An `InvokeMethodReply` frame for request `request_id`. `error` sets an
    /// `EnableTracingResponse` error string (see `check_enable_tracing_reply_detects_non_empty_error`
    /// for the encoding).
    fn reply_frame(request_id: Option<u64>, success: bool, error: Option<&str>) -> IpcFrame {
        let reply_proto = error.map(|err| {
            let mut proto = vec![0x08, 0x01, 0x1a];
            proto.push(u8::try_from(err.len()).expect("short test error string"));
            proto.extend_from_slice(err.as_bytes());
            proto
        });
        IpcFrame {
            request_id,
            msg: Some(ipc_frame::Msg::MsgInvokeMethodReply(ipc_frame::InvokeMethodReply {
                success: Some(success),
                has_more: Some(false),
                reply_proto,
            })),
            data_for_testing: Vec::new(),
        }
    }

    /// Feeds `before_flush` and then `before_disable` to a `StopReplies` the way Stopping does,
    /// and returns it. Panics if a wait ends before its last frame or doesn't end at it.
    fn run_stop_replies(
        enable_request_id: Option<u64>,
        before_flush: &[IpcFrame],
        before_disable: &[IpcFrame],
    ) -> StopReplies {
        let mut replies = StopReplies::new(enable_request_id);
        for (i, frame) in before_flush.iter().enumerate() {
            let done = replies.on_frame_before_flush_reply(frame, FLUSH_ID);
            assert_eq!(done, i + 1 == before_flush.len(), "Flush wait, frame {i}: {frame:?}");
        }
        for (i, frame) in before_disable.iter().enumerate() {
            let done = replies.on_frame_before_disable_replies(frame, DISABLE_ID);
            assert_eq!(done, i + 1 == before_disable.len(), "Disable wait, frame {i}: {frame:?}");
        }
        replies
    }

    #[fuchsia::test]
    fn stop_replies_waits_for_enable_reply_after_disable_reply() {
        let replies = run_stop_replies(
            Some(ENABLE_ID),
            &[reply_frame(Some(FLUSH_ID), true, None)],
            &[reply_frame(Some(DISABLE_ID), true, None), reply_frame(Some(ENABLE_ID), true, None)],
        );
        assert!(!replies.perfetto_stop_error);
    }

    #[fuchsia::test]
    fn stop_replies_accepts_enable_reply_before_disable_reply() {
        let replies = run_stop_replies(
            Some(ENABLE_ID),
            &[reply_frame(Some(FLUSH_ID), true, None)],
            &[reply_frame(Some(ENABLE_ID), true, None), reply_frame(Some(DISABLE_ID), true, None)],
        );
        assert!(!replies.perfetto_stop_error);
    }

    #[fuchsia::test]
    fn stop_replies_accepts_enable_reply_during_flush_wait() {
        let replies = run_stop_replies(
            Some(ENABLE_ID),
            &[reply_frame(Some(ENABLE_ID), true, None), reply_frame(Some(FLUSH_ID), true, None)],
            // The EnableTracing reply has already arrived, so the DisableTracing reply ends the
            // second wait.
            &[reply_frame(Some(DISABLE_ID), true, None)],
        );
        assert!(!replies.perfetto_stop_error);
    }

    #[fuchsia::test]
    fn stop_replies_without_enable_request_waits_only_for_disable_reply() {
        let replies = run_stop_replies(
            None,
            &[reply_frame(Some(FLUSH_ID), true, None)],
            &[reply_frame(Some(DISABLE_ID), true, None)],
        );
        assert!(!replies.perfetto_stop_error);
    }

    #[fuchsia::test]
    fn stop_replies_reports_failed_flush() {
        let replies = run_stop_replies(
            Some(ENABLE_ID),
            &[reply_frame(Some(FLUSH_ID), false, None)],
            &[reply_frame(Some(DISABLE_ID), true, None), reply_frame(Some(ENABLE_ID), true, None)],
        );
        assert!(replies.perfetto_stop_error);
    }

    #[fuchsia::test]
    fn stop_replies_reports_rejected_enable_tracing() {
        let replies = run_stop_replies(
            Some(ENABLE_ID),
            &[reply_frame(Some(FLUSH_ID), true, None)],
            &[reply_frame(Some(DISABLE_ID), true, None), reply_frame(Some(ENABLE_ID), false, None)],
        );
        assert!(replies.perfetto_stop_error);
    }

    #[fuchsia::test]
    fn stop_replies_reports_enable_tracing_error() {
        let replies = run_stop_replies(
            Some(ENABLE_ID),
            &[
                reply_frame(Some(ENABLE_ID), true, Some("Data source stop timed out")),
                reply_frame(Some(FLUSH_ID), true, None),
            ],
            &[reply_frame(Some(DISABLE_ID), true, None)],
        );
        assert!(replies.perfetto_stop_error);
    }

    #[fuchsia::test]
    fn stop_replies_ignores_unrelated_frames() {
        // Failed replies to other requests, and to no request, neither end a wait nor report an
        // error. With no EnableTracing request outstanding, a frame without a request ID isn't
        // taken for its reply.
        for enable_request_id in [Some(ENABLE_ID), None] {
            let unrelated = [reply_frame(Some(99), false, None), reply_frame(None, false, None)];
            let mut before_flush = unrelated.to_vec();
            before_flush.push(reply_frame(Some(FLUSH_ID), true, None));
            let mut before_disable = unrelated.to_vec();
            before_disable.push(reply_frame(Some(DISABLE_ID), true, None));
            if let Some(id) = enable_request_id {
                before_disable.push(reply_frame(Some(id), true, None));
            }
            let replies = run_stop_replies(enable_request_id, &before_flush, &before_disable);
            assert!(!replies.perfetto_stop_error, "enable_request_id: {enable_request_id:?}");
        }
    }
}
