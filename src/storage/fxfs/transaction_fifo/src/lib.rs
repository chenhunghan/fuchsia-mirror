// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Safe, decoupled shared-VMO FIFO (`transaction_fifo`) library for passing metadata transactions
//! between an external volume (e.g. Starnix) and the Fxfs filesystem.
//!
//! # Overview
//!
//! The transaction FIFO provides a zero-IPC, high-throughput channel over a shared Zircon VMO
//! to stream filesystem mutations from a client volume (the writer) to the Fxfs host filesystem
//! (the reader).
//!
//! To prevent Undefined Behavior when accessing shared memory that may be concurrently read or
//! modified by an untrusted peer, this crate strictly uses `storage_ptr_slice::PtrByteSlice` and
//! `storage_ptr_slice::MutPtrByteSlice` rather than standard Rust slices (`&[u8]` / `&mut [u8]`).
//!
//! # VMO Layout and Format
//!
//! The VMO consists of a 64-byte control header followed by an array of fixed-size slots:
//!
//! ```text
//! +-----------------------------------------------------------------------------------+
//! | Offset 0..8   | shared_state (AtomicU64: read_ptr: u16, write_ptr: u16, layer_seq: u32) |
//! +-----------------------------------------------------------------------------------+
//! | Offset 8..16  | available_metadata_space (AtomicU64: reserved byte balance)       |
//! +-----------------------------------------------------------------------------------+
//! | Offset 16..24 | fifo_state (AtomicU64: FIFO_STATE_CLOSED flag)                    |
//! +-----------------------------------------------------------------------------------+
//! | Offset 24..64 | Reserved padding (cacheline alignment)                            |
//! +-----------------------------------------------------------------------------------+
//! | Offset 64..   | Slot 0: [payload_len: u32 (4B)][serialized transaction bytes...]  |
//! |               | Slot 1: [payload_len: u32 (4B)][serialized transaction bytes...]  |
//! |               | ...                                                               |
//! |               | Slot N-1                                                          |
//! +-----------------------------------------------------------------------------------+
//! ```
//!
//! - **`shared_state` (`AtomicU64`)**:
//!   - Bits 0..16: `read_ptr` (16-bit sequence number). Updated by [`FifoReader`] as items are
//!     consumed.
//!   - Bits 16..32: `write_ptr` (16-bit sequence number). Updated by [`FifoWriter`] as items are
//!     published.
//!   - Bits 32..64: `layer_seq` (32-bit counter). Incremented by [`FifoReader`] during compaction
//!     when sealing a mutable layer file.
//! - **`available_metadata_space` (`AtomicU64`)**:
//!   - Tracks available reserved metadata space. Replenished by Fxfs and decremented by the writer
//!     for reservation-backed transactions. The reader validates that the peer never illegally
//!     increments this value.
//! - **`fifo_state` (`AtomicU64`)**:
//!   - Tracks lifecycle flags such as [`FIFO_STATE_CLOSED`] so writers can observe a fused/closed
//!     FIFO directly in shared memory without polling kernel VMO signals.
//! - **Slots Region**:
//!   - Contains a power-of-two number of slots (`capacity_slots`, capped at [`MAX_CAPACITY_SLOTS`]
//!     = 32,768 to prevent 16-bit sequence number aliasing).
//!   - Each slot begins with a 4-byte little-endian payload length followed by the serialized
//!     data (e.g. bincode-encoded [`WireTransaction`]).
//!
//! # Sequence Numbers and Serialization
//!
//! - `read_ptr` and `write_ptr` are wrapping 16-bit sequence numbers. The number of pending
//!   transactions is computed modulo `0x10000` via `write_ptr.wrapping_sub(read_ptr)`.
//! - Slots are indexed via modular arithmetic: `(seq as usize) % capacity_slots`. Because
//!   `capacity_slots` is a power of two, sequence numbers naturally wrap without discontinuous
//!   slot index jumps.
//! - Packing `read_ptr`, `write_ptr`, and `layer_seq` into a single 64-bit atomic word allows
//!   both reader and writer to perform atomic compare-and-swap (`try_update` / `update`)
//!   operations with Acquire/Release memory ordering, guaranteeing consistent snapshots across
//!   concurrent reads and writes.
//!
//! # Writer Role ([`FifoWriter`])
//!
//! 1. Checks `fifo_state` (`FIFO_STATE_CLOSED`) and available capacity in `shared_state`: if
//!    closed, fails with `FifoError::BadState`; if `write_ptr.wrapping_sub(read_ptr) ==
//!    capacity_slots`, fails with `FifoError::Full`. Writers can block on room using
//!    `FifoWriter::wait_for_room()`, which waits on [`FIFO_WRITABLE_SIGNAL`] and
//!    [`FIFO_CLOSED_SIGNAL`].
//! 2. Reserves metadata space from `available_metadata_space` if the transaction requires a
//!    reservation ([`ReservationBacked::needs_reservation`]).
//! 3. Writes the serialized payload and 4-byte length prefix to `slot_idx`.
//! 4. Atomically increments `write_ptr` in `shared_state` with `AcqRel` ordering, publishing the
//!    transaction and snapshotting the current `layer_seq`.
//! 5. If the FIFO was previously empty, signals [`FIFO_READABLE_SIGNAL`] on the VMO to wake the
//!    reader.
//! 6. Returns [`CommitResult`]: if `layer_seq` advanced since the last commit, reports
//!    `new_layer_seq` and `layers_sealed` so the client volume can seal its local in-memory layer.
//!
//! # Reader Role ([`FifoReader`])
//!
//! 1. Waits on [`FIFO_READABLE_SIGNAL`] when the FIFO is empty.
//! 2. Drains pending transactions up to the captured `write_ptr`, deserializing payloads safely
//!    from shared memory and applying mutations via a caller-provided closure.
//! 3. Atomically advances `read_ptr` to the consumed sequence number with `Release` ordering.
//! 4. If the FIFO was previously full, signals [`FIFO_WRITABLE_SIGNAL`] to wake waiting writers.
//! 5. **Compaction and Layer Sealing ([`FifoReader::seal_layer_and_consume`])**:
//!    - Atomically increments `layer_seq` and snapshots `write_ptr` at that exact linearization
//!      point.
//!    - Drains all transactions up to that snapshot into the current mutable layer.
//!    - Calls `seal_fn(new_layer_seq)` to close the active layer and open the new layer file.
//!    - Advances `read_ptr`. Transactions committed after the linearization point observed
//!      the incremented `layer_seq` and will be drained into the new layer.

mod errors;
mod header;
mod reader;
mod writer;

use bitflags::bitflags;
pub use errors::FifoError;
use fxfs::object_store::transaction::{Mutation, Transaction};
pub use header::{
    FIFO_STATE_CLOSED, FIFO_STATE_OFFSET, HEADER_SIZE, MAX_CAPACITY_SLOTS, METADATA_SPACE_OFFSET,
    ReservationBacked, SharedStateWord, TRANSACTION_METADATA_MAX_AMOUNT,
};
pub use reader::FifoReader;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};
pub use writer::{CommitResult, FifoWriter};

/// Signal asserted when the FIFO has data available to read.
pub const FIFO_READABLE_SIGNAL: zx::Signals = zx::Signals::USER_0;
/// Signal asserted when the FIFO has room for writes.
pub const FIFO_WRITABLE_SIGNAL: zx::Signals = zx::Signals::USER_1;
/// Signal asserted when the FIFO is closed due to shutdown or an unrecoverable error.
pub const FIFO_CLOSED_SIGNAL: zx::Signals = zx::Signals::USER_4;

bitflags! {
    /// Bitmask of flags for [`WireTransaction`].
    #[derive(
        Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
    )]
    #[serde(transparent)]
    pub struct WireTransactionFlags: u32 {
        /// Indicates that this transaction borrows metadata space directly from Fxfs's
        /// global metadata reservation (for operations like unlink or truncate/shrink
        /// that do not increase net space usage after compaction).
        const BORROW_METADATA = 1 << 0;
    }
}

/// `WireTransaction` is the concrete, serializable transaction structure passed over `FifoWriter`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct WireTransaction {
    /// Bitmask of transaction flags (e.g. [`WireTransactionFlags::BORROW_METADATA`]).
    pub flags: WireTransactionFlags,
    /// Ordered mutations to apply in this transaction.
    pub mutations: Vec<Mutation>,
}

impl WireTransaction {
    /// Returns `true` if this transaction borrows metadata space from the global metadata
    /// reservation (corresponding to Fxfs `MetadataReservation::Borrowed`) rather than consuming
    /// a new per-transaction metadata reservation.
    // TODO(https://fxbug.dev/578104735): Think about preventing misuse of borrowing.
    pub fn borrow_metadata_space(&self) -> bool {
        self.flags.contains(WireTransactionFlags::BORROW_METADATA)
    }
}

impl ReservationBacked for WireTransaction {
    fn needs_reservation(&self) -> bool {
        !self.borrow_metadata_space()
    }
}

impl From<&Transaction<'_>> for WireTransaction {
    fn from(transaction: &Transaction<'_>) -> Self {
        let mut flags = WireTransactionFlags::empty();
        if transaction.metadata_reservation.is_borrowed() {
            flags |= WireTransactionFlags::BORROW_METADATA;
        }
        let mutations = transaction.mutations().iter().map(|m| m.mutation.clone()).collect();
        Self { flags, mutations }
    }
}

/// Returns the canonical bincode configuration shared by [`FifoWriter`] and [`FifoReader`]
/// (fixed-width integer encoding, rejecting trailing bytes).
#[inline]
pub(crate) fn bincode_options() -> impl bincode::Options {
    use bincode::Options;
    bincode::DefaultOptions::new().with_fixint_encoding()
}

/// Shared RAII wrapper around the VMAR-mapped FIFO VMO, header atomics, and slot byte slices.
pub(crate) struct MappedFifoVmo {
    vmo: Arc<zx::Vmo>,
    mapped_addr: usize,
    mapped_size: usize,
    capacity_slots: usize,
    slot_size_bytes: usize,
}

impl fmt::Debug for MappedFifoVmo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MappedFifoVmo")
            .field("vmo", &self.vmo)
            .field("mapped_addr", &self.mapped_addr)
            .field("mapped_size", &self.mapped_size)
            .field("capacity_slots", &self.capacity_slots)
            .field("slot_size_bytes", &self.slot_size_bytes)
            .finish()
    }
}

impl MappedFifoVmo {
    pub(crate) fn new(vmo: zx::Vmo, capacity_slots: u32) -> Result<Self, FifoError> {
        if !capacity_slots.is_power_of_two()
            || capacity_slots == 0
            || capacity_slots > MAX_CAPACITY_SLOTS
        {
            return Err(FifoError::InvalidConfiguration);
        }
        let capacity_slots = capacity_slots as usize;
        let vmo_size = vmo.get_size()? as usize;
        if vmo_size <= HEADER_SIZE {
            return Err(FifoError::InvalidConfiguration);
        }
        let slot_size_bytes = (vmo_size - HEADER_SIZE) / capacity_slots;
        if slot_size_bytes < 4 {
            return Err(FifoError::InvalidConfiguration);
        }
        let mapped_addr = fuchsia_runtime::vmar_root_self().map(
            0,
            &vmo,
            0,
            vmo_size,
            zx::VmarFlags::PERM_READ
                | zx::VmarFlags::PERM_WRITE
                | zx::VmarFlags::REQUIRE_NON_RESIZABLE,
        )?;
        Ok(Self {
            vmo: Arc::new(vmo),
            mapped_addr,
            mapped_size: vmo_size,
            capacity_slots,
            slot_size_bytes,
        })
    }

    #[inline]
    pub(crate) fn vmo(&self) -> &zx::Vmo {
        &self.vmo
    }

    #[inline]
    pub(crate) fn vmo_arc(&self) -> Arc<zx::Vmo> {
        Arc::clone(&self.vmo)
    }

    #[inline]
    pub(crate) fn capacity_slots(&self) -> usize {
        self.capacity_slots
    }

    #[inline]
    pub(crate) fn slot_size_bytes(&self) -> usize {
        self.slot_size_bytes
    }

    #[inline]
    pub(crate) fn shared_state_atomic(&self) -> &AtomicU64 {
        // SAFETY: `mapped_addr` is page-aligned and points to offset 0 of a valid RW VMAR mapping
        // of size `mapped_size > HEADER_SIZE` that outlives `&self`.
        unsafe { AtomicU64::from_ptr(self.mapped_addr as *mut u64) }
    }

    #[inline]
    pub(crate) fn available_metadata_space_atomic(&self) -> &AtomicU64 {
        // SAFETY: `mapped_addr + METADATA_SPACE_OFFSET` (offset 8) is 8-byte aligned and within
        // the `HEADER_SIZE` (64-byte) header of our valid RW VMAR mapping.
        unsafe { AtomicU64::from_ptr((self.mapped_addr + METADATA_SPACE_OFFSET) as *mut u64) }
    }

    #[inline]
    pub(crate) fn fifo_state_atomic(&self) -> &AtomicU64 {
        // SAFETY: `mapped_addr + FIFO_STATE_OFFSET` (offset 16) is 8-byte aligned and within
        // the `HEADER_SIZE` (64-byte) header of our valid RW VMAR mapping.
        unsafe { AtomicU64::from_ptr((self.mapped_addr + FIFO_STATE_OFFSET) as *mut u64) }
    }

    #[inline]
    pub(crate) fn slot_slice(&self, slot_idx: usize) -> PtrByteSlice<'_> {
        assert!(slot_idx < self.capacity_slots);
        let slot_offset = HEADER_SIZE + slot_idx * self.slot_size_bytes;
        let ptr = (self.mapped_addr + slot_offset) as *const u8;
        let raw_slice = std::ptr::slice_from_raw_parts(ptr, self.slot_size_bytes);
        // SAFETY: `raw_slice` covers `self.slot_size_bytes` bytes at `slot_offset`, which is
        // in-bounds of our mapped VMAR range for `slot_idx < self.capacity_slots` (enforced above).
        unsafe { PtrByteSlice::new(raw_slice) }
    }

    #[inline]
    pub(crate) fn slot_mut_slice(&mut self, slot_idx: usize) -> MutPtrByteSlice<'_> {
        assert!(slot_idx < self.capacity_slots);
        let slot_offset = HEADER_SIZE + slot_idx * self.slot_size_bytes;
        let ptr = (self.mapped_addr + slot_offset) as *mut u8;
        let raw_slice = std::ptr::slice_from_raw_parts_mut(ptr, self.slot_size_bytes);
        // SAFETY: `raw_slice` covers `self.slot_size_bytes` bytes at `slot_offset`, which is
        // in-bounds of our mapped VMAR range for `slot_idx < self.capacity_slots` (enforced above).
        unsafe { MutPtrByteSlice::new(raw_slice) }
    }
}

impl Drop for MappedFifoVmo {
    fn drop(&mut self) {
        if self.mapped_addr != 0 {
            // SAFETY: We do not retain any references or slices to this mapping since all accesses
            // go through lifetime-bounded `storage_ptr_slice` views and header `AtomicU64` refs.
            let _ = unsafe {
                fuchsia_runtime::vmar_root_self().unmap(self.mapped_addr, self.mapped_size)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_sync::Mutex;
    use futures::StreamExt;
    use fxfs::filesystem::FxFilesystem;
    use fxfs::object_handle::ObjectHandle;
    use fxfs::object_store::object_record::{ObjectAttributes, ObjectKey, ObjectKind, ObjectValue};
    use fxfs::object_store::transaction::{LockKey, Options, ReservationOptions, lock_keys};
    use fxfs::object_store::volume::root_volume;
    use fxfs::object_store::{HandleOptions, NewChildStoreOptions, ObjectStore};
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    #[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
    struct TestTransaction {
        tx_id: u64,
        payload: [u8; 24],
    }

    impl TestTransaction {
        fn reserving(tx_id: u64) -> Self {
            let mut payload = [0u8; 24];
            payload[0] = 0xFE;
            Self { tx_id, payload }
        }

        fn borrowing(tx_id: u64) -> Self {
            let mut payload = [0u8; 24];
            payload[0] = 0x00;
            Self { tx_id, payload }
        }
    }

    impl ReservationBacked for TestTransaction {
        fn needs_reservation(&self) -> bool {
            self.payload[0] == 0xFE
        }
    }

    const SLOT_SIZE: usize = 128;

    fn create_vmo_pair(capacity_slots: u32) -> (zx::Vmo, zx::Vmo) {
        let vmo_size = HEADER_SIZE + (capacity_slots as usize) * SLOT_SIZE;
        let vmo = zx::Vmo::create(vmo_size as u64).expect("VMO creation failed");
        let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Duplicate failed");
        (vmo, vmo_dup)
    }

    #[fuchsia::test]
    fn test_push_pop_basic() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).expect("FifoWriter new");
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).expect("FifoReader new");

        assert!(writer.has_room());
        assert_eq!(reader.pending_transactions_count(), 0);
        assert_eq!(writer.vmo().get_size().unwrap(), reader.vmo().get_size().unwrap());

        let tx = TestTransaction { tx_id: 123, payload: [0xAA; 24] };

        let commit_res = writer.write_transaction(&tx).expect("write_transaction");

        assert_eq!(commit_res.tx_seq, 0);
        assert_eq!(commit_res.new_layer_seq, None);
        assert_eq!(commit_res.layers_sealed, 0);

        assert_eq!(reader.pending_transactions_count(), 1);

        let mut consumed_ids = Vec::new();
        let consumed_count = reader
            .consume_transactions(|seq, read_tx: &TestTransaction| -> Result<(), FifoError> {
                assert_eq!(*read_tx, tx);
                consumed_ids.push(seq);
                Ok(())
            })
            .expect("consume_transactions");

        assert_eq!(consumed_count, 1);
        assert_eq!(consumed_ids, vec![0]);
        assert_eq!(reader.pending_transactions_count(), 0);
    }

    #[fuchsia::test]
    fn test_add_metadata_space_race_false_positive_tamper() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        let initial_space = TRANSACTION_METADATA_MAX_AMOUNT * 2;
        reader.set_available_metadata_space(initial_space);

        let tx = TestTransaction::reserving(1);
        writer.write_transaction(&tx).unwrap();

        reader.add_available_metadata_space(1000).unwrap();

        reader.consume_transactions(|_, _| Ok::<(), FifoError>(())).unwrap();

        // Subsequent replenishments must succeed without a false-positive tamper error.
        reader
            .add_available_metadata_space(1000)
            .expect("second add should succeed without tamper false positive");
    }

    #[fuchsia::test]
    fn test_consume_transactions_if_any_match() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).expect("FifoWriter new");
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).expect("FifoReader new");

        let tx1 = TestTransaction { tx_id: 10, payload: [1; 24] };
        let tx2 = TestTransaction { tx_id: 20, payload: [2; 24] };

        writer.write_transaction(&tx1).expect("write tx1");
        writer.write_transaction(&tx2).expect("write tx2");

        assert_eq!(reader.pending_transactions_count(), 2);

        // 1. Predicate does not match any transaction (tx_id == 30).
        let mut consumed_count = 0;
        let consumed = reader
            .consume_transactions_if_any_match(
                |tx| tx.tx_id == 30,
                |_seq, _tx| {
                    consumed_count += 1;
                    Ok::<(), FifoError>(())
                },
            )
            .expect("consume_transactions_if_any_match");

        assert_eq!(consumed, 0);
        assert_eq!(consumed_count, 0);
        assert_eq!(reader.pending_transactions_count(), 2); // Read ptr unchanged!

        // 2. Predicate matches tx2 (tx_id == 20). Both tx1 and tx2 must be consumed in order.
        let mut consumed_ids = Vec::new();
        let consumed = reader
            .consume_transactions_if_any_match(
                |tx| tx.tx_id == 20,
                |_seq, tx| {
                    consumed_ids.push(tx.tx_id);
                    Ok::<(), FifoError>(())
                },
            )
            .expect("consume_transactions_if_any_match");

        assert_eq!(consumed, 2);
        assert_eq!(consumed_ids, vec![10, 20]);
        assert_eq!(reader.pending_transactions_count(), 0);
    }

    #[fuchsia::test]
    fn test_fifo_full_and_empty() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(2);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 2).expect("FifoWriter new");
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 2).expect("FifoReader new");

        let tx1 = TestTransaction { tx_id: 1, payload: [1; 24] };
        let tx2 = TestTransaction { tx_id: 2, payload: [2; 24] };
        let tx3 = TestTransaction { tx_id: 3, payload: [3; 24] };

        writer.write_transaction(&tx1).unwrap();
        writer.write_transaction(&tx2).unwrap();

        assert!(!writer.has_room());
        let err = writer.write_transaction(&tx3);
        assert_eq!(err, Err(FifoError::Full));

        // Reader consumes all pending transactions (both slots).
        reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }).unwrap();
        assert!(writer.has_room());

        // Now writer can write tx3
        writer.write_transaction(&tx3).unwrap();
    }

    #[fuchsia::test]
    fn test_compaction_seal_handshake() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).expect("FifoWriter new");
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).expect("FifoReader new");

        let tx1 = TestTransaction { tx_id: 10, payload: [10; 24] };
        writer.write_transaction(&tx1).unwrap();

        let mut drained_ids = Vec::new();
        let mut sealed_layers = Vec::new();

        let new_layer_seq = reader
            .seal_layer_and_consume(
                |seq, tx: &TestTransaction| -> Result<(), FifoError> {
                    drained_ids.push((seq, tx.tx_id));
                    Ok(())
                },
                |layer_seq| -> Result<(), FifoError> {
                    sealed_layers.push(layer_seq);
                    Ok(())
                },
            )
            .expect("seal_layer_and_consume");

        assert_eq!(new_layer_seq, 1);
        assert_eq!(drained_ids, vec![(0, 10)]);
        assert_eq!(sealed_layers, vec![1]);

        // When writer writes next transaction, it should observe the new layer_seq = 1 and
        // layers_sealed = 1
        let tx2 = TestTransaction { tx_id: 20, payload: [20; 24] };
        let commit_res = writer.write_transaction(&tx2).unwrap();

        assert_eq!(commit_res.new_layer_seq, Some(1));
        assert_eq!(commit_res.layers_sealed, 1);
    }

    #[fuchsia::test]
    fn test_increment_layer_seq() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        assert_eq!(reader.increment_layer_seq(), 1);
        assert_eq!(reader.increment_layer_seq(), 2);

        let tx = TestTransaction { tx_id: 1, payload: [1; 24] };
        let res = writer.write_transaction(&tx).unwrap();
        assert_eq!(res.new_layer_seq, Some(2));
        assert_eq!(res.layers_sealed, 2);
    }

    #[fuchsia::test]
    fn test_multi_threaded_chaos_interleaving() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(16);
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 16).unwrap()));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 16).unwrap();

        let total_items = 200u64;
        let writer_clone = Arc::clone(&writer);

        let writer_handle = std::thread::spawn(move || {
            for i in 0..total_items {
                let tx = TestTransaction { tx_id: i, payload: [0; 24] };
                loop {
                    let mut guard = writer_clone.lock();
                    if guard.has_room() {
                        guard.write_transaction(&tx).unwrap();
                        break;
                    }
                    drop(guard);
                    std::thread::yield_now();
                }
            }
        });

        let mut received_items = Vec::new();
        let mut total_consumed = 0u64;
        let mut cycle = 0;

        while total_consumed < total_items {
            cycle += 1;
            if cycle % 5 == 0 {
                // Perform a compaction seal every 5 iterations
                let _ = reader
                    .seal_layer_and_consume(
                        |_, tx: &TestTransaction| -> Result<(), FifoError> {
                            received_items.push(tx.tx_id);
                            total_consumed += 1;
                            Ok(())
                        },
                        |_| -> Result<(), FifoError> { Ok(()) },
                    )
                    .unwrap();
            } else {
                let _count = reader
                    .consume_transactions(|_, tx: &TestTransaction| -> Result<(), FifoError> {
                        received_items.push(tx.tx_id);
                        total_consumed += 1;
                        Ok(())
                    })
                    .unwrap();
            }
            std::thread::yield_now();
        }

        writer_handle.join().unwrap();
        assert_eq!(received_items.len(), total_items as usize);
        for (idx, id) in received_items.into_iter().enumerate() {
            assert_eq!(id, idx as u64);
        }
    }

    #[fuchsia::test]
    fn test_async_wait_for_data_and_room() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(2);
        let writer = Arc::new(Mutex::new(
            FifoWriter::<TestTransaction>::new(vmo_writer, 2).expect("FifoWriter new"),
        ));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 2).expect("FifoReader new");

        let (tx_step, rx_step) = mpsc::channel();
        let (tx_writer_sleeping, mut rx_writer_sleeping) = futures::channel::mpsc::unbounded();
        writer.lock().callback_before_wait_for_room_sleep = Some(Arc::new(move || {
            let _ = tx_writer_sleeping.unbounded_send(());
        }));

        let reader_thread = std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutor::default();
            executor.run_singlethreaded(async move {
                // Step 1: Poll wait_for_data once while empty so OnSignals is registered, then
                // notify writer and await wakeup.
                {
                    let mut wait_fut = std::pin::pin!(reader.wait_for_data());
                    assert!(futures::poll!(wait_fut.as_mut()).is_pending());
                    tx_step.send("reader_waiting").unwrap();
                    wait_fut.await.expect("wait_for_data");
                }

                let count = reader
                    .consume_transactions(|seq, tx: &TestTransaction| -> Result<(), FifoError> {
                        assert_eq!(tx.tx_id, 100);
                        assert_eq!(seq, 0);
                        Ok(())
                    })
                    .expect("consume_transactions");
                assert_eq!(count, 1);

                // Step 2: Tell writer we are ready for the fill, wait until writer blocks in
                // wait_for_room (confirming both slots are populated), then drain both slots.
                tx_step.send("reader_ready_for_fill").unwrap();
                rx_writer_sleeping.next().await.unwrap();
                reader.wait_for_data().await.expect("wait_for_data");

                let count = reader
                    .consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) })
                    .expect("consume");
                assert_eq!(count, 2);

                // Step 3: Wait for and consume tx4 written after wait_for_room unblocked.
                reader.wait_for_data().await.expect("wait_for_data tx4");
                let mut tx4_ids = Vec::new();
                let count = reader
                    .consume_transactions(|_, tx: &TestTransaction| -> Result<(), FifoError> {
                        tx4_ids.push(tx.tx_id);
                        Ok(())
                    })
                    .expect("consume tx4");
                assert_eq!(count, 1);
                assert_eq!(tx4_ids, vec![103]);
            })
        });

        let writer_thread = std::thread::spawn(move || {
            assert_eq!(rx_step.recv().unwrap(), "reader_waiting");

            let tx1 = TestTransaction { tx_id: 100, payload: [1; 24] };
            writer.lock().write_transaction(&tx1).expect("write tx1");

            assert_eq!(rx_step.recv().unwrap(), "reader_ready_for_fill");

            let tx2 = TestTransaction { tx_id: 101, payload: [2; 24] };
            let tx3 = TestTransaction { tx_id: 102, payload: [3; 24] };
            writer.lock().write_transaction(&tx2).expect("write tx2");
            writer.lock().write_transaction(&tx3).expect("write tx3");
            FifoWriter::wait_for_room(&writer).expect("wait_for_room");
            assert!(writer.lock().has_room());

            let tx4 = TestTransaction { tx_id: 103, payload: [4; 24] };
            writer.lock().write_transaction(&tx4).expect("write tx4");
        });

        reader_thread.join().expect("reader thread join");
        writer_thread.join().expect("writer thread join");
    }

    #[fuchsia::test]
    fn test_async_wait_for_room_on_seal() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(2);
        let writer = Arc::new(Mutex::new(
            FifoWriter::<TestTransaction>::new(vmo_writer, 2).expect("FifoWriter new"),
        ));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 2).expect("FifoReader new");

        let (tx_sleeping, rx_sleeping) = mpsc::channel();
        writer.lock().callback_before_wait_for_room_sleep = Some(Arc::new(move || {
            let _ = tx_sleeping.send(());
        }));

        let reader_thread = std::thread::spawn(move || {
            rx_sleeping.recv().unwrap();

            let new_layer = reader
                .seal_layer_and_consume(
                    |_, _| -> Result<(), FifoError> { Ok(()) },
                    |_| -> Result<(), FifoError> { Ok(()) },
                )
                .expect("seal");
            assert_eq!(new_layer, 1);
        });

        let writer_thread = std::thread::spawn(move || {
            let tx1 = TestTransaction { tx_id: 1, payload: [1; 24] };
            let tx2 = TestTransaction { tx_id: 2, payload: [2; 24] };
            writer.lock().write_transaction(&tx1).unwrap();
            writer.lock().write_transaction(&tx2).unwrap();
            assert!(!writer.lock().has_room());

            FifoWriter::wait_for_room(&writer).expect("wait_for_room");
            assert!(writer.lock().has_room());
        });

        reader_thread.join().expect("reader thread join");
        writer_thread.join().expect("writer thread join");
    }

    #[fuchsia::test]
    fn test_interleaved_write_during_compaction_drain() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap()));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        let tx1 = TestTransaction { tx_id: 10, payload: [10; 24] };
        writer.lock().write_transaction(&tx1).unwrap();

        let writer_clone = Arc::clone(&writer);
        reader.callback_after_layer_seq_increment = Some(Arc::new(move || {
            let tx2 = TestTransaction { tx_id: 20, payload: [20; 24] };
            let commit_res = writer_clone.lock().write_transaction(&tx2).unwrap();
            assert_eq!(commit_res.tx_seq, 1);
            assert_eq!(commit_res.new_layer_seq, Some(1));
        }));

        let mut drained_ids = Vec::new();
        let new_layer_seq = reader
            .seal_layer_and_consume(
                |seq, tx: &TestTransaction| -> Result<(), FifoError> {
                    drained_ids.push((seq, tx.tx_id));
                    Ok(())
                },
                |_| -> Result<(), FifoError> { Ok(()) },
            )
            .expect("seal_layer_and_consume");

        assert_eq!(new_layer_seq, 1);
        assert_eq!(drained_ids, vec![(0, 10)]);
        assert_eq!(reader.pending_transactions_count(), 1);

        let mut next_drained_ids = Vec::new();
        reader
            .consume_transactions(|seq, tx: &TestTransaction| -> Result<(), FifoError> {
                next_drained_ids.push((seq, tx.tx_id));
                Ok(())
            })
            .expect("consume_transactions");
        assert_eq!(next_drained_ids, vec![(1, 20)]);
        assert_eq!(reader.pending_transactions_count(), 0);
    }

    #[fuchsia::test]
    fn test_interleaved_layer_seq_increment_before_write_ptr_increment() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let reader =
            Arc::new(Mutex::new(FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap()));

        let reader_clone = Arc::clone(&reader);
        writer.callback_after_payload_write = Some(Arc::new(move || {
            let new_layer_seq = reader_clone
                .lock()
                .seal_layer_and_consume(
                    |_, _| -> Result<(), FifoError> {
                        panic!("Should not drain any items when write_ptr is 0");
                    },
                    |_| -> Result<(), FifoError> { Ok(()) },
                )
                .unwrap();
            assert_eq!(new_layer_seq, 1);
        }));

        let tx1 = TestTransaction { tx_id: 99, payload: [99; 24] };
        let commit_res = writer.write_transaction(&tx1).unwrap();

        assert_eq!(commit_res.tx_seq, 0);
        assert_eq!(commit_res.new_layer_seq, Some(1));
        assert_eq!(reader.lock().pending_transactions_count(), 1);
    }

    #[fuchsia::test]
    fn test_interleaved_read_ptr_update_during_write() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap()));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        let tx1 = TestTransaction { tx_id: 1, payload: [1; 24] };
        writer.lock().write_transaction(&tx1).unwrap();

        let writer_clone = Arc::clone(&writer);
        reader.callback_before_read_ptr_update = Some(Arc::new(move || {
            let tx2 = TestTransaction { tx_id: 2, payload: [2; 24] };
            let commit_res = writer_clone.lock().write_transaction(&tx2).unwrap();
            assert_eq!(commit_res.tx_seq, 1);
        }));

        let count = reader
            .consume_transactions(|seq, _| -> Result<(), FifoError> {
                assert_eq!(seq, 0);
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 1);

        assert_eq!(reader.pending_transactions_count(), 1);
    }

    #[fuchsia::test]
    fn test_metadata_space_accounting_and_tamper_detection() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        let initial_space = TRANSACTION_METADATA_MAX_AMOUNT * 3;
        reader.set_available_metadata_space(initial_space);
        assert_eq!(reader.available_metadata_space(), initial_space);

        // Writer reserves one transaction -> space drops by TRANSACTION_METADATA_MAX_AMOUNT
        let tx1 = TestTransaction::reserving(1);
        writer.write_transaction(&tx1).unwrap();
        assert_eq!(reader.available_metadata_space(), TRANSACTION_METADATA_MAX_AMOUNT * 2);

        // Fxfs replenishes 1,000 bytes -> space increases
        let new_val = reader.add_available_metadata_space(1_000).expect("Replenish success");

        assert_eq!(new_val, (TRANSACTION_METADATA_MAX_AMOUNT * 2) + 1_000);
        assert_eq!(
            writer.available_metadata_space(),
            (TRANSACTION_METADATA_MAX_AMOUNT * 2) + 1_000
        );

        // Evil volume writes 999,999 to offset 8 in VMO
        let bad_val = 999_999_u64;
        vmo_bad_peer.write(&bad_val.to_ne_bytes(), METADATA_SPACE_OFFSET as u64).unwrap();
        assert_eq!(reader.available_metadata_space(), 999_999);

        // Fxfs attempts to add space and detects current (999,999) > last_known -> InvalidState!
        let result = reader.add_available_metadata_space(1_000);
        assert_eq!(result, Err(FifoError::InvalidState));
    }

    #[fuchsia::test]
    fn test_metadata_borrowing_bypasses_reservation() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(2);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 2).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 2).unwrap();

        // 0 metadata space available
        reader.set_available_metadata_space(0);
        assert_eq!(reader.available_metadata_space(), 0);

        // Reserving transaction must fail with NoSpace
        let tx_reserving = TestTransaction::reserving(1);
        assert_eq!(writer.write_transaction(&tx_reserving), Err(FifoError::NoSpace));

        // Borrowing transaction must succeed even with 0 available space
        let tx_borrowing = TestTransaction::borrowing(2);
        assert!(writer.write_transaction(&tx_borrowing).is_ok());
        assert_eq!(reader.available_metadata_space(), 0);
    }

    #[fuchsia::test]
    fn test_wait_for_room_interleaved_refill_race() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(1);
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 1).unwrap()));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 1).unwrap();

        // Fill FIFO (1 slot capacity)
        let tx1 = TestTransaction { tx_id: 1, payload: [1; 24] };
        writer.lock().write_transaction(&tx1).unwrap();
        assert!(!writer.lock().has_room());

        let writer_weak = Arc::downgrade(&writer);
        let (tx_interleaved, rx_interleaved) = mpsc::channel();
        let stealer_ran = Arc::new(AtomicBool::new(false));
        let (tx_sleeping, rx_sleeping) = mpsc::channel();

        {
            let mut guard = writer.lock();
            guard.callback_after_wait_for_room_signal = Some(Arc::new(move || {
                if !stealer_ran.swap(true, Ordering::SeqCst) {
                    // Task B intercepts after Task A wakes up from wait_one, before Task A
                    // re-acquires lock!
                    let tx_stealer = TestTransaction { tx_id: 99, payload: [99; 24] };
                    let writer_arc = writer_weak.upgrade().unwrap();
                    let commit_res = writer_arc.lock().write_transaction(&tx_stealer);
                    tx_interleaved.send(commit_res).unwrap();
                }
            }));
            guard.callback_before_wait_for_room_sleep = Some(Arc::new(move || {
                let _ = tx_sleeping.send(());
            }));
        }

        let writer_task = writer.clone();
        let (tx_step, rx_step) = mpsc::channel();

        let thread_handle = std::thread::spawn(move || {
            tx_step.send("waiting_started").unwrap();
            FifoWriter::wait_for_room(&writer_task).unwrap();
            tx_step.send("wait_completed").unwrap();
        });

        assert_eq!(rx_step.recv().unwrap(), "waiting_started");
        let _ = rx_sleeping.recv().unwrap();

        // Reader consumes 1 transaction (frees 1 slot and signals USER_1)
        let _ = reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }).unwrap();

        // Task B (stealer) inserted its transaction inside the hook!
        let stealer_res = rx_interleaved.recv().unwrap();
        assert!(stealer_res.is_ok());

        // Wait for Task A to re-enter sleep state in wait_for_room
        let _ = rx_sleeping.recv().unwrap();

        // Now reader consumes another transaction so Task A can finally wake up and succeed
        let _ = reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }).unwrap();

        assert_eq!(rx_step.recv().unwrap(), "wait_completed");
        thread_handle.join().unwrap();
        assert_eq!(Arc::strong_count(&writer), 1);
    }

    #[fuchsia::test]
    fn test_wait_for_room_pre_freed_slot() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(2);
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 2).unwrap()));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 2).unwrap();

        // Fill FIFO
        let tx1 = TestTransaction { tx_id: 1, payload: [1; 24] };
        let tx2 = TestTransaction { tx_id: 2, payload: [2; 24] };
        writer.lock().write_transaction(&tx1).unwrap();
        writer.lock().write_transaction(&tx2).unwrap();

        // Reader frees 1 slot before wait_for_room is invoked
        reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }).unwrap();

        // Calling wait_for_room with an existing slot available should return immediately
        FifoWriter::wait_for_room(&writer).unwrap();
        assert!(writer.lock().has_room());
    }

    #[fuchsia::test(threads = 4)]
    async fn test_multi_threaded_writers_single_slot_frees_one_writer() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(1);
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 1).unwrap()));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 1).unwrap();

        // Fill FIFO (1 slot capacity)
        let tx1 = TestTransaction { tx_id: 1, payload: [1; 24] };
        writer.lock().write_transaction(&tx1).unwrap();
        assert!(!writer.lock().has_room());

        let (tx_done, mut rx_done) = futures::channel::mpsc::unbounded();
        let num_writers = 5usize;

        let mut tasks = Vec::new();
        for i in 0..num_writers {
            let writer_clone = Arc::clone(&writer);
            let tx_done_clone = tx_done.clone();
            tasks.push(fuchsia_async::Task::spawn(async move {
                let tx = TestTransaction { tx_id: 10 + i as u64, payload: [10 + i as u8; 24] };
                loop {
                    let commit_res = writer_clone.lock().write_transaction(&tx);
                    match commit_res {
                        Ok(res) => {
                            let _ = tx_done_clone.unbounded_send((i, res.tx_seq));
                            break;
                        }
                        Err(FifoError::Full) => {
                            let w = Arc::clone(&writer_clone);
                            fuchsia_async::unblock(move || FifoWriter::wait_for_room(&w))
                                .await
                                .unwrap();
                        }
                        Err(e) => panic!("Unexpected error: {:?}", e),
                    }
                }
            }));
        }

        let mut completed = Vec::new();

        // Gradually consume remaining slots until all 5 tasks finish
        for _ in 0..num_writers {
            fuchsia_async::Timer::new(fuchsia_async::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(10),
            ))
            .await;
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }).unwrap();
            let res = rx_done.next().await.unwrap();
            completed.push(res);
        }

        for task in tasks {
            task.await;
        }

        assert_eq!(completed.len(), num_writers);
    }

    #[fuchsia::test]
    fn test_ringbuffer_wraparound() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        // Initialize near sequence wrap-around point: write_ptr = 0xFFFE, read_ptr = 0xFFFE
        writer.set_initial_sequence_for_test(0xFFFE, 0xFFFE, 0);
        reader.set_initial_sequence_for_test(0xFFFE, 0);

        assert_eq!(reader.pending_transactions_count(), 0);

        // Write 4 transactions to wrap 0xFFFE -> 0xFFFF -> 0x0000 -> 0x0001
        let tx1 = TestTransaction { tx_id: 1, payload: [1; 24] };
        let tx2 = TestTransaction { tx_id: 2, payload: [2; 24] };
        let tx3 = TestTransaction { tx_id: 3, payload: [3; 24] };
        let tx4 = TestTransaction { tx_id: 4, payload: [4; 24] };

        assert_eq!(writer.write_transaction(&tx1).unwrap().tx_seq, 0xFFFE);
        assert_eq!(writer.write_transaction(&tx2).unwrap().tx_seq, 0xFFFF);
        assert_eq!(writer.write_transaction(&tx3).unwrap().tx_seq, 0x0000);
        assert_eq!(writer.write_transaction(&tx4).unwrap().tx_seq, 0x0001);

        assert_eq!(reader.pending_transactions_count(), 4);
        assert!(!writer.has_room());

        let mut consumed_seqs = Vec::new();
        let count = reader
            .consume_transactions(|seq, tx: &TestTransaction| -> Result<(), FifoError> {
                consumed_seqs.push((seq, tx.tx_id));
                Ok(())
            })
            .unwrap();

        assert_eq!(count, 4);
        assert_eq!(consumed_seqs, vec![(0xFFFE, 1), (0xFFFF, 2), (0x0000, 3), (0x0001, 4)]);
        assert_eq!(reader.pending_transactions_count(), 0);
        assert!(writer.has_room());
    }

    #[fuchsia::test]
    fn test_layer_seq_rollover() {
        assert!(SharedStateWord::is_newer_layer(0x0000_0001, 0x0000_0000));
        assert!(SharedStateWord::is_newer_layer(0x0000_0000, 0xFFFF_FFFF));
        assert!(!SharedStateWord::is_newer_layer(0xFFFF_FFFF, 0x0000_0000));
        assert!(!SharedStateWord::is_newer_layer(5, 5));
    }

    #[fuchsia::test]
    fn test_wire_transaction() {
        let wire_tx =
            WireTransaction { flags: WireTransactionFlags::BORROW_METADATA, mutations: vec![] };
        assert_eq!(wire_tx.flags, WireTransactionFlags::BORROW_METADATA);
        assert!(!wire_tx.needs_reservation());

        let wire_tx2 = WireTransaction { flags: WireTransactionFlags::empty(), mutations: vec![] };
        assert_eq!(wire_tx2.flags, WireTransactionFlags::empty());
        assert!(wire_tx2.needs_reservation());

        let bytes = bincode::serialize(&wire_tx).unwrap();
        let deserialized: WireTransaction = bincode::deserialize(&bytes).unwrap();
        assert_eq!(wire_tx, deserialized);
    }

    #[fuchsia::test]
    fn test_invalid_configurations() {
        let vmo = zx::Vmo::create(4096).unwrap();
        // Non-power of two capacity
        assert_eq!(
            FifoWriter::<TestTransaction>::new(
                vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                3
            )
            .err(),
            Some(FifoError::InvalidConfiguration)
        );
        // Zero capacity
        assert_eq!(
            FifoWriter::<TestTransaction>::new(
                vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                0
            )
            .err(),
            Some(FifoError::InvalidConfiguration)
        );
        // Excess capacity
        assert_eq!(
            FifoWriter::<TestTransaction>::new(
                vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                1 << 16
            )
            .err(),
            Some(FifoError::InvalidConfiguration)
        );
        // Slot size too small (capacity = 1024 gives (4096 - 64) / 1024 = 3 bytes < 4)
        assert_eq!(
            FifoWriter::<TestTransaction>::new(
                vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                1024
            )
            .err(),
            Some(FifoError::InvalidConfiguration)
        );
        // Dirty header word rejected by FifoReader::new
        let dirty_vmo = zx::Vmo::create(4096).unwrap();
        dirty_vmo.write(&1u64.to_ne_bytes(), 0).unwrap();
        assert_eq!(
            FifoReader::<TestTransaction>::new(dirty_vmo, 4).err(),
            Some(FifoError::InvalidState)
        );
    }

    #[fuchsia::test]
    fn test_advance_layer_seq_to() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        assert_eq!(writer.last_known_layer_seq(), 0);
        assert_eq!(writer.advance_layer_seq_to(0), 0);
        assert_eq!(writer.advance_layer_seq_to(3), 3);
        assert_eq!(writer.last_known_layer_seq(), 3);
        // Older or equal layer sequence numbers are ignored (including modular wrap-around).
        assert_eq!(writer.advance_layer_seq_to(2), 3);
        assert_eq!(writer.advance_layer_seq_to(0xFFFF_FFFF), 3);

        // If the reader seals up to layer 3, a writer already advanced to 3 does not re-report it.
        for _ in 0..3 {
            reader.increment_layer_seq();
        }
        let commit1 = writer.write_transaction(&TestTransaction::borrowing(1)).unwrap();
        assert_eq!(commit1.new_layer_seq, None);
        assert_eq!(commit1.layers_sealed, 0);

        // Wrap-around across u32::MAX via advance_layer_seq_to.
        writer.set_initial_sequence_for_test(1, 0, 0xFFFF_FFFE);
        assert_eq!(writer.advance_layer_seq_to(0x0000_0001), 0x0000_0001);
        assert_eq!(writer.advance_layer_seq_to(0xFFFF_FFFF), 0x0000_0001);
    }

    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    struct DynamicTestTransaction {
        data: Vec<u8>,
    }

    impl ReservationBacked for DynamicTestTransaction {
        fn needs_reservation(&self) -> bool {
            false
        }
    }

    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    struct ZeroSizedTransaction;

    impl ReservationBacked for ZeroSizedTransaction {
        fn needs_reservation(&self) -> bool {
            false
        }
    }

    #[fuchsia::test]
    fn test_extreme_ringbuffer_wraparound_multiple_capacities() {
        for capacity in [1, 2, 4, 8, 16, 64, 512, 4096, 32768] {
            let vmo_size = HEADER_SIZE + (capacity as usize) * SLOT_SIZE;
            let vmo_w = zx::Vmo::create(vmo_size as u64).unwrap();
            let vmo_r = vmo_w.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
            let mut writer = FifoWriter::<TestTransaction>::new(vmo_w, capacity).unwrap();
            let mut reader = FifoReader::<TestTransaction>::new(vmo_r, capacity).unwrap();

            // Set initial sequence numbers near 16-bit wrap point (0xFFF0)
            writer.set_initial_sequence_for_test(0xFFF0, 0xFFF0, 0);
            reader.set_initial_sequence_for_test(0xFFF0, 0);

            // Execute 150 push/pop cycles to wrap past 0xFFFF into 0x0000..
            let mut current_seq: u16 = 0xFFF0;
            for i in 0..150u64 {
                let tx = TestTransaction::borrowing(i);
                let commit_res = writer.write_transaction(&tx).unwrap();
                assert_eq!(commit_res.tx_seq, current_seq);

                let mut consumed_seq = None;
                let count = reader
                    .consume_transactions(
                        |seq, read_tx: &TestTransaction| -> Result<(), FifoError> {
                            assert_eq!(read_tx.tx_id, i);
                            consumed_seq = Some(seq);
                            Ok(())
                        },
                    )
                    .unwrap();
                assert_eq!(count, 1);
                assert_eq!(consumed_seq, Some(current_seq));

                current_seq = current_seq.wrapping_add(1);
            }
        }
    }

    #[fuchsia::test]
    fn test_extreme_layer_seq_rollover_end_to_end() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        // Start near u32::MAX (0xFFFF_FFFE)
        writer.set_initial_sequence_for_test(0, 0, 0xFFFF_FFFE);
        reader.set_initial_sequence_for_test(0, 0xFFFF_FFFE);

        // First seal: 0xFFFF_FFFE -> 0xFFFF_FFFF
        let new_layer = reader
            .seal_layer_and_consume(
                |_, _| -> Result<(), FifoError> { Ok(()) },
                |layer| -> Result<(), FifoError> {
                    assert_eq!(layer, 0xFFFF_FFFF);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(new_layer, 0xFFFF_FFFF);

        let tx1 = TestTransaction::borrowing(1);
        let res1 = writer.write_transaction(&tx1).unwrap();
        assert_eq!(res1.new_layer_seq, Some(0xFFFF_FFFF));
        assert_eq!(res1.layers_sealed, 1);

        // Second seal: 0xFFFF_FFFF -> 0x0000_0000 (Rollover!)
        let new_layer = reader
            .seal_layer_and_consume(
                |seq, tx: &TestTransaction| -> Result<(), FifoError> {
                    assert_eq!(seq, 0);
                    assert_eq!(tx.tx_id, 1);
                    Ok(())
                },
                |layer| -> Result<(), FifoError> {
                    assert_eq!(layer, 0x0000_0000);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(new_layer, 0x0000_0000);

        let tx2 = TestTransaction::borrowing(2);
        let res2 = writer.write_transaction(&tx2).unwrap();
        assert_eq!(res2.new_layer_seq, Some(0x0000_0000));
        assert_eq!(res2.layers_sealed, 1);

        // Multiple rapid seals across rollover: 0 -> 1 -> 2 -> 3 -> 4
        for expected in 1..=4u32 {
            assert_eq!(reader.increment_layer_seq(), expected);
        }

        let tx3 = TestTransaction::borrowing(3);
        let res3 = writer.write_transaction(&tx3).unwrap();
        assert_eq!(res3.new_layer_seq, Some(4));
        assert_eq!(res3.layers_sealed, 4);
    }

    #[fuchsia::test]
    fn test_single_slot_ringbuffer_stress() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(1);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 1).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 1).unwrap();

        // Give reader metadata space for reserving transactions
        reader.set_available_metadata_space(TRANSACTION_METADATA_MAX_AMOUNT * 2000);

        // Wrap-around across 16-bit sequence boundary with single slot
        writer.set_initial_sequence_for_test(0xFFF8, 0xFFF8, 0);
        reader.set_initial_sequence_for_test(0xFFF8, 0);

        let mut expected_seq: u16 = 0xFFF8;
        for i in 0..1000u64 {
            assert!(writer.has_room());
            assert_eq!(reader.pending_transactions_count(), 0);

            // Alternate between reserving and borrowing
            let tx = if i % 2 == 0 {
                TestTransaction::reserving(i)
            } else {
                TestTransaction::borrowing(i)
            };
            let commit_res = writer.write_transaction(&tx).unwrap();
            assert_eq!(commit_res.tx_seq, expected_seq);

            // Buffer is now full
            assert!(!writer.has_room());
            assert_eq!(reader.pending_transactions_count(), 1);

            // Attempting another write MUST fail with FifoError::Full
            let tx_overflow = TestTransaction::borrowing(99999);
            assert_eq!(writer.write_transaction(&tx_overflow), Err(FifoError::Full));

            // Reader consumes the single slot
            let mut read_count = 0;
            let count = reader
                .consume_transactions(|seq, read_tx: &TestTransaction| -> Result<(), FifoError> {
                    assert_eq!(seq, expected_seq);
                    assert_eq!(read_tx.tx_id, i);
                    read_count += 1;
                    Ok(())
                })
                .unwrap();
            assert_eq!(count, 1);
            assert_eq!(read_count, 1);

            // Buffer is now empty again
            assert!(writer.has_room());
            assert_eq!(reader.pending_transactions_count(), 0);

            expected_seq = expected_seq.wrapping_add(1);
        }
    }

    #[fuchsia::test]
    fn test_empty_vs_full_burst_transitions() {
        let capacity = 8;
        let (vmo_writer, vmo_reader) = create_vmo_pair(capacity);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, capacity).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, capacity).unwrap();

        // Give reader ample metadata space
        reader.set_available_metadata_space(TRANSACTION_METADATA_MAX_AMOUNT * 5000);

        // Start near sequence wrap point
        writer.set_initial_sequence_for_test(0xFFE0, 0xFFE0, 0);
        reader.set_initial_sequence_for_test(0xFFE0, 0);

        let mut expected_seq: u16 = 0xFFE0;
        let mut next_id = 0u64;

        for _burst in 0..50 {
            assert!(writer.has_room());
            assert_eq!(reader.pending_transactions_count(), 0);

            // Fill all 8 slots
            for _ in 0..capacity {
                let tx = if next_id % 2 == 0 {
                    TestTransaction::reserving(next_id)
                } else {
                    TestTransaction::borrowing(next_id)
                };
                let commit_res = writer.write_transaction(&tx).unwrap();
                assert_eq!(commit_res.tx_seq, expected_seq);
                expected_seq = expected_seq.wrapping_add(1);
                next_id += 1;
            }

            // Buffer is completely full
            assert!(!writer.has_room());
            assert_eq!(reader.pending_transactions_count(), capacity as usize);

            // 9th write fails
            let tx_overflow = TestTransaction::borrowing(99999);
            assert_eq!(writer.write_transaction(&tx_overflow), Err(FifoError::Full));

            // Drain all 8 slots at once
            let mut drained_ids = Vec::new();
            let count = reader
                .consume_transactions(|_, tx: &TestTransaction| -> Result<(), FifoError> {
                    drained_ids.push(tx.tx_id);
                    Ok(())
                })
                .unwrap();

            assert_eq!(count, capacity as usize);
            assert_eq!(drained_ids.len(), capacity as usize);
            assert_eq!(reader.pending_transactions_count(), 0);
            assert!(writer.has_room());
        }
    }

    #[fuchsia::test]
    fn test_payload_boundaries_exact_and_oversized() {
        let capacity_slots = 2u32;
        let (vmo_w, vmo_r) = create_vmo_pair(capacity_slots);
        let actual_vmo_size = vmo_w.get_size().unwrap() as usize;
        let slot_size_bytes = (actual_vmo_size - HEADER_SIZE) / (capacity_slots as usize);

        let mut writer = FifoWriter::<DynamicTestTransaction>::new(vmo_w, capacity_slots).unwrap();
        let mut reader = FifoReader::<DynamicTestTransaction>::new(vmo_r, capacity_slots).unwrap();

        // Bincode Vec<u8> overhead is 8 bytes (u64 length prefix).
        // Total serialized bytes = 8 + payload.len().
        // Max allowed serialized bytes = slot_size_bytes - 4.
        // Therefore max data len = slot_size_bytes - 4 - 8.
        let max_data_len = slot_size_bytes - 4 - 8;
        let exact_fit_tx = DynamicTestTransaction { data: vec![0xAB; max_data_len] };

        let commit_res = writer.write_transaction(&exact_fit_tx).expect("Exact fit should succeed");
        assert_eq!(commit_res.tx_seq, 0);

        let mut read_tx = None;
        let count = reader
            .consume_transactions(|_, tx: &DynamicTestTransaction| -> Result<(), FifoError> {
                read_tx = Some(tx.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(read_tx, Some(exact_fit_tx));

        // 1 byte oversized: data len = max_data_len + 1 -> serialized size = max_data_len + 9
        // = slot_size_bytes - 3. (slot_size_bytes - 3) + 4 = slot_size_bytes + 1 > slot_size_bytes.
        let oversized_tx = DynamicTestTransaction { data: vec![0xCD; max_data_len + 1] };
        assert_eq!(writer.write_transaction(&oversized_tx), Err(FifoError::InvalidConfiguration));

        // Test reader rejection when slot length header in VMO is tampered
        let tx_valid = DynamicTestTransaction { data: vec![1, 2, 3] };
        let _ = writer.write_transaction(&tx_valid).unwrap();

        // Tamper slot 1's length header in VMO: set length to slot_size_bytes - 3 (1 byte over
        // the max allowed payload of slot_size_bytes - 4).
        let slot_1_offset = (HEADER_SIZE + slot_size_bytes) as u64;
        let corrupted_len = (slot_size_bytes as u32) - 3;
        reader.vmo().write(&corrupted_len.to_ne_bytes(), slot_1_offset).unwrap();

        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::InvalidConfiguration)
        );
    }

    #[fuchsia::test]
    fn test_small_slot_size() {
        // With capacity = 4096 on a 20480-byte (5 * 4096) VMO:
        // (20480 - 64) / 4096 = 4 bytes per slot.
        let vmo = zx::Vmo::create(5 * 4096).unwrap();
        let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<ZeroSizedTransaction>::new(vmo, 4096).unwrap();
        let mut reader = FifoReader::<ZeroSizedTransaction>::new(vmo_dup, 4096).unwrap();

        // ZeroSizedTransaction serializes to 0 bytes in bincode.
        // 0 + 4 = 4 <= 4 bytes slot size.
        let res = writer.write_transaction(&ZeroSizedTransaction).unwrap();
        assert_eq!(res.tx_seq, 0);

        let mut read_tx = None;
        let count = reader
            .consume_transactions(|seq, tx: &ZeroSizedTransaction| -> Result<(), FifoError> {
                assert_eq!(seq, 0);
                read_tx = Some(tx.clone());
                Ok(())
            })
            .unwrap();

        assert_eq!(count, 1);
        assert_eq!(read_tx, Some(ZeroSizedTransaction));
    }

    #[fuchsia::test]
    fn test_wire_transaction_empty_mutation_list_fifo_roundtrip() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(2);
        let mut writer = FifoWriter::<WireTransaction>::new(vmo_writer, 2).unwrap();
        let mut reader = FifoReader::<WireTransaction>::new(vmo_reader, 2).unwrap();

        let tx_reserving_empty =
            WireTransaction { flags: WireTransactionFlags::empty(), mutations: vec![] };
        let tx_borrowing_empty =
            WireTransaction { flags: WireTransactionFlags::BORROW_METADATA, mutations: vec![] };

        // Give reader enough metadata space for the reserving transaction
        reader.set_available_metadata_space(TRANSACTION_METADATA_MAX_AMOUNT * 2);

        writer.write_transaction(&tx_reserving_empty).expect("Reserving empty mutations write");
        writer.write_transaction(&tx_borrowing_empty).expect("Borrowing empty mutations write");

        let mut received = Vec::new();
        let count = reader
            .consume_transactions(|seq, tx: &WireTransaction| -> Result<(), FifoError> {
                received.push((seq, tx.clone()));
                Ok(())
            })
            .unwrap();

        assert_eq!(count, 2);
        assert_eq!(received[0], (0, tx_reserving_empty));
        assert_eq!(received[1], (1, tx_borrowing_empty));
    }

    #[fuchsia::test(threads = 4)]
    async fn test_adversarial_multi_threaded_high_contention_single_slot() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(1);
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 1).unwrap()));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 1).unwrap();

        let num_writers = 20;
        let txs_per_writer = 10;
        let total_txs = num_writers * txs_per_writer;

        let (tx_done, mut rx_done) = futures::channel::mpsc::unbounded();
        let mut tasks = Vec::new();

        for writer_id in 0..num_writers {
            let writer_clone = Arc::clone(&writer);
            let tx_done_clone = tx_done.clone();
            tasks.push(fuchsia_async::Task::spawn(async move {
                for i in 0..txs_per_writer {
                    let tx_id = (writer_id as u64) * 1000 + (i as u64);
                    let tx = TestTransaction::borrowing(tx_id);
                    loop {
                        let res = writer_clone.lock().write_transaction(&tx);
                        match res {
                            Ok(commit) => {
                                tx_done_clone
                                    .unbounded_send((writer_id, tx_id, commit.tx_seq))
                                    .unwrap();
                                break;
                            }
                            Err(FifoError::Full) => {
                                let w = Arc::clone(&writer_clone);
                                fuchsia_async::unblock(move || FifoWriter::wait_for_room(&w))
                                    .await
                                    .unwrap();
                            }
                            Err(e) => panic!("Unexpected error: {:?}", e),
                        }
                    }
                }
            }));
        }

        let mut received_seqs = Vec::new();
        let mut received_ids = Vec::new();

        while received_ids.len() < total_txs {
            fuchsia_async::Timer::new(fuchsia_async::MonotonicInstant::after(
                zx::MonotonicDuration::from_micros(200),
            ))
            .await;

            let _ = reader
                .consume_transactions(|seq, tx: &TestTransaction| -> Result<(), FifoError> {
                    received_seqs.push(seq);
                    received_ids.push(tx.tx_id);
                    Ok(())
                })
                .unwrap();
        }

        for task in tasks {
            task.await;
        }

        let mut writer_results = Vec::new();
        for _ in 0..total_txs {
            writer_results.push(rx_done.next().await.unwrap());
        }

        assert_eq!(received_seqs.len(), total_txs);
        assert_eq!(received_ids.len(), total_txs);
        for (i, seq) in received_seqs.iter().enumerate() {
            assert_eq!(*seq, i as u16);
        }
        writer_results.sort_by_key(|&(_, _, seq)| seq);
        for (i, &(_, tx_id, seq)) in writer_results.iter().enumerate() {
            assert_eq!(seq, i as u16);
            assert_eq!(tx_id, received_ids[i]);
        }
    }

    #[fuchsia::test(threads = 4)]
    async fn test_adversarial_compaction_drain_racing_with_flood_writers() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap()));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        let num_writers = 4;
        let txs_per_writer = 25;
        let total_txs = num_writers * txs_per_writer;

        let (tx_done, mut rx_done) = futures::channel::mpsc::unbounded();
        let mut tasks = Vec::new();

        for writer_id in 0..num_writers {
            let writer_clone = Arc::clone(&writer);
            let tx_done_clone = tx_done.clone();
            tasks.push(fuchsia_async::Task::spawn(async move {
                for i in 0..txs_per_writer {
                    let tx = TestTransaction::borrowing((writer_id as u64) * 1000 + i as u64);
                    loop {
                        let res = writer_clone.lock().write_transaction(&tx);
                        match res {
                            Ok(commit) => {
                                tx_done_clone.unbounded_send((writer_id, commit)).unwrap();
                                break;
                            }
                            Err(FifoError::Full) => {
                                let w = Arc::clone(&writer_clone);
                                fuchsia_async::unblock(move || FifoWriter::wait_for_room(&w))
                                    .await
                                    .unwrap();
                            }
                            Err(e) => panic!("Unexpected error: {:?}", e),
                        }
                    }
                }
            }));
        }

        let mut total_drained = 0;
        let mut total_seals = 0;
        let mut drained_by_layer = Vec::new();

        while total_drained < total_txs {
            fuchsia_async::Timer::new(fuchsia_async::MonotonicInstant::after(
                zx::MonotonicDuration::from_micros(500),
            ))
            .await;

            if total_seals < 5 && total_drained < total_txs - 10 {
                let mut layer_items = Vec::new();
                let layer_seq = reader
                    .seal_layer_and_consume(
                        |seq, tx: &TestTransaction| -> Result<(), FifoError> {
                            layer_items.push((seq, tx.tx_id));
                            total_drained += 1;
                            Ok(())
                        },
                        |_| -> Result<(), FifoError> { Ok(()) },
                    )
                    .unwrap();
                drained_by_layer.push((Some(layer_seq), layer_items));
                total_seals += 1;
            } else {
                let mut batch_items = Vec::new();
                let _ = reader
                    .consume_transactions(|seq, tx: &TestTransaction| -> Result<(), FifoError> {
                        batch_items.push((seq, tx.tx_id));
                        total_drained += 1;
                        Ok(())
                    })
                    .unwrap();
                if !batch_items.is_empty() {
                    drained_by_layer.push((None, batch_items));
                }
            }
        }

        for task in tasks {
            task.await;
        }

        let mut writer_results = Vec::new();
        for _ in 0..total_txs {
            writer_results.push(rx_done.next().await.unwrap());
        }

        assert_eq!(total_drained, total_txs);
        assert_eq!(writer_results.len(), total_txs);

        // Verify contiguous sequence ordering across all drained batches and compaction
        // linearizability: any commit that observed `new_layer_seq == Some(L)` happened after
        // seal `L` incremented `layer_seq`, so it must never have been drained into seal `<= L`.
        let mut expected_seq = 0u16;
        let mut sealed_seq_for_tx = vec![None; total_txs];
        for (sealed_layer, items) in &drained_by_layer {
            for &(seq, _) in items {
                assert_eq!(seq, expected_seq);
                sealed_seq_for_tx[seq as usize] = *sealed_layer;
                expected_seq += 1;
            }
        }
        assert_eq!(expected_seq as usize, total_txs);

        for &(_, commit) in &writer_results {
            if let Some(observed_layer) = commit.new_layer_seq
                && let Some(drained_into_seal) = sealed_seq_for_tx[commit.tx_seq as usize]
            {
                assert!(
                    drained_into_seal > observed_layer,
                    "tx {} observed layer {} but was drained into already-sealed layer {}",
                    commit.tx_seq,
                    observed_layer,
                    drained_into_seal
                );
            }
        }
    }

    #[fuchsia::test]
    fn test_adversarial_metadata_tampering_and_overdraw() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        // 1. Initial available metadata space = 0 -> Reserving write fails with NoSpace
        reader.set_available_metadata_space(0);
        let tx_reserving = TestTransaction::reserving(1);
        assert_eq!(writer.write_transaction(&tx_reserving), Err(FifoError::NoSpace));

        // 2. Insufficient space (e.g. 100 bytes < 98_304) -> Reserving write fails with NoSpace
        reader.set_available_metadata_space(100);
        assert_eq!(writer.write_transaction(&tx_reserving), Err(FifoError::NoSpace));

        // 3. Exact space for 1 transaction (98_304) -> Reserving write succeeds and exhausts space
        reader.set_available_metadata_space(TRANSACTION_METADATA_MAX_AMOUNT);
        assert!(writer.write_transaction(&tx_reserving).is_ok());
        assert_eq!(reader.available_metadata_space(), 0);
        assert_eq!(writer.write_transaction(&tx_reserving), Err(FifoError::NoSpace));
        reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }).unwrap();
        assert_eq!(reader.last_known_available_metadata_space(), 0);

        // 4. Tampering: Untrusted peer writes u64::MAX to offset 8 in VMO
        vmo_bad_peer.write(&u64::MAX.to_ne_bytes(), METADATA_SPACE_OFFSET as u64).unwrap();
        assert_eq!(reader.available_metadata_space(), u64::MAX);

        // Fxfs attempts to add space and detects tampering -> returns InvalidState and fuses FIFO
        assert_eq!(reader.add_available_metadata_space(1000), Err(FifoError::InvalidState));
        assert!(reader.is_fused());
        assert_eq!(writer.write_transaction(&tx_reserving), Err(FifoError::BadState));
        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::BadState)
        );

        // 5a. Tampering slot payload length to 1 byte past the slot boundary
        // (`actual_slot_size - 3`, exceeding max payload `actual_slot_size - 4`)
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();
        let actual_slot_size = (reader.vmo().get_size().unwrap() as usize - HEADER_SIZE) / 4;

        let tx_valid = TestTransaction::borrowing(2);
        writer.write_transaction(&tx_valid).unwrap();

        let slot_0_offset = HEADER_SIZE as u64;
        let boundary_overflow_len = (actual_slot_size - 3) as u32;
        vmo_bad_peer.write(&boundary_overflow_len.to_le_bytes(), slot_0_offset).unwrap();

        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::InvalidConfiguration)
        );
        assert!(reader.is_fused());
        assert_eq!(writer.write_transaction(&tx_valid), Err(FifoError::BadState));

        // 5b. Tampering slot payload length to u32::MAX (arithmetic overflow guard on `len + 4`)
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();
        writer.write_transaction(&tx_valid).unwrap();
        vmo_bad_peer.write(&u32::MAX.to_le_bytes(), slot_0_offset).unwrap();
        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::InvalidConfiguration)
        );
        assert!(reader.is_fused());

        // 6a. Tampering slot payload with truncated length (non-deserializable)
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();
        writer.write_transaction(&tx_valid).unwrap();

        let garbage_len = 20u32;
        vmo_bad_peer.write(&garbage_len.to_le_bytes(), slot_0_offset).unwrap();
        vmo_bad_peer.write(&[0xFF; 20], slot_0_offset + 4).unwrap();

        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::InvalidConfiguration)
        );
        assert!(reader.is_fused());
        assert_eq!(writer.write_transaction(&tx_valid), Err(FifoError::BadState));

        // 6b. Tampering slot payload length to include trailing unconsumed bytes
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();
        writer.write_transaction(&tx_valid).unwrap();

        let mut raw_len = [0u8; 4];
        vmo_bad_peer.read(&mut raw_len, slot_0_offset).unwrap();
        let trailing_len = u32::from_le_bytes(raw_len) + 1;
        vmo_bad_peer.write(&trailing_len.to_le_bytes(), slot_0_offset).unwrap();

        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::InvalidConfiguration)
        );
        assert!(reader.is_fused());
    }

    #[fuchsia::test]
    async fn test_adversarial_async_cancellation_and_resource_cleanup() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(1);
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 1).unwrap()));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 1).unwrap();

        // 1. Fill and drain the 1-slot FIFO, verifying wait_for_room resolves cleanly.
        let tx = TestTransaction::borrowing(1);
        writer.lock().write_transaction(&tx).unwrap();
        assert!(!writer.lock().has_room());

        // Reader consumes 1 slot -> asserts USER_1
        reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }).unwrap();

        // New wait_for_room must resolve cleanly
        FifoWriter::wait_for_room(&writer).unwrap();
        assert!(writer.lock().has_room());

        // Fill FIFO again
        writer.lock().write_transaction(&tx).unwrap();

        // 2. Repeatedly spawn and cancel wait_for_data on reader 200 times
        reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }).unwrap();
        assert_eq!(reader.pending_transactions_count(), 0);

        for _ in 0..200 {
            let mut wait_future = Box::pin(reader.wait_for_data());
            let _ = futures::poll!(&mut wait_future);
            drop(wait_future);
        }

        // Writer pushes transaction -> asserts USER_0
        writer.lock().write_transaction(&tx).unwrap();

        // New wait_for_data must resolve cleanly
        reader.wait_for_data().await.unwrap();
        assert_eq!(reader.pending_transactions_count(), 1);
    }

    #[fuchsia::test]
    fn test_shared_state_word_tampering_detection() {
        let tx1 = TestTransaction::borrowing(1);

        // 1. Peer tampers write_ptr so pending > capacity_slots (e.g. write_ptr = 10 > 4)
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();
        writer.write_transaction(&tx1).unwrap();

        let tampered_write = SharedStateWord(0).with_read_ptr(0).with_write_ptr(10);
        vmo_bad_peer.write(&tampered_write.0.to_ne_bytes(), 0).unwrap();
        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::InvalidState)
        );
        assert!(reader.is_fused());
        assert_eq!(
            reader.seal_layer_and_consume(
                |_, _| -> Result<(), FifoError> { Ok(()) },
                |_| -> Result<(), FifoError> { Ok(()) },
            ),
            Err(FifoError::BadState)
        );
        assert_eq!(writer.write_transaction(&tx1), Err(FifoError::BadState));

        // 2. Peer tampers read_ptr so read_ptr != last_known_write_ptr
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();
        writer.write_transaction(&tx1).unwrap();

        let tampered_read = SharedStateWord(0).with_read_ptr(1).with_write_ptr(2);
        vmo_bad_peer.write(&tampered_read.0.to_ne_bytes(), 0).unwrap();
        assert_eq!(
            reader.seal_layer_and_consume(
                |_, _| -> Result<(), FifoError> { Ok(()) },
                |_| -> Result<(), FifoError> { Ok(()) },
            ),
            Err(FifoError::InvalidState)
        );
        assert!(reader.is_fused());
        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::BadState)
        );
        assert_eq!(writer.write_transaction(&tx1), Err(FifoError::BadState));

        // 3. Valid state (read_ptr = 0, write_ptr = 1), then tamper read_ptr mid-consume
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();
        writer.write_transaction(&tx1).unwrap();

        let vmo_bad_peer_clone = vmo_bad_peer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        reader.callback_before_read_ptr_update = Some(Arc::new(move || {
            let tampered = SharedStateWord(0).with_read_ptr(3).with_write_ptr(1);
            vmo_bad_peer_clone.write(&tampered.0.to_ne_bytes(), 0).unwrap();
        }));
        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::InvalidState)
        );
        assert!(reader.is_fused());
        assert_eq!(writer.write_transaction(&tx1), Err(FifoError::BadState));

        // 4. Peer tampers reader-owned layer_seq before consume_transactions and before
        // seal_layer_and_consume
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();
        writer.write_transaction(&tx1).unwrap();

        let tampered_layer =
            SharedStateWord(0).with_read_ptr(0).with_write_ptr(1).with_layer_seq(9);
        vmo_bad_peer.write(&tampered_layer.0.to_ne_bytes(), 0).unwrap();
        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::InvalidState)
        );
        assert!(reader.is_fused());

        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();
        writer.write_transaction(&tx1).unwrap();
        vmo_bad_peer.write(&tampered_layer.0.to_ne_bytes(), 0).unwrap();
        assert_eq!(
            reader.seal_layer_and_consume(
                |_, _| -> Result<(), FifoError> { Ok(()) },
                |_| -> Result<(), FifoError> { Ok(()) },
            ),
            Err(FifoError::InvalidState)
        );
        assert!(reader.is_fused());
    }

    #[fuchsia::test]
    async fn test_fifo_closed_signal_wakes_waiters() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(1);
        let vmo_signaler = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 1).unwrap()));
        let reader = FifoReader::<TestTransaction>::new(vmo_reader, 1).unwrap();

        // 1. Reader wait_for_data returns Ok(()) when FIFO_CLOSED_SIGNAL is asserted on empty FIFO
        vmo_signaler.signal(zx::Signals::empty(), FIFO_CLOSED_SIGNAL).unwrap();
        reader.wait_for_data().await.unwrap();

        // 2. Writer wait_for_room returns BadState when FIFO_CLOSED_SIGNAL is asserted on full FIFO
        vmo_signaler.signal(FIFO_CLOSED_SIGNAL, zx::Signals::empty()).unwrap();
        writer.lock().write_transaction(&TestTransaction::borrowing(1)).unwrap();
        assert!(!writer.lock().has_room());
        vmo_signaler.signal(zx::Signals::empty(), FIFO_CLOSED_SIGNAL).unwrap();
        assert_eq!(FifoWriter::wait_for_room(&writer), Err(FifoError::BadState));
    }

    #[fuchsia::test]
    async fn test_fifo_closed_header_and_signal_fails_wait_for_room_and_write_transaction() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(2);
        let vmo_signaler = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let writer =
            Arc::new(Mutex::new(FifoWriter::<TestTransaction>::new(vmo_writer, 2).unwrap()));
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 2).unwrap();
        reader.set_available_metadata_space(TRANSACTION_METADATA_MAX_AMOUNT * 2);

        assert!(!writer.lock().is_closed());

        // Reader fuses the FIFO via reader.fuse() (sets FIFO_STATE_CLOSED in header and asserts
        // FIFO_CLOSED_SIGNAL on VMO).
        reader.fuse();
        assert!(reader.is_fused());
        assert!(writer.lock().is_closed());

        // Clear FIFO_CLOSED_SIGNAL on the VMO to verify that fast-path checks use the shared
        // header word (`fifo_state`) rather than querying VMO signals.
        vmo_signaler.signal(FIFO_CLOSED_SIGNAL, zx::Signals::empty()).unwrap();

        // 1. write_transaction fails with BadState immediately even though the FIFO has room,
        // metadata space, and no VMO signals asserted.
        let mut guard = writer.lock();
        assert!(guard.has_room());
        assert_eq!(
            guard.write_transaction(&TestTransaction::borrowing(1)),
            Err(FifoError::BadState)
        );
        drop(guard);

        // 2. wait_for_room fails with BadState immediately via the shared header word.
        assert_eq!(FifoWriter::wait_for_room(&writer), Err(FifoError::BadState));
    }

    #[fuchsia::test]
    fn test_reader_decrements_and_enforces_metadata_space_per_transaction() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        // Initial metadata space: 2 transactions worth
        reader.set_available_metadata_space(TRANSACTION_METADATA_MAX_AMOUNT * 2);
        assert_eq!(
            reader.last_known_available_metadata_space(),
            TRANSACTION_METADATA_MAX_AMOUNT * 2
        );

        // Writer writes 1 reserving and 1 borrowing transaction
        let tx_reserving = TestTransaction::reserving(1);
        let tx_borrowing = TestTransaction::borrowing(2);
        writer.write_transaction(&tx_reserving).unwrap();
        writer.write_transaction(&tx_borrowing).unwrap();

        // Consume transactions
        let consumed =
            reader.consume_transactions(|_seq, _tx| -> Result<(), FifoError> { Ok(()) }).unwrap();
        assert_eq!(consumed, 2);

        // Reader's last_known_available_metadata_space decremented by 1 *
        // TRANSACTION_METADATA_MAX_AMOUNT
        assert_eq!(reader.last_known_available_metadata_space(), TRANSACTION_METADATA_MAX_AMOUNT);
        assert_eq!(reader.available_metadata_space(), TRANSACTION_METADATA_MAX_AMOUNT);

        // Now test tampering detection: peer maliciously increments available_metadata_space
        vmo_bad_peer
            .write(
                &(TRANSACTION_METADATA_MAX_AMOUNT * 10).to_ne_bytes(),
                METADATA_SPACE_OFFSET as u64,
            )
            .unwrap();
        writer.write_transaction(&tx_borrowing).unwrap();

        // Consuming must detect tampering (vmo_available > last_known_available_metadata_space)
        let res = reader.consume_transactions(|_seq, _tx| -> Result<(), FifoError> { Ok(()) });
        assert_eq!(res, Err(FifoError::InvalidState));
    }

    #[fuchsia::test]
    async fn test_wire_transaction_from_transaction_and_roundtrip() {
        let device = DeviceHolder::new(FakeDevice::new(8192, 512));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_vol
            .new_volume("test_vol", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let store_id = store.store_object_id();

        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<WireTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<WireTransaction>::new(vmo_reader, 4).unwrap();
        reader.set_available_metadata_space(TRANSACTION_METADATA_MAX_AMOUNT * 2);

        // 1. Construct a reserving Transaction and convert via WireTransaction::from(&tx)
        let mut tx_new = store
            .new_transaction(
                lock_keys![],
                Options { reservation: ReservationOptions::New, ..Default::default() },
            )
            .await
            .expect("new_transaction failed");
        let created =
            ObjectStore::create_object(&store, &mut tx_new, HandleOptions::default(), None)
                .await
                .expect("create_object failed");
        let created_id = created.object_id();
        let wire_new = WireTransaction::from(&tx_new);
        drop(tx_new);

        assert!(!wire_new.borrow_metadata_space());
        assert!(wire_new.needs_reservation());
        assert!(!wire_new.mutations.is_empty());

        // 2. Construct a borrowing Transaction and convert via WireTransaction::from(&tx)
        let mut tx_borrowed = store
            .new_transaction(
                lock_keys![LockKey::object(store_id, created_id)],
                Options { reservation: ReservationOptions::Borrowed, ..Default::default() },
            )
            .await
            .expect("new_transaction borrowed failed");
        tx_borrowed.add(
            store_id,
            Mutation::replace_or_insert_object(
                ObjectKey::object(created_id),
                ObjectValue::Object {
                    kind: ObjectKind::File { refs: 1 },
                    attributes: ObjectAttributes::default(),
                },
            ),
        );
        let wire_borrowed = WireTransaction::from(&tx_borrowed);
        drop(tx_borrowed);

        assert!(wire_borrowed.borrow_metadata_space());
        assert!(!wire_borrowed.needs_reservation());

        // 3. Push both over FifoWriter and consume on FifoReader
        writer.write_transaction(&wire_new).expect("write wire_new");
        assert_eq!(reader.available_metadata_space(), TRANSACTION_METADATA_MAX_AMOUNT);
        writer.write_transaction(&wire_borrowed).expect("write wire_borrowed");
        assert_eq!(reader.available_metadata_space(), TRANSACTION_METADATA_MAX_AMOUNT);

        let mut consumed = Vec::new();
        let count = reader
            .consume_transactions_if_any_match(
                |tx| tx.borrow_metadata_space(),
                |seq, tx| -> Result<(), FifoError> {
                    consumed.push((seq, tx.clone()));
                    Ok(())
                },
            )
            .expect("consume_transactions_if_any_match");
        assert_eq!(count, 2);
        assert_eq!(consumed, vec![(0, wire_new), (1, wire_borrowed)]);

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    fn test_callback_error_propagation() {
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        writer.write_transaction(&TestTransaction::borrowing(1)).unwrap();
        writer.write_transaction(&TestTransaction::borrowing(2)).unwrap();

        // Error in consume_transactions apply_fn fuses the FIFO closed for both reads and writes
        let res = reader.consume_transactions(|_, _| Err(FifoError::NoSpace));
        assert_eq!(res, Err(FifoError::NoSpace));
        assert!(reader.is_fused());
        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::BadState)
        );
        assert_eq!(
            writer.write_transaction(&TestTransaction::borrowing(3)),
            Err(FifoError::BadState)
        );

        // Error in seal_layer_and_consume seal_fn propagates error and fuses the FIFO
        let (vmo_writer2, vmo_reader2) = create_vmo_pair(4);
        let mut writer2 = FifoWriter::<TestTransaction>::new(vmo_writer2, 4).unwrap();
        let mut reader2 = FifoReader::<TestTransaction>::new(vmo_reader2, 4).unwrap();
        writer2.write_transaction(&TestTransaction::borrowing(1)).unwrap();

        let res = reader2.seal_layer_and_consume(
            |_, _| -> Result<(), FifoError> { Ok(()) },
            |_| Err(FifoError::NoSpace),
        );
        assert_eq!(res, Err(FifoError::NoSpace));
        assert!(reader2.is_fused());
        assert_eq!(
            reader2.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::BadState)
        );
        assert_eq!(
            writer2.write_transaction(&TestTransaction::borrowing(2)),
            Err(FifoError::BadState)
        );
    }

    #[fuchsia::test]
    fn test_adversarial_infinite_reservation_tamper_detection() {
        // Case 1: Peer replenishes metadata space to overdraw reservations during
        // `consume_transactions`. Even though the shared atomic returns to 0, `FifoReader` tracks
        // `last_known_available_metadata_space` and rejects the unbacked 2nd transaction before
        // invoking `apply_fn` for it, fusing the FIFO closed.
        let (vmo_writer, vmo_reader) = create_vmo_pair(4);
        let vmo_bad_peer = vmo_writer.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer = FifoWriter::<TestTransaction>::new(vmo_writer, 4).unwrap();
        let mut reader = FifoReader::<TestTransaction>::new(vmo_reader, 4).unwrap();

        // 1. Initial space for 1 transaction.
        reader.set_available_metadata_space(TRANSACTION_METADATA_MAX_AMOUNT);

        // 2. Writer reserves 1 transaction. Shared space -> 0.
        writer.write_transaction(&TestTransaction::reserving(1)).unwrap();
        assert_eq!(reader.available_metadata_space(), 0);

        // 3. Bad peer replenishes space to MAX_AMOUNT illegally.
        vmo_bad_peer
            .write(&TRANSACTION_METADATA_MAX_AMOUNT.to_ne_bytes(), METADATA_SPACE_OFFSET as u64)
            .unwrap();

        // 4. Writer writes a 2nd reserving transaction. Shared space -> 0 (illegal overdraw).
        writer.write_transaction(&TestTransaction::reserving(2)).unwrap();

        // 5. Fxfs attempts to consume both: tx 1 consumes the sole reservation; tx 2 immediately
        // fails with `InvalidState` before `apply_fn` runs for it, fusing the FIFO.
        let mut applied_ids = Vec::new();
        let err = reader
            .consume_transactions(|_, tx| -> Result<(), FifoError> {
                applied_ids.push(tx.tx_id);
                Ok(())
            })
            .unwrap_err();
        assert_eq!(err, FifoError::InvalidState);
        assert_eq!(applied_ids, vec![1]);
        assert!(reader.is_fused());
        assert_eq!(reader.add_available_metadata_space(100), Err(FifoError::BadState));
        assert_eq!(
            reader.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }),
            Err(FifoError::BadState)
        );
        assert_eq!(
            writer.write_transaction(&TestTransaction::borrowing(3)),
            Err(FifoError::BadState)
        );

        // Case 2: Same overdraw during `seal_layer_and_consume` rejects the unbacked 2nd
        // transaction before calling `apply_fn` on it or invoking `seal_fn`.
        let (vmo_writer2, vmo_reader2) = create_vmo_pair(4);
        let vmo_bad_peer2 = vmo_writer2.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer2 = FifoWriter::<TestTransaction>::new(vmo_writer2, 4).unwrap();
        let mut reader2 = FifoReader::<TestTransaction>::new(vmo_reader2, 4).unwrap();
        reader2.set_available_metadata_space(TRANSACTION_METADATA_MAX_AMOUNT);
        writer2.write_transaction(&TestTransaction::reserving(1)).unwrap();
        vmo_bad_peer2
            .write(&TRANSACTION_METADATA_MAX_AMOUNT.to_ne_bytes(), METADATA_SPACE_OFFSET as u64)
            .unwrap();
        writer2.write_transaction(&TestTransaction::reserving(2)).unwrap();

        let mut sealed_ids = Vec::new();
        let mut seal_called = false;
        let err2 = reader2
            .seal_layer_and_consume(
                |_, tx| -> Result<(), FifoError> {
                    sealed_ids.push(tx.tx_id);
                    Ok(())
                },
                |_| -> Result<(), FifoError> {
                    seal_called = true;
                    Ok(())
                },
            )
            .unwrap_err();
        assert_eq!(err2, FifoError::InvalidState);
        assert_eq!(sealed_ids, vec![1]);
        assert!(!seal_called);
        assert!(reader2.is_fused());

        // Case 3: Peer inflates credit to 2 * MAX_AMOUNT and pushes 1 reserving transaction,
        // leaving shared credit (MAX_AMOUNT) > updated `last_known_available_metadata_space` (0).
        let (vmo_writer3, vmo_reader3) = create_vmo_pair(4);
        let vmo_bad_peer3 = vmo_writer3.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut writer3 = FifoWriter::<TestTransaction>::new(vmo_writer3, 4).unwrap();
        let mut reader3 = FifoReader::<TestTransaction>::new(vmo_reader3, 4).unwrap();
        reader3.set_available_metadata_space(TRANSACTION_METADATA_MAX_AMOUNT);
        vmo_bad_peer3
            .write(
                &(TRANSACTION_METADATA_MAX_AMOUNT * 2).to_ne_bytes(),
                METADATA_SPACE_OFFSET as u64,
            )
            .unwrap();
        writer3.write_transaction(&TestTransaction::reserving(1)).unwrap();

        let err3 =
            reader3.consume_transactions(|_, _| -> Result<(), FifoError> { Ok(()) }).unwrap_err();
        assert_eq!(err3, FifoError::InvalidState);
        assert!(reader3.is_fused());
    }
}
