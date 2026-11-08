// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::errors::FifoError;
use crate::header::{
    FIFO_STATE_CLOSED, ReservationBacked, SharedStateWord, TRANSACTION_METADATA_MAX_AMOUNT,
};
use crate::{
    FIFO_CLOSED_SIGNAL, FIFO_READABLE_SIGNAL, FIFO_WRITABLE_SIGNAL, MappedFifoVmo, bincode_options,
};
use bincode::Options;
use serde::de::DeserializeOwned;
use std::fmt;
use std::marker::PhantomData;
#[cfg(test)]
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Explicit interface for the reader role (Fxfs).
pub struct FifoReader<T> {
    mapping: MappedFifoVmo,
    /// Last known transaction sequence number consumed by Fxfs (`fxfs_last_processed_write_ptr`).
    last_known_write_ptr: u16,
    /// Local Fxfs tracking of the reader-owned `layer_seq` field in `shared_state`.
    /// Validated whenever `shared_state` is inspected or updated to detect peer tampering.
    last_known_layer_seq: u32,
    /// Local Fxfs tracking of the last known available metadata space.
    /// Used to detect if an untrusted peer illegally increased the value in shared memory.
    last_known_available_metadata_space: u64,
    /// Whether the FIFO has been fused closed due to an unrecoverable error.
    fused: bool,
    /// Reusable buffer for copying serialized payloads out of shared memory into private memory
    /// before deserialization without per-slot heap allocations.
    scratch_buf: Vec<u8>,
    #[cfg(test)]
    pub(crate) callback_after_layer_seq_increment: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    pub(crate) callback_before_read_ptr_update: Option<Arc<dyn Fn() + Send + Sync>>,
    _marker: PhantomData<T>,
}

impl<T> fmt::Debug for FifoReader<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FifoReader")
            .field("mapping", &self.mapping)
            .field("last_known_write_ptr", &self.last_known_write_ptr)
            .field("last_known_layer_seq", &self.last_known_layer_seq)
            .field("last_known_available_metadata_space", &self.last_known_available_metadata_space)
            .field("fused", &self.is_fused())
            .finish()
    }
}

impl<T> FifoReader<T> {
    /// Creates a new reader interface over a zero-initialized `vmo` with the negotiated
    /// `capacity_slots`.
    /// The slot size in bytes is automatically derived from the VMO size and capacity.
    pub fn new(vmo: zx::Vmo, capacity_slots: u32) -> Result<Self, FifoError> {
        let mapping = MappedFifoVmo::new(vmo, capacity_slots)?;
        if mapping.shared_state_atomic().load(Ordering::Acquire) != 0
            || mapping.available_metadata_space_atomic().load(Ordering::Relaxed) != 0
            || mapping.fifo_state_atomic().load(Ordering::Acquire) != 0
        {
            return Err(FifoError::InvalidState);
        }
        Ok(Self {
            mapping,
            last_known_write_ptr: 0,
            last_known_layer_seq: 0,
            last_known_available_metadata_space: 0,
            fused: false,
            scratch_buf: Vec::new(),
            #[cfg(test)]
            callback_after_layer_seq_increment: None,
            #[cfg(test)]
            callback_before_read_ptr_update: None,
            _marker: PhantomData,
        })
    }

    /// Returns a reference to the underlying VMO.
    pub fn vmo(&self) -> &zx::Vmo {
        self.mapping.vmo()
    }

    /// Marks the reader as fused, sets [`FIFO_STATE_CLOSED`] in the shared header, and asserts
    /// [`FIFO_CLOSED_SIGNAL`] (`USER_4`) on the shared VMO to wake any blocked waiters.
    pub fn fuse(&mut self) {
        if self.fused {
            return;
        }
        self.fused = true;
        // `Ordering::Release` ensures all prior reader updates to `shared_state` happen-before a
        // writer observes `FIFO_STATE_CLOSED`; pairs with the `Ordering::Acquire` load in
        // `FifoWriter::is_closed`.
        self.mapping.fifo_state_atomic().fetch_or(FIFO_STATE_CLOSED, Ordering::Release);
        let _ = self.vmo().signal(zx::Signals::empty(), FIFO_CLOSED_SIGNAL);
    }

    /// Returns `true` if the FIFO has been fused closed due to an unrecoverable error.
    pub fn is_fused(&self) -> bool {
        self.fused
    }

    /// Returns the current available metadata space as observed in shared memory.
    pub fn available_metadata_space(&self) -> u64 {
        // `Ordering::Relaxed` suffices: `available_metadata_space` is a standalone credit counter
        // that does not guard any auxiliary shared memory. At tamper-check sites after draining,
        // the writer's prior `reserve_metadata_space` decrement already happened-before this load
        // via the `AcqRel`/`Acquire` synchronization on `shared_state`.
        self.mapping.available_metadata_space_atomic().load(Ordering::Relaxed)
    }

    /// Returns the last known available metadata space tracked locally by Fxfs.
    pub fn last_known_available_metadata_space(&self) -> u64 {
        self.last_known_available_metadata_space
    }

    /// Sets the available metadata space to an exact value and updates local tracking.
    pub fn set_available_metadata_space(&mut self, amount: u64) {
        self.last_known_available_metadata_space = amount;
        // `Ordering::Relaxed` suffices: initializes the standalone credit balance before sharing
        // the VMO (or in single-threaded test setup) and guards no auxiliary shared memory.
        self.mapping.available_metadata_space_atomic().store(amount, Ordering::Relaxed);
    }

    /// Adds `amount` of metadata space back into shared memory.
    /// Fails with [`FifoError::InvalidState`] if the stored value in shared memory is greater
    /// than `last_known_available_metadata_space` (detecting peer tampering), fusing the FIFO.
    pub fn add_available_metadata_space(&mut self, amount: u64) -> Result<u64, FifoError> {
        if self.is_fused() {
            return Err(FifoError::BadState);
        }
        let atomic = self.mapping.available_metadata_space_atomic();
        // `Ordering::Relaxed` suffices for both the initial load and the CAS loop: this atomically
        // replenishes a standalone credit counter concurrently with
        // `FifoWriter::reserve_metadata_space` without guarding auxiliary shared memory, and prior
        // consumed reservations already happened-before this call via `shared_state`.
        let mut current = atomic.load(Ordering::Relaxed);
        loop {
            // Tampering check: The volume peer can ONLY decrease metadata space by reserving it.
            // It can NEVER legally increase metadata space!
            if current > self.last_known_available_metadata_space {
                self.fuse();
                return Err(FifoError::InvalidState);
            }
            let new_val = current.saturating_add(amount);
            match atomic.compare_exchange_weak(
                current,
                new_val,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.last_known_available_metadata_space =
                        self.last_known_available_metadata_space.saturating_add(amount);
                    return Ok(new_val);
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Returns the number of unconsumed transactions waiting in the FIFO.
    pub fn pending_transactions_count(&self) -> usize {
        // `Ordering::Acquire` pairs with the `Ordering::AcqRel` `write_ptr` increment in
        // `FifoWriter::write_transaction` (and any intervening RMW release sequence on
        // `shared_state`).
        let current = SharedStateWord(self.mapping.shared_state_atomic().load(Ordering::Acquire));
        let pending = current.write_ptr().wrapping_sub(self.last_known_write_ptr);
        pending as usize
    }

    /// Waits asynchronously until new transactions arrive ([`FIFO_READABLE_SIGNAL`]) or the FIFO
    /// is closed ([`FIFO_CLOSED_SIGNAL`]).
    pub async fn wait_for_data(&self) -> Result<(), FifoError> {
        if self.is_fused() {
            return Err(FifoError::BadState);
        }
        loop {
            if self.pending_transactions_count() > 0 {
                return Ok(());
            }
            self.vmo().signal(FIFO_READABLE_SIGNAL, zx::Signals::empty())?;
            if self.pending_transactions_count() > 0 {
                // Re-assert `FIFO_READABLE_SIGNAL` in case another concurrent waiter cleared or
                // is about to await `OnSignals` on `&self`.
                self.vmo().signal(zx::Signals::empty(), FIFO_READABLE_SIGNAL)?;
                return Ok(());
            }
            let signals = fuchsia_async::OnSignals::new(
                self.vmo(),
                FIFO_READABLE_SIGNAL | FIFO_CLOSED_SIGNAL,
            )
            .await?;
            if signals.contains(FIFO_CLOSED_SIGNAL) && self.pending_transactions_count() == 0 {
                return Ok(());
            }
        }
    }
}

impl<T: DeserializeOwned + ReservationBacked> FifoReader<T> {
    fn read_item_from_slot(&mut self, slot_idx: usize) -> Result<T, FifoError> {
        let slot_slice = self.mapping.slot_slice(slot_idx);
        let raw_len = slot_slice.read::<u32>().ok_or(FifoError::InvalidConfiguration)?;
        let payload_len = u32::from_le(raw_len) as usize;
        if payload_len.checked_add(4).is_none_or(|total| total > self.mapping.slot_size_bytes()) {
            return Err(FifoError::InvalidConfiguration);
        }
        self.scratch_buf.clear();
        slot_slice.subslice(4..4 + payload_len).append_to(&mut self.scratch_buf);
        bincode_options()
            .with_limit(payload_len as u64)
            .deserialize(&self.scratch_buf)
            .map_err(|_| FifoError::InvalidConfiguration)
    }

    /// Validates `shared_state` on entry to a drain operation and returns
    /// `(target_write_ptr, pending)`.
    fn load_and_validate_pending(&self) -> Result<(u16, u16), FifoError> {
        // `Ordering::Acquire` ensures all of the peer's slot payload writes and credit
        // reservations in shared memory happen-before our subsequent slot reads and tamper check;
        // pairs with the `Ordering::AcqRel` `write_ptr` update in `FifoWriter::write_transaction`
        // (preserved across any intervening RMW release sequence on `shared_state`).
        let current = SharedStateWord(self.mapping.shared_state_atomic().load(Ordering::Acquire));
        if current.read_ptr() != self.last_known_write_ptr
            || current.layer_seq() != self.last_known_layer_seq
        {
            return Err(FifoError::InvalidState);
        }
        let target_write_ptr = current.write_ptr();
        let pending = target_write_ptr.wrapping_sub(self.last_known_write_ptr);
        if (pending as usize) > self.mapping.capacity_slots() {
            return Err(FifoError::InvalidState);
        }
        Ok((target_write_ptr, pending))
    }

    /// Drains all entries in `self.last_known_write_ptr .. target_write_ptr`, enforcing per-item
    /// metadata reservations and updating `self.last_known_available_metadata_space` and
    /// `self.last_known_write_ptr`.
    fn drain_pending_up_to<F, E>(
        &mut self,
        target_write_ptr: u16,
        apply_fn: &mut F,
    ) -> Result<usize, FifoError>
    where
        F: FnMut(u16, &T) -> Result<(), E>,
        FifoError: From<E>,
    {
        let start_ptr = self.last_known_write_ptr;
        let pending = target_write_ptr.wrapping_sub(start_ptr);
        let capacity_slots = self.mapping.capacity_slots();
        let mut reserved_consumed = 0u64;
        for i in 0..pending {
            let seq = start_ptr.wrapping_add(i);
            let slot_idx = (seq as usize) % capacity_slots;
            let item = self.read_item_from_slot(slot_idx)?;
            if item.needs_reservation() {
                if self.last_known_available_metadata_space.saturating_sub(reserved_consumed)
                    < TRANSACTION_METADATA_MAX_AMOUNT
                {
                    return Err(FifoError::InvalidState);
                }
                reserved_consumed += TRANSACTION_METADATA_MAX_AMOUNT;
            }
            apply_fn(seq, &item)?;
        }

        self.last_known_available_metadata_space =
            self.last_known_available_metadata_space.saturating_sub(reserved_consumed);
        if self.available_metadata_space() > self.last_known_available_metadata_space {
            return Err(FifoError::InvalidState);
        }

        self.last_known_write_ptr = target_write_ptr;
        Ok(pending as usize)
    }

    /// Atomically publishes `new_read_ptr` in `shared_state` and wakes blocked writers if the FIFO
    /// was previously full.
    fn advance_read_ptr(&self, old_read_ptr: u16, new_read_ptr: u16) -> Result<(), FifoError> {
        #[cfg(test)]
        if let Some(cb) = &self.callback_before_read_ptr_update {
            cb();
        }

        let consumed_pending = new_read_ptr.wrapping_sub(old_read_ptr);
        let expected_layer_seq = self.last_known_layer_seq;
        let capacity_slots = self.mapping.capacity_slots();

        // Atomically update `read_ptr` via `try_update` without clobbering any concurrent
        // `write_ptr` updates made by the peer during our draining loop.
        // `Ordering::Release` on success ensures our `read_item_from_slot` reads happen-before the
        // writer overwrites reclaimed slots on wrap-around; pairs with the `Ordering::Acquire`
        // loads in `FifoWriter::write_transaction` and `FifoWriter::has_room`. `Ordering::Relaxed`
        // suffices for the initial/retry load (`fetch_order`) since no slot memory is read from it.
        let prev_state_result = self.mapping.shared_state_atomic().try_update(
            Ordering::Release,
            Ordering::Relaxed,
            |val| {
                let state = SharedStateWord(val);
                let total_pending = state.write_ptr().wrapping_sub(old_read_ptr);
                if state.read_ptr() != old_read_ptr
                    || state.layer_seq() != expected_layer_seq
                    || (total_pending as usize) > capacity_slots
                    || total_pending < consumed_pending
                {
                    None
                } else {
                    Some(state.with_read_ptr(new_read_ptr).0)
                }
            },
        );
        let prev_state = SharedStateWord(prev_state_result.map_err(|_| FifoError::InvalidState)?);
        if prev_state.is_full(capacity_slots as u32) {
            self.vmo().signal(zx::Signals::empty(), FIFO_WRITABLE_SIGNAL)?;
        }
        Ok(())
    }

    /// Consumes all available transactions from the FIFO if at least one satisfies `predicate`,
    /// calling `apply_fn` for each consumed transaction.
    /// If no pending transactions satisfy `predicate`, no transactions are consumed and the read
    /// pointer is unchanged.
    ///
    /// Note that `predicate` is strictly an optimization hint to avoid draining the FIFO when no
    /// relevant transactions are pending. Pending slots are re-read from shared memory during
    /// consumption rather than buffered across passes, so a misbehaving peer could modify slot
    /// contents between `predicate` and `apply_fn`. Callers **must** still validate every single
    /// record passed to `apply_fn` and must **not** assume `predicate` performed any validation.
    ///
    /// If any error occurs during reading or applying transactions, the FIFO is fused closed for
    /// both further reads and writes.
    pub fn consume_transactions_if_any_match<P, F, E>(
        &mut self,
        predicate: P,
        apply_fn: F,
    ) -> Result<usize, FifoError>
    where
        P: FnMut(&T) -> bool,
        F: FnMut(u16, &T) -> Result<(), E>,
        FifoError: From<E>,
    {
        if self.is_fused() {
            return Err(FifoError::BadState);
        }
        self.consume_transactions_if_any_match_inner(predicate, apply_fn)
            .inspect_err(|_| self.fuse())
    }

    fn consume_transactions_if_any_match_inner<P, F, E>(
        &mut self,
        mut predicate: P,
        mut apply_fn: F,
    ) -> Result<usize, FifoError>
    where
        P: FnMut(&T) -> bool,
        F: FnMut(u16, &T) -> Result<(), E>,
        FifoError: From<E>,
    {
        let (target_write_ptr, pending) = self.load_and_validate_pending()?;
        if pending == 0 {
            return Ok(0);
        }

        // We intentionally do not buffer tested items across the predicate scan and the drain pass:
        // `predicate` is purely an optimization to skip unnecessary drains, and `apply_fn` must
        // independently validate every record it consumes regardless.
        let capacity_slots = self.mapping.capacity_slots();
        let mut match_found = false;
        for i in 0..pending {
            let seq = self.last_known_write_ptr.wrapping_add(i);
            let slot_idx = (seq as usize) % capacity_slots;
            let item = self.read_item_from_slot(slot_idx)?;
            if predicate(&item) {
                match_found = true;
                break;
            }
        }

        if !match_found {
            return Ok(0);
        }

        let old_read_ptr = self.last_known_write_ptr;
        let consumed = self.drain_pending_up_to(target_write_ptr, &mut apply_fn)?;
        self.advance_read_ptr(old_read_ptr, target_write_ptr)?;
        Ok(consumed)
    }

    /// Consumes all available transactions from the FIFO, calling `apply_fn` for each transaction.
    ///
    /// If any error occurs during reading or applying transactions, the FIFO is fused closed for
    /// both further reads and writes.
    pub fn consume_transactions<F, E>(&mut self, apply_fn: F) -> Result<usize, FifoError>
    where
        F: FnMut(u16, &T) -> Result<(), E>,
        FifoError: From<E>,
    {
        if self.is_fused() {
            return Err(FifoError::BadState);
        }
        self.consume_transactions_inner(apply_fn).inspect_err(|_| self.fuse())
    }

    fn consume_transactions_inner<F, E>(&mut self, mut apply_fn: F) -> Result<usize, FifoError>
    where
        F: FnMut(u16, &T) -> Result<(), E>,
        FifoError: From<E>,
    {
        let (target_write_ptr, pending) = self.load_and_validate_pending()?;
        if pending == 0 {
            return Ok(0);
        }

        let old_read_ptr = self.last_known_write_ptr;
        let consumed = self.drain_pending_up_to(target_write_ptr, &mut apply_fn)?;
        self.advance_read_ptr(old_read_ptr, target_write_ptr)?;
        Ok(consumed)
    }

    /// Atomically increments `layer_seq`, drains all transactions committed up to that
    /// linearization point via `apply_fn`, invokes `seal_fn(new_layer_seq)` to seal the current
    /// mutable layer, and advances `read_ptr` in shared memory.
    ///
    /// Any error returned from this method is a fatal connection-level error that fuses the FIFO
    /// closed immediately.
    pub fn seal_layer_and_consume<F, S, E>(
        &mut self,
        apply_fn: F,
        seal_fn: S,
    ) -> Result<u32, FifoError>
    where
        F: FnMut(u16, &T) -> Result<(), E>,
        S: FnOnce(u32) -> Result<(), E>,
        FifoError: From<E>,
    {
        if self.is_fused() {
            return Err(FifoError::BadState);
        }
        self.seal_layer_and_consume_inner(apply_fn, seal_fn).inspect_err(|_| self.fuse())
    }

    fn seal_layer_and_consume_inner<F, S, E>(
        &mut self,
        mut apply_fn: F,
        seal_fn: S,
    ) -> Result<u32, FifoError>
    where
        F: FnMut(u16, &T) -> Result<(), E>,
        S: FnOnce(u32) -> Result<(), E>,
        FifoError: From<E>,
    {
        // 1. Atomically increment `layer_seq` and capture `write_ptr` at the exact linearization
        // point. `Ordering::AcqRel` on success pairs with the `Ordering::AcqRel` `write_ptr` RMW
        // in `FifoWriter::write_transaction`: the `Acquire` half ensures the peer's slot payload
        // writes and credit reservations up to `captured_write_ptr` happen-before our slot reads
        // and tamper check, while the `Release` half publishes the incremented `layer_seq` to
        // subsequent writer commits. `Ordering::Relaxed` suffices for `fetch_order` since no slot
        // memory is read if the CAS retries or fails.
        let old_read_ptr = self.last_known_write_ptr;
        let expected_layer_seq = self.last_known_layer_seq;
        let capacity_slots = self.mapping.capacity_slots();
        let old_state_result = self.mapping.shared_state_atomic().try_update(
            Ordering::AcqRel,
            Ordering::Relaxed,
            |val| {
                let state = SharedStateWord(val);
                let pending = state.write_ptr().wrapping_sub(old_read_ptr);
                if state.read_ptr() != old_read_ptr
                    || state.layer_seq() != expected_layer_seq
                    || (pending as usize) > capacity_slots
                {
                    None
                } else {
                    Some(state.with_incremented_layer_seq().0)
                }
            },
        );
        let old_state = SharedStateWord(old_state_result.map_err(|_| FifoError::InvalidState)?);
        let captured_write_ptr = old_state.write_ptr();
        let new_layer_seq = expected_layer_seq.wrapping_add(1);
        self.last_known_layer_seq = new_layer_seq;

        #[cfg(test)]
        if let Some(cb) = &self.callback_after_layer_seq_increment {
            cb();
        }

        // 2. Drain all pending entries up to `captured_write_ptr` into Fxfs's current mutable
        // layer. Any mutations pushed by the peer AFTER step 1 observed
        // `layer_seq == new_layer_seq` on their atomic update and were committed into
        // `new_layer_seq` on the volume side; they must wait to be consumed into Fxfs's
        // `new_layer_seq` after `seal_fn` completes.
        self.drain_pending_up_to(captured_write_ptr, &mut apply_fn)?;

        // 3. Invoke the seal callback so Fxfs seals the current mutable layer and opens
        // `new_layer_seq` right now after draining captured mutations, while inside the
        // compaction critical section.
        seal_fn(new_layer_seq)?;

        // 4. Atomically update `read_ptr` so the volume knows these slots are free, preserving any
        // new `write_ptr` pushed during our drain + seal, and signal the volume if previously full.
        self.advance_read_ptr(old_read_ptr, captured_write_ptr)?;

        Ok(new_layer_seq)
    }

    /// Atomically increments `layer_seq` in the shared state word and returns the new `layer_seq`.
    #[cfg(test)]
    pub(crate) fn increment_layer_seq(&mut self) -> u32 {
        // `Ordering::Release` publishes the incremented `layer_seq` to the `Ordering::AcqRel`
        // `write_ptr` RMW in `FifoWriter::write_transaction` (and preserves active release
        // sequences on `shared_state`). `Ordering::Relaxed` suffices for `fetch_order` since this
        // method reads no slot memory.
        let old_state = SharedStateWord(self.mapping.shared_state_atomic().update(
            Ordering::Release,
            Ordering::Relaxed,
            |val| SharedStateWord(val).with_incremented_layer_seq().0,
        ));
        let new_layer_seq = old_state.layer_seq().wrapping_add(1);
        self.last_known_layer_seq = new_layer_seq;
        new_layer_seq
    }

    #[cfg(test)]
    pub(crate) fn set_initial_sequence_for_test(&mut self, write_ptr: u16, layer_seq: u32) {
        self.last_known_write_ptr = write_ptr;
        self.last_known_layer_seq = layer_seq;
    }
}
