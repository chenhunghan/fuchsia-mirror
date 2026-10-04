// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! FIDL server for the `fuchsia.driver.lab` wire contract, bridging the
//! host-facing protocol to the host-testable core logic.

use crate::fuchsia_backends::LiveBackend;
use fidl_fuchsia_driver_lab as flab;
use fuchsia_async::ScopeHandle;
use futures::{FutureExt as _, TryStreamExt};
use lab_proxy_core::access_policy::{AccessClass, AccessRule, Denial, WritePrecondition};
use lab_proxy_core::audit_ring::{AuditRecord, AuditRing, Decision, OpStatus};
use lab_proxy_core::executor::{
    ClockOpError, Executor, GpioReadError, GpioWriteError, I2cTransferError, PollError, ReadError,
    ResetOpError, SequenceItem as CoreSequenceItem, SerialOpError, SnapshotError, SnapshotItem,
    SpiTransmitError, WriteError,
};
use lab_proxy_core::hardware_backend::Clock;
use lab_proxy_core::interrupt::InterruptManager;
use lab_proxy_core::session::{OpenError, ProxyIdentity, RunContext, SessionManager, SessionMode};
use std::sync::{Arc, Mutex};

fn fidl_to_core_sequence_item(item: &flab::SequenceItem) -> Option<CoreSequenceItem> {
    match item {
        flab::SequenceItem::Read32(read) => {
            Some(CoreSequenceItem::Read32 { resource: read.resource, offset: read.offset })
        }
        flab::SequenceItem::Write32(write) => Some(CoreSequenceItem::Write32 {
            resource: write.resource,
            offset: write.offset,
            value: write.value,
            write_mask: write.write_mask,
            precondition: write
                .precondition
                .as_ref()
                .map(|p| WritePrecondition { expected: p.expected, mask: p.mask }),
            readback: write.readback,
        }),
        flab::SequenceItem::Poll32(poll) => Some(CoreSequenceItem::Poll32 {
            resource: poll.resource,
            offset: poll.offset,
            expected: poll.expected,
            mask: poll.mask,
            interval_ns: poll.interval_ns,
            timeout_ns: poll.timeout_ns,
        }),
        flab::SequenceItem::DelayNs(delay) => Some(CoreSequenceItem::DelayNs(*delay)),
        flab::SequenceItem::Barrier(_) => Some(CoreSequenceItem::Barrier),
        flab::SequenceItem::GpioRead(read) => {
            Some(CoreSequenceItem::GpioRead { resource: read.resource })
        }
        flab::SequenceItem::GpioWrite(write) => {
            Some(CoreSequenceItem::GpioWrite { resource: write.resource, value: write.value })
        }
        flab::SequenceItem::I2cTransfer(transfer) => Some(CoreSequenceItem::I2cTransfer {
            resource: transfer.resource,
            write_data: transfer.write_data.clone(),
            read_length: transfer.read_length,
        }),
        flab::SequenceItem::SpiTransmit(transmit) => Some(CoreSequenceItem::SpiTransmit {
            resource: transmit.resource,
            tx_data: transmit.tx_data.clone(),
        }),
        _ => None,
    }
}

/// Wire-contract version served by this driver.
pub const PROTOCOL_MAJOR: u32 = 1;
/// Wire-contract minor version.
pub const PROTOCOL_MINOR: u32 = 0;

/// Clock backed by zx monotonic time.
#[derive(Debug, Default)]
pub struct ZxClock;

impl Clock for ZxClock {
    fn now_ns(&mut self) -> i64 {
        zx::MonotonicInstant::get().into_nanos()
    }
}

impl lab_proxy_core::hardware_backend::Timer for ZxClock {
    async fn sleep(&mut self, duration_ns: i64) {
        if duration_ns > 0 {
            fuchsia_async::Timer::new(zx::MonotonicInstant::after(zx::Duration::from_nanos(
                duration_ns,
            )))
            .await;
        }
    }
}

/// Shared proxy state behind one lock and cancellation token.
#[derive(Debug)]
pub struct ProxyState {
    /// Session lifecycle and per-session policy.
    pub sessions: SessionManager,
    /// The shared read-path executor over the acquired MMIO mappings.
    pub executor: Executor<LiveBackend, ZxClock>,
    /// The instance audit ring.
    pub audit: AuditRing,
    /// Per-resource description digests, reported by `Describe` for
    /// persistent grant matching.
    pub resource_digests: std::collections::BTreeMap<u32, String>,
    /// Interrupt manager coordinating interrupt observations.
    pub interrupts: InterruptManager,
    /// Structured configuration bounds.
    pub config: lab_proxy_core::config::ProxyConfig,
}

/// Handle to the shared proxy state and cancellation token.
pub struct SharedStateData {
    pub inner: Mutex<ProxyState>,
    pub abort_token: std::sync::atomic::AtomicBool,
}

pub type SharedState = Arc<SharedStateData>;

fn now_ns() -> i64 {
    zx::MonotonicInstant::get().into_nanos()
}

fn denial_to_fidl(denial: Denial) -> flab::OperationError {
    match denial {
        Denial::UnknownResource => flab::OperationError::UnknownResource,
        Denial::UnsupportedWidth => flab::OperationError::UnsupportedWidth,
        Denial::UnsupportedAccessClass => flab::OperationError::UnsupportedAccessClass,
        Denial::Misaligned => flab::OperationError::Misaligned,
        Denial::OffsetOverflow => flab::OperationError::OffsetOverflow,
        Denial::OutOfLogicalBounds => flab::OperationError::OutOfLogicalBounds,
        Denial::OutOfMappedBounds => flab::OperationError::OutOfMappedBounds,
        Denial::NotPermittedByCeiling => flab::OperationError::NotPermittedByCeiling,
        Denial::HardDenied => flab::OperationError::HardDenied,
        Denial::UnknownReadsNotPermitted => flab::OperationError::UnknownReadsNotPermitted,
        Denial::PollNotPermitted => flab::OperationError::PollNotPermitted,
        Denial::NotInAllowlist => flab::OperationError::NotInAllowlist,
        Denial::LimitExceeded => flab::OperationError::LimitExceeded,
        Denial::StaleIdentity => flab::OperationError::StaleIdentity,
        Denial::MutationLeaseContention => flab::OperationError::MutationLeaseContention,
        Denial::NotAccepting => flab::OperationError::NotAccepting,
        Denial::UnsupportedExpectation => flab::OperationError::UnsupportedExpectation,
        Denial::PreconditionFailed => flab::OperationError::PreconditionFailed,
        Denial::MissingPrecondition => flab::OperationError::MissingPrecondition,
        Denial::Timeout => flab::OperationError::Timeout,
        Denial::ReadOnlySession => flab::OperationError::ReadOnlySession,
        Denial::WriteNotPermitted => flab::OperationError::WriteNotPermitted,
        Denial::UnsupportedMethod => flab::OperationError::UnsupportedMethod,
        Denial::TransferTooLarge => flab::OperationError::TransferTooLarge,
    }
}

fn open_error_to_fidl(error: &OpenError) -> flab::OpenSessionError {
    match error {
        OpenError::NotAccepting => flab::OpenSessionError::NotAccepting,
        OpenError::StaleBootId => flab::OpenSessionError::StaleBootId,
        OpenError::StaleProxyGeneration => flab::OpenSessionError::StaleProxyGeneration,
        OpenError::StaleResourceDigest => flab::OpenSessionError::StaleResourceDigest,
        OpenError::StalePolicyDigest => flab::OpenSessionError::StalePolicyDigest,
        OpenError::RejectedAllowlist { .. } => flab::OpenSessionError::RejectedAllowlist,
        OpenError::MutationLeaseHeld | OpenError::MutationNotPermitted => {
            flab::OpenSessionError::MutationLeaseHeld
        }
        OpenError::UnsupportedExpectation => flab::OpenSessionError::UnsupportedExpectation,
    }
}

fn open_error_denial(error: &OpenError) -> Denial {
    match error {
        OpenError::NotAccepting => Denial::NotAccepting,
        OpenError::StaleBootId
        | OpenError::StaleProxyGeneration
        | OpenError::StaleResourceDigest
        | OpenError::StalePolicyDigest => Denial::StaleIdentity,
        OpenError::RejectedAllowlist { denial, .. } => *denial,
        OpenError::MutationLeaseHeld => Denial::MutationLeaseContention,
        OpenError::MutationNotPermitted => Denial::NotPermittedByCeiling,
        OpenError::UnsupportedExpectation => Denial::UnsupportedExpectation,
    }
}

fn describe(state: &SharedState) -> flab::ProxyDescription {
    let state = state.inner.lock().unwrap();
    let identity = state.sessions.identity();
    let resources: Vec<flab::ResourceDescription> = state
        .sessions
        .resources()
        .iter()
        .map(|(id, resource)| flab::ResourceDescription {
            id: Some(*id),
            name: Some(resource.name.clone()),
            kind: Some(match resource.kind {
                lab_proxy_core::access_policy::ResourceKind::Mmio => flab::ResourceKind::Mmio,
                lab_proxy_core::access_policy::ResourceKind::Gpio => flab::ResourceKind::Gpio,
                lab_proxy_core::access_policy::ResourceKind::I2c => flab::ResourceKind::I2C,
                lab_proxy_core::access_policy::ResourceKind::Spi => flab::ResourceKind::Spi,
                lab_proxy_core::access_policy::ResourceKind::Interrupt => {
                    flab::ResourceKind::Interrupt
                }
                lab_proxy_core::access_policy::ResourceKind::Clock => flab::ResourceKind::Clock,
                lab_proxy_core::access_policy::ResourceKind::Reset => flab::ResourceKind::Reset,
                lab_proxy_core::access_policy::ResourceKind::Serial => flab::ResourceKind::Serial,
            }),
            logical_size: Some(resource.logical_size),
            digest: state.resource_digests.get(id).cloned(),
            ..Default::default()
        })
        .collect();
    flab::ProxyDescription {
        protocol_major: Some(PROTOCOL_MAJOR),
        protocol_minor: Some(PROTOCOL_MINOR),
        proxy_generation: Some(identity.proxy_generation),
        boot_id: Some(identity.boot_id.clone()),
        resource_digest: Some(identity.resource_digest.clone()),
        policy_digest: Some(identity.policy_digest.clone()),
        resources: Some(resources),
        max_snapshot_items: Some(state.executor.limits().max_snapshot_items as u32),
        audit_capacity: Some(state.audit.capacity() as u64),
        // Phase 1 never activates takeover; node_moniker and
        // topology_generation stay absent until the proxy binds to a
        // real node.
        takeover: Some(flab::TakeoverState::NotActive),
        ..Default::default()
    }
}

fn open_session(
    state: &SharedState,
    context: flab::RunContext,
    mode: flab::SessionMode,
    expectations: flab::Expectations,
    allowlist: Vec<flab::AccessRule>,
) -> Result<u64, OpenError> {
    let context = RunContext {
        run_id: context.run_id.unwrap_or_default(),
        case_id: context.case_id.unwrap_or_default(),
        plan_digest: context.plan_digest.unwrap_or_default(),
        host_tool_version: context.host_tool_version.unwrap_or_default(),
    };
    if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
        let mut state = state.inner.lock().unwrap();
        state.audit.append(AuditRecord {
            session: None,
            resource: None,
            operation: "open_session",
            offset: None,
            decision: Decision::Denied(Denial::NotAccepting),
            status: OpStatus::Rejected,
            value: None,
            timestamp_ns: now_ns(),
            run_id: Some(context.run_id),
            item_index: None,
        });
        return Err(OpenError::NotAccepting);
    }
    let mode = match mode {
        flab::SessionMode::ReadOnly => SessionMode::ReadOnly,
        flab::SessionMode::Mutating => SessionMode::Mutating,
    };
    // Reserved phase 2 expectations fail closed rather than being
    // silently ignored.
    if expectations.topology_generation.is_some() || expectations.bound_driver_url.is_some() {
        let mut state = state.inner.lock().unwrap();
        state.audit.append(AuditRecord {
            session: None,
            resource: None,
            operation: "open_session",
            offset: None,
            decision: Decision::Denied(Denial::UnsupportedExpectation),
            status: OpStatus::Rejected,
            value: None,
            timestamp_ns: now_ns(),
            run_id: Some(context.run_id),
            item_index: None,
        });
        return Err(OpenError::UnsupportedExpectation);
    }
    // A required expectation the host omitted can never match the real
    // identity, so absence falls through to the per-field stale check.
    let expectations = ProxyIdentity {
        boot_id: expectations.boot_id.unwrap_or_default(),
        proxy_generation: expectations.proxy_generation.unwrap_or_default(),
        resource_digest: expectations.resource_digest.unwrap_or_default(),
        policy_digest: expectations.policy_digest.unwrap_or_default(),
    };
    let rules: Vec<AccessRule> = allowlist
        .iter()
        .map(|rule| AccessRule {
            resource: rule.resource,
            offset: rule.offset,
            width: rule.width,
            class: match rule.class {
                flab::AccessClass::ReadOnce => AccessClass::ReadOnce,
                flab::AccessClass::Snapshot => AccessClass::Snapshot,
                flab::AccessClass::Write => AccessClass::Write,
                flab::AccessClass::Poll => AccessClass::Poll,
                flab::AccessClass::Sequence => AccessClass::Sequence,
                flab::AccessClass::Protocol => AccessClass::Protocol,
                flab::AccessClass::Interrupt => AccessClass::Interrupt,
            },
        })
        .collect();

    let mut state = state.inner.lock().unwrap();
    let run_id = context.run_id.clone();
    let result = state.sessions.open_session(context, mode, &expectations, rules);
    let record = match &result {
        Ok(id) => AuditRecord {
            session: Some(*id),
            resource: None,
            operation: "open_session",
            offset: None,
            decision: Decision::Allowed,
            status: OpStatus::Ok,
            value: None,
            timestamp_ns: now_ns(),
            run_id: Some(run_id),
            item_index: None,
        },
        Err(error) => {
            // A rejected allowlist records the first offending rule, as
            // the wire contract documents.
            let (resource, offset) = match error {
                OpenError::RejectedAllowlist { rule, .. } => {
                    (Some(rule.resource), Some(rule.offset))
                }
                _ => (None, None),
            };
            AuditRecord {
                session: None,
                resource,
                operation: "open_session",
                offset,
                decision: Decision::Denied(open_error_denial(error)),
                status: OpStatus::Rejected,
                value: None,
                timestamp_ns: now_ns(),
                run_id: Some(run_id),
                item_index: None,
            }
        }
    };
    state.audit.append(record);
    if let Ok(id) = &result {
        if mode == SessionMode::Mutating {
            let mut q_record = AuditRecord::lifecycle("quiesce_engaged", now_ns());
            q_record.session = Some(*id);
            state.audit.append(q_record);
        }
    }
    result
}

/// Serves one `Proxy` connection, spawning a task per opened
/// session on `scope`.
pub async fn serve_proxy(
    state: SharedState,
    scope: ScopeHandle,
    mut stream: flab::Proxy_RequestStream,
) {
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            flab::Proxy_Request::Describe { responder } => {
                let description = describe(&state);
                let _ = responder.send(&description);
            }
            flab::Proxy_Request::OpenSession {
                context,
                mode,
                expectations,
                allowlist,
                session,
                responder,
            } => match open_session(&state, context, mode, expectations, allowlist) {
                Ok(id) => {
                    scope.spawn(serve_session(
                        state.clone(),
                        id,
                        session.into_stream(),
                        scope.clone(),
                    ));
                    let _ = responder.send(Ok(id));
                }
                Err(error) => {
                    let _ = responder.send(Err(open_error_to_fidl(&error)));
                }
            },
            flab::Proxy_Request::_UnknownMethod { .. } => {}
        }
    }
}

async fn serve_session(
    state: SharedState,
    id: u64,
    mut stream: flab::SessionRequestStream,
    scope: ScopeHandle,
) {
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            flab::SessionRequest::Read32 { resource, offset, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => session
                            .policy
                            .check_read32(resource, offset, AccessClass::ReadOnce)
                            .map_err(denial_to_fidl),
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .read32(&session.policy, audit, id, resource, offset)
                                .map(|outcome| {
                                    (outcome.value, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    ReadError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ReadError::Backend { .. } => flab::OperationError::BackendFault,
                                })
                        }
                        Ok(()) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::ReadOnce, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "read32",
                                        offset: Some(offset),
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .read32(&session.policy, audit, id, resource, offset)
                                        .map(|outcome| {
                                            (outcome.value, outcome.audit_seq, outcome.timestamp_ns)
                                        })
                                        .map_err(|error| match error {
                                            ReadError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ReadError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::Snapshot { items, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let items: Vec<SnapshotItem> = items
                    .iter()
                    .map(|item| SnapshotItem { resource: item.resource, offset: item.offset })
                    .collect();
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    if sessions.session(id).is_none() {
                        Err(flab::OperationError::StaleIdentity)
                    } else {
                        let now = now_ns();
                        match sessions.check_access(id, AccessClass::Snapshot, now) {
                            Err(denial) => {
                                let record = AuditRecord {
                                    session: Some(id),
                                    resource: None,
                                    operation: "snapshot",
                                    offset: None,
                                    decision: Decision::Denied(denial),
                                    status: OpStatus::Rejected,
                                    value: None,
                                    timestamp_ns: now,
                                    run_id: None,
                                    item_index: None,
                                };
                                audit.append(record);
                                Err(denial_to_fidl(denial))
                            }
                            Ok(()) => {
                                let session = sessions.session(id).unwrap();
                                executor
                                    .snapshot(&session.policy, audit, id, &items)
                                    .map(|outcome| {
                                        let results: Vec<flab::SnapshotItemResult> = outcome
                                            .results
                                            .iter()
                                            .map(|result| flab::SnapshotItemResult {
                                                item: flab::SnapshotItem {
                                                    resource: result.item.resource,
                                                    offset: result.item.offset,
                                                },
                                                ok: result.value.is_ok(),
                                                value: result.value.unwrap_or(0),
                                                audit_seq: result.audit_seq,
                                                timestamp_ns: result.timestamp_ns,
                                            })
                                            .collect();
                                        (results, outcome.complete)
                                    })
                                    .map_err(|error| match error {
                                        SnapshotError::TooManyItems { .. } => {
                                            flab::OperationError::LimitExceeded
                                        }
                                        SnapshotError::Rejected { denial, .. } => {
                                            denial_to_fidl(denial)
                                        }
                                    })
                            }
                        }
                    }
                };
                let _ = responder.send(
                    result
                        .as_ref()
                        .map(|(results, complete)| (results.as_slice(), *complete))
                        .map_err(|error| *error),
                );
            }
            flab::SessionRequest::Write32 {
                resource,
                offset,
                value,
                write_mask,
                precondition,
                readback,
                responder,
            } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let precondition =
                    precondition.map(|p| WritePrecondition { expected: p.expected, mask: p.mask });
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => session
                            .policy
                            .check_write32(resource, offset, write_mask, precondition.as_ref())
                            .map_err(denial_to_fidl),
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .write32(
                                    &session.policy,
                                    audit,
                                    id,
                                    resource,
                                    offset,
                                    value,
                                    write_mask,
                                    precondition,
                                    readback,
                                )
                                .map(|outcome| {
                                    (
                                        outcome.readback_value,
                                        outcome.audit_seq,
                                        outcome.timestamp_ns,
                                    )
                                })
                                .map_err(|error| match error {
                                    WriteError::Denied { denial, .. } => denial_to_fidl(denial),
                                    WriteError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Write, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "write32",
                                        offset: Some(offset),
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .write32(
                                            &session.policy,
                                            audit,
                                            id,
                                            resource,
                                            offset,
                                            value,
                                            write_mask,
                                            precondition,
                                            readback,
                                        )
                                        .map(|outcome| {
                                            (
                                                outcome.readback_value,
                                                outcome.audit_seq,
                                                outcome.timestamp_ns,
                                            )
                                        })
                                        .map_err(|error| match error {
                                            WriteError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            WriteError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::Poll32 {
                resource,
                offset,
                expected,
                mask,
                interval_ns,
                timeout_ns,
                responder,
            } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let start_ts = now_ns();
                let deadline = start_ts.saturating_add(timeout_ns);

                // Initial lock: validate session and policy.
                let initial_check = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, audit, .. } = guard;
                    match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            match session.policy.check_poll32(
                                resource,
                                offset,
                                mask,
                                interval_ns,
                                timeout_ns,
                            ) {
                                Ok(()) => {
                                    if let Err(denial) = sessions.check_deadline(id, timeout_ns) {
                                        let record = AuditRecord {
                                            session: Some(id),
                                            resource: Some(resource),
                                            operation: "poll32",
                                            offset: Some(offset),
                                            decision: Decision::Denied(denial),
                                            status: OpStatus::Rejected,
                                            value: None,
                                            timestamp_ns: now_ns(),
                                            run_id: None,
                                            item_index: None,
                                        };
                                        audit.append(record);
                                        Err(denial_to_fidl(denial))
                                    } else if let Err(denial) =
                                        sessions.check_access(id, AccessClass::Poll, now_ns())
                                    {
                                        let record = AuditRecord {
                                            session: Some(id),
                                            resource: Some(resource),
                                            operation: "poll32",
                                            offset: Some(offset),
                                            decision: Decision::Denied(denial),
                                            status: OpStatus::Rejected,
                                            value: None,
                                            timestamp_ns: now_ns(),
                                            run_id: None,
                                            item_index: None,
                                        };
                                        audit.append(record);
                                        Err(denial_to_fidl(denial))
                                    } else {
                                        Ok(())
                                    }
                                }
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "poll32",
                                        offset: Some(offset),
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now_ns(),
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                            }
                        }
                    }
                };

                if let Err(err) = initial_check {
                    let _ = responder.send(Err(err));
                    continue;
                }

                let mut last_val = None;
                let poll_res = loop {
                    if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                        break Err(flab::OperationError::NotAccepting);
                    }
                    // Lock to perform one read step
                    let step_res = {
                        let guard = &mut *state.inner.lock().unwrap();
                        let ProxyState { sessions, executor, audit, .. } = guard;
                        match sessions.session(id) {
                            None => Err(flab::OperationError::StaleIdentity),
                            Some(_) => match executor
                                .poll32_read_step(audit, id, resource, offset, expected, mask, None)
                            {
                                Ok(Ok(outcome)) => Ok(Some(outcome)),
                                Ok(Err(val)) => {
                                    last_val = Some(val);
                                    Ok(None)
                                }
                                Err(PollError::Denied { denial, .. }) => {
                                    Err(denial_to_fidl(denial))
                                }
                                Err(PollError::Backend { .. }) => {
                                    Err(flab::OperationError::BackendFault)
                                }
                            },
                        }
                    };

                    match step_res {
                        Err(err) => break Err(err),
                        Ok(Some(outcome)) => {
                            break Ok((outcome.value, outcome.audit_seq, outcome.timestamp_ns));
                        }
                        Ok(None) => {
                            let now = now_ns();
                            if now >= deadline {
                                let guard = &mut *state.inner.lock().unwrap();
                                let ProxyState { sessions, executor, audit, .. } = guard;
                                if sessions.session(id).is_none() {
                                    break Err(flab::OperationError::StaleIdentity);
                                } else {
                                    executor.poll32_timeout(
                                        audit, id, resource, offset, last_val, None,
                                    );
                                    break Err(flab::OperationError::Timeout);
                                }
                            }
                            let sleep_dur = if interval_ns > 0 {
                                let rem = deadline.saturating_sub(now);
                                interval_ns.min(rem)
                            } else {
                                0
                            };
                            if sleep_dur > 0 {
                                fuchsia_async::Timer::new(zx::MonotonicInstant::after(
                                    zx::Duration::from_nanos(sleep_dur),
                                ))
                                .await;
                            } else {
                                fuchsia_async::Timer::new(zx::MonotonicInstant::after(
                                    zx::Duration::from_nanos(1000),
                                ))
                                .await;
                            }
                            if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                                break Err(flab::OperationError::NotAccepting);
                            }
                        }
                    }
                };

                let _ = responder.send(poll_res);
            }
            flab::SessionRequest::ExecuteSequence { items, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let mut core_items = Vec::with_capacity(items.len());
                let mut unknown_item_index = None;
                for (idx, item) in items.iter().enumerate() {
                    match fidl_to_core_sequence_item(item) {
                        Some(core_item) => core_items.push(core_item),
                        None => {
                            unknown_item_index = Some(idx);
                            break;
                        }
                    }
                }

                // Initial lock: whole-sequence prevalidation
                let prevalidation = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            if let Some(idx) = unknown_item_index {
                                let record = AuditRecord {
                                    session: Some(id),
                                    resource: None,
                                    operation: "sequence",
                                    offset: None,
                                    decision: Decision::Denied(Denial::UnsupportedAccessClass),
                                    status: OpStatus::Rejected,
                                    value: None,
                                    timestamp_ns: now_ns(),
                                    run_id: None,
                                    item_index: Some(idx as u32),
                                };
                                audit.append(record);
                                Err(flab::OperationError::UnsupportedAccessClass)
                            } else if core_items.len() > executor.limits().max_sequence_items {
                                let record = AuditRecord {
                                    session: Some(id),
                                    resource: None,
                                    operation: "sequence",
                                    offset: None,
                                    decision: Decision::Denied(Denial::LimitExceeded),
                                    status: OpStatus::Rejected,
                                    value: None,
                                    timestamp_ns: now_ns(),
                                    run_id: None,
                                    item_index: None,
                                };
                                audit.append(record);
                                Err(flab::OperationError::LimitExceeded)
                            } else {
                                match executor.prevalidate_sequence(&session.policy, &core_items) {
                                    Ok(()) => {
                                        let now = now_ns();
                                        match sessions.check_access(id, AccessClass::Sequence, now)
                                        {
                                            Err(denial) => {
                                                let record = AuditRecord {
                                                    session: Some(id),
                                                    resource: None,
                                                    operation: "sequence",
                                                    offset: None,
                                                    decision: Decision::Denied(denial),
                                                    status: OpStatus::Rejected,
                                                    value: None,
                                                    timestamp_ns: now,
                                                    run_id: None,
                                                    item_index: None,
                                                };
                                                audit.append(record);
                                                Err(denial_to_fidl(denial))
                                            }
                                            Ok(()) => Ok(()),
                                        }
                                    }
                                    Err((idx, denial)) => {
                                        let record = AuditRecord {
                                            session: Some(id),
                                            resource: None,
                                            operation: "sequence",
                                            offset: None,
                                            decision: Decision::Denied(denial),
                                            status: OpStatus::Rejected,
                                            value: None,
                                            timestamp_ns: now_ns(),
                                            run_id: None,
                                            item_index: Some(idx as u32),
                                        };
                                        audit.append(record);
                                        Err(denial_to_fidl(denial))
                                    }
                                }
                            }
                        }
                    }
                };

                if let Err(err) = prevalidation {
                    let _ = responder.send(Err(err));
                    continue;
                }

                // Sequence execution loop
                let seq_start_ts = now_ns();
                let mut results = Vec::with_capacity(core_items.len());
                let mut complete = true;

                for (index, item) in core_items.iter().enumerate() {
                    let idx = index as u32;

                    if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                        results.push(flab::SequenceItemResult {
                            index: idx,
                            ok: false,
                            outcome: flab::SequenceItemOutcome::Error(
                                flab::OperationError::NotAccepting,
                            ),
                        });
                        complete = false;
                        break;
                    }

                    let max_sequence_duration = {
                        let guard = state.inner.lock().unwrap();
                        guard.config.max_sequence_duration_ns as i64
                    };
                    if now_ns().saturating_sub(seq_start_ts) > max_sequence_duration {
                        let guard = &mut *state.inner.lock().unwrap();
                        let ProxyState { audit, .. } = guard;
                        let record = AuditRecord {
                            session: Some(id),
                            resource: None,
                            operation: "sequence",
                            offset: None,
                            decision: Decision::Denied(Denial::Timeout),
                            status: OpStatus::Rejected,
                            value: None,
                            timestamp_ns: now_ns(),
                            run_id: None,
                            item_index: Some(idx),
                        };
                        audit.append(record);
                        results.push(flab::SequenceItemResult {
                            index: idx,
                            ok: false,
                            outcome: flab::SequenceItemOutcome::Error(
                                flab::OperationError::Timeout,
                            ),
                        });
                        complete = false;
                        break;
                    }

                    match item {
                        CoreSequenceItem::Read32 { resource, offset } => {
                            let step = {
                                let guard = &mut *state.inner.lock().unwrap();
                                let ProxyState { sessions, executor, audit, .. } = guard;
                                match sessions.session(id) {
                                    None => Err(flab::OperationError::StaleIdentity),
                                    Some(session) => match executor.read32_internal(
                                        &session.policy,
                                        audit,
                                        id,
                                        *resource,
                                        *offset,
                                        Some(idx),
                                    ) {
                                        Ok(outcome) => Ok(flab::SequenceItemResult {
                                            index: idx,
                                            ok: true,
                                            outcome: flab::SequenceItemOutcome::Read32(
                                                flab::ReadResult {
                                                    value: outcome.value,
                                                    audit_seq: outcome.audit_seq,
                                                    timestamp_ns: outcome.timestamp_ns,
                                                },
                                            ),
                                        }),
                                        Err(ReadError::Denied { denial, .. }) => {
                                            Err(denial_to_fidl(denial))
                                        }
                                        Err(ReadError::Backend { .. }) => {
                                            Err(flab::OperationError::BackendFault)
                                        }
                                    },
                                }
                            };
                            match step {
                                Ok(res) => results.push(res),
                                Err(err) => {
                                    results.push(flab::SequenceItemResult {
                                        index: idx,
                                        ok: false,
                                        outcome: flab::SequenceItemOutcome::Error(err),
                                    });
                                    complete = false;
                                    break;
                                }
                            }
                        }
                        CoreSequenceItem::Write32 {
                            resource,
                            offset,
                            value,
                            write_mask,
                            precondition,
                            readback,
                        } => {
                            let step = {
                                let guard = &mut *state.inner.lock().unwrap();
                                let ProxyState { sessions, executor, audit, .. } = guard;
                                match sessions.session(id) {
                                    None => Err(flab::OperationError::StaleIdentity),
                                    Some(session) => match executor.write32_internal(
                                        &session.policy,
                                        audit,
                                        id,
                                        *resource,
                                        *offset,
                                        *value,
                                        *write_mask,
                                        *precondition,
                                        *readback,
                                        Some(idx),
                                    ) {
                                        Ok(outcome) => Ok(flab::SequenceItemResult {
                                            index: idx,
                                            ok: true,
                                            outcome: flab::SequenceItemOutcome::Write32(
                                                flab::WriteResult {
                                                    readback_value: outcome.readback_value,
                                                    audit_seq: outcome.audit_seq,
                                                    timestamp_ns: outcome.timestamp_ns,
                                                },
                                            ),
                                        }),
                                        Err(WriteError::Denied { denial, .. }) => {
                                            Err(denial_to_fidl(denial))
                                        }
                                        Err(WriteError::Backend { .. }) => {
                                            Err(flab::OperationError::BackendFault)
                                        }
                                    },
                                }
                            };
                            match step {
                                Ok(res) => results.push(res),
                                Err(err) => {
                                    results.push(flab::SequenceItemResult {
                                        index: idx,
                                        ok: false,
                                        outcome: flab::SequenceItemOutcome::Error(err),
                                    });
                                    complete = false;
                                    break;
                                }
                            }
                        }
                        CoreSequenceItem::Barrier => {
                            let step = {
                                let guard = &mut *state.inner.lock().unwrap();
                                let ProxyState { sessions, executor, audit, .. } = guard;
                                match sessions.session(id) {
                                    None => Err(flab::OperationError::StaleIdentity),
                                    Some(_) => {
                                        let ts = now_ns();
                                        executor.barrier();
                                        let record = AuditRecord {
                                            session: Some(id),
                                            resource: None,
                                            operation: "barrier",
                                            offset: None,
                                            decision: Decision::Allowed,
                                            status: OpStatus::Ok,
                                            value: None,
                                            timestamp_ns: ts,
                                            run_id: None,
                                            item_index: Some(idx),
                                        };
                                        audit.append(record);
                                        Ok(flab::SequenceItemResult {
                                            index: idx,
                                            ok: true,
                                            outcome: flab::SequenceItemOutcome::Barrier(
                                                flab::Barrier,
                                            ),
                                        })
                                    }
                                }
                            };
                            match step {
                                Ok(res) => results.push(res),
                                Err(err) => {
                                    results.push(flab::SequenceItemResult {
                                        index: idx,
                                        ok: false,
                                        outcome: flab::SequenceItemOutcome::Error(err),
                                    });
                                    complete = false;
                                    break;
                                }
                            }
                        }
                        CoreSequenceItem::DelayNs(delay_ns) => {
                            if *delay_ns > 0 {
                                fuchsia_async::Timer::new(zx::MonotonicInstant::after(
                                    zx::Duration::from_nanos(*delay_ns),
                                ))
                                .await;
                            }
                            if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                                results.push(flab::SequenceItemResult {
                                    index: idx,
                                    ok: false,
                                    outcome: flab::SequenceItemOutcome::Error(
                                        flab::OperationError::NotAccepting,
                                    ),
                                });
                                complete = false;
                                break;
                            }
                            let step = {
                                let guard = &mut *state.inner.lock().unwrap();
                                let ProxyState { sessions, audit, .. } = guard;
                                match sessions.session(id) {
                                    None => Err(flab::OperationError::StaleIdentity),
                                    Some(_) => {
                                        let ts = now_ns();
                                        let record = AuditRecord {
                                            session: Some(id),
                                            resource: None,
                                            operation: "delay",
                                            offset: None,
                                            decision: Decision::Allowed,
                                            status: OpStatus::Ok,
                                            value: None,
                                            timestamp_ns: ts,
                                            run_id: None,
                                            item_index: Some(idx),
                                        };
                                        audit.append(record);
                                        Ok(flab::SequenceItemResult {
                                            index: idx,
                                            ok: true,
                                            outcome: flab::SequenceItemOutcome::DelayNs(
                                                flab::DelayNs,
                                            ),
                                        })
                                    }
                                }
                            };
                            match step {
                                Ok(res) => results.push(res),
                                Err(err) => {
                                    results.push(flab::SequenceItemResult {
                                        index: idx,
                                        ok: false,
                                        outcome: flab::SequenceItemOutcome::Error(err),
                                    });
                                    complete = false;
                                    break;
                                }
                            }
                        }
                        CoreSequenceItem::Poll32 {
                            resource,
                            offset,
                            expected,
                            mask,
                            interval_ns,
                            timeout_ns,
                        } => {
                            let start_ts = now_ns();
                            let deadline = start_ts.saturating_add(*timeout_ns);
                            let mut last_val = None;
                            let poll_res = loop {
                                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                                    break Err(flab::OperationError::NotAccepting);
                                }
                                if now_ns().saturating_sub(seq_start_ts) > max_sequence_duration {
                                    let guard = &mut *state.inner.lock().unwrap();
                                    let ProxyState { sessions, executor, audit, .. } = guard;
                                    if sessions.session(id).is_none() {
                                        break Err(flab::OperationError::StaleIdentity);
                                    } else {
                                        executor.poll32_timeout(
                                            audit,
                                            id,
                                            *resource,
                                            *offset,
                                            last_val,
                                            Some(idx),
                                        );
                                        break Err(flab::OperationError::Timeout);
                                    }
                                }
                                let step = {
                                    let guard = &mut *state.inner.lock().unwrap();
                                    let ProxyState { sessions, executor, audit, .. } = guard;
                                    match sessions.session(id) {
                                        None => Err(flab::OperationError::StaleIdentity),
                                        Some(_) => match executor.poll32_read_step(
                                            audit,
                                            id,
                                            *resource,
                                            *offset,
                                            *expected,
                                            *mask,
                                            Some(idx),
                                        ) {
                                            Ok(Ok(outcome)) => Ok(Some(outcome)),
                                            Ok(Err(val)) => {
                                                last_val = Some(val);
                                                Ok(None)
                                            }
                                            Err(PollError::Denied { denial, .. }) => {
                                                Err(denial_to_fidl(denial))
                                            }
                                            Err(PollError::Backend { .. }) => {
                                                Err(flab::OperationError::BackendFault)
                                            }
                                        },
                                    }
                                };
                                match step {
                                    Err(err) => break Err(err),
                                    Ok(Some(outcome)) => {
                                        break Ok(flab::SequenceItemResult {
                                            index: idx,
                                            ok: true,
                                            outcome: flab::SequenceItemOutcome::Poll32(
                                                flab::PollResult {
                                                    value: outcome.value,
                                                    audit_seq: outcome.audit_seq,
                                                    timestamp_ns: outcome.timestamp_ns,
                                                },
                                            ),
                                        });
                                    }
                                    Ok(None) => {
                                        let now = now_ns();
                                        if now >= deadline {
                                            let guard = &mut *state.inner.lock().unwrap();
                                            let ProxyState { sessions, executor, audit, .. } =
                                                guard;
                                            if sessions.session(id).is_none() {
                                                break Err(flab::OperationError::StaleIdentity);
                                            } else {
                                                executor.poll32_timeout(
                                                    audit,
                                                    id,
                                                    *resource,
                                                    *offset,
                                                    last_val,
                                                    Some(idx),
                                                );
                                                break Err(flab::OperationError::Timeout);
                                            }
                                        }
                                        let sleep_dur = if *interval_ns > 0 {
                                            let rem = deadline.saturating_sub(now);
                                            (*interval_ns).min(rem)
                                        } else {
                                            0
                                        };
                                        if sleep_dur > 0 {
                                            fuchsia_async::Timer::new(zx::MonotonicInstant::after(
                                                zx::Duration::from_nanos(sleep_dur),
                                            ))
                                            .await;
                                        } else {
                                            fuchsia_async::Timer::new(zx::MonotonicInstant::after(
                                                zx::Duration::from_nanos(1000),
                                            ))
                                            .await;
                                        }
                                        if state
                                            .abort_token
                                            .load(std::sync::atomic::Ordering::SeqCst)
                                        {
                                            break Err(flab::OperationError::NotAccepting);
                                        }
                                    }
                                }
                            };
                            match poll_res {
                                Ok(res) => results.push(res),
                                Err(err) => {
                                    results.push(flab::SequenceItemResult {
                                        index: idx,
                                        ok: false,
                                        outcome: flab::SequenceItemOutcome::Error(err),
                                    });
                                    complete = false;
                                    break;
                                }
                            }
                        }
                        CoreSequenceItem::GpioRead { resource } => {
                            let step = {
                                let guard = &mut *state.inner.lock().unwrap();
                                let ProxyState { sessions, executor, audit, .. } = guard;
                                match sessions.session(id) {
                                    None => Err(flab::OperationError::StaleIdentity),
                                    Some(session) => match executor.gpio_read_internal(
                                        &session.policy,
                                        audit,
                                        id,
                                        *resource,
                                        Some(idx),
                                    ) {
                                        Ok(outcome) => Ok(flab::SequenceItemResult {
                                            index: idx,
                                            ok: true,
                                            outcome: flab::SequenceItemOutcome::GpioRead(
                                                flab::GpioReadResult {
                                                    value: outcome.value,
                                                    audit_seq: outcome.audit_seq,
                                                    timestamp_ns: outcome.timestamp_ns,
                                                },
                                            ),
                                        }),
                                        Err(GpioReadError::Denied { denial, .. }) => {
                                            Err(denial_to_fidl(denial))
                                        }
                                        Err(GpioReadError::Backend { .. }) => {
                                            Err(flab::OperationError::BackendFault)
                                        }
                                    },
                                }
                            };
                            match step {
                                Ok(res) => results.push(res),
                                Err(err) => {
                                    results.push(flab::SequenceItemResult {
                                        index: idx,
                                        ok: false,
                                        outcome: flab::SequenceItemOutcome::Error(err),
                                    });
                                    complete = false;
                                    break;
                                }
                            }
                        }
                        CoreSequenceItem::GpioWrite { resource, value } => {
                            let step = {
                                let guard = &mut *state.inner.lock().unwrap();
                                let ProxyState { sessions, executor, audit, .. } = guard;
                                match sessions.session(id) {
                                    None => Err(flab::OperationError::StaleIdentity),
                                    Some(session) => match executor.gpio_write_internal(
                                        &session.policy,
                                        audit,
                                        id,
                                        *resource,
                                        *value,
                                        Some(idx),
                                    ) {
                                        Ok(outcome) => Ok(flab::SequenceItemResult {
                                            index: idx,
                                            ok: true,
                                            outcome: flab::SequenceItemOutcome::GpioWrite(
                                                flab::GpioWriteResult {
                                                    audit_seq: outcome.audit_seq,
                                                    timestamp_ns: outcome.timestamp_ns,
                                                },
                                            ),
                                        }),
                                        Err(GpioWriteError::Denied { denial, .. }) => {
                                            Err(denial_to_fidl(denial))
                                        }
                                        Err(GpioWriteError::Backend { .. }) => {
                                            Err(flab::OperationError::BackendFault)
                                        }
                                    },
                                }
                            };
                            match step {
                                Ok(res) => results.push(res),
                                Err(err) => {
                                    results.push(flab::SequenceItemResult {
                                        index: idx,
                                        ok: false,
                                        outcome: flab::SequenceItemOutcome::Error(err),
                                    });
                                    complete = false;
                                    break;
                                }
                            }
                        }
                        CoreSequenceItem::I2cTransfer { resource, write_data, read_length } => {
                            let step = {
                                let guard = &mut *state.inner.lock().unwrap();
                                let ProxyState { sessions, executor, audit, .. } = guard;
                                match sessions.session(id) {
                                    None => Err(flab::OperationError::StaleIdentity),
                                    Some(session) => match executor.i2c_transfer_internal(
                                        &session.policy,
                                        audit,
                                        id,
                                        *resource,
                                        write_data,
                                        *read_length,
                                        Some(idx),
                                    ) {
                                        Ok(outcome) => Ok(flab::SequenceItemResult {
                                            index: idx,
                                            ok: true,
                                            outcome: flab::SequenceItemOutcome::I2cTransfer(
                                                flab::I2cTransferResult {
                                                    read_data: outcome.read_data,
                                                    audit_seq: outcome.audit_seq,
                                                    timestamp_ns: outcome.timestamp_ns,
                                                },
                                            ),
                                        }),
                                        Err(I2cTransferError::Denied { denial, .. }) => {
                                            Err(denial_to_fidl(denial))
                                        }
                                        Err(I2cTransferError::Backend { .. }) => {
                                            Err(flab::OperationError::BackendFault)
                                        }
                                    },
                                }
                            };
                            match step {
                                Ok(res) => results.push(res),
                                Err(err) => {
                                    results.push(flab::SequenceItemResult {
                                        index: idx,
                                        ok: false,
                                        outcome: flab::SequenceItemOutcome::Error(err),
                                    });
                                    complete = false;
                                    break;
                                }
                            }
                        }
                        CoreSequenceItem::SpiTransmit { resource, tx_data } => {
                            let step = {
                                let guard = &mut *state.inner.lock().unwrap();
                                let ProxyState { sessions, executor, audit, .. } = guard;
                                match sessions.session(id) {
                                    None => Err(flab::OperationError::StaleIdentity),
                                    Some(session) => match executor.spi_transmit_internal(
                                        &session.policy,
                                        audit,
                                        id,
                                        *resource,
                                        tx_data,
                                        Some(idx),
                                    ) {
                                        Ok(outcome) => Ok(flab::SequenceItemResult {
                                            index: idx,
                                            ok: true,
                                            outcome: flab::SequenceItemOutcome::SpiTransmit(
                                                flab::SpiTransmitResult {
                                                    rx_data: outcome.rx_data,
                                                    audit_seq: outcome.audit_seq,
                                                    timestamp_ns: outcome.timestamp_ns,
                                                },
                                            ),
                                        }),
                                        Err(SpiTransmitError::Denied { denial, .. }) => {
                                            Err(denial_to_fidl(denial))
                                        }
                                        Err(SpiTransmitError::Backend { .. }) => {
                                            Err(flab::OperationError::BackendFault)
                                        }
                                    },
                                }
                            };
                            match step {
                                Ok(res) => results.push(res),
                                Err(err) => {
                                    results.push(flab::SequenceItemResult {
                                        index: idx,
                                        ok: false,
                                        outcome: flab::SequenceItemOutcome::Error(err),
                                    });
                                    complete = false;
                                    break;
                                }
                            }
                        }
                    }
                }

                let _ = responder.send(Ok((results.as_slice(), complete)));
            }
            flab::SessionRequest::ReadAudit { cursor, limit, responder } => {
                let (page, boot_id, proxy_generation) = {
                    let state = state.inner.lock().unwrap();
                    let limit = (limit.min(flab::MAX_AUDIT_PAGE_ENTRIES)) as usize;
                    let identity = state.sessions.identity();
                    (
                        state.audit.read(cursor, limit),
                        identity.boot_id.clone(),
                        identity.proxy_generation,
                    )
                };
                let entries: Vec<flab::AuditEntry> = page
                    .entries
                    .iter()
                    .map(|entry| flab::AuditEntry {
                        seq: Some(entry.seq),
                        session: entry.record.session,
                        resource: entry.record.resource,
                        operation: Some(entry.record.operation.to_string()),
                        offset: entry.record.offset,
                        decision: Some(match entry.record.decision {
                            Decision::Allowed => flab::AuditDecision::Allowed,
                            Decision::Denied(_) => flab::AuditDecision::Denied,
                        }),
                        denial: match entry.record.decision {
                            Decision::Allowed => None,
                            Decision::Denied(denial) => Some(denial_to_fidl(denial)),
                        },
                        status: Some(match entry.record.status {
                            OpStatus::Ok => flab::AuditStatus::Ok,
                            OpStatus::Rejected => flab::AuditStatus::Rejected,
                            OpStatus::BackendFault => flab::AuditStatus::BackendFault,
                        }),
                        value: entry.record.value,
                        timestamp_ns: Some(entry.record.timestamp_ns),
                        run_id: entry.record.run_id.clone(),
                        item_index: entry.record.item_index,
                        proxy_generation: Some(proxy_generation),
                        boot_id: Some(boot_id.clone()),
                        ..Default::default()
                    })
                    .collect();
                let _ = responder.send(
                    &entries,
                    page.oldest_retained.is_some(),
                    page.oldest_retained.unwrap_or(0),
                    page.next_cursor,
                );
            }
            flab::SessionRequest::GpioRead { resource, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_gpio_read(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .gpio_read(&session.policy, audit, id, resource)
                                .map(|outcome| {
                                    (outcome.value, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    GpioReadError::Denied { denial, .. } => denial_to_fidl(denial),
                                    GpioReadError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "gpio_read",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .gpio_read(&session.policy, audit, id, resource)
                                        .map(|outcome| {
                                            (outcome.value, outcome.audit_seq, outcome.timestamp_ns)
                                        })
                                        .map_err(|error| match error {
                                            GpioReadError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            GpioReadError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::GpioWrite { resource, value, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_gpio_write(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .gpio_write(&session.policy, audit, id, resource, value)
                                .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                .map_err(|error| match error {
                                    GpioWriteError::Denied { denial, .. } => denial_to_fidl(denial),
                                    GpioWriteError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "gpio_write",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .gpio_write(&session.policy, audit, id, resource, value)
                                        .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                        .map_err(|error| match error {
                                            GpioWriteError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            GpioWriteError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::I2cTransfer { resource, write_data, read_length, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => session
                            .policy
                            .check_i2c_transfer(resource, write_data.len(), read_length as usize)
                            .map_err(denial_to_fidl),
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .i2c_transfer(
                                    &session.policy,
                                    audit,
                                    id,
                                    resource,
                                    &write_data,
                                    read_length,
                                )
                                .map(|outcome| {
                                    (outcome.read_data, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    I2cTransferError::Denied { denial, .. } => {
                                        denial_to_fidl(denial)
                                    }
                                    I2cTransferError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "i2c_transfer",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .i2c_transfer(
                                            &session.policy,
                                            audit,
                                            id,
                                            resource,
                                            &write_data,
                                            read_length,
                                        )
                                        .map(|outcome| {
                                            (
                                                outcome.read_data,
                                                outcome.audit_seq,
                                                outcome.timestamp_ns,
                                            )
                                        })
                                        .map_err(|error| match error {
                                            I2cTransferError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            I2cTransferError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(
                    result
                        .as_ref()
                        .map(|(data, seq, ts)| (data.as_slice(), *seq, *ts))
                        .map_err(|err| *err),
                );
            }
            flab::SessionRequest::SpiTransmit { resource, tx_data, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => session
                            .policy
                            .check_spi_transmit(resource, tx_data.len())
                            .map_err(denial_to_fidl),
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .spi_transmit(&session.policy, audit, id, resource, &tx_data)
                                .map(|outcome| {
                                    (outcome.rx_data, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    SpiTransmitError::Denied { denial, .. } => {
                                        denial_to_fidl(denial)
                                    }
                                    SpiTransmitError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "spi_transmit",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .spi_transmit(
                                            &session.policy,
                                            audit,
                                            id,
                                            resource,
                                            &tx_data,
                                        )
                                        .map(|outcome| {
                                            (
                                                outcome.rx_data,
                                                outcome.audit_seq,
                                                outcome.timestamp_ns,
                                            )
                                        })
                                        .map_err(|error| match error {
                                            SpiTransmitError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            SpiTransmitError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(
                    result
                        .as_ref()
                        .map(|(data, seq, ts)| (data.as_slice(), *seq, *ts))
                        .map_err(|err| *err),
                );
            }
            flab::SessionRequest::OpenGpio { resource, endpoint, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let check = {
                    let guard = state.inner.lock().unwrap();
                    match guard.sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => match session.policy.resource(resource) {
                            None => Err(flab::OperationError::UnknownResource),
                            Some(res) => {
                                if res.kind != lab_proxy_core::access_policy::ResourceKind::Gpio {
                                    Err(flab::OperationError::UnsupportedMethod)
                                } else {
                                    Ok(())
                                }
                            }
                        },
                    }
                };
                match check {
                    Ok(()) => {
                        scope.spawn(serve_gpio_endpoint(
                            state.clone(),
                            id,
                            resource,
                            endpoint.into_stream(),
                        ));
                        let _ = responder.send(Ok(()));
                    }
                    Err(err) => {
                        let _ = responder.send(Err(err));
                    }
                }
            }
            flab::SessionRequest::OpenI2c { resource, endpoint, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let check = {
                    let guard = state.inner.lock().unwrap();
                    match guard.sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => match session.policy.resource(resource) {
                            None => Err(flab::OperationError::UnknownResource),
                            Some(res) => {
                                if res.kind != lab_proxy_core::access_policy::ResourceKind::I2c {
                                    Err(flab::OperationError::UnsupportedMethod)
                                } else {
                                    Ok(())
                                }
                            }
                        },
                    }
                };
                match check {
                    Ok(()) => {
                        scope.spawn(serve_i2c_endpoint(
                            state.clone(),
                            id,
                            resource,
                            endpoint.into_stream(),
                        ));
                        let _ = responder.send(Ok(()));
                    }
                    Err(err) => {
                        let _ = responder.send(Err(err));
                    }
                }
            }
            flab::SessionRequest::OpenSpi { resource, endpoint, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let check = {
                    let guard = state.inner.lock().unwrap();
                    match guard.sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => match session.policy.resource(resource) {
                            None => Err(flab::OperationError::UnknownResource),
                            Some(res) => {
                                if res.kind != lab_proxy_core::access_policy::ResourceKind::Spi {
                                    Err(flab::OperationError::UnsupportedMethod)
                                } else {
                                    Ok(())
                                }
                            }
                        },
                    }
                };
                match check {
                    Ok(()) => {
                        scope.spawn(serve_spi_endpoint(
                            state.clone(),
                            id,
                            resource,
                            endpoint.into_stream(),
                        ));
                        let _ = responder.send(Ok(()));
                    }
                    Err(err) => {
                        let _ = responder.send(Err(err));
                    }
                }
            }
            flab::SessionRequest::WaitForInterrupt {
                resource,
                after_sequence,
                timeout_ns,
                responder,
            } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }

                let (tx, rx) = futures::channel::oneshot::channel();

                let wait_init = {
                    let mut guard = state.inner.lock().unwrap();
                    let ProxyState { sessions, interrupts, audit, .. } = &mut *guard;
                    match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => match session.policy.check_interrupt(resource) {
                            Err(denial) => {
                                let record = AuditRecord {
                                    session: Some(id),
                                    resource: Some(resource),
                                    operation: "wait_for_interrupt",
                                    offset: None,
                                    decision: Decision::Denied(denial),
                                    status: OpStatus::Rejected,
                                    value: None,
                                    timestamp_ns: now_ns(),
                                    run_id: None,
                                    item_index: None,
                                };
                                audit.append(record);
                                Err(denial_to_fidl(denial))
                            }
                            Ok(()) => {
                                if let Err(denial) = sessions.check_deadline(id, timeout_ns) {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "wait_for_interrupt",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now_ns(),
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                } else if let Err(denial) =
                                    sessions.check_access(id, AccessClass::Interrupt, now_ns())
                                {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "wait_for_interrupt",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now_ns(),
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                } else {
                                    match interrupts.wait_for_interrupt(
                                        resource,
                                        after_sequence,
                                        Box::new(move |event| {
                                            let _ = tx.send(event);
                                        }),
                                    ) {
                                        Ok(Some(event)) => Ok(Some(event)),
                                        Ok(None) => Ok(None),
                                        Err(denial) => {
                                            let record = AuditRecord {
                                                session: Some(id),
                                                resource: Some(resource),
                                                operation: "wait_for_interrupt",
                                                offset: None,
                                                decision: Decision::Denied(denial),
                                                status: OpStatus::Rejected,
                                                value: None,
                                                timestamp_ns: now_ns(),
                                                run_id: None,
                                                item_index: None,
                                            };
                                            audit.append(record);
                                            Err(denial_to_fidl(denial))
                                        }
                                    }
                                }
                            }
                        },
                    }
                };

                let event_result = match wait_init {
                    Err(err) => Err(err),
                    Ok(Some(event)) => Ok(event),
                    Ok(None) => {
                        if timeout_ns <= 0 {
                            Err(flab::OperationError::Timeout)
                        } else {
                            futures::select! {
                                res = rx.fuse() => {
                                    match res {
                                        Ok(event) => Ok(event),
                                        Err(_) => Err(flab::OperationError::NotAccepting),
                                    }
                                }
                                _ = fuchsia_async::Timer::new(zx::MonotonicInstant::after(
                                    zx::Duration::from_nanos(timeout_ns),
                                )).fuse() => {
                                    Err(flab::OperationError::Timeout)
                                }
                            }
                        }
                    }
                };

                let fidl_res = match event_result {
                    Ok(event) => {
                        let mut guard = state.inner.lock().unwrap();
                        let record = AuditRecord {
                            session: Some(id),
                            resource: Some(resource),
                            operation: "wait_for_interrupt",
                            offset: None,
                            decision: Decision::Allowed,
                            status: OpStatus::Ok,
                            value: Some(event.count as u32),
                            timestamp_ns: event.timestamp_ns,
                            run_id: None,
                            item_index: None,
                        };
                        guard.audit.append(record);
                        Ok((
                            event.resource,
                            event.sequence,
                            event.count,
                            event.timestamp_ns,
                            event.coalesced_count,
                        ))
                    }
                    Err(err) => {
                        if err == flab::OperationError::Timeout {
                            let mut guard = state.inner.lock().unwrap();
                            let record = AuditRecord {
                                session: Some(id),
                                resource: Some(resource),
                                operation: "wait_for_interrupt",
                                offset: None,
                                decision: Decision::Denied(Denial::Timeout),
                                status: OpStatus::Rejected,
                                value: None,
                                timestamp_ns: now_ns(),
                                run_id: None,
                                item_index: None,
                            };
                            guard.audit.append(record);
                        }
                        Err(err)
                    }
                };

                let _ = responder.send(fidl_res);
            }
            flab::SessionRequest::ClockEnable { resource, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_clock_enable(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .clock_enable(&session.policy, audit, id, resource)
                                .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                .map_err(|error| match error {
                                    ClockOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ClockOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "clock_enable",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .clock_enable(&session.policy, audit, id, resource)
                                        .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                        .map_err(|error| match error {
                                            ClockOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ClockOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::ClockDisable { resource, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_clock_disable(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .clock_disable(&session.policy, audit, id, resource)
                                .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                .map_err(|error| match error {
                                    ClockOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ClockOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "clock_disable",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .clock_disable(&session.policy, audit, id, resource)
                                        .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                        .map_err(|error| match error {
                                            ClockOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ClockOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::ClockIsEnabled { resource, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_clock_is_enabled(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .clock_is_enabled(&session.policy, audit, id, resource)
                                .map(|outcome| {
                                    (outcome.enabled, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    ClockOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ClockOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "clock_is_enabled",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .clock_is_enabled(&session.policy, audit, id, resource)
                                        .map(|outcome| {
                                            (
                                                outcome.enabled,
                                                outcome.audit_seq,
                                                outcome.timestamp_ns,
                                            )
                                        })
                                        .map_err(|error| match error {
                                            ClockOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ClockOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::ClockSetRate { resource, hz, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_clock_set_rate(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .clock_set_rate(&session.policy, audit, id, resource, hz)
                                .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                .map_err(|error| match error {
                                    ClockOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ClockOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "clock_set_rate",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .clock_set_rate(&session.policy, audit, id, resource, hz)
                                        .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                        .map_err(|error| match error {
                                            ClockOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ClockOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::ClockQueryRate { resource, hz_in, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_clock_query_rate(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .clock_query_rate(&session.policy, audit, id, resource, hz_in)
                                .map(|outcome| {
                                    (outcome.hz_out, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    ClockOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ClockOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "clock_query_rate",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .clock_query_rate(
                                            &session.policy,
                                            audit,
                                            id,
                                            resource,
                                            hz_in,
                                        )
                                        .map(|outcome| {
                                            (
                                                outcome.hz_out,
                                                outcome.audit_seq,
                                                outcome.timestamp_ns,
                                            )
                                        })
                                        .map_err(|error| match error {
                                            ClockOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ClockOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::ClockGetRate { resource, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_clock_get_rate(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .clock_get_rate(&session.policy, audit, id, resource)
                                .map(|outcome| {
                                    (outcome.hz, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    ClockOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ClockOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "clock_get_rate",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .clock_get_rate(&session.policy, audit, id, resource)
                                        .map(|outcome| {
                                            (outcome.hz, outcome.audit_seq, outcome.timestamp_ns)
                                        })
                                        .map_err(|error| match error {
                                            ClockOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ClockOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::ResetAssert { resource, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_reset_assert(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .reset_assert(&session.policy, audit, id, resource)
                                .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                .map_err(|error| match error {
                                    ResetOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ResetOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "reset_assert",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .reset_assert(&session.policy, audit, id, resource)
                                        .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                        .map_err(|error| match error {
                                            ResetOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ResetOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::ResetDeassert { resource, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_reset_deassert(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .reset_deassert(&session.policy, audit, id, resource)
                                .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                .map_err(|error| match error {
                                    ResetOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ResetOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "reset_deassert",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .reset_deassert(&session.policy, audit, id, resource)
                                        .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                        .map_err(|error| match error {
                                            ResetOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ResetOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::ResetToggle { resource, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_reset_toggle(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .reset_toggle(&session.policy, audit, id, resource)
                                .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                .map_err(|error| match error {
                                    ResetOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ResetOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "reset_toggle",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .reset_toggle(&session.policy, audit, id, resource)
                                        .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                        .map_err(|error| match error {
                                            ResetOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ResetOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::ResetStatus { resource, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_reset_status(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .reset_status(&session.policy, audit, id, resource)
                                .map(|outcome| {
                                    (outcome.asserted, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    ResetOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    ResetOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "reset_status",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .reset_status(&session.policy, audit, id, resource)
                                        .map(|outcome| {
                                            (
                                                outcome.asserted,
                                                outcome.audit_seq,
                                                outcome.timestamp_ns,
                                            )
                                        })
                                        .map_err(|error| match error {
                                            ResetOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            ResetOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::SerialRead { resource, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_serial_read(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .serial_read(&session.policy, audit, id, resource)
                                .map(|outcome| {
                                    (outcome.data, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    SerialOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    SerialOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "serial_read",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .serial_read(&session.policy, audit, id, resource)
                                        .map(|outcome| {
                                            (outcome.data, outcome.audit_seq, outcome.timestamp_ns)
                                        })
                                        .map_err(|error| match error {
                                            SerialOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            SerialOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(
                    result
                        .as_ref()
                        .map(|(data, seq, ts)| (data.as_slice(), *seq, *ts))
                        .map_err(|err| *err),
                );
            }
            flab::SessionRequest::SerialWrite { resource, data, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => session
                            .policy
                            .check_serial_write(resource, data.len())
                            .map_err(denial_to_fidl),
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(id).unwrap();
                            executor
                                .serial_write(&session.policy, audit, id, resource, &data)
                                .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                .map_err(|error| match error {
                                    SerialOpError::Denied { denial, .. } => denial_to_fidl(denial),
                                    SerialOpError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(id),
                                        resource: Some(resource),
                                        operation: "serial_write",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(id).unwrap();
                                    executor
                                        .serial_write(&session.policy, audit, id, resource, &data)
                                        .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                        .map_err(|error| match error {
                                            SerialOpError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            SerialOpError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::SessionRequest::_UnknownMethod { .. } => {}
        }
    }

    // Channel closed: close the session, release the mutation lease, and
    // audit the closure.
    let mut state = state.inner.lock().unwrap();
    state.interrupts.cancel_waiters();
    let was_mutating =
        state.sessions.session(id).map(|s| s.mode == SessionMode::Mutating).unwrap_or(false);
    if state.sessions.close_session(id) {
        if was_mutating {
            let mut q_record = AuditRecord::lifecycle("quiesce_released", now_ns());
            q_record.session = Some(id);
            state.audit.append(q_record);
        }
        let mut record = AuditRecord::lifecycle("session_closed", now_ns());
        record.session = Some(id);
        state.audit.append(record);
    }
}

async fn serve_gpio_endpoint(
    state: SharedState,
    session_id: u64,
    resource: u32,
    mut stream: flab::GpioEndpointRequestStream,
) {
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            flab::GpioEndpointRequest::Read { responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(session_id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_gpio_read(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(session_id).unwrap();
                            executor
                                .gpio_read(&session.policy, audit, session_id, resource)
                                .map(|outcome| {
                                    (outcome.value, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    GpioReadError::Denied { denial, .. } => denial_to_fidl(denial),
                                    GpioReadError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(session_id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(session_id),
                                        resource: Some(resource),
                                        operation: "gpio_read",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(session_id).unwrap();
                                    executor
                                        .gpio_read(&session.policy, audit, session_id, resource)
                                        .map(|outcome| {
                                            (outcome.value, outcome.audit_seq, outcome.timestamp_ns)
                                        })
                                        .map_err(|error| match error {
                                            GpioReadError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            GpioReadError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::GpioEndpointRequest::Write { value, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(session_id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => {
                            session.policy.check_gpio_write(resource).map_err(denial_to_fidl)
                        }
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(session_id).unwrap();
                            executor
                                .gpio_write(&session.policy, audit, session_id, resource, value)
                                .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                .map_err(|error| match error {
                                    GpioWriteError::Denied { denial, .. } => denial_to_fidl(denial),
                                    GpioWriteError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(session_id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(session_id),
                                        resource: Some(resource),
                                        operation: "gpio_write",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(session_id).unwrap();
                                    executor
                                        .gpio_write(
                                            &session.policy,
                                            audit,
                                            session_id,
                                            resource,
                                            value,
                                        )
                                        .map(|outcome| (outcome.audit_seq, outcome.timestamp_ns))
                                        .map_err(|error| match error {
                                            GpioWriteError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            GpioWriteError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(result);
            }
            flab::GpioEndpointRequest::_UnknownMethod { .. } => {}
        }
    }
}

async fn serve_i2c_endpoint(
    state: SharedState,
    session_id: u64,
    resource: u32,
    mut stream: flab::I2cEndpointRequestStream,
) {
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            flab::I2cEndpointRequest::Transfer { write_data, read_length, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(session_id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => session
                            .policy
                            .check_i2c_transfer(resource, write_data.len(), read_length as usize)
                            .map_err(denial_to_fidl),
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(session_id).unwrap();
                            executor
                                .i2c_transfer(
                                    &session.policy,
                                    audit,
                                    session_id,
                                    resource,
                                    &write_data,
                                    read_length,
                                )
                                .map(|outcome| {
                                    (outcome.read_data, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    I2cTransferError::Denied { denial, .. } => {
                                        denial_to_fidl(denial)
                                    }
                                    I2cTransferError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(session_id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(session_id),
                                        resource: Some(resource),
                                        operation: "i2c_transfer",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(session_id).unwrap();
                                    executor
                                        .i2c_transfer(
                                            &session.policy,
                                            audit,
                                            session_id,
                                            resource,
                                            &write_data,
                                            read_length,
                                        )
                                        .map(|outcome| {
                                            (
                                                outcome.read_data,
                                                outcome.audit_seq,
                                                outcome.timestamp_ns,
                                            )
                                        })
                                        .map_err(|error| match error {
                                            I2cTransferError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            I2cTransferError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(
                    result
                        .as_ref()
                        .map(|(data, seq, ts)| (data.as_slice(), *seq, *ts))
                        .map_err(|err| *err),
                );
            }
            flab::I2cEndpointRequest::_UnknownMethod { .. } => {}
        }
    }
}

async fn serve_spi_endpoint(
    state: SharedState,
    session_id: u64,
    resource: u32,
    mut stream: flab::SpiEndpointRequestStream,
) {
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            flab::SpiEndpointRequest::Transmit { tx_data, responder } => {
                if state.abort_token.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = responder.send(Err(flab::OperationError::NotAccepting));
                    continue;
                }
                let result = {
                    let guard = &mut *state.inner.lock().unwrap();
                    let ProxyState { sessions, executor, audit, .. } = guard;
                    let check = match sessions.session(session_id) {
                        None => Err(flab::OperationError::StaleIdentity),
                        Some(session) => session
                            .policy
                            .check_spi_transmit(resource, tx_data.len())
                            .map_err(denial_to_fidl),
                    };
                    match check {
                        Err(flab::OperationError::StaleIdentity) => {
                            Err(flab::OperationError::StaleIdentity)
                        }
                        Err(_) => {
                            let session = sessions.session(session_id).unwrap();
                            executor
                                .spi_transmit(
                                    &session.policy,
                                    audit,
                                    session_id,
                                    resource,
                                    &tx_data,
                                )
                                .map(|outcome| {
                                    (outcome.rx_data, outcome.audit_seq, outcome.timestamp_ns)
                                })
                                .map_err(|error| match error {
                                    SpiTransmitError::Denied { denial, .. } => {
                                        denial_to_fidl(denial)
                                    }
                                    SpiTransmitError::Backend { .. } => {
                                        flab::OperationError::BackendFault
                                    }
                                })
                        }
                        Ok(_) => {
                            let now = now_ns();
                            match sessions.check_access(session_id, AccessClass::Protocol, now) {
                                Err(denial) => {
                                    let record = AuditRecord {
                                        session: Some(session_id),
                                        resource: Some(resource),
                                        operation: "spi_transmit",
                                        offset: None,
                                        decision: Decision::Denied(denial),
                                        status: OpStatus::Rejected,
                                        value: None,
                                        timestamp_ns: now,
                                        run_id: None,
                                        item_index: None,
                                    };
                                    audit.append(record);
                                    Err(denial_to_fidl(denial))
                                }
                                Ok(()) => {
                                    let session = sessions.session(session_id).unwrap();
                                    executor
                                        .spi_transmit(
                                            &session.policy,
                                            audit,
                                            session_id,
                                            resource,
                                            &tx_data,
                                        )
                                        .map(|outcome| {
                                            (
                                                outcome.rx_data,
                                                outcome.audit_seq,
                                                outcome.timestamp_ns,
                                            )
                                        })
                                        .map_err(|error| match error {
                                            SpiTransmitError::Denied { denial, .. } => {
                                                denial_to_fidl(denial)
                                            }
                                            SpiTransmitError::Backend { .. } => {
                                                flab::OperationError::BackendFault
                                            }
                                        })
                                }
                            }
                        }
                    }
                };
                let _ = responder.send(
                    result
                        .as_ref()
                        .map(|(data, seq, ts)| (data.as_slice(), *seq, *ts))
                        .map_err(|err| *err),
                );
            }
            flab::SpiEndpointRequest::_UnknownMethod { .. } => {}
        }
    }
}
