// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Layered access validation for proxy operations.
//!
//! Every hardware access must be permitted by every layer: actual resource
//! bounds, the immutable target ceiling, the exact session allowlist, and
//! per-operation structural validation (width, alignment, and overflow).
//! Rejection at any layer denies the access without touching hardware.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

/// Identifies a resource within one proxy instance.
pub type ResourceId = u32;

/// The only MMIO access width supported by the V1 wire contract, in bytes.
pub const WIDTH32: u32 = 4;

/// Session mode. A read-only session can never issue a write; a mutating
/// session additionally requires the exclusive mutation lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMode {
    /// The session may only read.
    ReadOnly,
    /// The session holds the exclusive mutation lease.
    Mutating,
}

/// Access classes distinguished by policy. A grant for one class never
/// authorizes another: a one-shot read grant does not authorize polling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccessClass {
    /// A single 32-bit read.
    ReadOnce,
    /// A read performed as part of a bounded snapshot.
    Snapshot,
    /// A repeated bounded read.
    Poll,
    /// A masked write.
    Write,
    /// An operation within a bounded sequence.
    Sequence,
    /// An operation on a protocol-backed resource (GPIO, I2C, SPI).
    Protocol,
    /// An interrupt observation.
    Interrupt,
}

/// The kind of hardware resource offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResourceKind {
    Mmio,
    Gpio,
    I2c,
    Spi,
    Interrupt,
    Clock,
    Reset,
    Serial,
}

/// Description of a resource offered by the parent node. Descriptions
/// never contain physical addresses or raw handles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MmioResource {
    /// Stable logical name.
    pub name: String,
    /// Resource kind.
    pub kind: ResourceKind,
    /// Size of the logical resource in bytes.
    pub logical_size: u64,
    /// Size of the actual backing mapping in bytes. Accesses must fit both
    /// the logical and the mapped size.
    pub mapped_size: u64,
}

impl MmioResource {
    pub fn mmio(name: impl Into<String>, logical_size: u64, mapped_size: u64) -> Self {
        Self { name: name.into(), kind: ResourceKind::Mmio, logical_size, mapped_size }
    }

    pub fn gpio(name: impl Into<String>) -> Self {
        Self { name: name.into(), kind: ResourceKind::Gpio, logical_size: 1, mapped_size: 1 }
    }

    pub fn i2c(name: impl Into<String>) -> Self {
        Self { name: name.into(), kind: ResourceKind::I2c, logical_size: 0, mapped_size: 0 }
    }

    pub fn spi(name: impl Into<String>) -> Self {
        Self { name: name.into(), kind: ResourceKind::Spi, logical_size: 0, mapped_size: 0 }
    }

    pub fn interrupt(name: impl Into<String>) -> Self {
        Self { name: name.into(), kind: ResourceKind::Interrupt, logical_size: 0, mapped_size: 0 }
    }

    pub fn clock(name: impl Into<String>) -> Self {
        Self { name: name.into(), kind: ResourceKind::Clock, logical_size: 0, mapped_size: 0 }
    }

    pub fn reset(name: impl Into<String>) -> Self {
        Self { name: name.into(), kind: ResourceKind::Reset, logical_size: 0, mapped_size: 0 }
    }

    pub fn serial(name: impl Into<String>) -> Self {
        Self { name: name.into(), kind: ResourceKind::Serial, logical_size: 0, mapped_size: 0 }
    }
}

/// Target ceiling specification for one writable register.
///
/// Writes are denied unless the immutable target ceiling contains an exact
/// writable-register entry (Spec 9.4). A host/session grant cannot invent a
/// write entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WritableRegister {
    /// Byte offset from the start of the logical resource.
    pub offset: u64,
    /// Access width in bytes (must be 4 for V1).
    pub width: u32,
    /// Bitmask of writable bits permitted by policy.
    pub allow_mask: u32,
    /// Whether read-modify-write is permitted for partial mask writes.
    pub allow_rmw: bool,
    /// Whether a precondition is required for any write to this register.
    pub require_precondition: bool,
    /// Bitmask of valid precondition bits.
    pub precondition_mask: u32,
    /// Whether readback is configured/supported.
    pub readback: bool,
}

/// A precondition required or checked before executing a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WritePrecondition {
    /// Expected value of bits selected by `mask`.
    pub expected: u32,
    /// Bitmask of bits to check.
    pub mask: u32,
}

/// Target ceiling policy for a protocol-backed resource (GPIO, I2C, SPI).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolCeiling {
    /// Allowed method names (e.g. "read", "write", "transfer", "transmit").
    pub allowed_methods: BTreeSet<String>,
    /// Maximum transfer or transmit size in bytes.
    pub max_transfer_size: u32,
    /// Whether mutating operations are permitted.
    pub allow_mutating: bool,
}

impl ProtocolCeiling {
    pub fn default_for(kind: ResourceKind) -> Self {
        let mut allowed_methods = BTreeSet::new();
        match kind {
            ResourceKind::Gpio => {
                allowed_methods.insert("read".to_string());
                allowed_methods.insert("write".to_string());
                Self { allowed_methods, max_transfer_size: 1, allow_mutating: true }
            }
            ResourceKind::I2c => {
                allowed_methods.insert("transfer".to_string());
                Self { allowed_methods, max_transfer_size: 8192, allow_mutating: true }
            }
            ResourceKind::Spi => {
                allowed_methods.insert("transmit".to_string());
                Self { allowed_methods, max_transfer_size: 8192, allow_mutating: true }
            }
            ResourceKind::Interrupt => {
                allowed_methods.insert("wait".to_string());
                Self { allowed_methods, max_transfer_size: 0, allow_mutating: false }
            }
            ResourceKind::Clock => {
                allowed_methods.insert("enable".to_string());
                allowed_methods.insert("disable".to_string());
                allowed_methods.insert("is_enabled".to_string());
                allowed_methods.insert("set_rate".to_string());
                allowed_methods.insert("query_rate".to_string());
                allowed_methods.insert("get_rate".to_string());
                Self { allowed_methods, max_transfer_size: 0, allow_mutating: true }
            }
            ResourceKind::Reset => {
                allowed_methods.insert("assert".to_string());
                allowed_methods.insert("deassert".to_string());
                allowed_methods.insert("toggle".to_string());
                allowed_methods.insert("status".to_string());
                Self { allowed_methods, max_transfer_size: 0, allow_mutating: true }
            }
            ResourceKind::Serial => {
                allowed_methods.insert("read".to_string());
                allowed_methods.insert("write".to_string());
                Self { allowed_methods, max_transfer_size: 8192, allow_mutating: true }
            }
            ResourceKind::Mmio => {
                Self { allowed_methods, max_transfer_size: 0, allow_mutating: false }
            }
        }
    }
}

/// Immutable per-resource read ceiling. A resource with no ceiling entry
/// permits nothing.
#[derive(Clone, Debug, Default)]
pub struct ResourceCeiling {
    /// Byte ranges that may never be read, even with operator consent.
    pub hard_denied: Vec<Range<u64>>,
    /// Whether otherwise-unclassified reads may be enabled by an exact
    /// session allowlist rule.
    pub allow_unknown_reads: bool,
    /// Whether repeated polling reads may be authorized by an exact
    /// session allowlist rule.
    pub allow_poll: bool,
    /// Exact writable registers permitted by immutable target policy.
    pub writable_registers: Vec<WritableRegister>,
    /// Protocol-specific ceiling when the resource is protocol-backed.
    pub protocol: Option<ProtocolCeiling>,
    /// Whether interrupt observation may be authorized by session allowlist.
    pub allow_interrupt: bool,
}

impl ResourceCeiling {
    /// Looks up a writable register definition at `offset`.
    pub fn get_writable_register(&self, offset: u64) -> Option<&WritableRegister> {
        self.writable_registers.iter().find(|r| r.offset == offset)
    }
}

/// One exact session allowlist rule. Wildcards do not exist: a rule matches
/// exactly one resource, offset, width, and access class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AccessRule {
    /// Resource the rule applies to.
    pub resource: ResourceId,
    /// Byte offset from the start of the logical resource.
    pub offset: u64,
    /// Access width in bytes.
    pub width: u32,
    /// Access class the rule authorizes.
    pub class: AccessClass,
}

/// Why an access or allowlist rule was rejected. Reasons stay
/// machine-readable; they must never exist only in a human detail string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denial {
    /// The resource ID is not offered by this proxy instance.
    UnknownResource,
    /// The requested width is not supported.
    UnsupportedWidth,
    /// The requested access class is not supported by the V1 executor.
    UnsupportedAccessClass,
    /// The offset is not aligned to the access width.
    Misaligned,
    /// `offset + width` overflows.
    OffsetOverflow,
    /// The access does not fit the logical resource size.
    OutOfLogicalBounds,
    /// The access does not fit the actual backing mapping.
    OutOfMappedBounds,
    /// The target ceiling has no entry for this resource.
    NotPermittedByCeiling,
    /// The offset lies in a hard-denied range; operator consent cannot
    /// enable it.
    HardDenied,
    /// The ceiling does not permit operator-authorized unknown reads on
    /// this resource.
    UnknownReadsNotPermitted,
    /// The ceiling does not permit polling on this resource.
    PollNotPermitted,
    /// No exact session allowlist rule matches the access.
    NotInAllowlist,
    /// A bounded-execution limit (for example maximum snapshot items) was
    /// exceeded.
    LimitExceeded,
    /// A session-open expectation (boot, generation, or digest) was stale.
    /// Used in audit records for rejected session opens.
    StaleIdentity,
    /// The exclusive mutation lease was already held.
    MutationLeaseContention,
    /// The proxy is stopping and accepts no new sessions.
    NotAccepting,
    /// The request set a field this phase does not implement (for
    /// example a phase 2 takeover expectation). Fail closed rather than
    /// silently ignoring the field.
    UnsupportedExpectation,
    /// A precondition specified for a write failed.
    PreconditionFailed,
    /// Target policy requires a precondition, but none was provided.
    MissingPrecondition,
    /// An asynchronous poll timed out without matching.
    Timeout,
    /// A write was attempted in a read-only session.
    ReadOnlySession,
    /// A write was attempted to an offset or bits not permitted by target policy.
    WriteNotPermitted,
    /// A requested protocol method is not permitted by target policy.
    UnsupportedMethod,
    /// A requested protocol transfer exceeds maximum allowed size.
    TransferTooLarge,
}

/// The validated policy engine for one session.
///
/// Construction validates the entire allowlist against the resources and
/// ceiling; a rejected allowlist creates no engine and therefore no
/// session, and no hardware operation is performed.
#[derive(Debug)]
pub struct AccessPolicy {
    mode: SessionMode,
    resources: BTreeMap<ResourceId, MmioResource>,
    ceiling: BTreeMap<ResourceId, ResourceCeiling>,
    allowlist: BTreeSet<AccessRule>,
}

impl AccessPolicy {
    /// Builds a policy engine, validating every allowlist rule structurally
    /// and against the ceiling. Returns the first offending rule and the
    /// reason on failure.
    pub fn new(
        mode: SessionMode,
        resources: BTreeMap<ResourceId, MmioResource>,
        ceiling: BTreeMap<ResourceId, ResourceCeiling>,
        allowlist: impl IntoIterator<Item = AccessRule>,
    ) -> Result<Self, (AccessRule, Denial)> {
        let allowlist: BTreeSet<AccessRule> = allowlist.into_iter().collect();
        let policy = Self { mode, resources, ceiling, allowlist };
        for rule in &policy.allowlist {
            policy.validate_rule(rule).map_err(|denial| (*rule, denial))?;
        }
        Ok(policy)
    }

    /// The session mode (read-only or mutating).
    pub fn mode(&self) -> SessionMode {
        self.mode
    }

    /// IDs of every resource this policy knows about.
    pub fn resource_ids(&self) -> impl Iterator<Item = ResourceId> {
        self.resources.keys().copied()
    }

    /// Returns the descriptor for `id`, if known.
    pub fn resource(&self, id: ResourceId) -> Option<&MmioResource> {
        self.resources.get(&id)
    }

    /// Immediately-before-access validation for a 32-bit read. This covers
    /// resource lookup, width, alignment, overflow, logical and mapped
    /// bounds, ceiling hard denies, unknown-read permission, and the exact
    /// allowlist match. Session/generation staleness is the session
    /// server's job and the volatile read itself is the executor's.
    pub fn check_read32(
        &self,
        resource: ResourceId,
        offset: u64,
        class: AccessClass,
    ) -> Result<(), Denial> {
        if !matches!(class, AccessClass::ReadOnce | AccessClass::Snapshot | AccessClass::Sequence) {
            return Err(Denial::UnsupportedAccessClass);
        }
        self.validate_read(resource, offset, WIDTH32)?;
        let rule = AccessRule { resource, offset, width: WIDTH32, class };
        let rule_seq =
            AccessRule { resource, offset, width: WIDTH32, class: AccessClass::Sequence };
        if !self.allowlist.contains(&rule) && !self.allowlist.contains(&rule_seq) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    /// Immediately-before-access validation for a 32-bit write.
    pub fn check_write32(
        &self,
        resource: ResourceId,
        offset: u64,
        write_mask: u32,
        precondition: Option<&WritePrecondition>,
    ) -> Result<&WritableRegister, Denial> {
        if self.mode != SessionMode::Mutating {
            return Err(Denial::ReadOnlySession);
        }
        if write_mask == 0 {
            return Err(Denial::WriteNotPermitted);
        }
        self.validate_write_rule(resource, offset, WIDTH32)?;
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        let reg = ceiling.get_writable_register(offset).ok_or(Denial::WriteNotPermitted)?;
        if (write_mask & !reg.allow_mask) != 0 {
            return Err(Denial::WriteNotPermitted);
        }
        if write_mask != 0xFFFF_FFFF && !reg.allow_rmw {
            return Err(Denial::WriteNotPermitted);
        }
        if reg.require_precondition && precondition.is_none() {
            return Err(Denial::MissingPrecondition);
        }
        if let Some(pre) = precondition {
            if !reg.allow_rmw {
                return Err(Denial::WriteNotPermitted);
            }
            if pre.mask == 0 || (pre.mask & !reg.precondition_mask) != 0 {
                return Err(Denial::WriteNotPermitted);
            }
        }
        let rule_write = AccessRule { resource, offset, width: WIDTH32, class: AccessClass::Write };
        let rule_seq =
            AccessRule { resource, offset, width: WIDTH32, class: AccessClass::Sequence };
        if !self.allowlist.contains(&rule_write) && !self.allowlist.contains(&rule_seq) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(reg)
    }

    /// Immediately-before-access validation for repeated polling reads.
    pub fn check_poll32(
        &self,
        resource: ResourceId,
        offset: u64,
        mask: u32,
        interval_ns: i64,
        timeout_ns: i64,
    ) -> Result<(), Denial> {
        if mask == 0 {
            return Err(Denial::Misaligned);
        }
        if interval_ns < 0 || timeout_ns < 0 {
            return Err(Denial::LimitExceeded);
        }
        self.validate_read(resource, offset, WIDTH32)?;
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        if !ceiling.allow_poll {
            return Err(Denial::PollNotPermitted);
        }
        let rule_poll = AccessRule { resource, offset, width: WIDTH32, class: AccessClass::Poll };
        let rule_seq =
            AccessRule { resource, offset, width: WIDTH32, class: AccessClass::Sequence };
        if !self.allowlist.contains(&rule_poll) && !self.allowlist.contains(&rule_seq) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    /// Pre-session validation of one allowlist rule.
    fn validate_rule(&self, rule: &AccessRule) -> Result<(), Denial> {
        match rule.class {
            AccessClass::ReadOnce | AccessClass::Snapshot => {
                self.validate_read(rule.resource, rule.offset, rule.width)
            }
            AccessClass::Poll => {
                self.validate_read(rule.resource, rule.offset, rule.width)?;
                let ceiling =
                    self.ceiling.get(&rule.resource).ok_or(Denial::NotPermittedByCeiling)?;
                if !ceiling.allow_poll {
                    return Err(Denial::PollNotPermitted);
                }
                Ok(())
            }
            AccessClass::Write => {
                if self.mode != SessionMode::Mutating {
                    return Err(Denial::ReadOnlySession);
                }
                self.validate_write_rule(rule.resource, rule.offset, rule.width)?;
                let ceiling =
                    self.ceiling.get(&rule.resource).ok_or(Denial::NotPermittedByCeiling)?;
                if ceiling.get_writable_register(rule.offset).is_none() {
                    return Err(Denial::WriteNotPermitted);
                }
                Ok(())
            }
            AccessClass::Sequence => {
                let desc = self.resources.get(&rule.resource).ok_or(Denial::UnknownResource)?;
                if desc.kind != ResourceKind::Mmio {
                    let ceiling =
                        self.ceiling.get(&rule.resource).ok_or(Denial::NotPermittedByCeiling)?;
                    if ceiling.protocol.is_none() {
                        return Err(Denial::NotPermittedByCeiling);
                    }
                    return Ok(());
                }
                if rule.width != WIDTH32 {
                    return Err(Denial::UnsupportedWidth);
                }
                if rule.offset % u64::from(rule.width) != 0 {
                    return Err(Denial::Misaligned);
                }
                let end =
                    rule.offset.checked_add(u64::from(rule.width)).ok_or(Denial::OffsetOverflow)?;
                if end > desc.logical_size {
                    return Err(Denial::OutOfLogicalBounds);
                }
                if end > desc.mapped_size {
                    return Err(Denial::OutOfMappedBounds);
                }
                let ceiling =
                    self.ceiling.get(&rule.resource).ok_or(Denial::NotPermittedByCeiling)?;
                if ceiling
                    .hard_denied
                    .iter()
                    .any(|range| range.start < end && rule.offset < range.end)
                {
                    return Err(Denial::HardDenied);
                }
                Ok(())
            }
            AccessClass::Protocol => {
                let desc = self.resources.get(&rule.resource).ok_or(Denial::UnknownResource)?;
                if desc.kind == ResourceKind::Mmio {
                    return Err(Denial::UnsupportedAccessClass);
                }
                let ceiling =
                    self.ceiling.get(&rule.resource).ok_or(Denial::NotPermittedByCeiling)?;
                if ceiling.protocol.is_none() {
                    return Err(Denial::NotPermittedByCeiling);
                }
                Ok(())
            }
            AccessClass::Interrupt => {
                let desc = self.resources.get(&rule.resource).ok_or(Denial::UnknownResource)?;
                if desc.kind != ResourceKind::Interrupt {
                    return Err(Denial::UnsupportedAccessClass);
                }
                let ceiling =
                    self.ceiling.get(&rule.resource).ok_or(Denial::NotPermittedByCeiling)?;
                if !ceiling.allow_interrupt {
                    return Err(Denial::NotPermittedByCeiling);
                }
                Ok(())
            }
        }
    }

    /// Immediately-before-access validation for an interrupt observation.
    pub fn check_interrupt(&self, resource: ResourceId) -> Result<(), Denial> {
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if desc.kind != ResourceKind::Interrupt {
            return Err(Denial::UnknownResource);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        if !ceiling.allow_interrupt {
            return Err(Denial::NotPermittedByCeiling);
        }
        let rule = AccessRule { resource, offset: 0, width: 0, class: AccessClass::Interrupt };
        if !self.allowlist.contains(&rule) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    /// Immediately-before-access validation for a GPIO read.
    pub fn check_gpio_read(&self, resource: ResourceId) -> Result<(), Denial> {
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if desc.kind != ResourceKind::Gpio {
            return Err(Denial::UnknownResource);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        let proto = ceiling.protocol.as_ref().ok_or(Denial::NotPermittedByCeiling)?;
        if !proto.allowed_methods.contains("read") {
            return Err(Denial::UnsupportedMethod);
        }
        if !self.is_protocol_allowed(resource, false) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    /// Immediately-before-access validation for a GPIO write. Requires mutation lease.
    pub fn check_gpio_write(&self, resource: ResourceId) -> Result<(), Denial> {
        if self.mode != SessionMode::Mutating {
            return Err(Denial::ReadOnlySession);
        }
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if desc.kind != ResourceKind::Gpio {
            return Err(Denial::UnknownResource);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        let proto = ceiling.protocol.as_ref().ok_or(Denial::NotPermittedByCeiling)?;
        if !proto.allow_mutating || !proto.allowed_methods.contains("write") {
            return Err(Denial::WriteNotPermitted);
        }
        if !self.is_protocol_allowed(resource, true) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    /// Immediately-before-access validation for an I2C transfer.
    pub fn check_i2c_transfer(
        &self,
        resource: ResourceId,
        write_len: usize,
        read_len: usize,
    ) -> Result<(), Denial> {
        let is_mutating = write_len > 0;
        if is_mutating && self.mode != SessionMode::Mutating {
            return Err(Denial::ReadOnlySession);
        }
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if desc.kind != ResourceKind::I2c {
            return Err(Denial::UnknownResource);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        let proto = ceiling.protocol.as_ref().ok_or(Denial::NotPermittedByCeiling)?;
        if !proto.allowed_methods.contains("transfer") {
            return Err(Denial::UnsupportedMethod);
        }
        if is_mutating && !proto.allow_mutating {
            return Err(Denial::WriteNotPermitted);
        }
        let total_len = write_len.saturating_add(read_len);
        if total_len as u32 > proto.max_transfer_size {
            return Err(Denial::TransferTooLarge);
        }
        if !self.is_protocol_allowed(resource, is_mutating) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    /// Immediately-before-access validation for a SPI transmit. Requires mutation lease.
    pub fn check_spi_transmit(&self, resource: ResourceId, tx_len: usize) -> Result<(), Denial> {
        if self.mode != SessionMode::Mutating {
            return Err(Denial::ReadOnlySession);
        }
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if desc.kind != ResourceKind::Spi {
            return Err(Denial::UnknownResource);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        let proto = ceiling.protocol.as_ref().ok_or(Denial::NotPermittedByCeiling)?;
        if !proto.allowed_methods.contains("transmit") {
            return Err(Denial::UnsupportedMethod);
        }
        if !proto.allow_mutating {
            return Err(Denial::WriteNotPermitted);
        }
        if tx_len as u32 > proto.max_transfer_size {
            return Err(Denial::TransferTooLarge);
        }
        if !self.is_protocol_allowed(resource, true) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    /// Immediately-before-access validation for Clock enable. Requires mutation lease.
    pub fn check_clock_enable(&self, resource: ResourceId) -> Result<(), Denial> {
        self.check_clock_op(resource, "enable", true)
    }

    /// Immediately-before-access validation for Clock disable. Requires mutation lease.
    pub fn check_clock_disable(&self, resource: ResourceId) -> Result<(), Denial> {
        self.check_clock_op(resource, "disable", true)
    }

    /// Immediately-before-access validation for Clock is_enabled query.
    pub fn check_clock_is_enabled(&self, resource: ResourceId) -> Result<(), Denial> {
        self.check_clock_op(resource, "is_enabled", false)
    }

    /// Immediately-before-access validation for Clock set_rate. Requires mutation lease.
    pub fn check_clock_set_rate(&self, resource: ResourceId) -> Result<(), Denial> {
        self.check_clock_op(resource, "set_rate", true)
    }

    /// Immediately-before-access validation for Clock query_rate.
    pub fn check_clock_query_rate(&self, resource: ResourceId) -> Result<(), Denial> {
        self.check_clock_op(resource, "query_rate", false)
    }

    /// Immediately-before-access validation for Clock get_rate.
    pub fn check_clock_get_rate(&self, resource: ResourceId) -> Result<(), Denial> {
        self.check_clock_op(resource, "get_rate", false)
    }

    fn check_clock_op(
        &self,
        resource: ResourceId,
        method: &str,
        is_mutating: bool,
    ) -> Result<(), Denial> {
        if is_mutating && self.mode != SessionMode::Mutating {
            return Err(Denial::ReadOnlySession);
        }
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if desc.kind != ResourceKind::Clock {
            return Err(Denial::UnknownResource);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        let proto = ceiling.protocol.as_ref().ok_or(Denial::NotPermittedByCeiling)?;
        if !proto.allowed_methods.contains(method) {
            return Err(Denial::UnsupportedMethod);
        }
        if is_mutating && !proto.allow_mutating {
            return Err(Denial::WriteNotPermitted);
        }
        if !self.is_protocol_allowed(resource, is_mutating) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    /// Immediately-before-access validation for Reset assert. Requires mutation lease.
    pub fn check_reset_assert(&self, resource: ResourceId) -> Result<(), Denial> {
        self.check_reset_op(resource, "assert", true)
    }

    /// Immediately-before-access validation for Reset deassert. Requires mutation lease.
    pub fn check_reset_deassert(&self, resource: ResourceId) -> Result<(), Denial> {
        self.check_reset_op(resource, "deassert", true)
    }

    /// Immediately-before-access validation for Reset toggle. Requires mutation lease.
    pub fn check_reset_toggle(&self, resource: ResourceId) -> Result<(), Denial> {
        self.check_reset_op(resource, "toggle", true)
    }

    /// Immediately-before-access validation for Reset status query.
    pub fn check_reset_status(&self, resource: ResourceId) -> Result<(), Denial> {
        self.check_reset_op(resource, "status", false)
    }

    fn check_reset_op(
        &self,
        resource: ResourceId,
        method: &str,
        is_mutating: bool,
    ) -> Result<(), Denial> {
        if is_mutating && self.mode != SessionMode::Mutating {
            return Err(Denial::ReadOnlySession);
        }
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if desc.kind != ResourceKind::Reset {
            return Err(Denial::UnknownResource);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        let proto = ceiling.protocol.as_ref().ok_or(Denial::NotPermittedByCeiling)?;
        if !proto.allowed_methods.contains(method) {
            return Err(Denial::UnsupportedMethod);
        }
        if is_mutating && !proto.allow_mutating {
            return Err(Denial::WriteNotPermitted);
        }
        if !self.is_protocol_allowed(resource, is_mutating) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    /// Immediately-before-access validation for Serial read.
    pub fn check_serial_read(&self, resource: ResourceId) -> Result<(), Denial> {
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if desc.kind != ResourceKind::Serial {
            return Err(Denial::UnknownResource);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        let proto = ceiling.protocol.as_ref().ok_or(Denial::NotPermittedByCeiling)?;
        if !proto.allowed_methods.contains("read") {
            return Err(Denial::UnsupportedMethod);
        }
        if !self.is_protocol_allowed(resource, false) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    /// Immediately-before-access validation for Serial write. Requires mutation lease.
    pub fn check_serial_write(&self, resource: ResourceId, tx_len: usize) -> Result<(), Denial> {
        if self.mode != SessionMode::Mutating {
            return Err(Denial::ReadOnlySession);
        }
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if desc.kind != ResourceKind::Serial {
            return Err(Denial::UnknownResource);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        let proto = ceiling.protocol.as_ref().ok_or(Denial::NotPermittedByCeiling)?;
        if !proto.allowed_methods.contains("write") {
            return Err(Denial::UnsupportedMethod);
        }
        if !proto.allow_mutating {
            return Err(Denial::WriteNotPermitted);
        }
        if tx_len as u32 > proto.max_transfer_size {
            return Err(Denial::TransferTooLarge);
        }
        if !self.is_protocol_allowed(resource, true) {
            return Err(Denial::NotInAllowlist);
        }
        Ok(())
    }

    fn is_protocol_allowed(&self, resource: ResourceId, is_mutating: bool) -> bool {
        self.allowlist.iter().any(|r| {
            r.resource == resource
                && (r.class == AccessClass::Protocol
                    || r.class == AccessClass::Sequence
                    || (is_mutating && r.class == AccessClass::Write)
                    || (!is_mutating
                        && (r.class == AccessClass::ReadOnce || r.class == AccessClass::Snapshot)))
        })
    }

    /// Structural and ceiling checks shared by rule prevalidation and
    /// per-access validation for reads.
    fn validate_read(&self, resource: ResourceId, offset: u64, width: u32) -> Result<(), Denial> {
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if width != WIDTH32 {
            return Err(Denial::UnsupportedWidth);
        }
        if offset % u64::from(width) != 0 {
            return Err(Denial::Misaligned);
        }
        let end = offset.checked_add(u64::from(width)).ok_or(Denial::OffsetOverflow)?;
        if end > desc.logical_size {
            return Err(Denial::OutOfLogicalBounds);
        }
        if end > desc.mapped_size {
            return Err(Denial::OutOfMappedBounds);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        if ceiling.hard_denied.iter().any(|range| range.start < end && offset < range.end) {
            return Err(Denial::HardDenied);
        }
        if !ceiling.allow_unknown_reads {
            return Err(Denial::UnknownReadsNotPermitted);
        }
        Ok(())
    }

    /// Structural and ceiling checks for writes.
    fn validate_write_rule(
        &self,
        resource: ResourceId,
        offset: u64,
        width: u32,
    ) -> Result<(), Denial> {
        let desc = self.resources.get(&resource).ok_or(Denial::UnknownResource)?;
        if width != WIDTH32 {
            return Err(Denial::UnsupportedWidth);
        }
        if offset % u64::from(width) != 0 {
            return Err(Denial::Misaligned);
        }
        let end = offset.checked_add(u64::from(width)).ok_or(Denial::OffsetOverflow)?;
        if end > desc.logical_size {
            return Err(Denial::OutOfLogicalBounds);
        }
        if end > desc.mapped_size {
            return Err(Denial::OutOfMappedBounds);
        }
        let ceiling = self.ceiling.get(&resource).ok_or(Denial::NotPermittedByCeiling)?;
        if ceiling.hard_denied.iter().any(|range| range.start < end && offset < range.end) {
            return Err(Denial::HardDenied);
        }
        let reg = ceiling.get_writable_register(offset).ok_or(Denial::WriteNotPermitted)?;
        if reg.width != width {
            return Err(Denial::UnsupportedWidth);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: ResourceId = 1;
    const LOCKED: ResourceId = 2;
    const NO_CEILING: ResourceId = 3;
    const SHORT_MAP: ResourceId = 4;

    fn resources() -> BTreeMap<ResourceId, MmioResource> {
        BTreeMap::from([
            (
                CTRL,
                MmioResource {
                    name: "ctrl".to_string(),
                    kind: ResourceKind::Mmio,
                    logical_size: 0x100,
                    mapped_size: 0x1000,
                },
            ),
            (
                LOCKED,
                MmioResource {
                    name: "locked".to_string(),
                    kind: ResourceKind::Mmio,
                    logical_size: 0x100,
                    mapped_size: 0x100,
                },
            ),
            (
                NO_CEILING,
                MmioResource {
                    name: "no-ceiling".to_string(),
                    kind: ResourceKind::Mmio,
                    logical_size: 0x100,
                    mapped_size: 0x100,
                },
            ),
            (
                SHORT_MAP,
                MmioResource {
                    name: "short-map".to_string(),
                    kind: ResourceKind::Mmio,
                    logical_size: 0x2000,
                    mapped_size: 0x10,
                },
            ),
        ])
    }

    fn ceiling() -> BTreeMap<ResourceId, ResourceCeiling> {
        BTreeMap::from([
            (
                CTRL,
                ResourceCeiling {
                    hard_denied: vec![0x40..0x44],
                    allow_unknown_reads: true,
                    allow_poll: true,
                    writable_registers: vec![
                        WritableRegister {
                            offset: 0x20,
                            width: WIDTH32,
                            allow_mask: 0x0000FFFF,
                            allow_rmw: true,
                            require_precondition: false,
                            precondition_mask: 0,
                            readback: false,
                        },
                        WritableRegister {
                            offset: 0x24,
                            width: WIDTH32,
                            allow_mask: 0xFFFFFFFF,
                            allow_rmw: true,
                            require_precondition: true,
                            precondition_mask: 0x00000001,
                            readback: true,
                        },
                    ],
                    protocol: None,
                    allow_interrupt: false,
                },
            ),
            (
                LOCKED,
                ResourceCeiling {
                    hard_denied: vec![],
                    allow_unknown_reads: false,
                    allow_poll: false,
                    writable_registers: vec![],
                    protocol: None,
                    allow_interrupt: false,
                },
            ),
            (
                SHORT_MAP,
                ResourceCeiling {
                    hard_denied: vec![],
                    allow_unknown_reads: true,
                    allow_poll: false,
                    writable_registers: vec![],
                    protocol: None,
                    allow_interrupt: false,
                },
            ),
        ])
    }

    fn read_rule(resource: ResourceId, offset: u64) -> AccessRule {
        AccessRule { resource, offset, width: WIDTH32, class: AccessClass::ReadOnce }
    }

    fn policy(rules: &[AccessRule]) -> AccessPolicy {
        AccessPolicy::new(SessionMode::ReadOnly, resources(), ceiling(), rules.iter().copied())
            .unwrap()
    }

    #[test]
    fn exact_read_allowed() {
        let policy = policy(&[read_rule(CTRL, 0x3c)]);
        assert_eq!(policy.check_read32(CTRL, 0x3c, AccessClass::ReadOnce), Ok(()));
    }

    #[test]
    fn nearby_offset_not_authorized() {
        let policy = policy(&[read_rule(CTRL, 0x3c)]);
        assert_eq!(
            policy.check_read32(CTRL, 0x38, AccessClass::ReadOnce),
            Err(Denial::NotInAllowlist)
        );
    }

    #[test]
    fn access_classes_are_distinct() {
        let policy = policy(&[read_rule(CTRL, 0x3c)]);
        assert_eq!(
            policy.check_read32(CTRL, 0x3c, AccessClass::Snapshot),
            Err(Denial::NotInAllowlist)
        );
        assert_eq!(
            policy.check_read32(CTRL, 0x3c, AccessClass::Poll),
            Err(Denial::UnsupportedAccessClass)
        );
        assert_eq!(
            policy.check_read32(CTRL, 0x3c, AccessClass::Write),
            Err(Denial::UnsupportedAccessClass)
        );
    }

    #[test]
    fn unknown_resource_denied() {
        let policy = policy(&[]);
        assert_eq!(
            policy.check_read32(99, 0x0, AccessClass::ReadOnce),
            Err(Denial::UnknownResource)
        );
    }

    #[test]
    fn misaligned_denied() {
        let policy = policy(&[read_rule(CTRL, 0x3c)]);
        assert_eq!(policy.check_read32(CTRL, 0x3e, AccessClass::ReadOnce), Err(Denial::Misaligned));
    }

    #[test]
    fn offset_overflow_denied() {
        let policy = policy(&[]);
        // u64::MAX - 3 is 4-byte aligned, so this reaches the overflow
        // check rather than the alignment check.
        assert_eq!(
            policy.check_read32(CTRL, u64::MAX - 3, AccessClass::ReadOnce),
            Err(Denial::OffsetOverflow)
        );
    }

    #[test]
    fn logical_bounds_enforced() {
        let policy = policy(&[read_rule(CTRL, 0xfc)]);
        assert_eq!(policy.check_read32(CTRL, 0xfc, AccessClass::ReadOnce), Ok(()));
        assert_eq!(
            policy.check_read32(CTRL, 0x100, AccessClass::ReadOnce),
            Err(Denial::OutOfLogicalBounds)
        );
    }

    #[test]
    fn mapped_bounds_enforced() {
        let policy = policy(&[read_rule(SHORT_MAP, 0xc)]);
        assert_eq!(policy.check_read32(SHORT_MAP, 0xc, AccessClass::ReadOnce), Ok(()));
        assert_eq!(
            policy.check_read32(SHORT_MAP, 0x10, AccessClass::ReadOnce),
            Err(Denial::OutOfMappedBounds)
        );
    }

    #[test]
    fn hard_denied_wins_over_allowlist() {
        // The rule cannot even enter the allowlist.
        let denied = AccessPolicy::new(
            SessionMode::ReadOnly,
            resources(),
            ceiling(),
            [read_rule(CTRL, 0x40)],
        );
        assert_eq!(denied.unwrap_err(), (read_rule(CTRL, 0x40), Denial::HardDenied));
        // And a direct check is denied before the allowlist is consulted.
        let policy = policy(&[]);
        assert_eq!(policy.check_read32(CTRL, 0x40, AccessClass::ReadOnce), Err(Denial::HardDenied));
    }

    #[test]
    fn read_adjacent_to_hard_denied_range_allowed() {
        let policy = policy(&[read_rule(CTRL, 0x44)]);
        assert_eq!(policy.check_read32(CTRL, 0x44, AccessClass::ReadOnce), Ok(()));
    }

    #[test]
    fn unknown_reads_disabled_by_ceiling() {
        let denied = AccessPolicy::new(
            SessionMode::ReadOnly,
            resources(),
            ceiling(),
            [read_rule(LOCKED, 0x0)],
        );
        assert_eq!(denied.unwrap_err(), (read_rule(LOCKED, 0x0), Denial::UnknownReadsNotPermitted));
    }

    #[test]
    fn missing_ceiling_entry_permits_nothing() {
        let policy = policy(&[]);
        assert_eq!(
            policy.check_read32(NO_CEILING, 0x0, AccessClass::ReadOnce),
            Err(Denial::NotPermittedByCeiling)
        );
    }

    #[test]
    fn allowlist_prevalidation_checks_poll_permission() {
        // CTRL has allow_poll: true
        let rule_poll_allowed =
            AccessRule { resource: CTRL, offset: 0x0, width: WIDTH32, class: AccessClass::Poll };
        assert!(
            AccessPolicy::new(SessionMode::ReadOnly, resources(), ceiling(), [rule_poll_allowed])
                .is_ok()
        );

        // SHORT_MAP has allow_poll: false
        let rule_poll_denied = AccessRule {
            resource: SHORT_MAP,
            offset: 0x0,
            width: WIDTH32,
            class: AccessClass::Poll,
        };
        let denied =
            AccessPolicy::new(SessionMode::ReadOnly, resources(), ceiling(), [rule_poll_denied]);
        assert_eq!(denied.unwrap_err(), (rule_poll_denied, Denial::PollNotPermitted));
    }

    #[test]
    fn allowlist_prevalidation_rejects_write_in_read_only_session() {
        let rule =
            AccessRule { resource: CTRL, offset: 0x20, width: WIDTH32, class: AccessClass::Write };
        let denied = AccessPolicy::new(SessionMode::ReadOnly, resources(), ceiling(), [rule]);
        assert_eq!(denied.unwrap_err(), (rule, Denial::ReadOnlySession));
    }

    #[test]
    fn allowlist_prevalidation_accepts_valid_write_in_mutating_session() {
        let rule =
            AccessRule { resource: CTRL, offset: 0x20, width: WIDTH32, class: AccessClass::Write };
        let policy = AccessPolicy::new(SessionMode::Mutating, resources(), ceiling(), [rule]);
        assert!(policy.is_ok());
    }

    #[test]
    fn write_checks_enforce_session_mode_mask_and_preconditions() {
        let write_rule =
            AccessRule { resource: CTRL, offset: 0x20, width: WIDTH32, class: AccessClass::Write };
        let write_rule_pre =
            AccessRule { resource: CTRL, offset: 0x24, width: WIDTH32, class: AccessClass::Write };
        let policy = AccessPolicy::new(
            SessionMode::Mutating,
            resources(),
            ceiling(),
            [write_rule, write_rule_pre],
        )
        .unwrap();

        // Valid write on offset 0x20 (allow_mask 0x0000FFFF)
        let reg = policy.check_write32(CTRL, 0x20, 0x000000FF, None).unwrap();
        assert_eq!(reg.offset, 0x20);

        // Disallowed bit in write_mask
        assert_eq!(
            policy.check_write32(CTRL, 0x20, 0x00010000, None),
            Err(Denial::WriteNotPermitted)
        );

        // Zero mask rejected
        assert_eq!(policy.check_write32(CTRL, 0x20, 0, None), Err(Denial::WriteNotPermitted));

        // Offset 0x24 requires precondition
        assert_eq!(
            policy.check_write32(CTRL, 0x24, 0xFFFFFFFF, None),
            Err(Denial::MissingPrecondition)
        );

        // Invalid precondition mask (precondition_mask is 0x1)
        let bad_pre = WritePrecondition { expected: 1, mask: 0x2 };
        assert_eq!(
            policy.check_write32(CTRL, 0x24, 0xFFFFFFFF, Some(&bad_pre)),
            Err(Denial::WriteNotPermitted)
        );

        // Valid precondition
        let good_pre = WritePrecondition { expected: 1, mask: 0x1 };
        assert!(policy.check_write32(CTRL, 0x24, 0xFFFFFFFF, Some(&good_pre)).is_ok());
    }

    #[test]
    fn allowlist_prevalidation_rejects_bad_width() {
        let rule =
            AccessRule { resource: CTRL, offset: 0x0, width: 8, class: AccessClass::ReadOnce };
        let denied = AccessPolicy::new(SessionMode::ReadOnly, resources(), ceiling(), [rule]);
        assert_eq!(denied.unwrap_err(), (rule, Denial::UnsupportedWidth));
    }

    #[test]
    fn allowlist_prevalidation_rejects_out_of_bounds() {
        let rule = read_rule(CTRL, 0x200);
        let denied = AccessPolicy::new(SessionMode::ReadOnly, resources(), ceiling(), [rule]);
        assert_eq!(denied.unwrap_err(), (rule, Denial::OutOfLogicalBounds));
    }

    #[test]
    fn protocol_policy_gpio_and_i2c_and_spi() {
        const GPIO_ID: ResourceId = 10;
        const I2C_ID: ResourceId = 11;
        const SPI_ID: ResourceId = 12;

        let mut res = BTreeMap::new();
        res.insert(GPIO_ID, MmioResource::gpio("gpio-a"));
        res.insert(I2C_ID, MmioResource::i2c("i2c-sensor"));
        res.insert(SPI_ID, MmioResource::spi("spi-flash"));

        let mut ceil = BTreeMap::new();
        let mut gpio_ceil = ResourceCeiling::default();
        gpio_ceil.protocol = Some(ProtocolCeiling::default_for(ResourceKind::Gpio));
        ceil.insert(GPIO_ID, gpio_ceil);

        let mut i2c_ceil = ResourceCeiling::default();
        i2c_ceil.protocol = Some(ProtocolCeiling::default_for(ResourceKind::I2c));
        ceil.insert(I2C_ID, i2c_ceil);

        let mut spi_ceil = ResourceCeiling::default();
        spi_ceil.protocol = Some(ProtocolCeiling::default_for(ResourceKind::Spi));
        ceil.insert(SPI_ID, spi_ceil);

        let proto_rule =
            |r| AccessRule { resource: r, offset: 0, width: 0, class: AccessClass::Protocol };

        // Read-only session
        let ro_policy = AccessPolicy::new(
            SessionMode::ReadOnly,
            res.clone(),
            ceil.clone(),
            [proto_rule(GPIO_ID), proto_rule(I2C_ID), proto_rule(SPI_ID)],
        )
        .expect("policy creation ok");

        // GPIO read ok in read-only
        assert!(ro_policy.check_gpio_read(GPIO_ID).is_ok());
        // GPIO write rejected in read-only session
        assert_eq!(ro_policy.check_gpio_write(GPIO_ID), Err(Denial::ReadOnlySession));

        // I2C read-only transfer (write_len=0, read_len=4) ok
        assert!(ro_policy.check_i2c_transfer(I2C_ID, 0, 4).is_ok());
        // I2C write transfer (write_len=2) rejected in read-only
        assert_eq!(ro_policy.check_i2c_transfer(I2C_ID, 2, 4), Err(Denial::ReadOnlySession));

        // SPI transmit (mutating) rejected in read-only
        assert_eq!(ro_policy.check_spi_transmit(SPI_ID, 4), Err(Denial::ReadOnlySession));

        // Mutating session
        let mut_policy = AccessPolicy::new(
            SessionMode::Mutating,
            res,
            ceil,
            [proto_rule(GPIO_ID), proto_rule(I2C_ID), proto_rule(SPI_ID)],
        )
        .expect("policy creation ok");

        // Now mutating operations succeed
        assert!(mut_policy.check_gpio_write(GPIO_ID).is_ok());
        assert!(mut_policy.check_i2c_transfer(I2C_ID, 2, 4).is_ok());
        assert!(mut_policy.check_spi_transmit(SPI_ID, 4).is_ok());

        // Transfer exceeding max transfer size rejected
        assert_eq!(
            mut_policy.check_i2c_transfer(I2C_ID, 5000, 5000),
            Err(Denial::TransferTooLarge)
        );
        assert_eq!(mut_policy.check_spi_transmit(SPI_ID, 10000), Err(Denial::TransferTooLarge));
    }
}
