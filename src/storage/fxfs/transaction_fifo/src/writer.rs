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
use fuchsia_sync::Mutex;
use serde::Serialize;
use std::fmt;
use std::marker::PhantomData;
#[cfg(test)]
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Explicit interface for the writer role (the volume / Starnix peer).
pub struct FifoWriter<T> {
    mapping: MappedFifoVmo,
    /// Locally tracked layer sequence number to detect when Fxfs seals a layer during compaction.
    last_known_layer_seq: u32,
    /// Reusable buffer for framing `[payload_len: u32][serialized bytes]` without per-commit heap
    /// allocations.
    scratch_buf: Vec<u8>,
    #[cfg(test)]
    pub(crate) callback_after_payload_write: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    pub(crate) callback_before_wait_for_room_sleep: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    pub(crate) callback_after_wait_for_room_signal: Option<Arc<dyn Fn() + Send + Sync>>,
    _marker: PhantomData<T>,
}

impl<T> fmt::Debug for FifoWriter<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FifoWriter")
            .field("mapping", &self.mapping)
            .field("last_known_layer_seq", &self.last_known_layer_seq)
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// Result of committing a transaction on the volume side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitResult {
    /// The sequence number assigned to the committed transaction (`write_ptr`).
    pub tx_seq: u16,
    /// If `Some(new_layer_seq)`, Fxfs completed compaction and incremented the layer file sequence
    /// number since our last commit. The volume must seal its local mutable layer and start a new
    /// layer.
    pub new_layer_seq: Option<u32>,
    /// The number of layers sealed since the last commit (normally 1, but can be > 1 if multiple
    /// compactions occurred).
    pub layers_sealed: u32,
}

impl<T> FifoWriter<T> {
    /// Creates a new writer interface over `vmo` with the negotiated `capacity_slots`.
    /// The slot size in bytes is automatically derived from the VMO size and capacity.
    pub fn new(vmo: zx::Vmo, capacity_slots: u32) -> Result<Self, FifoError> {
        let mapping = MappedFifoVmo::new(vmo, capacity_slots)?;
        Ok(Self {
            mapping,
            last_known_layer_seq: 0,
            scratch_buf: Vec::new(),
            #[cfg(test)]
            callback_after_payload_write: None,
            #[cfg(test)]
            callback_before_wait_for_room_sleep: None,
            #[cfg(test)]
            callback_after_wait_for_room_signal: None,
            _marker: PhantomData,
        })
    }

    /// Returns a reference to the underlying VMO.
    pub fn vmo(&self) -> &zx::Vmo {
        self.mapping.vmo()
    }

    /// Returns `true` if the FIFO has been marked closed in the shared header.
    pub fn is_closed(&self) -> bool {
        // `Ordering::Acquire` ensures all reader state updates prior to fusing happen-before we
        // handle the closed error; pairs with the `Ordering::Release` `fetch_or` in
        // `FifoReader::fuse`.
        (self.mapping.fifo_state_atomic().load(Ordering::Acquire) & FIFO_STATE_CLOSED) != 0
    }

    /// Returns the current available metadata space as observed in shared memory.
    pub fn available_metadata_space(&self) -> u64 {
        // `Ordering::Relaxed` suffices: `available_metadata_space` is a standalone credit balance
        // counter that does not guard any auxiliary shared memory.
        self.mapping.available_metadata_space_atomic().load(Ordering::Relaxed)
    }

    /// Atomically reserves `TRANSACTION_METADATA_MAX_AMOUNT` bytes of metadata space from shared
    /// memory. Returns `Err(FifoError::NoSpace)` if metadata space is insufficient. Fxfs
    /// guarantees that there is always enough space for a full FIFO worth of transactions, so this
    /// should not transiently fail in practice.
    fn reserve_metadata_space(&self) -> Result<(), FifoError> {
        let atomic = self.mapping.available_metadata_space_atomic();
        let amount = TRANSACTION_METADATA_MAX_AMOUNT;
        // `Ordering::Relaxed` suffices for both the initial load and the CAS loop: this atomically
        // deducts from a standalone credit counter concurrently with
        // `FifoReader::add_available_metadata_space`. Ordering before the reader's post-drain
        // tamper check is established by the subsequent `Ordering::AcqRel` `write_ptr` update on
        // `shared_state` in `write_transaction` pairing with the reader's `Ordering::Acquire`
        // `shared_state` load.
        let mut avail = atomic.load(Ordering::Relaxed);
        loop {
            if avail < amount {
                return Err(FifoError::NoSpace);
            }
            match atomic.compare_exchange_weak(
                avail,
                avail - amount,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => avail = actual,
            }
        }
    }

    /// Returns the last known layer sequence number tracked locally by the writer.
    pub fn last_known_layer_seq(&self) -> u32 {
        self.last_known_layer_seq
    }

    /// Advances the writer's locally tracked layer sequence number to `target_layer_seq` if newer.
    pub fn advance_layer_seq_to(&mut self, target_layer_seq: u32) -> u32 {
        if SharedStateWord::is_newer_layer(target_layer_seq, self.last_known_layer_seq) {
            self.last_known_layer_seq = target_layer_seq;
        }
        self.last_known_layer_seq
    }

    /// Checks if there is room for at least one transaction in the FIFO.
    pub fn has_room(&self) -> bool {
        // `Ordering::Acquire` pairs with the `Ordering::Release` `read_ptr` updates in
        // `FifoReader` (preserved across any RMW release sequence on `shared_state`).
        let current = SharedStateWord(self.mapping.shared_state_atomic().load(Ordering::Acquire));
        !current.is_full(self.mapping.capacity_slots() as u32)
    }

    /// Blocks until Fxfs consumes entries and signals the VMO as writable
    /// ([`FIFO_WRITABLE_SIGNAL`]), or until the FIFO is closed.
    /// The mutex lock is dropped before waiting on signals.
    pub fn wait_for_room(fifo: &Mutex<Self>) -> Result<(), FifoError> {
        loop {
            #[cfg(test)]
            let (cb_before_sleep, cb_after_signal);
            let vmo = {
                let guard = fifo.lock();
                // Load `shared_state` (`has_room`, `Ordering::Acquire`) before `fifo_state`
                // (`is_closed`, `Ordering::Acquire`). Because the reader never advances `read_ptr`
                // after setting `FIFO_STATE_CLOSED`, preventing Load-Load reordering ensures we
                // never observe `is_closed() == false` followed by room freed right before the
                // reader fused.
                let has_room = guard.has_room();
                if guard.is_closed() {
                    return Err(FifoError::BadState);
                }
                if has_room {
                    return Ok(());
                }
                // De-assert any stale `FIFO_WRITABLE_SIGNAL` (`signal(clear_mask, set_mask)`)
                // before sleeping, then re-check `has_room` and `is_closed` in case the reader
                // drained or fused right before we cleared the signal.
                guard.vmo().signal(FIFO_WRITABLE_SIGNAL, zx::Signals::empty())?;
                let has_room = guard.has_room();
                if guard.is_closed() {
                    return Err(FifoError::BadState);
                }
                if has_room {
                    // Re-assert `FIFO_WRITABLE_SIGNAL` in case another concurrent waiter already
                    // dropped the mutex and is about to enter `wait_one`.
                    guard.vmo().signal(zx::Signals::empty(), FIFO_WRITABLE_SIGNAL)?;
                    return Ok(());
                }
                #[cfg(test)]
                {
                    cb_before_sleep = guard.callback_before_wait_for_room_sleep.clone();
                    cb_after_signal = guard.callback_after_wait_for_room_signal.clone();
                }
                guard.mapping.vmo_arc()
            };

            #[cfg(test)]
            if let Some(cb) = cb_before_sleep {
                cb();
            }

            let signals = vmo
                .wait_one(FIFO_WRITABLE_SIGNAL | FIFO_CLOSED_SIGNAL, zx::MonotonicInstant::INFINITE)
                .to_result()?;

            #[cfg(test)]
            if let Some(cb) = cb_after_signal {
                cb();
            }

            if signals.contains(FIFO_CLOSED_SIGNAL) {
                return Err(FifoError::BadState);
            }
        }
    }
}

impl<T: Serialize + ReservationBacked> FifoWriter<T> {
    /// Writes `item` into the next available slot in the circular buffer using `MutPtrByteSlice`
    /// to prevent UB.
    ///
    /// If `item.needs_reservation()` is true, attempts to reserve
    /// `TRANSACTION_METADATA_MAX_AMOUNT` bytes from `available_metadata_space` after verifying
    /// slot availability. Returns `Err(FifoError::NoSpace)` if metadata space is exhausted.
    ///
    /// If `item.needs_reservation()` is false (for metadata-borrowing transactions like `unlink`),
    /// metadata reservation is bypassed.
    ///
    /// Returns `Err(FifoError::BadState)` if the FIFO has been closed, or `Err(FifoError::Full)`
    /// if the FIFO is full.
    pub fn write_transaction(&mut self, item: &T) -> Result<CommitResult, FifoError> {
        // `Ordering::Acquire` on `shared_state` pairs with the `Ordering::Release` `read_ptr`
        // updates in `FifoReader` (preserved across RMW release sequences) so the reader's slot
        // reads happen-before we overwrite a reclaimed slot on wrap-around. It also prevents the
        // subsequent `is_closed()` `Acquire` load from reordering ahead of `shared_state`, which
        // combined with the reader never advancing `read_ptr` after setting `FIFO_STATE_CLOSED`
        // ensures we never observe `is_closed() == false` followed by room freed right before a
        // fuse.
        let current = SharedStateWord(self.mapping.shared_state_atomic().load(Ordering::Acquire));
        if self.is_closed() {
            return Err(FifoError::BadState);
        }
        let capacity_slots = self.mapping.capacity_slots();
        if current.is_full(capacity_slots as u32) {
            return Err(FifoError::Full);
        }

        let slot_size_bytes = self.mapping.slot_size_bytes();
        let max_payload_len = (slot_size_bytes - 4) as u64;
        self.scratch_buf.clear();
        self.scratch_buf.extend_from_slice(&[0u8; 4]);
        bincode_options()
            .with_limit(max_payload_len)
            .serialize_into(&mut self.scratch_buf, item)
            .map_err(|_| FifoError::InvalidConfiguration)?;
        if self.scratch_buf.len() > slot_size_bytes {
            return Err(FifoError::InvalidConfiguration);
        }
        let payload_len = u32::try_from(self.scratch_buf.len() - 4)
            .map_err(|_| FifoError::InvalidConfiguration)?;
        self.scratch_buf[..4].copy_from_slice(&payload_len.to_le_bytes());

        // 1. Reserve metadata space directly inside write_transaction after acquiring slot
        // availability.
        if item.needs_reservation()
            && let Err(e) = self.reserve_metadata_space()
        {
            if self.is_closed() {
                return Err(FifoError::BadState);
            }
            return Err(e);
        }

        let slot_idx = (current.write_ptr() as usize) % capacity_slots;

        // 2. Write the length-prefixed payload into our reserved slot before publishing
        // `write_ptr`.
        let total_len = self.scratch_buf.len();
        let mut slot_slice = self.mapping.slot_mut_slice(slot_idx);
        slot_slice.subslice_mut(0..total_len).copy_from_slice(&self.scratch_buf);

        #[cfg(test)]
        if let Some(cb) = &self.callback_after_payload_write {
            cb();
        }

        // 3. Atomically increment `write_ptr` using `update`.
        // `Ordering::AcqRel` on success: the `Release` half pairs with the `Ordering::Acquire`
        // loads/RMWs in `FifoReader` so our slot payload writes and `reserve_metadata_space`
        // deduction happen-before the reader's slot reads and tamper check (and preserves active
        // `read_ptr` release sequences), while the `Acquire` half pairs with the `Ordering::AcqRel`
        // / `Ordering::Release` `layer_seq` updates in `FifoReader::seal_layer_and_consume` and
        // `FifoReader::increment_layer_seq` when reading `old_state.layer_seq()`.
        // `Ordering::Relaxed` suffices for `fetch_order` for the initial/retry load.
        let old_state = SharedStateWord(self.mapping.shared_state_atomic().update(
            Ordering::AcqRel,
            Ordering::Relaxed,
            |val| SharedStateWord(val).with_incremented_write_ptr().0,
        ));
        let observed_layer_seq = old_state.layer_seq();

        // 4. Check if layer_seq > last_known_layer_seq using modular difference
        let is_newer =
            SharedStateWord::is_newer_layer(observed_layer_seq, self.last_known_layer_seq);
        let (new_layer_seq, layers_sealed) = if is_newer {
            let delta = observed_layer_seq.wrapping_sub(self.last_known_layer_seq);
            self.last_known_layer_seq = observed_layer_seq;
            (Some(observed_layer_seq), delta)
        } else {
            (None, 0)
        };

        // 5. If FIFO was previously empty, signal VMO as readable (FIFO_READABLE_SIGNAL)
        if old_state.is_empty() {
            self.vmo().signal(zx::Signals::empty(), FIFO_READABLE_SIGNAL)?;
        }

        Ok(CommitResult { tx_seq: old_state.write_ptr(), new_layer_seq, layers_sealed })
    }

    #[cfg(test)]
    pub(crate) fn set_initial_sequence_for_test(
        &mut self,
        write_ptr: u16,
        read_ptr: u16,
        layer_seq: u32,
    ) {
        let word = SharedStateWord(0)
            .with_read_ptr(read_ptr)
            .with_write_ptr(write_ptr)
            .with_layer_seq(layer_seq);
        self.mapping.shared_state_atomic().store(word.0, Ordering::Release);
        self.last_known_layer_seq = layer_seq;
    }
}
