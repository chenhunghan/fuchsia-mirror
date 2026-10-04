// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Software state banks, runtime knobs, and diagnostic triggers for in-situ
//! driver experimentation.
//!
//! [`StateBank`] implements [`MmioBackend`] and [`ResourceBackend`] over a
//! 32-bit word-addressed virtual bank of 4-byte aligned offsets. A driver can
//! register:
//! - **Observable state slots** ([`StateBank::define_state_slot`]): lock-free
//!   [`AtomicU32`] slots readable by the host (via `read32`, `poll32`, or
//!   `snapshot`) and updated by the driver via [`StateSlotHandle`].
//! - **Runtime knobs** ([`StateBank::define_knob`]): writable [`AtomicU32`]
//!   slots (included in `writable_registers`) for fault/race-window injection
//!   or runtime fix-toggle experiments.
//! - **Read probes** ([`StateBank::define_read_probe`]): synchronous callbacks
//!   evaluated on demand when the host reads the offset.
//! - **Diagnostic triggers** ([`StateBank::define_trigger`]): writable offsets
//!   backed by a bounded callback `Fn(u32) -> Result<u32, BackendError>` whose
//!   returned status word is stored for readback and subsequent polling.

use crate::access_policy::{ResourceCeiling, WritableRegister};
use crate::hardware_backend::{BackendError, MmioBackend};
use crate::protocol_resource_adapter::ResourceBackend;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering, fence};

const WORD_SIZE: u64 = 4;
const FULL_WRITE_MASK: u32 = 0xFFFF_FFFF;

/// Lock-free handle to a 32-bit state slot or runtime knob shared between an
/// instrumented driver and a [`StateBank`].
#[derive(Clone, Debug)]
pub struct StateSlotHandle {
    inner: Arc<AtomicU32>,
}

impl StateSlotHandle {
    /// Creates a new slot handle initialized to `initial`.
    pub fn new(initial: u32) -> Self {
        Self { inner: Arc::new(AtomicU32::new(initial)) }
    }

    /// Loads the current 32-bit value with sequentially consistent ordering.
    pub fn load(&self) -> u32 {
        self.inner.load(Ordering::SeqCst)
    }

    /// Stores a new 32-bit value with sequentially consistent ordering.
    pub fn store(&self, value: u32) {
        self.inner.store(value, Ordering::SeqCst);
    }

    /// Atomically adds `delta` to the current value, returning the previous value.
    pub fn fetch_add(&self, delta: u32) -> u32 {
        self.inner.fetch_add(delta, Ordering::SeqCst)
    }

    /// Atomically bitwise-ORs `mask` into the current value, returning the previous value.
    pub fn fetch_or(&self, mask: u32) -> u32 {
        self.inner.fetch_or(mask, Ordering::SeqCst)
    }

    /// Atomically bitwise-ANDs `mask` into the current value, returning the previous value.
    pub fn fetch_and(&self, mask: u32) -> u32 {
        self.inner.fetch_and(mask, Ordering::SeqCst)
    }

    /// Returns a reference to the underlying [`AtomicU32`].
    pub fn as_atomic(&self) -> &AtomicU32 {
        &self.inner
    }
}

impl Default for StateSlotHandle {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Callback type for on-demand 32-bit state read probes.
pub type ReadProbeFn = dyn Fn() -> Result<u32, BackendError> + Send + Sync;

/// Callback type for write-triggered diagnostic actions.
pub type TriggerFn = dyn Fn(u32) -> Result<u32, BackendError> + Send + Sync;

/// Entry bound to a 4-byte aligned offset inside a [`StateBank`].
#[derive(Clone)]
pub enum StateEntry {
    /// Lock-free atomic 32-bit slot (read-only state slot or writable runtime knob).
    AtomicSlot(Arc<AtomicU32>),
    /// Synchronous read callback computed on demand during `read32`.
    ReadProbe(Arc<ReadProbeFn>),
    /// Write-triggered callback that stores its returned 32-bit status code into
    /// `last_result` for immediate readback or subsequent `read32`/`poll32`.
    Trigger {
        /// Most recent result returned by `callback` (initialized to `0`).
        last_result: Arc<AtomicU32>,
        /// Bounded callback executed on `write32`.
        callback: Arc<TriggerFn>,
    },
}

impl std::fmt::Debug for StateEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AtomicSlot(slot) => {
                f.debug_tuple("AtomicSlot").field(&slot.load(Ordering::SeqCst)).finish()
            }
            Self::ReadProbe(_) => f.debug_struct("ReadProbe").field("callback", &"<fn>").finish(),
            Self::Trigger { last_result, .. } => f
                .debug_struct("Trigger")
                .field("last_result", &last_result.load(Ordering::SeqCst))
                .finish(),
        }
    }
}

/// Word-addressed software state bank exposing observable driver state slots,
/// policy-gated runtime knobs, read probes, and diagnostic triggers.
#[derive(Clone, Debug)]
pub struct StateBank {
    logical_size: u64,
    entries: BTreeMap<u64, StateEntry>,
    writable_offsets: Vec<u64>,
}

impl StateBank {
    /// Creates an empty [`StateBank`] with the given byte `logical_size`.
    pub fn new(logical_size: u64) -> Self {
        Self { logical_size, entries: BTreeMap::new(), writable_offsets: Vec::new() }
    }

    /// Returns the logical byte size of the state bank.
    pub fn logical_size(&self) -> u64 {
        self.logical_size
    }

    /// Returns the 4-byte aligned offsets that are configured as writable knobs
    /// or triggers.
    pub fn writable_offsets(&self) -> &[u64] {
        &self.writable_offsets
    }

    /// Builds the default [`ResourceCeiling`] for this state bank, allowing
    /// 32-bit reads and polls within `logical_size` and permitting writes only
    /// on offsets registered via [`Self::define_knob`] or [`Self::define_trigger`].
    pub fn ceiling(&self) -> ResourceCeiling {
        ResourceCeiling {
            hard_denied: Vec::new(),
            allow_unknown_reads: true,
            allow_poll: true,
            writable_registers: self
                .writable_offsets
                .iter()
                .map(|&offset| WritableRegister {
                    offset,
                    width: WORD_SIZE as u32,
                    allow_mask: FULL_WRITE_MASK,
                    allow_rmw: true,
                    require_precondition: false,
                    precondition_mask: 0,
                    readback: true,
                })
                .collect(),
            protocol: None,
            allow_interrupt: false,
        }
    }

    /// Defines a read-only 32-bit observable state slot at a 4-byte aligned
    /// `offset` and returns a [`StateSlotHandle`] for the driver to update.
    pub fn define_state_slot(&mut self, offset: u64, initial: u32) -> StateSlotHandle {
        let handle = StateSlotHandle::new(initial);
        self.writable_offsets.retain(|&existing| existing != offset);
        self.entries.insert(offset, StateEntry::AtomicSlot(handle.inner.clone()));
        handle
    }

    /// Defines a policy-gated writable 32-bit runtime knob at a 4-byte aligned
    /// `offset` and returns a [`StateSlotHandle`] for the driver to read.
    pub fn define_knob(&mut self, offset: u64, initial: u32) -> StateSlotHandle {
        let handle = StateSlotHandle::new(initial);
        if !self.writable_offsets.contains(&offset) {
            self.writable_offsets.push(offset);
        }
        self.entries.insert(offset, StateEntry::AtomicSlot(handle.inner.clone()));
        handle
    }

    /// Defines a read-only on-demand probe callback at a 4-byte aligned `offset`.
    pub fn define_read_probe(
        &mut self,
        offset: u64,
        probe: impl Fn() -> Result<u32, BackendError> + Send + Sync + 'static,
    ) {
        self.writable_offsets.retain(|&existing| existing != offset);
        self.entries.insert(offset, StateEntry::ReadProbe(Arc::new(probe)));
    }

    /// Defines a write-triggered diagnostic action at a 4-byte aligned `offset`.
    ///
    /// Writing `value` to `offset` invokes `trigger(value)` and stores the
    /// returned `u32` status into the entry's `last_result` slot so `readback`
    /// or subsequent `read32`/`poll32` calls observe the trigger outcome.
    pub fn define_trigger(
        &mut self,
        offset: u64,
        trigger: impl Fn(u32) -> Result<u32, BackendError> + Send + Sync + 'static,
    ) {
        if !self.writable_offsets.contains(&offset) {
            self.writable_offsets.push(offset);
        }
        self.entries.insert(
            offset,
            StateEntry::Trigger {
                last_result: Arc::new(AtomicU32::new(0)),
                callback: Arc::new(trigger),
            },
        );
    }

    fn check_aligned_bounds(&self, offset: u64) -> Result<(), BackendError> {
        if !offset.is_multiple_of(WORD_SIZE) {
            return Err(BackendError::Fault);
        }
        let end = offset.checked_add(WORD_SIZE).ok_or(BackendError::Fault)?;
        if end > self.logical_size {
            return Err(BackendError::Fault);
        }
        Ok(())
    }
}

impl MmioBackend for StateBank {
    fn read32(&mut self, offset: u64) -> Result<u32, BackendError> {
        self.check_aligned_bounds(offset)?;
        match self.entries.get(&offset) {
            Some(StateEntry::AtomicSlot(slot)) => Ok(slot.load(Ordering::SeqCst)),
            Some(StateEntry::ReadProbe(probe)) => probe(),
            Some(StateEntry::Trigger { last_result, .. }) => Ok(last_result.load(Ordering::SeqCst)),
            None => Ok(0),
        }
    }

    fn write32(&mut self, offset: u64, value: u32) -> Result<(), BackendError> {
        self.check_aligned_bounds(offset)?;
        if !self.writable_offsets.contains(&offset) {
            return Err(BackendError::Fault);
        }
        match self.entries.get(&offset) {
            Some(StateEntry::AtomicSlot(slot)) => {
                slot.store(value, Ordering::SeqCst);
                Ok(())
            }
            Some(StateEntry::Trigger { last_result, callback }) => {
                let outcome = callback(value)?;
                last_result.store(outcome, Ordering::SeqCst);
                Ok(())
            }
            Some(StateEntry::ReadProbe(_)) | None => Err(BackendError::Fault),
        }
    }

    fn barrier(&mut self) {
        fence(Ordering::SeqCst);
    }
}

impl ResourceBackend for StateBank {
    fn read32(&mut self, offset: u64) -> Result<u32, BackendError> {
        MmioBackend::read32(self, offset)
    }

    fn write32(&mut self, offset: u64, value: u32) -> Result<(), BackendError> {
        MmioBackend::write32(self, offset, value)
    }

    fn barrier(&mut self) {
        MmioBackend::barrier(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access_policy::{
        AccessClass, AccessPolicy, AccessRule, Denial, MmioResource, SessionMode,
    };
    use crate::audit_ring::AuditRing;
    use crate::executor::{ExecLimits, Executor};
    use crate::hardware_backend::FakeClock;

    #[test]
    fn state_slot_and_knob_roundtrip() {
        let mut bank = StateBank::new(0x20);
        let state_slot = bank.define_state_slot(0x00, 0x1111_2222);
        let knob_slot = bank.define_knob(0x04, 0);

        assert_eq!(bank.logical_size(), 0x20);
        assert_eq!(bank.writable_offsets(), &[0x04]);

        // Driver updates state slot; host reads it.
        assert_eq!(MmioBackend::read32(&mut bank, 0x00), Ok(0x1111_2222));
        state_slot.store(0x3333_4444);
        assert_eq!(MmioBackend::read32(&mut bank, 0x00), Ok(0x3333_4444));
        assert_eq!(state_slot.fetch_add(1), 0x3333_4444);
        assert_eq!(MmioBackend::read32(&mut bank, 0x00), Ok(0x3333_4445));

        // Direct backend write to read-only state slot fails.
        assert_eq!(MmioBackend::write32(&mut bank, 0x00, 0x9999), Err(BackendError::Fault));
        assert_eq!(state_slot.load(), 0x3333_4445);

        // Host writes runtime knob; driver observes new knob value.
        assert_eq!(MmioBackend::write32(&mut bank, 0x04, 250), Ok(()));
        assert_eq!(knob_slot.load(), 250);
        assert_eq!(MmioBackend::read32(&mut bank, 0x04), Ok(250));
    }

    #[test]
    fn read_probe_and_trigger_execution() {
        let mut bank = StateBank::new(0x20);
        let counter = Arc::new(AtomicU32::new(10));
        let counter_for_probe = counter.clone();
        bank.define_read_probe(0x08, move || Ok(counter_for_probe.fetch_add(1, Ordering::SeqCst)));

        let trigger_calls = Arc::new(AtomicU32::new(0));
        let trigger_calls_clone = trigger_calls.clone();
        bank.define_trigger(0x0C, move |arg| {
            if arg == 0xBAD {
                return Err(BackendError::Fault);
            }
            trigger_calls_clone.fetch_add(1, Ordering::SeqCst);
            Ok(arg.wrapping_mul(2))
        });

        // Read probe computes on demand and rejects writes.
        assert_eq!(MmioBackend::read32(&mut bank, 0x08), Ok(10));
        assert_eq!(MmioBackend::read32(&mut bank, 0x08), Ok(11));
        assert_eq!(MmioBackend::write32(&mut bank, 0x08, 1), Err(BackendError::Fault));

        // Trigger starts with last_result == 0, updates on write32, and propagates faults.
        assert_eq!(MmioBackend::read32(&mut bank, 0x0C), Ok(0));
        assert_eq!(MmioBackend::write32(&mut bank, 0x0C, 21), Ok(()));
        assert_eq!(trigger_calls.load(Ordering::SeqCst), 1);
        assert_eq!(MmioBackend::read32(&mut bank, 0x0C), Ok(42));

        assert_eq!(MmioBackend::write32(&mut bank, 0x0C, 0xBAD), Err(BackendError::Fault));
        assert_eq!(MmioBackend::read32(&mut bank, 0x0C), Ok(42));
    }

    #[test]
    fn executor_enforces_state_bank_ceiling_and_supports_trigger_readback() {
        let mut bank = StateBank::new(0x20);
        let violations = bank.define_state_slot(0x00, 0);
        let fix_knob = bank.define_knob(0x04, 0);
        let violations_for_trigger = violations.clone();
        let fix_for_trigger = fix_knob.clone();
        bank.define_trigger(0x08, move |_arg| {
            if fix_for_trigger.load() == 0 {
                violations_for_trigger.fetch_add(1);
                Ok(1)
            } else {
                Ok(0)
            }
        });

        let ceiling = bank.ceiling();
        let mut resources = BTreeMap::new();
        resources.insert(0, MmioResource::mmio("state0", 0x20, 0x20));
        let mut ceilings = BTreeMap::new();
        ceilings.insert(0, ceiling);

        // Policy rejects allowlist attempting to write read-only state slot 0x00.
        let bad_rules =
            vec![AccessRule { resource: 0, offset: 0x00, width: 4, class: AccessClass::Write }];
        assert!(
            AccessPolicy::new(
                SessionMode::Mutating,
                resources.clone(),
                ceilings.clone(),
                bad_rules
            )
            .is_err()
        );

        // Valid mutating session allowlist for reading 0x00 and writing knob 0x04 + trigger 0x08.
        let rules = vec![
            AccessRule { resource: 0, offset: 0x00, width: 4, class: AccessClass::ReadOnce },
            AccessRule { resource: 0, offset: 0x04, width: 4, class: AccessClass::Write },
            AccessRule { resource: 0, offset: 0x08, width: 4, class: AccessClass::Write },
        ];
        let policy = AccessPolicy::new(SessionMode::Mutating, resources, ceilings, rules)
            .expect("valid policy");

        let mut backends = BTreeMap::new();
        backends.insert(0, bank);
        let mut executor = Executor::new(backends, FakeClock::new(100, 10), ExecLimits::default());
        let mut audit = AuditRing::new(32);

        // Attempting to write read-only slot 0x00 is denied by policy.
        let denied =
            executor.write32(&policy, &mut audit, 1, 0, 0x00, 1, FULL_WRITE_MASK, None, true);
        assert!(matches!(
            denied,
            Err(crate::executor::WriteError::Denied { denial: Denial::WriteNotPermitted, .. })
        ));

        // Trigger with fix_knob == 0 increments violations and returns readback 1.
        let trig_bug = executor
            .write32(&policy, &mut audit, 1, 0, 0x08, 1, FULL_WRITE_MASK, None, true)
            .expect("trigger write");
        assert_eq!(trig_bug.readback_value, 1);
        let read_v = executor.read32(&policy, &mut audit, 1, 0, 0x00).expect("read violations");
        assert_eq!(read_v.value, 1);

        // Enable fix_knob = 1 and re-trigger: readback is 0 and violations remain 1.
        let knob_write = executor
            .write32(&policy, &mut audit, 1, 0, 0x04, 1, FULL_WRITE_MASK, None, true)
            .expect("knob write");
        assert_eq!(knob_write.readback_value, 1);
        let trig_fixed = executor
            .write32(&policy, &mut audit, 1, 0, 0x08, 1, FULL_WRITE_MASK, None, true)
            .expect("trigger write with fix");
        assert_eq!(trig_fixed.readback_value, 0);
        let read_v2 =
            executor.read32(&policy, &mut audit, 1, 0, 0x00).expect("read violations after fix");
        assert_eq!(read_v2.value, 1);
    }
}
