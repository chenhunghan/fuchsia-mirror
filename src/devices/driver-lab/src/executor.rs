// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Read-path executor: policy check, hardware access, and audit for scalar
//! reads and bounded snapshots.
//!
//! Every attempted operation is audited, including rejections, which never
//! touch hardware. A snapshot validates every item before the first read
//! and is an ordered series of reads, not an atomic hardware snapshot.

use crate::access_policy::{AccessClass, AccessPolicy, Denial, ResourceId, WritePrecondition};
use crate::audit_ring::{AuditRecord, AuditRing, Decision, OpStatus};
use crate::hardware_backend::{BackendError, Clock, Timer};
use crate::protocol_resource_adapter::ResourceBackend;
use std::collections::BTreeMap;

/// Instance-wide execution limits, derived from the target ceiling.
#[derive(Clone, Copy, Debug)]
pub struct ExecLimits {
    /// Maximum number of items in one snapshot.
    pub max_snapshot_items: usize,
    /// Maximum number of items in one sequence.
    pub max_sequence_items: usize,
    /// Maximum single delay in nanoseconds.
    pub max_delay_ns: i64,
    /// Maximum duration of an entire sequence in nanoseconds.
    pub max_sequence_duration_ns: i64,
}

impl Default for ExecLimits {
    fn default() -> Self {
        Self {
            max_snapshot_items: 64,
            max_sequence_items: 64,
            max_delay_ns: 5_000_000_000,             // 5 seconds
            max_sequence_duration_ns: 1_000_000_000, // 1 second
        }
    }
}

/// One requested snapshot read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotItem {
    /// Resource to read.
    pub resource: ResourceId,
    /// Byte offset from the start of the logical resource.
    pub offset: u64,
}

/// A completed scalar read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadOutcome {
    /// The value read.
    pub value: u32,
    /// Audit sequence assigned to the read.
    pub audit_seq: u64,
    /// Timestamp of the read.
    pub timestamp_ns: i64,
}

/// Why a scalar read did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// Policy rejected the read; hardware was not touched.
    Denied {
        /// The rejecting policy layer's reason.
        denial: Denial,
        /// Audit sequence of the rejection record.
        audit_seq: u64,
    },
    /// The backend failed after policy allowed the read.
    Backend {
        /// The backend error.
        error: BackendError,
        /// Audit sequence of the failure record.
        audit_seq: u64,
    },
}

/// The outcome of a completed 32-bit write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Value read back if readback was configured or requested.
    pub readback_value: u32,
    /// Audit sequence assigned to the write.
    pub audit_seq: u64,
    /// Timestamp of the write.
    pub timestamp_ns: i64,
}

/// Why a 32-bit write did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// Policy rejected the write; hardware was not touched.
    Denied {
        /// The rejecting policy layer's reason.
        denial: Denial,
        /// Audit sequence of the rejection record.
        audit_seq: u64,
    },
    /// The backend failed after policy allowed the write.
    Backend {
        /// The backend error.
        error: BackendError,
        /// Audit sequence of the failure record.
        audit_seq: u64,
    },
}

/// The outcome of a completed 32-bit poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollOutcome {
    /// Final value read from the register.
    pub value: u32,
    /// Audit sequence assigned to the matching read.
    pub audit_seq: u64,
    /// Timestamp of the matching read.
    pub timestamp_ns: i64,
}

/// Why a 32-bit poll did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollError {
    /// Policy rejected the poll or the poll timed out.
    Denied {
        /// The rejection or timeout reason.
        denial: Denial,
        /// Audit sequence of the rejection record.
        audit_seq: u64,
    },
    /// The backend failed during polling.
    Backend {
        /// The backend error.
        error: BackendError,
        /// Audit sequence of the failure record.
        audit_seq: u64,
    },
}

/// The outcome of a completed GPIO read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpioReadOutcome {
    pub value: bool,
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// Why a GPIO read did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpioReadError {
    Denied { denial: Denial, audit_seq: u64 },
    Backend { error: BackendError, audit_seq: u64 },
}

/// The outcome of a completed GPIO write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpioWriteOutcome {
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// Why a GPIO write did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpioWriteError {
    Denied { denial: Denial, audit_seq: u64 },
    Backend { error: BackendError, audit_seq: u64 },
}

/// The outcome of a completed I2C transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cTransferOutcome {
    pub read_data: Vec<u8>,
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// Why an I2C transfer did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum I2cTransferError {
    Denied { denial: Denial, audit_seq: u64 },
    Backend { error: BackendError, audit_seq: u64 },
}

/// The outcome of a completed SPI transmit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpiTransmitOutcome {
    pub rx_data: Vec<u8>,
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// Why a SPI transmit did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpiTransmitError {
    Denied { denial: Denial, audit_seq: u64 },
    Backend { error: BackendError, audit_seq: u64 },
}

/// The outcome of a completed Clock enable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockEnableOutcome {
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// The outcome of a completed Clock disable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockDisableOutcome {
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// The outcome of a completed Clock is_enabled query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockIsEnabledOutcome {
    pub enabled: bool,
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// The outcome of a completed Clock set_rate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockSetRateOutcome {
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// The outcome of a completed Clock query_rate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockQueryRateOutcome {
    pub hz_out: u64,
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// The outcome of a completed Clock get_rate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockGetRateOutcome {
    pub hz: u64,
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// Why a Clock operation did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockOpError {
    Denied { denial: Denial, audit_seq: u64 },
    Backend { error: BackendError, audit_seq: u64 },
}

/// The outcome of a completed Reset assert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetAssertOutcome {
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// The outcome of a completed Reset deassert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetDeassertOutcome {
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// The outcome of a completed Reset toggle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetToggleOutcome {
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// The outcome of a completed Reset status query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetStatusOutcome {
    pub asserted: bool,
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// Why a Reset operation did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetOpError {
    Denied { denial: Denial, audit_seq: u64 },
    Backend { error: BackendError, audit_seq: u64 },
}

/// The outcome of a completed Serial read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialReadOutcome {
    pub data: Vec<u8>,
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// The outcome of a completed Serial write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialWriteOutcome {
    pub audit_seq: u64,
    pub timestamp_ns: i64,
}

/// Why a Serial operation did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialOpError {
    Denied { denial: Denial, audit_seq: u64 },
    Backend { error: BackendError, audit_seq: u64 },
}

/// An operation within a bounded sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SequenceItem {
    Read32 {
        resource: ResourceId,
        offset: u64,
    },
    Write32 {
        resource: ResourceId,
        offset: u64,
        value: u32,
        write_mask: u32,
        precondition: Option<WritePrecondition>,
        readback: bool,
    },
    Poll32 {
        resource: ResourceId,
        offset: u64,
        expected: u32,
        mask: u32,
        interval_ns: i64,
        timeout_ns: i64,
    },
    DelayNs(i64),
    Barrier,
    GpioRead {
        resource: ResourceId,
    },
    GpioWrite {
        resource: ResourceId,
        value: bool,
    },
    I2cTransfer {
        resource: ResourceId,
        write_data: Vec<u8>,
        read_length: u32,
    },
    SpiTransmit {
        resource: ResourceId,
        tx_data: Vec<u8>,
    },
}

/// The outcome of one sequence item that completed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SequenceItemOutcome {
    Read32(ReadOutcome),
    Write32(WriteOutcome),
    Poll32(PollOutcome),
    DelayNs,
    Barrier,
    Error(Denial),
    Backend(BackendError),
    GpioRead(GpioReadOutcome),
    GpioWrite(GpioWriteOutcome),
    I2cTransfer(I2cTransferOutcome),
    SpiTransmit(SpiTransmitOutcome),
}

/// Result of one sequence item that began execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequenceItemResult {
    pub index: u32,
    pub ok: bool,
    pub outcome: SequenceItemOutcome,
}

/// Outcome of a sequence whose prevalidation succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequenceOutcome {
    pub results: Vec<SequenceItemResult>,
    pub complete: bool,
}

/// Why a sequence was rejected before any hardware access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceError {
    TooManyItems { max: usize, audit_seq: u64 },
    Rejected { index: usize, denial: Denial, audit_seq: u64 },
}

/// The per-item result of a snapshot read that began execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotItemResult {
    /// The requested item.
    pub item: SnapshotItem,
    /// The value read, or the backend error.
    pub value: Result<u32, BackendError>,
    /// Audit sequence assigned to this item.
    pub audit_seq: u64,
    /// Timestamp of this item's read.
    pub timestamp_ns: i64,
}

/// Outcome of a snapshot whose prevalidation succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotOutcome {
    /// One result per item that began execution, in request order.
    pub results: Vec<SnapshotItemResult>,
    /// Whether every requested item completed successfully. When false the
    /// snapshot stopped at the first backend failure and later items never
    /// began.
    pub complete: bool,
}

/// Why a snapshot was rejected before any hardware access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// More items than the execution limit permits.
    TooManyItems {
        /// The enforced limit.
        max: usize,
        /// Audit sequence of the rejection record.
        audit_seq: u64,
    },
    /// An item failed prevalidation; no item was executed.
    Rejected {
        /// Index of the first offending item.
        index: usize,
        /// The rejecting policy layer's reason.
        denial: Denial,
        /// Audit sequence of the rejection record.
        audit_seq: u64,
    },
}

/// Read-path and sequence executor shared by all sessions. Each call takes the
/// calling session's validated [`AccessPolicy`].
#[derive(Debug)]
pub struct Executor<B, C> {
    backends: BTreeMap<ResourceId, B>,
    clock: C,
    limits: ExecLimits,
}

impl<B: ResourceBackend, C: Clock> Executor<B, C> {
    /// Creates an executor over the instance's backends.
    pub fn new(backends: BTreeMap<ResourceId, B>, clock: C, limits: ExecLimits) -> Self {
        Self { backends, clock, limits }
    }

    /// The execution limits, for capability reporting.
    pub fn limits(&self) -> ExecLimits {
        self.limits
    }

    /// The backend for `resource`, for diagnostics and tests.
    pub fn backend(&self, resource: ResourceId) -> Option<&B> {
        self.backends.get(&resource)
    }

    /// Reads a register once directly from the backend without policy or audit.
    pub fn poll_read_once(
        &mut self,
        resource: ResourceId,
        offset: u64,
    ) -> Result<u32, BackendError> {
        let backend = self.backends.get_mut(&resource).ok_or(BackendError::Fault)?;
        backend.read32(offset)
    }

    /// Invokes hardware MMIO barriers across all backends.
    pub fn barrier(&mut self) {
        for backend in self.backends.values_mut() {
            backend.barrier();
        }
    }

    /// Performs one policy-checked, audited 32-bit read.
    pub fn read32(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        offset: u64,
    ) -> Result<ReadOutcome, ReadError> {
        self.read32_internal(policy, audit, session, resource, offset, None)
    }

    /// Internal 32-bit read implementation supporting sequence item indexing.
    pub fn read32_internal(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        offset: u64,
        item_index: Option<u32>,
    ) -> Result<ReadOutcome, ReadError> {
        let timestamp_ns = self.clock.now_ns();
        let access_class =
            if item_index.is_some() { AccessClass::Sequence } else { AccessClass::ReadOnce };
        if let Err(denial) = policy.check_read32(resource, offset, access_class) {
            let mut record = op_record(
                session,
                resource,
                "read32",
                offset,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(ReadError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let mut record = op_record(
                session,
                resource,
                "read32",
                offset,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(ReadError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.read32(offset) {
            Ok(value) => {
                let mut record = op_record(
                    session,
                    resource,
                    "read32",
                    offset,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some(value),
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                Ok(ReadOutcome { value, audit_seq, timestamp_ns })
            }
            Err(error) => {
                let mut record = op_record(
                    session,
                    resource,
                    "read32",
                    offset,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                Err(ReadError::Backend { error, audit_seq })
            }
        }
    }

    /// Performs one policy-checked, audited 32-bit masked write.
    pub fn write32(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        offset: u64,
        value: u32,
        write_mask: u32,
        precondition: Option<WritePrecondition>,
        readback: bool,
    ) -> Result<WriteOutcome, WriteError> {
        self.write32_internal(
            policy,
            audit,
            session,
            resource,
            offset,
            value,
            write_mask,
            precondition,
            readback,
            None,
        )
    }

    /// Internal 32-bit write implementation supporting sequence item indexing.
    pub fn write32_internal(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        offset: u64,
        value: u32,
        write_mask: u32,
        precondition: Option<WritePrecondition>,
        readback: bool,
        item_index: Option<u32>,
    ) -> Result<WriteOutcome, WriteError> {
        let timestamp_ns = self.clock.now_ns();
        let reg = match policy.check_write32(resource, offset, write_mask, precondition.as_ref()) {
            Ok(r) => *r,
            Err(denial) => {
                let mut record = op_record(
                    session,
                    resource,
                    "write32",
                    offset,
                    Decision::Denied(denial),
                    OpStatus::Rejected,
                    None,
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                return Err(WriteError::Denied { denial, audit_seq });
            }
        };

        let Some(backend) = self.backends.get_mut(&resource) else {
            let mut record = op_record(
                session,
                resource,
                "write32",
                offset,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(WriteError::Backend { error: BackendError::Fault, audit_seq });
        };

        let needs_before_read =
            write_mask != 0xFFFF_FFFF || precondition.is_some() || reg.require_precondition;
        let mut before_val = 0u32;
        if needs_before_read {
            let read_ts = self.clock.now_ns();
            match backend.read32(offset) {
                Ok(val) => {
                    before_val = val;
                    let mut record = op_record(
                        session,
                        resource,
                        "write32_before_read",
                        offset,
                        Decision::Allowed,
                        OpStatus::Ok,
                        Some(val),
                        read_ts,
                    );
                    record.item_index = item_index;
                    audit.append(record);
                }
                Err(error) => {
                    let mut record = op_record(
                        session,
                        resource,
                        "write32_before_read",
                        offset,
                        Decision::Allowed,
                        OpStatus::BackendFault,
                        None,
                        read_ts,
                    );
                    record.item_index = item_index;
                    let audit_seq = audit.append(record);
                    return Err(WriteError::Backend { error, audit_seq });
                }
            }
        }

        if let Some(pre) = precondition {
            if (before_val & pre.mask) != (pre.expected & pre.mask) {
                let fail_ts = self.clock.now_ns();
                let mut record = op_record(
                    session,
                    resource,
                    "write32",
                    offset,
                    Decision::Denied(Denial::PreconditionFailed),
                    OpStatus::Rejected,
                    Some(before_val),
                    fail_ts,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                return Err(WriteError::Denied { denial: Denial::PreconditionFailed, audit_seq });
            }
        }

        let final_val = if write_mask == 0xFFFF_FFFF {
            value
        } else {
            (before_val & !write_mask) | (value & write_mask)
        };

        let write_ts = self.clock.now_ns();
        if let Err(error) = backend.write32(offset, final_val) {
            let mut record = op_record(
                session,
                resource,
                "write32",
                offset,
                Decision::Allowed,
                OpStatus::BackendFault,
                Some(final_val),
                write_ts,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(WriteError::Backend { error, audit_seq });
        }

        let mut record = op_record(
            session,
            resource,
            "write32",
            offset,
            Decision::Allowed,
            OpStatus::Ok,
            Some(final_val),
            write_ts,
        );
        record.item_index = item_index;
        let audit_seq = audit.append(record);

        backend.barrier();

        let do_readback = readback || reg.readback;
        let readback_val = if do_readback {
            let rb_ts = self.clock.now_ns();
            match backend.read32(offset) {
                Ok(rb) => {
                    let mut rb_record = op_record(
                        session,
                        resource,
                        "write32_readback",
                        offset,
                        Decision::Allowed,
                        OpStatus::Ok,
                        Some(rb),
                        rb_ts,
                    );
                    rb_record.item_index = item_index;
                    audit.append(rb_record);
                    rb
                }
                Err(error) => {
                    let mut rb_record = op_record(
                        session,
                        resource,
                        "write32_readback",
                        offset,
                        Decision::Allowed,
                        OpStatus::BackendFault,
                        None,
                        rb_ts,
                    );
                    rb_record.item_index = item_index;
                    let rb_audit_seq = audit.append(rb_record);
                    return Err(WriteError::Backend { error, audit_seq: rb_audit_seq });
                }
            }
        } else {
            0
        };

        Ok(WriteOutcome { readback_value: readback_val, audit_seq, timestamp_ns: write_ts })
    }

    /// Evaluates one polling read step against expected value and mask.
    pub fn poll32_read_step(
        &mut self,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        offset: u64,
        expected: u32,
        mask: u32,
        item_index: Option<u32>,
    ) -> Result<Result<PollOutcome, u32>, PollError> {
        let timestamp_ns = self.clock.now_ns();
        let val = match self.poll_read_once(resource, offset) {
            Ok(v) => v,
            Err(error) => {
                let mut record = op_record(
                    session,
                    resource,
                    "poll32",
                    offset,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                return Err(PollError::Backend { error, audit_seq });
            }
        };
        if (val & mask) == (expected & mask) {
            let mut record = op_record(
                session,
                resource,
                "poll32",
                offset,
                Decision::Allowed,
                OpStatus::Ok,
                Some(val),
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            Ok(Ok(PollOutcome { value: val, audit_seq, timestamp_ns }))
        } else {
            Ok(Err(val))
        }
    }

    /// Records an audit rejection when polling times out.
    pub fn poll32_timeout(
        &mut self,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        offset: u64,
        last_val: Option<u32>,
        item_index: Option<u32>,
    ) -> PollError {
        let timestamp_ns = self.clock.now_ns();
        let mut record = op_record(
            session,
            resource,
            "poll32",
            offset,
            Decision::Denied(Denial::Timeout),
            OpStatus::Rejected,
            last_val,
            timestamp_ns,
        );
        record.item_index = item_index;
        let audit_seq = audit.append(record);
        PollError::Denied { denial: Denial::Timeout, audit_seq }
    }

    /// Validates whole sequence before any hardware execution.
    pub fn prevalidate_sequence(
        &self,
        policy: &AccessPolicy,
        items: &[SequenceItem],
    ) -> Result<(), (usize, Denial)> {
        if items.len() > self.limits.max_sequence_items {
            return Err((0, Denial::LimitExceeded));
        }
        for (index, item) in items.iter().enumerate() {
            match item {
                SequenceItem::Read32 { resource, offset } => {
                    policy
                        .check_read32(*resource, *offset, AccessClass::Sequence)
                        .map_err(|d| (index, d))?;
                }
                SequenceItem::Write32 { resource, offset, write_mask, precondition, .. } => {
                    policy
                        .check_write32(*resource, *offset, *write_mask, precondition.as_ref())
                        .map_err(|d| (index, d))?;
                }
                SequenceItem::Poll32 {
                    resource, offset, mask, interval_ns, timeout_ns, ..
                } => {
                    policy
                        .check_poll32(*resource, *offset, *mask, *interval_ns, *timeout_ns)
                        .map_err(|d| (index, d))?;
                    if *timeout_ns > self.limits.max_delay_ns {
                        return Err((index, Denial::LimitExceeded));
                    }
                }
                SequenceItem::DelayNs(delay_ns) => {
                    if *delay_ns < 0 || *delay_ns > self.limits.max_delay_ns {
                        return Err((index, Denial::LimitExceeded));
                    }
                }
                SequenceItem::Barrier => {}
                SequenceItem::GpioRead { resource } => {
                    policy.check_gpio_read(*resource).map_err(|d| (index, d))?;
                }
                SequenceItem::GpioWrite { resource, .. } => {
                    policy.check_gpio_write(*resource).map_err(|d| (index, d))?;
                }
                SequenceItem::I2cTransfer { resource, write_data, read_length } => {
                    policy
                        .check_i2c_transfer(*resource, write_data.len(), *read_length as usize)
                        .map_err(|d| (index, d))?;
                }
                SequenceItem::SpiTransmit { resource, tx_data } => {
                    policy.check_spi_transmit(*resource, tx_data.len()).map_err(|d| (index, d))?;
                }
            }
        }
        Ok(())
    }

    /// Performs a bounded snapshot: every item is validated before the
    /// first read, then items execute in order, stopping at the first
    /// backend failure.
    pub fn snapshot(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        items: &[SnapshotItem],
    ) -> Result<SnapshotOutcome, SnapshotError> {
        if items.len() > self.limits.max_snapshot_items {
            let timestamp_ns = self.clock.now_ns();
            let audit_seq = audit.append(AuditRecord {
                session: Some(session),
                resource: None,
                operation: "snapshot",
                offset: None,
                decision: Decision::Denied(Denial::LimitExceeded),
                status: OpStatus::Rejected,
                value: None,
                timestamp_ns,
                run_id: None,
                item_index: None,
            });
            return Err(SnapshotError::TooManyItems {
                max: self.limits.max_snapshot_items,
                audit_seq,
            });
        }
        for (index, item) in items.iter().enumerate() {
            if let Err(denial) =
                policy.check_read32(item.resource, item.offset, AccessClass::Snapshot)
            {
                let timestamp_ns = self.clock.now_ns();
                let mut record = op_record(
                    session,
                    item.resource,
                    "snapshot",
                    item.offset,
                    Decision::Denied(denial),
                    OpStatus::Rejected,
                    None,
                    timestamp_ns,
                );
                record.item_index = Some(index as u32);
                let audit_seq = audit.append(record);
                return Err(SnapshotError::Rejected { index, denial, audit_seq });
            }
        }
        let mut results = Vec::with_capacity(items.len());
        let mut complete = true;
        for (index, item) in items.iter().enumerate() {
            let timestamp_ns = self.clock.now_ns();
            let value = match self.backends.get_mut(&item.resource) {
                Some(backend) => backend.read32(item.offset),
                None => Err(BackendError::Fault),
            };
            let (status, audited_value) = match value {
                Ok(read) => (OpStatus::Ok, Some(read)),
                Err(_) => (OpStatus::BackendFault, None),
            };
            let mut record = op_record(
                session,
                item.resource,
                "snapshot_read32",
                item.offset,
                Decision::Allowed,
                status,
                audited_value,
                timestamp_ns,
            );
            record.item_index = Some(index as u32);
            let audit_seq = audit.append(record);
            results.push(SnapshotItemResult { item: *item, value, audit_seq, timestamp_ns });
            if value.is_err() {
                complete = false;
                break;
            }
        }
        Ok(SnapshotOutcome { results, complete })
    }

    /// Performs one policy-checked, audited GPIO read.
    pub fn gpio_read(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
    ) -> Result<GpioReadOutcome, GpioReadError> {
        self.gpio_read_internal(policy, audit, session, resource, None)
    }

    /// Internal GPIO read implementation supporting sequence item indexing.
    pub fn gpio_read_internal(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        item_index: Option<u32>,
    ) -> Result<GpioReadOutcome, GpioReadError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_gpio_read(resource) {
            let mut record = op_record(
                session,
                resource,
                "gpio_read",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(GpioReadError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let mut record = op_record(
                session,
                resource,
                "gpio_read",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(GpioReadError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.gpio_read() {
            Ok(value) => {
                let mut record = op_record(
                    session,
                    resource,
                    "gpio_read",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some(value as u32),
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                Ok(GpioReadOutcome { value, audit_seq, timestamp_ns })
            }
            Err(error) => {
                let mut record = op_record(
                    session,
                    resource,
                    "gpio_read",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                Err(GpioReadError::Backend { error, audit_seq })
            }
        }
    }

    /// Performs one policy-checked, audited GPIO write.
    pub fn gpio_write(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        value: bool,
    ) -> Result<GpioWriteOutcome, GpioWriteError> {
        self.gpio_write_internal(policy, audit, session, resource, value, None)
    }

    /// Internal GPIO write implementation supporting sequence item indexing.
    pub fn gpio_write_internal(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        value: bool,
        item_index: Option<u32>,
    ) -> Result<GpioWriteOutcome, GpioWriteError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_gpio_write(resource) {
            let mut record = op_record(
                session,
                resource,
                "gpio_write",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                Some(value as u32),
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(GpioWriteError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let mut record = op_record(
                session,
                resource,
                "gpio_write",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                Some(value as u32),
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(GpioWriteError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.gpio_write(value) {
            Ok(()) => {
                let mut record = op_record(
                    session,
                    resource,
                    "gpio_write",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some(value as u32),
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                Ok(GpioWriteOutcome { audit_seq, timestamp_ns })
            }
            Err(error) => {
                let mut record = op_record(
                    session,
                    resource,
                    "gpio_write",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    Some(value as u32),
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                Err(GpioWriteError::Backend { error, audit_seq })
            }
        }
    }

    /// Performs one policy-checked, audited I2C transfer.
    pub fn i2c_transfer(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        write_data: &[u8],
        read_length: u32,
    ) -> Result<I2cTransferOutcome, I2cTransferError> {
        self.i2c_transfer_internal(policy, audit, session, resource, write_data, read_length, None)
    }

    /// Internal I2C transfer implementation supporting sequence item indexing.
    pub fn i2c_transfer_internal(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        write_data: &[u8],
        read_length: u32,
        item_index: Option<u32>,
    ) -> Result<I2cTransferOutcome, I2cTransferError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) =
            policy.check_i2c_transfer(resource, write_data.len(), read_length as usize)
        {
            let mut record = op_record(
                session,
                resource,
                "i2c_transfer",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(I2cTransferError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let mut record = op_record(
                session,
                resource,
                "i2c_transfer",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(I2cTransferError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.i2c_transfer(write_data, read_length as usize) {
            Ok(read_data) => {
                let mut record = op_record(
                    session,
                    resource,
                    "i2c_transfer",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some(read_data.len() as u32),
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                Ok(I2cTransferOutcome { read_data, audit_seq, timestamp_ns })
            }
            Err(error) => {
                let mut record = op_record(
                    session,
                    resource,
                    "i2c_transfer",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                Err(I2cTransferError::Backend { error, audit_seq })
            }
        }
    }

    /// Performs one policy-checked, audited SPI transmit.
    pub fn spi_transmit(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        tx_data: &[u8],
    ) -> Result<SpiTransmitOutcome, SpiTransmitError> {
        self.spi_transmit_internal(policy, audit, session, resource, tx_data, None)
    }

    /// Internal SPI transmit implementation supporting sequence item indexing.
    pub fn spi_transmit_internal(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        tx_data: &[u8],
        item_index: Option<u32>,
    ) -> Result<SpiTransmitOutcome, SpiTransmitError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_spi_transmit(resource, tx_data.len()) {
            let mut record = op_record(
                session,
                resource,
                "spi_transmit",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(SpiTransmitError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let mut record = op_record(
                session,
                resource,
                "spi_transmit",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(SpiTransmitError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.spi_transmit(tx_data) {
            Ok(rx_data) => {
                let mut record = op_record(
                    session,
                    resource,
                    "spi_transmit",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some(rx_data.len() as u32),
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                Ok(SpiTransmitOutcome { rx_data, audit_seq, timestamp_ns })
            }
            Err(error) => {
                let mut record = op_record(
                    session,
                    resource,
                    "spi_transmit",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                Err(SpiTransmitError::Backend { error, audit_seq })
            }
        }
    }

    pub fn clock_enable(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
    ) -> Result<ClockEnableOutcome, ClockOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_clock_enable(resource) {
            let record = op_record(
                session,
                resource,
                "clock_enable",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "clock_enable",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.clock_enable() {
            Ok(()) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_enable",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(ClockEnableOutcome { audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_enable",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(ClockOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn clock_disable(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
    ) -> Result<ClockDisableOutcome, ClockOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_clock_disable(resource) {
            let record = op_record(
                session,
                resource,
                "clock_disable",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "clock_disable",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.clock_disable() {
            Ok(()) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_disable",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(ClockDisableOutcome { audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_disable",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(ClockOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn clock_is_enabled(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
    ) -> Result<ClockIsEnabledOutcome, ClockOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_clock_is_enabled(resource) {
            let record = op_record(
                session,
                resource,
                "clock_is_enabled",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "clock_is_enabled",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.clock_is_enabled() {
            Ok(enabled) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_is_enabled",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some(enabled as u32),
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(ClockIsEnabledOutcome { enabled, audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_is_enabled",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(ClockOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn clock_set_rate(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        hz: u64,
    ) -> Result<ClockSetRateOutcome, ClockOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_clock_set_rate(resource) {
            let record = op_record(
                session,
                resource,
                "clock_set_rate",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "clock_set_rate",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.clock_set_rate(hz) {
            Ok(()) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_set_rate",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some((hz & 0xffffffff) as u32),
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(ClockSetRateOutcome { audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_set_rate",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(ClockOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn clock_query_rate(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        hz_in: u64,
    ) -> Result<ClockQueryRateOutcome, ClockOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_clock_query_rate(resource) {
            let record = op_record(
                session,
                resource,
                "clock_query_rate",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "clock_query_rate",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.clock_query_rate(hz_in) {
            Ok(hz_out) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_query_rate",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some((hz_out & 0xffffffff) as u32),
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(ClockQueryRateOutcome { hz_out, audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_query_rate",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(ClockOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn clock_get_rate(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
    ) -> Result<ClockGetRateOutcome, ClockOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_clock_get_rate(resource) {
            let record = op_record(
                session,
                resource,
                "clock_get_rate",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "clock_get_rate",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ClockOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.clock_get_rate() {
            Ok(hz) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_get_rate",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some((hz & 0xffffffff) as u32),
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(ClockGetRateOutcome { hz, audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "clock_get_rate",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(ClockOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn reset_assert(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
    ) -> Result<ResetAssertOutcome, ResetOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_reset_assert(resource) {
            let record = op_record(
                session,
                resource,
                "reset_assert",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ResetOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "reset_assert",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ResetOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.reset_assert() {
            Ok(()) => {
                let record = op_record(
                    session,
                    resource,
                    "reset_assert",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(ResetAssertOutcome { audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "reset_assert",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(ResetOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn reset_deassert(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
    ) -> Result<ResetDeassertOutcome, ResetOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_reset_deassert(resource) {
            let record = op_record(
                session,
                resource,
                "reset_deassert",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ResetOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "reset_deassert",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ResetOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.reset_deassert() {
            Ok(()) => {
                let record = op_record(
                    session,
                    resource,
                    "reset_deassert",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(ResetDeassertOutcome { audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "reset_deassert",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(ResetOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn reset_toggle(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
    ) -> Result<ResetToggleOutcome, ResetOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_reset_toggle(resource) {
            let record = op_record(
                session,
                resource,
                "reset_toggle",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ResetOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "reset_toggle",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ResetOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.reset_toggle() {
            Ok(()) => {
                let record = op_record(
                    session,
                    resource,
                    "reset_toggle",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(ResetToggleOutcome { audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "reset_toggle",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(ResetOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn reset_status(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
    ) -> Result<ResetStatusOutcome, ResetOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_reset_status(resource) {
            let record = op_record(
                session,
                resource,
                "reset_status",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ResetOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "reset_status",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(ResetOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.reset_status() {
            Ok(asserted) => {
                let record = op_record(
                    session,
                    resource,
                    "reset_status",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some(asserted as u32),
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(ResetStatusOutcome { asserted, audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "reset_status",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(ResetOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn serial_read(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
    ) -> Result<SerialReadOutcome, SerialOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_serial_read(resource) {
            let record = op_record(
                session,
                resource,
                "serial_read",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(SerialOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "serial_read",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(SerialOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.serial_read() {
            Ok(data) => {
                let record = op_record(
                    session,
                    resource,
                    "serial_read",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some(data.len() as u32),
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(SerialReadOutcome { data, audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "serial_read",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(SerialOpError::Backend { error, audit_seq })
            }
        }
    }

    pub fn serial_write(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        data: &[u8],
    ) -> Result<SerialWriteOutcome, SerialOpError> {
        let timestamp_ns = self.clock.now_ns();
        if let Err(denial) = policy.check_serial_write(resource, data.len()) {
            let record = op_record(
                session,
                resource,
                "serial_write",
                0,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(SerialOpError::Denied { denial, audit_seq });
        }
        let Some(backend) = self.backends.get_mut(&resource) else {
            let record = op_record(
                session,
                resource,
                "serial_write",
                0,
                Decision::Allowed,
                OpStatus::BackendFault,
                None,
                timestamp_ns,
            );
            let audit_seq = audit.append(record);
            return Err(SerialOpError::Backend { error: BackendError::Fault, audit_seq });
        };
        match backend.serial_write(data) {
            Ok(()) => {
                let record = op_record(
                    session,
                    resource,
                    "serial_write",
                    0,
                    Decision::Allowed,
                    OpStatus::Ok,
                    Some(data.len() as u32),
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Ok(SerialWriteOutcome { audit_seq, timestamp_ns })
            }
            Err(error) => {
                let record = op_record(
                    session,
                    resource,
                    "serial_write",
                    0,
                    Decision::Allowed,
                    OpStatus::BackendFault,
                    None,
                    timestamp_ns,
                );
                let audit_seq = audit.append(record);
                Err(SerialOpError::Backend { error, audit_seq })
            }
        }
    }
}

impl<B: ResourceBackend, C: Timer> Executor<B, C> {
    /// Performs an asynchronous 32-bit poll with an optional abort token.
    pub async fn poll32_with_abort(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        offset: u64,
        expected: u32,
        mask: u32,
        interval_ns: i64,
        timeout_ns: i64,
        abort: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<PollOutcome, PollError> {
        self.poll32_internal(
            policy,
            audit,
            session,
            resource,
            offset,
            expected,
            mask,
            interval_ns,
            timeout_ns,
            None,
            abort,
        )
        .await
    }

    /// Performs an asynchronous 32-bit poll.
    pub async fn poll32(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        offset: u64,
        expected: u32,
        mask: u32,
        interval_ns: i64,
        timeout_ns: i64,
    ) -> Result<PollOutcome, PollError> {
        self.poll32_with_abort(
            policy,
            audit,
            session,
            resource,
            offset,
            expected,
            mask,
            interval_ns,
            timeout_ns,
            None,
        )
        .await
    }

    /// Internal 32-bit poll implementation supporting sequence item indexing.
    pub async fn poll32_internal(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        resource: ResourceId,
        offset: u64,
        expected: u32,
        mask: u32,
        interval_ns: i64,
        timeout_ns: i64,
        item_index: Option<u32>,
        abort: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<PollOutcome, PollError> {
        let timestamp_ns = self.clock.now_ns();
        if let Some(token) = abort {
            if token.load(std::sync::atomic::Ordering::Relaxed) {
                let mut record = op_record(
                    session,
                    resource,
                    "poll32",
                    offset,
                    Decision::Denied(Denial::NotAccepting),
                    OpStatus::Rejected,
                    None,
                    timestamp_ns,
                );
                record.item_index = item_index;
                let audit_seq = audit.append(record);
                return Err(PollError::Denied { denial: Denial::NotAccepting, audit_seq });
            }
        }
        if let Err(denial) = policy.check_poll32(resource, offset, mask, interval_ns, timeout_ns) {
            let mut record = op_record(
                session,
                resource,
                "poll32",
                offset,
                Decision::Denied(denial),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(PollError::Denied { denial, audit_seq });
        }
        if timeout_ns > self.limits.max_delay_ns {
            let mut record = op_record(
                session,
                resource,
                "poll32",
                offset,
                Decision::Denied(Denial::LimitExceeded),
                OpStatus::Rejected,
                None,
                timestamp_ns,
            );
            record.item_index = item_index;
            let audit_seq = audit.append(record);
            return Err(PollError::Denied { denial: Denial::LimitExceeded, audit_seq });
        }

        let start_ns = self.clock.now_ns();
        let deadline_ns = start_ns.saturating_add(timeout_ns);
        let mut last_val: Option<u32> = None;
        loop {
            if let Some(token) = abort {
                if token.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut record = op_record(
                        session,
                        resource,
                        "poll32",
                        offset,
                        Decision::Denied(Denial::NotAccepting),
                        OpStatus::Rejected,
                        last_val,
                        self.clock.now_ns(),
                    );
                    record.item_index = item_index;
                    let audit_seq = audit.append(record);
                    return Err(PollError::Denied { denial: Denial::NotAccepting, audit_seq });
                }
            }
            match self
                .poll32_read_step(audit, session, resource, offset, expected, mask, item_index)?
            {
                Ok(outcome) => return Ok(outcome),
                Err(val) => {
                    last_val = Some(val);
                    let now = self.clock.now_ns();
                    if now >= deadline_ns {
                        return Err(self.poll32_timeout(
                            audit, session, resource, offset, last_val, item_index,
                        ));
                    }
                    let sleep_ns = interval_ns.min(deadline_ns - now);
                    if sleep_ns > 0 {
                        self.clock.sleep(sleep_ns).await;
                    }
                    if let Some(token) = abort {
                        if token.load(std::sync::atomic::Ordering::Relaxed) {
                            let mut record = op_record(
                                session,
                                resource,
                                "poll32",
                                offset,
                                Decision::Denied(Denial::NotAccepting),
                                OpStatus::Rejected,
                                last_val,
                                self.clock.now_ns(),
                            );
                            record.item_index = item_index;
                            let audit_seq = audit.append(record);
                            return Err(PollError::Denied {
                                denial: Denial::NotAccepting,
                                audit_seq,
                            });
                        }
                    }
                    if self.clock.now_ns() >= deadline_ns {
                        match self.poll32_read_step(
                            audit, session, resource, offset, expected, mask, item_index,
                        )? {
                            Ok(outcome) => return Ok(outcome),
                            Err(final_val) => {
                                return Err(self.poll32_timeout(
                                    audit,
                                    session,
                                    resource,
                                    offset,
                                    Some(final_val),
                                    item_index,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Executes a bounded ordered sequence.
    pub async fn execute_sequence(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        items: &[SequenceItem],
    ) -> Result<SequenceOutcome, SequenceError> {
        self.execute_sequence_with_abort(policy, audit, session, items, None).await
    }

    /// Executes a bounded ordered sequence with an optional abort token.
    pub async fn execute_sequence_with_abort(
        &mut self,
        policy: &AccessPolicy,
        audit: &mut AuditRing,
        session: u64,
        items: &[SequenceItem],
        abort: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<SequenceOutcome, SequenceError> {
        if items.len() > self.limits.max_sequence_items {
            let timestamp_ns = self.clock.now_ns();
            let record = AuditRecord {
                session: Some(session),
                resource: None,
                operation: "sequence",
                offset: None,
                decision: Decision::Denied(Denial::LimitExceeded),
                status: OpStatus::Rejected,
                value: None,
                timestamp_ns,
                run_id: None,
                item_index: None,
            };
            let audit_seq = audit.append(record);
            return Err(SequenceError::TooManyItems {
                max: self.limits.max_sequence_items,
                audit_seq,
            });
        }
        if let Err((index, denial)) = self.prevalidate_sequence(policy, items) {
            let timestamp_ns = self.clock.now_ns();
            let record = AuditRecord {
                session: Some(session),
                resource: None,
                operation: "sequence",
                offset: None,
                decision: Decision::Denied(denial),
                status: OpStatus::Rejected,
                value: None,
                timestamp_ns,
                run_id: None,
                item_index: Some(index as u32),
            };
            let audit_seq = audit.append(record);
            return Err(SequenceError::Rejected { index, denial, audit_seq });
        }
        if let Some(token) = abort {
            if token.load(std::sync::atomic::Ordering::Relaxed) {
                let timestamp_ns = self.clock.now_ns();
                let record = AuditRecord {
                    session: Some(session),
                    resource: None,
                    operation: "sequence",
                    offset: None,
                    decision: Decision::Denied(Denial::NotAccepting),
                    status: OpStatus::Rejected,
                    value: None,
                    timestamp_ns,
                    run_id: None,
                    item_index: None,
                };
                let audit_seq = audit.append(record);
                return Err(SequenceError::Rejected {
                    index: 0,
                    denial: Denial::NotAccepting,
                    audit_seq,
                });
            }
        }

        let mut results = Vec::with_capacity(items.len());
        let mut complete = true;
        let start_ts = self.clock.now_ns();

        for (index, item) in items.iter().enumerate() {
            let idx = index as u32;
            let now = self.clock.now_ns();

            if let Some(token) = abort {
                if token.load(std::sync::atomic::Ordering::Relaxed) {
                    let record = AuditRecord {
                        session: Some(session),
                        resource: None,
                        operation: "sequence",
                        offset: None,
                        decision: Decision::Denied(Denial::NotAccepting),
                        status: OpStatus::Rejected,
                        value: None,
                        timestamp_ns: now,
                        run_id: None,
                        item_index: Some(idx),
                    };
                    audit.append(record);
                    results.push(SequenceItemResult {
                        index: idx,
                        ok: false,
                        outcome: SequenceItemOutcome::Error(Denial::NotAccepting),
                    });
                    complete = false;
                    break;
                }
            }

            if now.saturating_sub(start_ts) > self.limits.max_sequence_duration_ns {
                let record = AuditRecord {
                    session: Some(session),
                    resource: None,
                    operation: "sequence",
                    offset: None,
                    decision: Decision::Denied(Denial::Timeout),
                    status: OpStatus::Rejected,
                    value: None,
                    timestamp_ns: now,
                    run_id: None,
                    item_index: Some(idx),
                };
                audit.append(record);
                results.push(SequenceItemResult {
                    index: idx,
                    ok: false,
                    outcome: SequenceItemOutcome::Error(Denial::Timeout),
                });
                complete = false;
                break;
            }

            match item {
                SequenceItem::Read32 { resource, offset } => {
                    match self.read32_internal(
                        policy,
                        audit,
                        session,
                        *resource,
                        *offset,
                        Some(idx),
                    ) {
                        Ok(outcome) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: true,
                                outcome: SequenceItemOutcome::Read32(outcome),
                            });
                        }
                        Err(ReadError::Denied { denial, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Error(denial),
                            });
                            complete = false;
                            break;
                        }
                        Err(ReadError::Backend { error, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Backend(error),
                            });
                            complete = false;
                            break;
                        }
                    }
                }
                SequenceItem::Write32 {
                    resource,
                    offset,
                    value,
                    write_mask,
                    precondition,
                    readback,
                } => {
                    match self.write32_internal(
                        policy,
                        audit,
                        session,
                        *resource,
                        *offset,
                        *value,
                        *write_mask,
                        *precondition,
                        *readback,
                        Some(idx),
                    ) {
                        Ok(outcome) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: true,
                                outcome: SequenceItemOutcome::Write32(outcome),
                            });
                        }
                        Err(WriteError::Denied { denial, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Error(denial),
                            });
                            complete = false;
                            break;
                        }
                        Err(WriteError::Backend { error, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Backend(error),
                            });
                            complete = false;
                            break;
                        }
                    }
                }
                SequenceItem::Poll32 {
                    resource,
                    offset,
                    expected,
                    mask,
                    interval_ns,
                    timeout_ns,
                } => {
                    match self
                        .poll32_internal(
                            policy,
                            audit,
                            session,
                            *resource,
                            *offset,
                            *expected,
                            *mask,
                            *interval_ns,
                            *timeout_ns,
                            Some(idx),
                            abort,
                        )
                        .await
                    {
                        Ok(outcome) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: true,
                                outcome: SequenceItemOutcome::Poll32(outcome),
                            });
                        }
                        Err(PollError::Denied { denial, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Error(denial),
                            });
                            complete = false;
                            break;
                        }
                        Err(PollError::Backend { error, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Backend(error),
                            });
                            complete = false;
                            break;
                        }
                    }
                }
                SequenceItem::DelayNs(delay_ns) => {
                    let ts = self.clock.now_ns();
                    if *delay_ns > 0 {
                        self.clock.sleep(*delay_ns).await;
                    }
                    let record = AuditRecord {
                        session: Some(session),
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
                    results.push(SequenceItemResult {
                        index: idx,
                        ok: true,
                        outcome: SequenceItemOutcome::DelayNs,
                    });
                }
                SequenceItem::Barrier => {
                    let ts = self.clock.now_ns();
                    self.barrier();
                    let record = AuditRecord {
                        session: Some(session),
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
                    results.push(SequenceItemResult {
                        index: idx,
                        ok: true,
                        outcome: SequenceItemOutcome::Barrier,
                    });
                }
                SequenceItem::GpioRead { resource } => {
                    match self.gpio_read_internal(policy, audit, session, *resource, Some(idx)) {
                        Ok(outcome) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: true,
                                outcome: SequenceItemOutcome::GpioRead(outcome),
                            });
                        }
                        Err(GpioReadError::Denied { denial, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Error(denial),
                            });
                            complete = false;
                            break;
                        }
                        Err(GpioReadError::Backend { error, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Backend(error),
                            });
                            complete = false;
                            break;
                        }
                    }
                }
                SequenceItem::GpioWrite { resource, value } => {
                    match self.gpio_write_internal(
                        policy,
                        audit,
                        session,
                        *resource,
                        *value,
                        Some(idx),
                    ) {
                        Ok(outcome) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: true,
                                outcome: SequenceItemOutcome::GpioWrite(outcome),
                            });
                        }
                        Err(GpioWriteError::Denied { denial, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Error(denial),
                            });
                            complete = false;
                            break;
                        }
                        Err(GpioWriteError::Backend { error, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Backend(error),
                            });
                            complete = false;
                            break;
                        }
                    }
                }
                SequenceItem::I2cTransfer { resource, write_data, read_length } => {
                    match self.i2c_transfer_internal(
                        policy,
                        audit,
                        session,
                        *resource,
                        write_data,
                        *read_length,
                        Some(idx),
                    ) {
                        Ok(outcome) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: true,
                                outcome: SequenceItemOutcome::I2cTransfer(outcome),
                            });
                        }
                        Err(I2cTransferError::Denied { denial, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Error(denial),
                            });
                            complete = false;
                            break;
                        }
                        Err(I2cTransferError::Backend { error, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Backend(error),
                            });
                            complete = false;
                            break;
                        }
                    }
                }
                SequenceItem::SpiTransmit { resource, tx_data } => {
                    match self.spi_transmit_internal(
                        policy,
                        audit,
                        session,
                        *resource,
                        tx_data,
                        Some(idx),
                    ) {
                        Ok(outcome) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: true,
                                outcome: SequenceItemOutcome::SpiTransmit(outcome),
                            });
                        }
                        Err(SpiTransmitError::Denied { denial, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Error(denial),
                            });
                            complete = false;
                            break;
                        }
                        Err(SpiTransmitError::Backend { error, .. }) => {
                            results.push(SequenceItemResult {
                                index: idx,
                                ok: false,
                                outcome: SequenceItemOutcome::Backend(error),
                            });
                            complete = false;
                            break;
                        }
                    }
                }
            }
        }
        Ok(SequenceOutcome { results, complete })
    }
}

fn op_record(
    session: u64,
    resource: ResourceId,
    operation: &'static str,
    offset: u64,
    decision: Decision,
    status: OpStatus,
    value: Option<u32>,
    timestamp_ns: i64,
) -> AuditRecord {
    AuditRecord {
        session: Some(session),
        resource: Some(resource),
        operation,
        offset: Some(offset),
        decision,
        status,
        value,
        timestamp_ns,
        run_id: None,
        item_index: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access_policy::{
        AccessRule, MmioResource, ResourceCeiling, SessionMode, WIDTH32, WritableRegister,
    };
    use crate::hardware_backend::{FakeClock, FakeMmio};

    const CTRL: ResourceId = 1;
    const SESSION: u64 = 7;

    fn rule(offset: u64, class: AccessClass) -> AccessRule {
        AccessRule { resource: CTRL, offset, width: WIDTH32, class }
    }

    fn make_executor(
        rules: &[AccessRule],
    ) -> (Executor<FakeMmio, FakeClock>, AccessPolicy, AuditRing) {
        let resources =
            BTreeMap::from([(CTRL, MmioResource::mmio("ctrl".to_string(), 0x100, 0x100))]);
        let ceiling = BTreeMap::from([(
            CTRL,
            ResourceCeiling {
                hard_denied: vec![],
                allow_unknown_reads: true,
                allow_poll: false,
                writable_registers: vec![],
                protocol: None,
                allow_interrupt: false,
            },
        )]);
        let policy =
            AccessPolicy::new(SessionMode::ReadOnly, resources, ceiling, rules.iter().copied())
                .unwrap();
        let mut mmio = FakeMmio::new();
        mmio.set(0x3c, 0xdead_beef);
        mmio.set(0x40, 0x1234_5678);
        mmio.fail_at(0x44);
        let backends = BTreeMap::from([(CTRL, mmio)]);
        let clock = FakeClock::new(1000, 10);
        let executor = Executor::new(
            backends,
            clock,
            ExecLimits {
                max_snapshot_items: 4,
                max_sequence_items: 4,
                max_delay_ns: 5_000_000_000,
                max_sequence_duration_ns: 1_000_000_000,
            },
        );
        (executor, policy, AuditRing::new(32))
    }

    fn make_mutating_executor(
        writable_registers: Vec<WritableRegister>,
        rules: &[AccessRule],
    ) -> (Executor<FakeMmio, FakeClock>, AccessPolicy, AuditRing) {
        let resources =
            BTreeMap::from([(CTRL, MmioResource::mmio("ctrl".to_string(), 0x100, 0x100))]);
        let ceiling = BTreeMap::from([(
            CTRL,
            ResourceCeiling {
                hard_denied: vec![],
                allow_unknown_reads: true,
                allow_poll: true,
                writable_registers,
                protocol: None,
                allow_interrupt: false,
            },
        )]);
        let policy =
            AccessPolicy::new(SessionMode::Mutating, resources, ceiling, rules.iter().copied())
                .unwrap();
        let mut mmio = FakeMmio::new();
        mmio.set(0x3c, 0xdead_beef);
        mmio.set(0x40, 0x1234_5678);
        mmio.fail_at(0x44);
        let backends = BTreeMap::from([(CTRL, mmio)]);
        let clock = FakeClock::new(1000, 10);
        let executor = Executor::new(
            backends,
            clock,
            ExecLimits {
                max_snapshot_items: 4,
                max_sequence_items: 8,
                max_delay_ns: 5_000_000_000,
                max_sequence_duration_ns: 1_000_000_000,
            },
        );
        (executor, policy, AuditRing::new(32))
    }

    fn accesses(executor: &Executor<FakeMmio, FakeClock>) -> &[u64] {
        &executor.backend(CTRL).unwrap().accesses
    }

    fn write_accesses(executor: &Executor<FakeMmio, FakeClock>) -> &[(u64, u32)] {
        &executor.backend(CTRL).unwrap().write_accesses
    }

    fn barriers(executor: &Executor<FakeMmio, FakeClock>) -> usize {
        executor.backend(CTRL).unwrap().barriers
    }

    #[test]
    fn read_allowed_returns_value_and_audits() {
        let (mut executor, policy, mut audit) = make_executor(&[rule(0x3c, AccessClass::ReadOnce)]);
        let outcome = executor.read32(&policy, &mut audit, SESSION, CTRL, 0x3c).unwrap();
        assert_eq!(outcome.value, 0xdead_beef);
        assert_eq!(outcome.timestamp_ns, 1000);
        assert_eq!(accesses(&executor), &[0x3c]);

        let entry = &audit.read(outcome.audit_seq, 1).entries[0];
        assert_eq!(entry.record.decision, Decision::Allowed);
        assert_eq!(entry.record.status, OpStatus::Ok);
        assert_eq!(entry.record.value, Some(0xdead_beef));
        assert_eq!(entry.record.session, Some(SESSION));
    }

    #[test]
    fn denied_read_touches_no_hardware_but_is_audited() {
        let (mut executor, policy, mut audit) = make_executor(&[rule(0x3c, AccessClass::ReadOnce)]);
        let error = executor.read32(&policy, &mut audit, SESSION, CTRL, 0x40).unwrap_err();
        let ReadError::Denied { denial, audit_seq } = error else {
            panic!("expected denial, got {error:?}");
        };
        assert_eq!(denial, Denial::NotInAllowlist);
        assert!(accesses(&executor).is_empty());

        let entry = &audit.read(audit_seq, 1).entries[0];
        assert_eq!(entry.record.decision, Decision::Denied(Denial::NotInAllowlist));
        assert_eq!(entry.record.status, OpStatus::Rejected);
        assert_eq!(entry.record.value, None);
    }

    #[test]
    fn read_backend_fault_is_audited() {
        let (mut executor, policy, mut audit) = make_executor(&[rule(0x44, AccessClass::ReadOnce)]);
        let error = executor.read32(&policy, &mut audit, SESSION, CTRL, 0x44).unwrap_err();
        let ReadError::Backend { error, audit_seq } = error else {
            panic!("expected backend error, got {error:?}");
        };
        assert_eq!(error, BackendError::Fault);

        let entry = &audit.read(audit_seq, 1).entries[0];
        assert_eq!(entry.record.decision, Decision::Allowed);
        assert_eq!(entry.record.status, OpStatus::BackendFault);
    }

    #[test]
    fn snapshot_reads_in_order_with_distinct_timestamps() {
        let (mut executor, policy, mut audit) =
            make_executor(&[rule(0x3c, AccessClass::Snapshot), rule(0x40, AccessClass::Snapshot)]);
        let items = [
            SnapshotItem { resource: CTRL, offset: 0x3c },
            SnapshotItem { resource: CTRL, offset: 0x40 },
        ];
        let outcome = executor.snapshot(&policy, &mut audit, SESSION, &items).unwrap();
        assert!(outcome.complete);
        assert_eq!(outcome.results.len(), 2);
        assert_eq!(outcome.results[0].value, Ok(0xdead_beef));
        assert_eq!(outcome.results[1].value, Ok(0x1234_5678));
        assert!(outcome.results[0].timestamp_ns < outcome.results[1].timestamp_ns);
        assert!(outcome.results[0].audit_seq < outcome.results[1].audit_seq);
        assert_eq!(accesses(&executor), &[0x3c, 0x40]);
    }

    #[test]
    fn snapshot_prevalidation_blocks_every_read() {
        let (mut executor, policy, mut audit) = make_executor(&[rule(0x3c, AccessClass::Snapshot)]);
        let items = [
            SnapshotItem { resource: CTRL, offset: 0x3c },
            SnapshotItem { resource: CTRL, offset: 0x48 },
        ];
        let error = executor.snapshot(&policy, &mut audit, SESSION, &items).unwrap_err();
        let SnapshotError::Rejected { index, denial, .. } = error else {
            panic!("expected rejection, got {error:?}");
        };
        assert_eq!(index, 1);
        assert_eq!(denial, Denial::NotInAllowlist);
        assert!(accesses(&executor).is_empty());
    }

    #[test]
    fn read_once_grant_does_not_authorize_snapshot() {
        let (mut executor, policy, mut audit) = make_executor(&[rule(0x3c, AccessClass::ReadOnce)]);
        let items = [SnapshotItem { resource: CTRL, offset: 0x3c }];
        let error = executor.snapshot(&policy, &mut audit, SESSION, &items).unwrap_err();
        let SnapshotError::Rejected { index, denial, .. } = error else {
            panic!("expected rejection, got {error:?}");
        };
        assert_eq!(index, 0);
        assert_eq!(denial, Denial::NotInAllowlist);
        assert!(accesses(&executor).is_empty());
    }

    #[test]
    fn snapshot_stops_at_first_backend_fault() {
        let (mut executor, policy, mut audit) = make_executor(&[
            rule(0x3c, AccessClass::Snapshot),
            rule(0x44, AccessClass::Snapshot),
            rule(0x40, AccessClass::Snapshot),
        ]);
        let items = [
            SnapshotItem { resource: CTRL, offset: 0x3c },
            SnapshotItem { resource: CTRL, offset: 0x44 },
            SnapshotItem { resource: CTRL, offset: 0x40 },
        ];
        let outcome = executor.snapshot(&policy, &mut audit, SESSION, &items).unwrap();
        assert!(!outcome.complete);
        assert_eq!(outcome.results.len(), 2);
        assert_eq!(outcome.results[0].value, Ok(0xdead_beef));
        assert_eq!(outcome.results[1].value, Err(BackendError::Fault));
        // The third item never began.
        assert_eq!(accesses(&executor), &[0x3c, 0x44]);

        let entry = &audit.read(outcome.results[1].audit_seq, 1).entries[0];
        assert_eq!(entry.record.status, OpStatus::BackendFault);
    }

    #[test]
    fn snapshot_item_limit_enforced() {
        let (mut executor, policy, mut audit) = make_executor(&[rule(0x3c, AccessClass::Snapshot)]);
        let items = [SnapshotItem { resource: CTRL, offset: 0x3c }; 5];
        let error = executor.snapshot(&policy, &mut audit, SESSION, &items).unwrap_err();
        let SnapshotError::TooManyItems { max, audit_seq } = error else {
            panic!("expected limit rejection, got {error:?}");
        };
        assert_eq!(max, 4);
        assert!(accesses(&executor).is_empty());

        let entry = &audit.read(audit_seq, 1).entries[0];
        assert_eq!(entry.record.decision, Decision::Denied(Denial::LimitExceeded));
    }

    #[test]
    fn missing_backend_is_an_audited_fault() {
        let resources =
            BTreeMap::from([(CTRL, MmioResource::mmio("ctrl".to_string(), 0x100, 0x100))]);
        let ceiling = BTreeMap::from([(
            CTRL,
            ResourceCeiling {
                hard_denied: vec![],
                allow_unknown_reads: true,
                allow_poll: false,
                writable_registers: vec![],
                protocol: None,
                allow_interrupt: false,
            },
        )]);
        let policy = AccessPolicy::new(
            SessionMode::ReadOnly,
            resources,
            ceiling,
            [rule(0x3c, AccessClass::ReadOnce)],
        )
        .unwrap();
        let clock = FakeClock::new(0, 1);
        let mut executor = Executor::<FakeMmio, _>::new(
            BTreeMap::new(),
            clock,
            ExecLimits {
                max_snapshot_items: 4,
                max_sequence_items: 4,
                max_delay_ns: 5_000_000_000,
                max_sequence_duration_ns: 1_000_000_000,
            },
        );
        let mut audit = AuditRing::new(4);
        let error = executor.read32(&policy, &mut audit, SESSION, CTRL, 0x3c).unwrap_err();
        let ReadError::Backend { error, audit_seq } = error else {
            panic!("expected backend error, got {error:?}");
        };
        assert_eq!(error, BackendError::Fault);
        let entry = &audit.read(audit_seq, 1).entries[0];
        assert_eq!(entry.record.status, OpStatus::BackendFault);
    }

    #[test]
    fn write_full_mask_writes_directly_and_applies_barrier() {
        let reg = WritableRegister {
            offset: 0x40,
            width: WIDTH32,
            allow_mask: 0xFFFF_FFFF,
            allow_rmw: false,
            require_precondition: false,
            precondition_mask: 0,
            readback: false,
        };
        let (mut executor, policy, mut audit) =
            make_mutating_executor(vec![reg], &[rule(0x40, AccessClass::Write)]);
        let outcome = executor
            .write32(
                &policy,
                &mut audit,
                SESSION,
                CTRL,
                0x40,
                0xcafe_babe,
                0xFFFF_FFFF,
                None,
                false,
            )
            .unwrap();
        assert_eq!(outcome.readback_value, 0);
        assert_eq!(write_accesses(&executor), &[(0x40, 0xcafe_babe)]);
        assert_eq!(barriers(&executor), 1);
        assert!(accesses(&executor).is_empty()); // No before-read since full-mask & no precondition

        let entry = &audit.read(outcome.audit_seq, 1).entries[0];
        assert_eq!(entry.record.decision, Decision::Allowed);
        assert_eq!(entry.record.status, OpStatus::Ok);
        assert_eq!(entry.record.value, Some(0xcafe_babe));
    }

    #[test]
    fn write_rmw_reads_before_and_modifies_bits() {
        let reg = WritableRegister {
            offset: 0x40,
            width: WIDTH32,
            allow_mask: 0x00FF_0000,
            allow_rmw: true,
            require_precondition: false,
            precondition_mask: 0,
            readback: false,
        };
        let (mut executor, policy, mut audit) =
            make_mutating_executor(vec![reg], &[rule(0x40, AccessClass::Write)]);
        // Original 0x40 is 0x1234_5678. We modify byte 2 to 0xab. Expected: 0x12ab_5678.
        let outcome = executor
            .write32(
                &policy,
                &mut audit,
                SESSION,
                CTRL,
                0x40,
                0x00ab_0000,
                0x00FF_0000,
                None,
                false,
            )
            .unwrap();
        assert_eq!(outcome.readback_value, 0);
        assert_eq!(accesses(&executor), &[0x40]); // Audited before-read
        assert_eq!(write_accesses(&executor), &[(0x40, 0x12ab_5678)]);
        assert_eq!(barriers(&executor), 1);

        let entries = &audit.read(0, 5).entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].record.operation, "write32_before_read");
        assert_eq!(entries[0].record.value, Some(0x1234_5678));
        assert_eq!(entries[1].record.operation, "write32");
        assert_eq!(entries[1].record.value, Some(0x12ab_5678));
    }

    #[test]
    fn write_precondition_satisfied_allows_write() {
        let reg = WritableRegister {
            offset: 0x40,
            width: WIDTH32,
            allow_mask: 0xFFFF_FFFF,
            allow_rmw: true,
            require_precondition: true,
            precondition_mask: 0xFFFF_0000,
            readback: false,
        };
        let pre = WritePrecondition { expected: 0x1234_0000, mask: 0xFFFF_0000 };
        let (mut executor, policy, mut audit) =
            make_mutating_executor(vec![reg], &[rule(0x40, AccessClass::Write)]);
        let outcome = executor
            .write32(
                &policy,
                &mut audit,
                SESSION,
                CTRL,
                0x40,
                0xcafe_babe,
                0xFFFF_FFFF,
                Some(pre),
                false,
            )
            .unwrap();
        assert_eq!(write_accesses(&executor), &[(0x40, 0xcafe_babe)]);
        assert_eq!(outcome.readback_value, 0);
    }

    #[test]
    fn write_precondition_failed_rejects_and_prevents_write() {
        let reg = WritableRegister {
            offset: 0x40,
            width: WIDTH32,
            allow_mask: 0xFFFF_FFFF,
            allow_rmw: true,
            require_precondition: true,
            precondition_mask: 0xFFFF_0000,
            readback: false,
        };
        let pre = WritePrecondition { expected: 0x9999_0000, mask: 0xFFFF_0000 };
        let (mut executor, policy, mut audit) =
            make_mutating_executor(vec![reg], &[rule(0x40, AccessClass::Write)]);
        let error = executor
            .write32(
                &policy,
                &mut audit,
                SESSION,
                CTRL,
                0x40,
                0xcafe_babe,
                0xFFFF_FFFF,
                Some(pre),
                false,
            )
            .unwrap_err();
        let WriteError::Denied { denial, audit_seq } = error else {
            panic!("expected denial, got {error:?}");
        };
        assert_eq!(denial, Denial::PreconditionFailed);
        assert!(write_accesses(&executor).is_empty()); // Hardware was not written!

        let entry = &audit.read(audit_seq, 1).entries[0];
        assert_eq!(entry.record.decision, Decision::Denied(Denial::PreconditionFailed));
        assert_eq!(entry.record.status, OpStatus::Rejected);
        assert_eq!(entry.record.value, Some(0x1234_5678)); // Audited before-value
    }

    #[test]
    fn write_readback_audited_and_returned() {
        let reg = WritableRegister {
            offset: 0x40,
            width: WIDTH32,
            allow_mask: 0xFFFF_FFFF,
            allow_rmw: false,
            require_precondition: false,
            precondition_mask: 0,
            readback: true,
        };
        let (mut executor, policy, mut audit) =
            make_mutating_executor(vec![reg], &[rule(0x40, AccessClass::Write)]);
        let outcome = executor
            .write32(
                &policy,
                &mut audit,
                SESSION,
                CTRL,
                0x40,
                0xcafe_babe,
                0xFFFF_FFFF,
                None,
                false,
            )
            .unwrap();
        assert_eq!(outcome.readback_value, 0xcafe_babe);

        let entries = &audit.read(0, 5).entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].record.operation, "write32");
        assert_eq!(entries[1].record.operation, "write32_readback");
        assert_eq!(entries[1].record.value, Some(0xcafe_babe));
    }

    #[test]
    fn write_denied_in_read_only_session() {
        let (mut executor, policy, mut audit) = make_executor(&[rule(0x40, AccessClass::ReadOnce)]);
        let error = executor
            .write32(
                &policy,
                &mut audit,
                SESSION,
                CTRL,
                0x40,
                0xcafe_babe,
                0xFFFF_FFFF,
                None,
                false,
            )
            .unwrap_err();
        let WriteError::Denied { denial, .. } = error else {
            panic!("expected denial, got {error:?}");
        };
        assert_eq!(denial, Denial::ReadOnlySession);
        assert!(write_accesses(&executor).is_empty());
    }

    #[test]
    fn poll32_matches_immediately_without_sleep() {
        let (mut executor, policy, mut audit) =
            make_mutating_executor(vec![], &[rule(0x3c, AccessClass::Poll)]);
        let outcome = futures::executor::block_on(executor.poll32(
            &policy,
            &mut audit,
            SESSION,
            CTRL,
            0x3c,
            0xdead_beef,
            0xFFFF_FFFF,
            100,
            1000,
        ))
        .unwrap();
        assert_eq!(outcome.value, 0xdead_beef);
        assert_eq!(executor.clock.sleeps.len(), 0);
        assert_eq!(accesses(&executor), &[0x3c]);
    }

    #[test]
    fn poll32_timeout_audited_and_records_sleeps() {
        let (mut executor, policy, mut audit) =
            make_mutating_executor(vec![], &[rule(0x3c, AccessClass::Poll)]);
        let error = futures::executor::block_on(executor.poll32(
            &policy,
            &mut audit,
            SESSION,
            CTRL,
            0x3c,
            0x9999_9999,
            0xFFFF_FFFF,
            100,
            300,
        ))
        .unwrap_err();
        let PollError::Denied { denial, audit_seq } = error else {
            panic!("expected timeout denial, got {error:?}");
        };
        assert_eq!(denial, Denial::Timeout);
        assert!(executor.clock.sleeps.len() > 0);

        let entry = &audit.read(audit_seq, 1).entries[0];
        assert_eq!(entry.record.operation, "poll32");
        assert_eq!(entry.record.decision, Decision::Denied(Denial::Timeout));
        assert_eq!(entry.record.status, OpStatus::Rejected);
        assert_eq!(entry.record.value, Some(0xdead_beef));
    }

    #[test]
    fn sequence_prevalidation_blocks_all_hardware_access() {
        let reg = WritableRegister {
            offset: 0x40,
            width: WIDTH32,
            allow_mask: 0xFFFF_FFFF,
            allow_rmw: false,
            require_precondition: false,
            precondition_mask: 0,
            readback: false,
        };
        let (mut executor, policy, mut audit) = make_mutating_executor(
            vec![reg],
            &[rule(0x3c, AccessClass::Sequence), rule(0x40, AccessClass::Sequence)],
        );
        let items = [
            SequenceItem::Read32 { resource: CTRL, offset: 0x3c },
            SequenceItem::Write32 {
                resource: CTRL,
                offset: 0x50, // Not permitted by ceiling or allowlist!
                value: 123,
                write_mask: 0xFFFF_FFFF,
                precondition: None,
                readback: false,
            },
        ];
        let error = futures::executor::block_on(
            executor.execute_sequence(&policy, &mut audit, SESSION, &items),
        )
        .unwrap_err();
        let SequenceError::Rejected { index, denial, .. } = error else {
            panic!("expected rejection, got {error:?}");
        };
        assert_eq!(index, 1);
        assert_eq!(denial, Denial::WriteNotPermitted);
        // Zero hardware reads or writes!
        assert!(accesses(&executor).is_empty());
        assert!(write_accesses(&executor).is_empty());
    }

    #[test]
    fn sequence_full_execution_succeeds_in_order() {
        let reg = WritableRegister {
            offset: 0x40,
            width: WIDTH32,
            allow_mask: 0xFFFF_FFFF,
            allow_rmw: false,
            require_precondition: false,
            precondition_mask: 0,
            readback: false,
        };
        let (mut executor, policy, mut audit) = make_mutating_executor(
            vec![reg],
            &[rule(0x3c, AccessClass::Sequence), rule(0x40, AccessClass::Sequence)],
        );
        let items = [
            SequenceItem::Read32 { resource: CTRL, offset: 0x3c },
            SequenceItem::Write32 {
                resource: CTRL,
                offset: 0x40,
                value: 0x9999_8888,
                write_mask: 0xFFFF_FFFF,
                precondition: None,
                readback: false,
            },
            SequenceItem::DelayNs(200),
            SequenceItem::Barrier,
            SequenceItem::Poll32 {
                resource: CTRL,
                offset: 0x40,
                expected: 0x9999_8888,
                mask: 0xFFFF_FFFF,
                interval_ns: 50,
                timeout_ns: 200,
            },
        ];
        let outcome = futures::executor::block_on(
            executor.execute_sequence(&policy, &mut audit, SESSION, &items),
        )
        .unwrap();
        assert!(outcome.complete);
        assert_eq!(outcome.results.len(), 5);
        assert!(outcome.results.iter().all(|r| r.ok));
        assert_eq!(write_accesses(&executor), &[(0x40, 0x9999_8888)]);
        assert_eq!(barriers(&executor), 2); // 1 from write, 1 explicit barrier item

        let entries = &audit.read(0, 10).entries;
        assert_eq!(entries.len(), 5);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.record.item_index, Some(i as u32));
        }
    }

    #[test]
    fn sequence_stops_at_first_runtime_failure() {
        let reg = WritableRegister {
            offset: 0x40,
            width: WIDTH32,
            allow_mask: 0xFFFF_FFFF,
            allow_rmw: true,
            require_precondition: true,
            precondition_mask: 0xFFFF_FFFF,
            readback: false,
        };
        let (mut executor, policy, mut audit) = make_mutating_executor(
            vec![reg],
            &[rule(0x3c, AccessClass::Sequence), rule(0x40, AccessClass::Sequence)],
        );
        let items = [
            SequenceItem::Read32 { resource: CTRL, offset: 0x3c },
            SequenceItem::Write32 {
                resource: CTRL,
                offset: 0x40,
                value: 0x9999_8888,
                write_mask: 0xFFFF_FFFF,
                precondition: Some(WritePrecondition { expected: 0x0000_0000, mask: 0xFFFF_FFFF }), // Will fail
                readback: false,
            },
            SequenceItem::Read32 { resource: CTRL, offset: 0x3c }, // Should never execute!
        ];
        let outcome = futures::executor::block_on(
            executor.execute_sequence(&policy, &mut audit, SESSION, &items),
        )
        .unwrap();
        assert!(!outcome.complete);
        assert_eq!(outcome.results.len(), 2);
        assert!(outcome.results[0].ok);
        assert!(!outcome.results[1].ok);
        // Third item never touched hardware
        assert_eq!(accesses(&executor), &[0x3c, 0x40]); // 0x3c from item 0, 0x40 from before-read in item 1
        assert!(write_accesses(&executor).is_empty());
    }

    #[test]
    fn sequence_stops_on_duration_timeout() {
        let (mut executor, policy, mut audit) =
            make_mutating_executor(vec![], &[rule(0x3c, AccessClass::Sequence)]);
        executor.limits.max_sequence_duration_ns = 500;
        let items = [
            SequenceItem::Read32 { resource: CTRL, offset: 0x3c },
            SequenceItem::DelayNs(600),
            SequenceItem::Read32 { resource: CTRL, offset: 0x3c },
        ];
        let outcome = futures::executor::block_on(
            executor.execute_sequence(&policy, &mut audit, SESSION, &items),
        )
        .unwrap();
        assert!(!outcome.complete);
        assert_eq!(outcome.results.len(), 3);
        assert!(outcome.results[0].ok);
        assert!(outcome.results[1].ok);
        assert!(!outcome.results[2].ok);
        assert_eq!(outcome.results[2].outcome, SequenceItemOutcome::Error(Denial::Timeout));
    }

    #[test]
    fn sequence_stops_on_abort() {
        let (mut executor, policy, mut audit) =
            make_mutating_executor(vec![], &[rule(0x3c, AccessClass::Sequence)]);
        let abort = std::sync::atomic::AtomicBool::new(true);
        let items = [SequenceItem::Read32 { resource: CTRL, offset: 0x3c }];
        let error = futures::executor::block_on(executor.execute_sequence_with_abort(
            &policy,
            &mut audit,
            SESSION,
            &items,
            Some(&abort),
        ))
        .unwrap_err();
        let SequenceError::Rejected { denial, .. } = error else {
            panic!("expected rejected error, got {error:?}");
        };
        assert_eq!(denial, Denial::NotAccepting);
    }

    #[test]
    fn poll32_aborts_when_token_set() {
        let (mut executor, policy, mut audit) =
            make_mutating_executor(vec![], &[rule(0x3c, AccessClass::Poll)]);
        let abort = std::sync::atomic::AtomicBool::new(true);
        let error = futures::executor::block_on(executor.poll32_with_abort(
            &policy,
            &mut audit,
            SESSION,
            CTRL,
            0x3c,
            0xdead_beef,
            0xFFFF_FFFF,
            100,
            1000,
            Some(&abort),
        ))
        .unwrap_err();
        let PollError::Denied { denial, .. } = error else {
            panic!("expected denial, got {error:?}");
        };
        assert_eq!(denial, Denial::NotAccepting);
    }

    #[test]
    fn protocol_gpio_standalone_read_and_write() {
        use crate::access_policy::ProtocolCeiling;
        let gpio_id = 2;
        let resources = BTreeMap::from([(gpio_id, MmioResource::gpio("gpio_pin".to_string()))]);
        let ceiling = BTreeMap::from([(
            gpio_id,
            ResourceCeiling {
                hard_denied: vec![],
                allow_unknown_reads: true,
                allow_poll: false,
                writable_registers: vec![],
                protocol: Some(ProtocolCeiling::default_for(
                    crate::access_policy::ResourceKind::Gpio,
                )),
                allow_interrupt: false,
            },
        )]);
        let policy_ro = AccessPolicy::new(
            SessionMode::ReadOnly,
            resources.clone(),
            ceiling.clone(),
            [AccessRule { resource: gpio_id, offset: 0, width: 0, class: AccessClass::Protocol }],
        )
        .unwrap();
        let policy_rw = AccessPolicy::new(
            SessionMode::Mutating,
            resources,
            ceiling,
            [AccessRule { resource: gpio_id, offset: 0, width: 0, class: AccessClass::Protocol }],
        )
        .unwrap();

        let fake_gpio = crate::protocol_resource_adapter::FakeGpio::new(true);
        let backends = BTreeMap::from([(gpio_id, fake_gpio)]);
        let clock = FakeClock::new(1000, 10);
        let mut executor = Executor::new(
            backends,
            clock,
            ExecLimits {
                max_snapshot_items: 4,
                max_sequence_items: 4,
                max_delay_ns: 5_000_000_000,
                max_sequence_duration_ns: 1_000_000_000,
            },
        );
        let mut audit = AuditRing::new(32);

        // Read in ReadOnly mode succeeds
        let read_outcome = executor.gpio_read(&policy_ro, &mut audit, SESSION, gpio_id).unwrap();
        assert_eq!(read_outcome.value, true);

        // Write in ReadOnly mode is denied (requires mutation)
        let write_err =
            executor.gpio_write(&policy_ro, &mut audit, SESSION, gpio_id, false).unwrap_err();
        assert_eq!(
            write_err,
            GpioWriteError::Denied { denial: Denial::ReadOnlySession, audit_seq: 1 }
        );

        // Write in Mutating mode succeeds
        let write_outcome =
            executor.gpio_write(&policy_rw, &mut audit, SESSION, gpio_id, false).unwrap();
        assert_eq!(write_outcome.audit_seq, 2);

        // Read back new value
        let read_outcome2 = executor.gpio_read(&policy_rw, &mut audit, SESSION, gpio_id).unwrap();
        assert_eq!(read_outcome2.value, false);
    }

    #[test]
    fn heterogeneous_sequence_executes_mmio_and_protocols() {
        use crate::access_policy::ProtocolCeiling;
        let mmio_id = 1;
        let gpio_id = 2;
        let i2c_id = 3;
        let spi_id = 4;

        let resources = BTreeMap::from([
            (mmio_id, MmioResource::mmio("mmio".to_string(), 0x100, 0x100)),
            (gpio_id, MmioResource::gpio("gpio".to_string())),
            (i2c_id, MmioResource::i2c("i2c".to_string())),
            (spi_id, MmioResource::spi("spi".to_string())),
        ]);
        let ceiling = BTreeMap::from([
            (
                mmio_id,
                ResourceCeiling {
                    hard_denied: vec![],
                    allow_unknown_reads: true,
                    allow_poll: false,
                    writable_registers: vec![],
                    protocol: None,
                    allow_interrupt: false,
                },
            ),
            (
                gpio_id,
                ResourceCeiling {
                    hard_denied: vec![],
                    allow_unknown_reads: false,
                    allow_poll: false,
                    writable_registers: vec![],
                    protocol: Some(ProtocolCeiling::default_for(
                        crate::access_policy::ResourceKind::Gpio,
                    )),
                    allow_interrupt: false,
                },
            ),
            (
                i2c_id,
                ResourceCeiling {
                    hard_denied: vec![],
                    allow_unknown_reads: false,
                    allow_poll: false,
                    writable_registers: vec![],
                    protocol: Some(ProtocolCeiling::default_for(
                        crate::access_policy::ResourceKind::I2c,
                    )),
                    allow_interrupt: false,
                },
            ),
            (
                spi_id,
                ResourceCeiling {
                    hard_denied: vec![],
                    allow_unknown_reads: false,
                    allow_poll: false,
                    writable_registers: vec![],
                    protocol: Some(ProtocolCeiling::default_for(
                        crate::access_policy::ResourceKind::Spi,
                    )),
                    allow_interrupt: false,
                },
            ),
        ]);
        let rules = [
            AccessRule {
                resource: mmio_id,
                offset: 0x10,
                width: WIDTH32,
                class: AccessClass::Sequence,
            },
            AccessRule { resource: gpio_id, offset: 0, width: 0, class: AccessClass::Sequence },
            AccessRule { resource: i2c_id, offset: 0, width: 0, class: AccessClass::Sequence },
            AccessRule { resource: spi_id, offset: 0, width: 0, class: AccessClass::Sequence },
        ];
        let policy = AccessPolicy::new(SessionMode::Mutating, resources, ceiling, rules).unwrap();

        let mut mmio = FakeMmio::new();
        mmio.set(0x10, 0xaabb_ccdd);

        let gpio = crate::protocol_resource_adapter::FakeGpio::new(false);

        let mut i2c = crate::protocol_resource_adapter::FakeI2c::new();
        i2c.set_read_response(vec![0x11, 0x22]);

        let mut spi = crate::protocol_resource_adapter::FakeSpi::new();
        spi.set_rx_response(vec![0x33, 0x44]);

        use crate::protocol_resource_adapter::DeviceBackend;
        let backends: BTreeMap<
            ResourceId,
            DeviceBackend<
                FakeMmio,
                crate::protocol_resource_adapter::FakeGpio,
                crate::protocol_resource_adapter::FakeI2c,
                crate::protocol_resource_adapter::FakeSpi,
            >,
        > = BTreeMap::from([
            (mmio_id, DeviceBackend::new_mmio(mmio)),
            (gpio_id, DeviceBackend::new_gpio(gpio)),
            (i2c_id, DeviceBackend::new_i2c(i2c)),
            (spi_id, DeviceBackend::new_spi(spi)),
        ]);

        let clock = FakeClock::new(1000, 10);
        let mut executor = Executor::new(
            backends,
            clock,
            ExecLimits {
                max_snapshot_items: 4,
                max_sequence_items: 10,
                max_delay_ns: 5_000_000_000,
                max_sequence_duration_ns: 1_000_000_000,
            },
        );
        let mut audit = AuditRing::new(32);

        let sequence = [
            SequenceItem::Read32 { resource: mmio_id, offset: 0x10 },
            SequenceItem::GpioWrite { resource: gpio_id, value: true },
            SequenceItem::DelayNs(100),
            SequenceItem::I2cTransfer { resource: i2c_id, write_data: vec![0x55], read_length: 2 },
            SequenceItem::SpiTransmit { resource: spi_id, tx_data: vec![0xaa, 0xbb] },
            SequenceItem::GpioRead { resource: gpio_id },
            SequenceItem::Barrier,
        ];

        let outcome = futures::executor::block_on(
            executor.execute_sequence(&policy, &mut audit, SESSION, &sequence),
        )
        .unwrap();

        assert!(outcome.complete);
        assert_eq!(outcome.results.len(), 7);
        assert!(outcome.results.iter().all(|r| r.ok));

        match &outcome.results[0].outcome {
            SequenceItemOutcome::Read32(r) => assert_eq!(r.value, 0xaabb_ccdd),
            other => panic!("expected Read32, got {other:?}"),
        }
        assert!(matches!(outcome.results[1].outcome, SequenceItemOutcome::GpioWrite(_)));
        assert_eq!(outcome.results[2].outcome, SequenceItemOutcome::DelayNs);
        match &outcome.results[3].outcome {
            SequenceItemOutcome::I2cTransfer(t) => assert_eq!(t.read_data, vec![0x11, 0x22]),
            other => panic!("expected I2cTransfer, got {other:?}"),
        }
        match &outcome.results[4].outcome {
            SequenceItemOutcome::SpiTransmit(t) => assert_eq!(t.rx_data, vec![0x33, 0x44]),
            other => panic!("expected SpiTransmit, got {other:?}"),
        }
        match &outcome.results[5].outcome {
            SequenceItemOutcome::GpioRead(r) => assert_eq!(r.value, true),
            other => panic!("expected GpioRead, got {other:?}"),
        }
        assert_eq!(outcome.results[6].outcome, SequenceItemOutcome::Barrier);

        let entries = &audit.read(0, 10).entries;
        assert_eq!(entries.len(), 7);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.record.item_index, Some(i as u32));
        }
    }
}
