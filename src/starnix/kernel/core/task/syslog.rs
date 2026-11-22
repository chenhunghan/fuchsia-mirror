// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::security;
use crate::task::dynamic_thread_spawner::SpawnRequestBuilder;
use crate::task::{
    CurrentTask, EventHandler, Kernel, ThreadLockupDetector, WaitCallback, WaitCanceler, WaitQueue,
    Waiter,
};
use crate::vfs::OutputBuffer;
use diagnostics_data::Severity;
use diagnostics_log_encoding::{
    Argument, FXT_HEADER_SIZE, Header, LOG_CONTROL_BIT, MONIKER, TRACING_FORMAT_LOG_RECORD_TYPE,
    Value,
};
use estimate_timeline::{DefaultFetcher, TimeFetcher, TimelineEstimator};
use fidl_fuchsia_diagnostics as fdiagnostics;
use fuchsia_component::client::connect_to_protocol_sync;
use fuchsia_inspect::Inspector;
use futures::FutureExt;
use smallvec::SmallVec;
use starnix_sync::{LockDepMutex, SyslogStateLock, SyslogSubscriptionLock};
use starnix_uapi::auth::CAP_SYSLOG;
use starnix_uapi::errors::{EAGAIN, Errno, errno, error};
use starnix_uapi::syslog::SyslogAction;
use starnix_uapi::vfs::FdEvents;
use std::cmp;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock, mpsc};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};

const BUFFER_SIZE: i32 = 1_049_000;

const NANOS_PER_SECOND: i64 = 1_000_000_000;
const MICROS_PER_NANOSECOND: i64 = 1_000;

#[derive(Default)]
pub struct Syslog {
    syscall_subscription: OnceLock<LockDepMutex<LogSubscription, SyslogSubscriptionLock>>,
    state: Arc<LockDepMutex<TimelineEstimator<DefaultFetcher>, SyslogStateLock>>,
}

#[derive(Debug)]
pub enum SyslogAccess {
    DevKmsgRead,
    ProcKmsg(SyslogAction),
    Syscall(SyslogAction),
}

impl Syslog {
    pub fn init(&self, kernel: &Kernel) -> Result<(), anyhow::Error> {
        let state = self.state.clone();
        kernel.inspect_node.record_lazy_child("syslog", move || {
            let state = state.clone();
            async move {
                let inspector = Inspector::default();
                let state_guard = state.lock();
                inspector.root().record_uint("max_timeline_size", state_guard.max_timeline_size());
                inspector
                    .root()
                    .record_uint("timeline_overflows", state_guard.timeline_overflows());
                Ok(inspector)
            }
            .boxed()
        });

        let subscription = LogSubscription::snapshot_then_subscribe(kernel)?;
        self.syscall_subscription.set(subscription.into()).expect("syslog inititialized once");
        Ok(())
    }

    pub fn access(
        &self,
        current_task: &CurrentTask,
        access: SyslogAccess,
    ) -> Result<GrantedSyslog<'_>, Errno> {
        Self::validate_access(current_task, access)?;
        let syscall_subscription = self.subscription()?;
        Ok(GrantedSyslog { syscall_subscription })
    }

    /// Validates that syslog access is unrestricted, or that the `current_task` has the relevant
    /// capability, and applies the SELinux policy.
    pub fn validate_access(current_task: &CurrentTask, access: SyslogAccess) -> Result<(), Errno> {
        let (action, check_capabilities) = match access {
            SyslogAccess::ProcKmsg(SyslogAction::Open) => (SyslogAction::Open, true),
            SyslogAccess::DevKmsgRead => (SyslogAction::ReadAll, true),
            SyslogAccess::Syscall(a) => (a, true),
            // If we got here we already validated Open on /proc/kmsg.
            SyslogAccess::ProcKmsg(a) => (a, false),
        };

        // According to syslog(2) man, ReadAll (3) and SizeBuffer (10) are allowed unprivileged
        // access only if restrict_dmsg is 0.
        let action_is_privileged = !matches!(
            access,
            SyslogAccess::Syscall(SyslogAction::ReadAll | SyslogAction::SizeBuffer)
                | SyslogAccess::DevKmsgRead,
        );
        let restrict_dmesg = current_task.kernel().restrict_dmesg.load(Ordering::Relaxed);
        if check_capabilities && (action_is_privileged || restrict_dmesg) {
            security::check_task_capable(current_task, CAP_SYSLOG)?;
        }

        security::check_syslog_access(current_task, action)?;
        Ok(())
    }

    pub fn snapshot_then_subscribe(kernel: &Kernel) -> Result<LogSubscription, Errno> {
        LogSubscription::snapshot_then_subscribe(kernel)
    }

    pub fn subscribe(kernel: &Kernel) -> Result<LogSubscription, Errno> {
        LogSubscription::subscribe(kernel)
    }

    fn subscription(
        &self,
    ) -> Result<&LockDepMutex<LogSubscription, SyslogSubscriptionLock>, Errno> {
        self.syscall_subscription.get().ok_or_else(|| errno!(ENOENT))
    }
}

pub struct GrantedSyslog<'a> {
    syscall_subscription: &'a LockDepMutex<LogSubscription, SyslogSubscriptionLock>,
}

impl GrantedSyslog<'_> {
    pub fn read(&self, out: &mut dyn OutputBuffer) -> Result<i32, Errno> {
        let mut subscription = self.syscall_subscription.lock();
        if let Some(log) = subscription.try_next()? {
            let size_to_write = cmp::min(log.len(), out.available() as usize);
            out.write(&log[..size_to_write])?;
            return Ok(size_to_write as i32);
        }
        Ok(0)
    }

    pub fn wait(&self, waiter: &Waiter, events: FdEvents, handler: EventHandler) -> WaitCanceler {
        self.syscall_subscription.lock().wait(waiter, events, handler)
    }

    pub fn blocking_read(
        &self,
        current_task: &CurrentTask,
        out: &mut dyn OutputBuffer,
    ) -> Result<i32, Errno> {
        let mut write_log = |log: Vec<u8>| {
            let size_to_write = cmp::min(log.len(), out.available() as usize);
            out.write(&log[..size_to_write])?;
            Ok(size_to_write as i32)
        };
        match self.syscall_subscription.lock().try_next() {
            Err(errno) if errno == EAGAIN => {}
            Err(errno) => return Err(errno),
            Ok(Some(log)) => return write_log(log),
            Ok(None) => return Ok(0),
        }
        let waiter = Waiter::new();
        loop {
            let mut subscription = self.syscall_subscription.lock();
            subscription.wait(&waiter, FdEvents::POLLIN | FdEvents::POLLHUP, WaitCallback::none());
            match subscription.try_next() {
                Err(errno) if errno == EAGAIN => {}
                Err(errno) => return Err(errno),
                Ok(Some(log)) => return write_log(log),
                Ok(None) => return Ok(0),
            }
            // Drop the subscription lock to avoid holding a lock while waiting.
            std::mem::drop(subscription);
            waiter.wait(current_task)?;
        }
    }

    pub fn read_all(&self, kernel: &Kernel, out: &mut dyn OutputBuffer) -> Result<i32, Errno> {
        let mut subscription = LogSubscription::snapshot(kernel)?;
        let mut buffer = ResultBuffer::new(out.available());
        while let Some(log_result) = subscription.next() {
            buffer.push(log_result?);
        }
        let result: Vec<u8> = buffer.into();
        out.write(result.as_slice())?;
        Ok(result.len() as i32)
    }

    pub fn size_unread(&self) -> Result<i32, Errno> {
        let mut subscription = self.syscall_subscription.lock();
        Ok(subscription.available()?.try_into().unwrap_or(i32::MAX))
    }

    pub fn size_buffer(&self) -> Result<i32, Errno> {
        // For now always return a constant for this.
        Ok(BUFFER_SIZE)
    }
}

#[derive(Debug)]
pub struct LogSubscription {
    pending: Option<Vec<u8>>,
    receiver: mpsc::Receiver<Result<Vec<u8>, Errno>>,
    waiters: Arc<WaitQueue>,
}

impl LogSubscription {
    pub fn wait(&self, waiter: &Waiter, events: FdEvents, handler: EventHandler) -> WaitCanceler {
        self.waiters.wait_async_fd_events(waiter, events, handler)
    }

    pub fn available(&mut self) -> Result<usize, Errno> {
        if let Some(log) = &self.pending {
            return Ok(log.len());
        }
        match self.try_next() {
            Err(err) if err == EAGAIN => Ok(0),
            Err(err) => Err(err),
            Ok(Some(log)) => {
                let size = log.len();
                self.pending.replace(log);
                return Ok(size);
            }
            Ok(None) => Ok(0),
        }
    }

    fn snapshot(kernel: &Kernel) -> Result<LogIterator, Errno> {
        LogIterator::new(&kernel.syslog, fdiagnostics::StreamMode::Snapshot)
    }

    fn subscribe(kernel: &Kernel) -> Result<Self, Errno> {
        Self::new_listening(kernel, fdiagnostics::StreamMode::Subscribe)
    }

    fn snapshot_then_subscribe(kernel: &Kernel) -> Result<Self, Errno> {
        Self::new_listening(kernel, fdiagnostics::StreamMode::SnapshotThenSubscribe)
    }

    fn new_listening(kernel: &Kernel, mode: fdiagnostics::StreamMode) -> Result<Self, Errno> {
        let iterator = LogIterator::new(&kernel.syslog, mode)?;
        let (snd, receiver) = mpsc::sync_channel(1);
        let waiters = Arc::new(WaitQueue::default());
        let waiters_clone = waiters.clone();
        let closure = move |_: &CurrentTask| {
            scopeguard::defer! {
                waiters_clone.notify_fd_events(FdEvents::POLLHUP);
            };
            for log in iterator {
                let send_result = {
                    let _waiting_guard = ThreadLockupDetector::pause_tracking();
                    snd.send(log)
                };
                if send_result.is_err() {
                    break;
                };
                waiters_clone.notify_fd_events(FdEvents::POLLIN);
            }
        };
        let req = SpawnRequestBuilder::new()
            .with_debug_name("syslog-listener")
            .with_sync_closure(closure)
            .build();
        kernel.kthreads.spawner().spawn_from_request(req);

        Ok(Self { receiver, waiters, pending: Default::default() })
    }

    fn try_next(&mut self) -> Result<Option<Vec<u8>>, Errno> {
        if let Some(value) = self.pending.take() {
            return Ok(Some(value));
        }
        match self.receiver.try_recv() {
            // We got the next log.
            Ok(Ok(log)) => Ok(Some(log)),
            // An error happened attempting to get the next log.
            Ok(Err(err)) => Err(err),
            // The channel was closed and there's no more messages in the queue.
            Err(mpsc::TryRecvError::Disconnected) => Ok(None),
            // No messages available but the channel hasn't closed.
            Err(mpsc::TryRecvError::Empty) => error!(EAGAIN),
        }
    }
}

struct LogIterator {
    iterator: fdiagnostics::BatchIteratorSynchronousProxy,
    pending_formatted_contents: VecDeque<fdiagnostics::FormattedContent>,
    vmo_buf: Vec<u8>,
    vmo_offset: usize,
    vmo_len: usize,
    formatted_buf: Vec<u8>,
    state: Arc<LockDepMutex<TimelineEstimator<DefaultFetcher>, SyslogStateLock>>,
    tags: std::collections::HashMap<u64, String>,
}

impl LogIterator {
    fn new(syslog: &Syslog, mode: fdiagnostics::StreamMode) -> Result<Self, Errno> {
        let accessor = connect_to_protocol_sync::<fdiagnostics::ArchiveAccessorMarker>()
            .map_err(|_| errno!(ENOENT, format!("Failed to connecto to ArchiveAccessor")))?;
        let is_subscribe = matches!(mode, fdiagnostics::StreamMode::Subscribe);
        let stream_parameters = fdiagnostics::StreamParameters {
            stream_mode: Some(mode),
            data_type: Some(fdiagnostics::DataType::Logs),
            format: Some(fdiagnostics::Format::Fxt),
            client_selector_configuration: Some(
                fdiagnostics::ClientSelectorConfiguration::SelectAll(true),
            ),
            ..fdiagnostics::StreamParameters::default()
        };
        let (client_end, server_end) =
            fidl::endpoints::create_endpoints::<fdiagnostics::BatchIteratorMarker>();
        accessor.stream_diagnostics(&stream_parameters, server_end).map_err(|err| {
            errno!(EIO, format!("ArchiveAccessor/StreamDiagnostics failed: {err}"))
        })?;
        let iterator = fdiagnostics::BatchIteratorSynchronousProxy::new(client_end.into_channel());
        if is_subscribe {
            let () = iterator.wait_for_ready(zx::MonotonicInstant::INFINITE).map_err(|err| {
                errno!(EIO, format!("Failed to wait for BatchIterator being ready: {err}"))
            })?;
        }
        Ok(Self {
            iterator,
            pending_formatted_contents: VecDeque::new(),
            vmo_buf: Vec::new(),
            vmo_offset: 0,
            vmo_len: 0,
            formatted_buf: Vec::new(),
            state: syslog.state.clone(),
            tags: std::collections::HashMap::new(),
        })
    }

    fn get_next(&mut self) -> Result<Option<Vec<u8>>, Errno> {
        loop {
            // Process remaining FXT records in the current VMO buffer.
            while self.vmo_offset < self.vmo_len {
                let current_slice = &self.vmo_buf[self.vmo_offset..self.vmo_len];
                if current_slice.len() < FXT_HEADER_SIZE {
                    self.vmo_offset = self.vmo_len;
                    break;
                }

                let (header, after_header) =
                    Header::read_from_prefix(current_slice).map_err(|_| {
                        self.vmo_offset = self.vmo_len;
                        errno!(EIO, "Invalid FXT header")
                    })?;

                let record_size_words = header.size_words() as usize;
                let record_len = record_size_words * 8;
                if record_len < 16 || current_slice.len() < record_len {
                    self.vmo_offset = self.vmo_len;
                    break;
                }

                let (record_bytes, _) = current_slice.split_at(record_len);
                self.vmo_offset += record_len;

                if header.raw_type() != TRACING_FORMAT_LOG_RECORD_TYPE {
                    continue;
                }

                let tag = header.tag();
                let is_manifest = (tag & LOG_CONTROL_BIT) != 0;
                let actual_tag = (tag & !LOG_CONTROL_BIT) as u64;

                let (timestamp_raw, _) = i64::read_from_prefix(after_header)
                    .map_err(|_| errno!(EIO, "Invalid FXT timestamp"))?;
                let timestamp = zx::BootInstant::from_nanos(timestamp_raw);

                let arguments_buffer = &record_bytes[16..];

                if is_manifest {
                    let mut moniker_str = None;
                    let mut args_slice = arguments_buffer;
                    while !args_slice.is_empty() {
                        let (arg, rem) =
                            diagnostics_log_encoding::parse::parse_argument(args_slice)
                                .map_err(|_| errno!(EIO, "Error parsing FXT argument"))?;
                        args_slice = rem;
                        if arg.name() == MONIKER {
                            if let Value::Text(t) = arg.value() {
                                moniker_str = Some(t.into_owned());
                            }
                        }
                    }
                    if let Some(m) = moniker_str {
                        let comp_name = moniker_to_component_name(&m);
                        self.tags.insert(actual_tag, comp_name);
                    }
                    continue;
                }

                let component_name =
                    self.tags.get(&actual_tag).map(|s| s.as_str()).unwrap_or("unknown");

                if let Some(log) = format_fxt_record(
                    header,
                    timestamp,
                    component_name,
                    arguments_buffer,
                    &self.state,
                    &mut self.formatted_buf,
                )? {
                    return Ok(Some(log));
                }
            }

            // Need more data from pending VMO batch contents.
            if let Some(formatted_content) = self.pending_formatted_contents.pop_front() {
                match formatted_content {
                    fdiagnostics::FormattedContent::Fxt(data) => {
                        let content_size = data
                            .get_content_size()
                            .map_err(|a| errno!(EIO, format!("Error {a} getting VMO size")))?
                            as usize;
                        if self.vmo_buf.len() < content_size {
                            self.vmo_buf.resize(content_size, 0);
                        }
                        data.read(&mut self.vmo_buf[..content_size], 0).map_err(|err| {
                            errno!(EIO, format!("failed to read logs vmo: {err}"))
                        })?;
                        self.vmo_offset = 0;
                        self.vmo_len = content_size;
                        continue;
                    }
                    format => {
                        unreachable!("we only request and expect FXT format. Got: {format:?}")
                    }
                }
            }

            // Fetch next batch from BatchIterator.
            let next_batch = {
                let _waiting_guard = ThreadLockupDetector::pause_tracking();
                self.iterator
                    .get_next(zx::MonotonicInstant::INFINITE)
                    .map_err(|_| errno!(ENOENT))?
                    .map_err(|_| errno!(ENOENT))?
            };
            if next_batch.is_empty() {
                return Ok(None);
            }
            self.pending_formatted_contents = VecDeque::from(next_batch);
        }
    }
}

impl Iterator for LogIterator {
    type Item = Result<Vec<u8>, Errno>;

    fn next(&mut self) -> Option<Result<Vec<u8>, Errno>> {
        self.get_next().transpose()
    }
}

impl Iterator for LogSubscription {
    type Item = Result<Vec<u8>, Errno>;

    fn next(&mut self) -> Option<Self::Item> {
        self.try_next().transpose()
    }
}

struct ResultBuffer {
    max_size: usize,
    buffer: VecDeque<Vec<u8>>,
    current_size: usize,
}

impl ResultBuffer {
    fn new(max_size: usize) -> Self {
        Self { max_size, buffer: VecDeque::default(), current_size: 0 }
    }

    fn push(&mut self, data: Vec<u8>) {
        while !self.buffer.is_empty() && self.current_size + data.len() > self.max_size {
            let old = self.buffer.pop_front().unwrap();
            self.current_size -= old.len();
        }
        self.current_size += data.len();
        self.buffer.push_back(data);
    }
}

impl Into<Vec<u8>> for ResultBuffer {
    fn into(self) -> Vec<u8> {
        let mut result = Vec::with_capacity(self.current_size);
        for mut item in self.buffer {
            result.append(&mut item);
        }
        // If we still exceed the size (for example, a single message of size N in a buffer of
        // size M when N>M), we trim the output.
        result.truncate(self.max_size);
        result
    }
}

#[derive(Debug, Eq, PartialEq, Copy, Clone, KnownLayout, TryFromBytes, Immutable, IntoBytes)]
#[repr(u8)]
pub enum KmsgLevel {
    Emergency = 0,
    Alert = 1,
    Critical = 2,
    Error = 3,
    Warning = 4,
    Notice = 5,
    Info = 6,
    Debug = 7,
}

impl KmsgLevel {
    fn from_raw(value: u8) -> Option<KmsgLevel> {
        zerocopy::try_transmute!(value).ok()
    }
}

/// Given a string starting with <[0-9]*>, returns the level interpreted from the lower 3 bits.
/// The next 8 is the facility, which we ignore atm.
/// If the string doesn't start with a valid level, we return None.
/// The slice returned is the rest of the message after the closing '>'.
///
/// Reference: https://www.kernel.org/doc/Documentation/ABI/testing/dev-kmsg
pub(crate) fn extract_level(msg: &[u8]) -> Option<(KmsgLevel, &[u8])> {
    let mut bytes_iter = msg.iter();
    let Some(c) = bytes_iter.next() else {
        return None;
    };
    if *c != b'<' {
        return None;
    }
    let Some(end) = bytes_iter.enumerate().find(|(_, c)| **c == b'>').map(|(i, _)| i + 1) else {
        return None;
    };
    std::str::from_utf8(&msg[1..end])
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|level| (level & 0x07) as u8)
        .and_then(KmsgLevel::from_raw)
        .map(|level| (level, &msg[end + 1..]))
}

fn moniker_to_component_name(moniker_str: &str) -> String {
    match diagnostics_data::ExtendedMoniker::parse_str(moniker_str) {
        Ok(diagnostics_data::ExtendedMoniker::ComponentInstance(moniker)) => {
            if moniker.is_root() {
                "<root>".to_string()
            } else if let Some(leaf) = moniker.leaf() {
                leaf.as_ref().to_string()
            } else {
                "unknown".to_string()
            }
        }
        Ok(diagnostics_data::ExtendedMoniker::ComponentManager) => "klog".to_string(),
        Err(_) => "unknown".to_string(),
    }
}

fn format_fxt_record<'a, T: TimeFetcher>(
    header: Header,
    timestamp: zx::BootInstant,
    component_name: &str,
    arguments_buffer: &'a [u8],
    state: &Arc<LockDepMutex<TimelineEstimator<T>, SyslogStateLock>>,
    formatted_buf: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>, Errno> {
    let mut tags = SmallVec::<[std::borrow::Cow<'a, str>; 4]>::new();
    let mut message: Option<std::borrow::Cow<'a, str>> = None;
    let mut pid: Option<u64> = None;

    let mut args_slice = arguments_buffer;
    while !args_slice.is_empty() {
        let (arg, rem) = diagnostics_log_encoding::parse::parse_argument(args_slice)
            .map_err(|_| errno!(EIO, "Error parsing FXT argument"))?;
        args_slice = rem;
        match arg {
            Argument::Tag(t) => {
                // TODO(b/299533466): remove this.
                if t.contains("fxlogcat") {
                    return Ok(None);
                }
                tags.push(t);
            }
            Argument::Message(m) => {
                message = Some(m);
            }
            Argument::Pid(p) => {
                pid = Some(p.raw_koid());
            }
            _ => {}
        }
    }

    let (level, msg_slice) = match message.as_deref().and_then(|msg| extract_level(msg.as_bytes()))
    {
        Some((lvl, rem)) => (lvl as u8, rem),
        None => {
            let raw_sev = header.severity();
            let sev = Severity::from(raw_sev);
            let lvl = match sev {
                Severity::Trace | Severity::Debug => KmsgLevel::Debug as u8,
                Severity::Info => KmsgLevel::Info as u8,
                Severity::Warn => KmsgLevel::Warning as u8,
                Severity::Error => KmsgLevel::Error as u8,
                Severity::Fatal => KmsgLevel::Critical as u8,
            };
            (lvl, message.as_deref().map(|m| m.as_bytes()).unwrap_or(b""))
        }
    };

    // TODO(https://fxbug.dev/433724019): this isn't correct strictly speaking, but will be in most
    // cases. We unapply the *current* offset and in the case where suspension happened between
    // when the log message was generated and when Starnix is forwarding the log message, this will
    // be different from the *actual* offset prior to suspension.
    let time = state.lock().boot_time_to_monotonic_time(timestamp);
    let time_nanos = time.into_nanos();
    let time_secs = time_nanos / NANOS_PER_SECOND;
    // Microsecond-level precision fractional time.
    let time_fract = (time_nanos % NANOS_PER_SECOND) / MICROS_PER_NANOSECOND;

    formatted_buf.clear();
    write!(formatted_buf, "<{level}>[{time_secs:05}.{time_fract:06}] {component_name}")
        .map_err(|_| errno!(EIO))?;

    match pid {
        Some(p) => write!(formatted_buf, "[{p}]: ").map_err(|_| errno!(EIO))?,
        None => write!(formatted_buf, ": ").map_err(|_| errno!(EIO))?,
    }

    for (i, tag) in tags.iter().enumerate() {
        if i != 0 {
            write!(formatted_buf, ",").map_err(|_| errno!(EIO))?;
        }
        write!(formatted_buf, "{tag}").map_err(|_| errno!(EIO))?;
    }
    if !tags.is_empty() {
        write!(formatted_buf, ": ").map_err(|_| errno!(EIO))?;
    }

    formatted_buf.extend_from_slice(msg_slice);

    // Second pass for structured payload keys
    let mut args_slice = arguments_buffer;
    while !args_slice.is_empty() {
        let (arg, rem) = diagnostics_log_encoding::parse::parse_argument(args_slice)
            .map_err(|_| errno!(EIO, "Error parsing FXT argument"))?;
        args_slice = rem;
        if let Argument::Other { name, value } = arg {
            match value {
                Value::SignedInt(v) => {
                    write!(formatted_buf, " {name}={v}").map_err(|_| errno!(EIO))?;
                }
                Value::UnsignedInt(v) => {
                    write!(formatted_buf, " {name}={v}").map_err(|_| errno!(EIO))?;
                }
                Value::Floating(v) => {
                    write!(formatted_buf, " {name}={v}").map_err(|_| errno!(EIO))?;
                }
                Value::Text(v) => {
                    write!(formatted_buf, " {name}={v}").map_err(|_| errno!(EIO))?;
                }
                Value::Boolean(v) => {
                    write!(formatted_buf, " {name}={v}").map_err(|_| errno!(EIO))?;
                }
            }
        }
    }

    formatted_buf.push(b'\n');
    Ok(Some(formatted_buf.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_result_buffer() {
        let mut buffer = ResultBuffer::new(100);
        buffer.push(vec![0; 200]);
        let result: Vec<u8> = buffer.into();
        assert_eq!(result.len(), 100);

        let mut buffer = ResultBuffer::new(100);
        buffer.push(Vec::from_iter(0..20));
        buffer.push(Vec::from_iter(20..50));
        let result: Vec<u8> = buffer.into();
        assert_eq!(result.len(), 50);
        for i in 0..50u8 {
            assert_eq!(result[i as usize], i);
        }

        let mut buffer = ResultBuffer::new(100);
        buffer.push(Vec::from_iter(0..20));
        buffer.push(Vec::from_iter(20..150));
        let result: Vec<u8> = buffer.into();
        assert_eq!(result.len(), 100);
        for i in 0..100u8 {
            assert_eq!(result[i as usize], i + 20u8);
        }

        let mut buffer = ResultBuffer::new(100);
        buffer.push(Vec::from_iter(0..20));
        buffer.push(Vec::from_iter(20..150));
        buffer.push(Vec::from_iter(150..210));
        let result: Vec<u8> = buffer.into();
        assert_eq!(result.len(), 60);
        for i in 0..60u8 {
            assert_eq!(result[i as usize], i + 150u8);
        }
    }

    #[test]
    fn test_extract_level() {
        for level in 0..8 {
            let msg = format!("<{level}> some message");
            let result = extract_level(msg.as_bytes()).map(|(x, i)| (x as u8, i));
            assert_eq!(Some((level, " some message".as_bytes())), result);
        }
    }

    #[test]
    fn parse_message_accepts_levels_greater_than_7() {
        assert_eq!(
            Some((KmsgLevel::Warning, " message".as_bytes())),
            extract_level("<100> message".as_bytes())
        );
    }

    #[test]
    fn parse_message_defaults_when_non_numbers() {
        assert_eq!(None, extract_level("<a> some message".as_bytes()));
    }

    #[test]
    fn parse_message_defaults_when_invalid_level_syntax() {
        assert_eq!(None, extract_level("<1 some message".as_bytes()));
    }

    #[test]
    fn parse_message_defaults_when_no_level() {
        assert_eq!(None, extract_level("some message".as_bytes()));
    }

    #[test]
    fn test_moniker_to_component_name() {
        assert_eq!(moniker_to_component_name("core/network/netstack"), "netstack");
        assert_eq!(moniker_to_component_name("bootstrap/archivist"), "archivist");
        assert_eq!(moniker_to_component_name("/"), "<root>");
        assert_eq!(moniker_to_component_name("."), "<root>");
        assert_eq!(moniker_to_component_name("<component_manager>"), "klog");
    }
}
