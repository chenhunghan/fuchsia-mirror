// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! # The Allocator
//!
//! The allocator in Fxfs is filesystem-wide entity responsible for managing the allocation of
//! regions of the device to "owners" (which are `ObjectStore`).
//!
//! Allocations are tracked in an LSMTree with coalescing used to merge neighboring allocations
//! with the same properties (owner and reference count). As of writing, reference counting is not
//! used. (Reference counts are intended for future use if/when snapshot support is implemented.)
//!
//! There are a number of important implementation features of the allocator that warrant further
//! documentation.
//!
//! ## Byte limits
//!
//! Fxfs is a multi-volume filesystem. Fuchsia with fxblob currently uses two primary volumes -
//! an unencrypted volume for blob storage and an encrypted volume for data storage.
//! Byte limits ensure that no one volume can consume all available storage. This is important
//! as software updates must always be possible (blobs) and conversely configuration data should
//! always be writable (data).
//!
//! ## Reservation tracking
//!
//! Fxfs on Fuchsia leverages write-back caching which allows us to buffer writes in RAM until
//! memory pressure, an explicit flush or a file close requires us to persist data to storage.
//!
//! To ensure that we do not over-commit in such cases (e.g. by writing many files to RAM only to
//! find out tha there is insufficient disk to store them all), the allocator includes a simple
//! reservation tracking mechanism.
//!
//! Reservation tracking is implemented hierarchically such that a reservation can portion out
//! sub-reservations, each of which may be "reserved" or "forgotten" when it comes time to actually
//! allocate space.
//!
//! ## Fixed locations
//!
//! The only fixed location files in Fxfs are the first 512kiB extents of the two superblocks that
//! exist as the first things on the disk (i.e. The first 1MiB). The allocator manages these
//! locations, but does so using a 'mark_allocated' method distinct from all other allocations in
//! which the location is left up to the allocator.
//!
//! ## Deallocated unusable regions
//!
//! It is not legal to reuse a deallocated disk region until after a flush. Transactions
//! are not guaranteed to be persisted until after a successful flush so any reuse of
//! a region before then followed by power loss may lead to corruption.
//!
//! e.g. Consider if we deallocate a file, reuse its extent for another file, then crash after
//! writing to the new file but not yet flushing the journal. At next mount we will have lost the
//! transaction despite overwriting the original file's data, leading to inconsistency errors (data
//! corruption).
//!
//! These regions are currently tracked in RAM in the allocator.
//!
//! ## Allocated but uncommitted regions
//!
//! These are allocated regions that are not (yet) persisted to disk. They are regular
//! file allocations, but are not stored on persistent storage until their transaction is committed
//! and safely flushed to disk.
//!
//! ## TRIMed unusable regions
//!
//! We periodically TRIM unallocated regions to give the SSD controller insight into which
//! parts of the device contain data. We must avoid using temporary TRIM allocations that are held
//! while we perform these operations.
//!
//! ## Volume deletion
//!
//! We make use of an optimisation in the case where an entire volume is deleted. In such cases,
//! rather than individually deallocate all disk regions associated with that volume, we make
//! note of the deletion and perform special merge operation on the next LSMTree compaction that
//! filters out allocations for the deleted volume.
//!
//! This is designed to make dropping of volumes significantly cheaper, but it does add some
//! additional complexity if implementing an allocator that implements data structures to track
//! free space (rather than just allocated space).
//!
//! ## Image generation
//!
//! The Fuchsia build process requires building an initial filesystem image. In the case of
//! fxblob-based boards, this is an Fxfs filesystem containing a volume with the base set of
//! blobs required to bootstrap the system. When we build such an image, we want it to be as compact
//! as possible as we're potentially packaging it up for distribution. To that end, our allocation
//! strategy (or at least the strategy used for image generation) should prefer to allocate from the
//! start of the device wherever possible.
pub mod merge;
pub mod strategy;

use crate::drop_event::DropEvent;
use crate::errors::FxfsError;
use crate::filesystem::{ApplyContext, ApplyMode, FxFilesystem, JournalingObject, SyncOptions};
use crate::log::*;
use crate::lsm_tree::skip_list_layer::SkipListLayer;
use crate::lsm_tree::types::{
    Coalesce, CoalescingIterator, FuzzyHash, Item, ItemRef, Layer, LayerIterator, LayerKey,
    MergeType, OrdLowerBound, OrdUpperBound, SortByU64, Value,
};
use crate::lsm_tree::{LSMTree, Query, compact_with_iterator, layer_from_handle, open_layers};
use crate::object_handle::{INVALID_OBJECT_ID, ObjectHandle, ReadObjectHandle};
use crate::object_store::object_manager::ReservationUpdate;
use crate::object_store::transaction::{
    AllocatorMutation, AssocObj, LockKey, Mutation, Options, ReservationOptions, Transaction,
    WriteGuard, lock_keys,
};
use crate::object_store::{
    DataObjectHandle, DirectWriter, Extent, HandleOptions, ObjectStore, ReservedId,
};
use crate::range::RangeExt;
use crate::round::round_div;
use crate::serialized_types::{
    DEFAULT_MAX_SERIALIZED_RECORD_SIZE, LATEST_VERSION, Version, Versioned, VersionedLatest,
};
use anyhow::{Context, Error, anyhow, bail, ensure};
use async_trait::async_trait;
use either::Either::{Left, Right};
use event_listener::EventListener;
use fprint::TypeFingerprint;
use fuchsia_async as fasync;
use fuchsia_inspect::HistogramProperty;
use fuchsia_sync::Mutex;
use futures::FutureExt;
use fxfs_macros::SerializeKey;
use merge::{filter_marked_for_deletion, filter_tombstones, merge};
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::hash::Hash;
use std::marker::PhantomData;
use std::num::{NonZero, Saturating};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use storage_units::BlockSize;

/// This trait is implemented by things that own reservations.
pub trait ReservationOwner: Send + Sync {
    /// Report that bytes are being released from the reservation back to the the |ReservationOwner|
    /// where |owner_object_id| is the owner under the root object store associated with the
    /// reservation.
    fn release_reservation(&self, owner_object_id: Option<u64>, amount: u64);
}

/// A reservation guarantees that when it comes time to actually allocate, it will not fail due to
/// lack of space.  Sub-reservations (a.k.a. holds) are possible which effectively allows part of a
/// reservation to be set aside until it's time to commit.  Reservations do offer some
/// thread-safety, but some responsibility is born by the caller: e.g. calling `forget` and
/// `reserve` at the same time from different threads is unsafe. Reservations are have an
/// |owner_object_id| which associates it with an object under the root object store that the
/// reservation is accounted against.
pub struct ReservationImpl<T: Borrow<U>, U: ReservationOwner + ?Sized> {
    owner: T,
    owner_object_id: Option<u64>,
    inner: Mutex<ReservationInner>,
    phantom: PhantomData<U>,
}

#[derive(Debug, Default)]
struct ReservationInner {
    // Amount currently held by this reservation.
    amount: u64,

    // Amount reserved by sub-reservations.
    reserved: u64,
}

impl<T: Borrow<U>, U: ReservationOwner + ?Sized> std::fmt::Debug for ReservationImpl<T, U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.lock().fmt(f)
    }
}

impl<T: Borrow<U> + Clone + Send + Sync, U: ReservationOwner + ?Sized> ReservationImpl<T, U> {
    pub fn new(owner: T, owner_object_id: Option<u64>, amount: u64) -> Self {
        Self {
            owner,
            owner_object_id,
            inner: Mutex::new(ReservationInner { amount, reserved: 0 }),
            phantom: PhantomData,
        }
    }

    pub fn owner(&self) -> &T {
        &self.owner
    }

    pub fn owner_object_id(&self) -> Option<u64> {
        self.owner_object_id
    }

    /// Returns the total amount of the reservation, not accounting for anything that might be held.
    pub fn amount(&self) -> u64 {
        self.inner.lock().amount
    }

    /// Adds more to the reservation.
    pub fn add(&self, amount: u64) {
        self.inner.lock().amount += amount;
    }

    /// Returns the entire amount of the reservation.  The caller is responsible for maintaining
    /// consistency, i.e. updating counters, etc, and there can be no sub-reservations (an assert
    /// will fire otherwise).
    pub fn forget(&self) -> u64 {
        let mut inner = self.inner.lock();
        assert_eq!(inner.reserved, 0);
        std::mem::take(&mut inner.amount)
    }

    /// Takes some of the reservation.  The caller is responsible for maintaining consistency,
    /// i.e. updating counters, etc.  This will assert that the amount being forgotten does not
    /// exceed the available reservation amount; the caller should ensure that this is the case.
    pub fn forget_some(&self, amount: u64) {
        let mut inner = self.inner.lock();
        inner.amount -= amount;
        assert!(inner.reserved <= inner.amount);
    }

    /// Returns a partial amount of the reservation. |amount| is passed the |limit| and should
    /// return the amount, which can be zero.
    fn reserve_with(&self, amount: impl FnOnce(u64) -> u64) -> ReservationImpl<&Self, Self> {
        let mut inner = self.inner.lock();
        let taken = amount(inner.amount - inner.reserved);
        inner.reserved += taken;
        ReservationImpl::new(self, self.owner_object_id, taken)
    }

    /// Reserves *exactly* amount if possible.
    pub fn reserve(&self, amount: u64) -> Option<ReservationImpl<&Self, Self>> {
        let mut inner = self.inner.lock();
        if inner.amount - inner.reserved < amount {
            None
        } else {
            inner.reserved += amount;
            Some(ReservationImpl::new(self, self.owner_object_id, amount))
        }
    }

    /// Commits a previously reserved amount from this reservation.  The caller is responsible for
    /// ensuring the amount was reserved.
    pub fn commit(&self, amount: u64) {
        let mut inner = self.inner.lock();
        inner.reserved -= amount;
        inner.amount -= amount;
    }

    /// Returns some of the reservation.
    pub fn give_back(&self, amount: u64) {
        self.owner.borrow().release_reservation(self.owner_object_id, amount);
        let mut inner = self.inner.lock();
        inner.amount -= amount;
        assert!(inner.reserved <= inner.amount);
    }

    /// Moves `amount` from this reservation to another reservation.
    pub fn move_to<V: Borrow<W> + Clone + Send + Sync, W: ReservationOwner + ?Sized>(
        &self,
        other: &ReservationImpl<V, W>,
        amount: u64,
    ) {
        assert_eq!(self.owner_object_id, other.owner_object_id());
        let mut inner = self.inner.lock();
        if let Some(amount) = inner.amount.checked_sub(amount) {
            inner.amount = amount;
        } else {
            std::mem::drop(inner);
            panic!("Insufficient reservation space");
        }
        other.add(amount);
    }
}

impl<T: Borrow<U>, U: ReservationOwner + ?Sized> Drop for ReservationImpl<T, U> {
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        assert_eq!(inner.reserved, 0);
        let owner_object_id = self.owner_object_id;
        if inner.amount > 0 {
            self.owner
                .borrow()
                .release_reservation(owner_object_id, std::mem::take(&mut inner.amount));
        }
    }
}

impl<T: Borrow<U> + Send + Sync, U: ReservationOwner + ?Sized> ReservationOwner
    for ReservationImpl<T, U>
{
    fn release_reservation(&self, owner_object_id: Option<u64>, amount: u64) {
        // Sub-reservations should belong to the same owner (or lack thereof).
        assert_eq!(owner_object_id, self.owner_object_id);
        let mut inner = self.inner.lock();
        assert!(inner.reserved >= amount, "{} >= {}", inner.reserved, amount);
        inner.reserved -= amount;
    }
}

pub type Reservation = ReservationImpl<Arc<dyn ReservationOwner>, dyn ReservationOwner>;

pub type Hold<'a> = ReservationImpl<&'a Reservation, Reservation>;

/// Our allocator implementation tracks extents with a reference count.  At time of writing, these
/// reference counts should never exceed 1, but that might change with snapshots and clones.
pub type AllocatorKey = AllocatorKeyV32;

#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    TypeFingerprint,
    SerializeKey,
    Versioned,
)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct AllocatorKeyV32 {
    pub device_range: Extent,
}

impl SortByU64 for AllocatorKey {
    fn get_leading_u64(&self) -> u64 {
        self.device_range.end / crate::object_store::extent::MIN_BLOCK_SIZE
    }
}

impl FuzzyHash for AllocatorKey {
    fn fuzzy_hash(&self) -> impl ExactSizeIterator<Item = u64> {
        // The allocator only uses FullScan and FullRange queries, which never consult bloom
        // filters.
        std::iter::empty()
    }

    fn is_range_key(&self) -> bool {
        true
    }
}

impl AllocatorKey {
    /// Returns a search key for `merge_into` that finds any touching predecessor to coalesce with.
    ///
    /// Extents in the tree are sorted by `cmp_upper_bound` as `(end, -start)`:
    ///
    /// ```text
    /// Tree:       [  Predecessor  ) [ Key to insert )
    /// Offsets:    X              10                20
    ///
    /// Search key:                 | (10..10)
    /// ```
    ///
    /// Searching with `10..10` has end=10, len=0. Because shorter extents sort earlier when
    /// ends match, `10..10` sorts before any valid extent `X..10` (len > 0). This positions the
    /// iterator at `X..10` so it can be merged with `10..20`.
    pub fn lower_bound_for_merge_into(&self) -> AllocatorKey {
        AllocatorKey { device_range: self.device_range.key_for_merge_into() }
    }
}

impl LayerKey for AllocatorKey {
    fn merge_type(&self) -> MergeType {
        MergeType::OptimizedMerge
    }

    fn next_key(&self) -> Option<Self> {
        Some(Self { device_range: Extent::search_key_from_offset(self.device_range.end) })
    }

    fn search_key(&self) -> Option<Self> {
        Some(Self { device_range: self.device_range.search_key() })
    }

    fn is_search_key(&self) -> bool {
        self.device_range.is_search_key()
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.device_range.overlaps(&other.device_range)
    }
}

impl OrdUpperBound for AllocatorKey {
    fn cmp_upper_bound(&self, other: &AllocatorKey) -> std::cmp::Ordering {
        // Defer to cmp_upper_bound ordering provided by Extent type which
        // uses (end, len) to order ranges.
        self.device_range.cmp_upper_bound(&other.device_range)
    }
}

impl OrdLowerBound for AllocatorKey {
    fn cmp_lower_bound(&self, other: &AllocatorKey) -> std::cmp::Ordering {
        // The ordering over range.end is significant here as it is used in
        // the heap ordering that feeds into our merge function and
        // a total ordering over range lets us remove a symmetry case from
        // the allocator merge function.
        self.device_range.cmp(&other.device_range)
    }
}

/// Allocations are "owned" by a single ObjectStore and are reference counted
/// (for future snapshot/clone support).
pub type AllocatorValue = AllocatorValueV32;
impl Value for AllocatorValue {
    const DELETED_MARKER: Self = Self::None;
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TypeFingerprint, Versioned)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum AllocatorValueV32 {
    // Tombstone variant indicating an extent is no longer allocated.
    None,
    // Used when we know there are no possible allocations below us in the stack.
    // This is currently all the time. We used to have a related Delta type but
    // it has been removed due to correctness issues (https://fxbug.dev/42179428).
    Abs { count: u64, owner_object_id: u64 },
}

pub type AllocatorItem = Item<AllocatorKey, AllocatorValue>;

/// Serialized information about the allocator.
pub type AllocatorInfo = AllocatorInfoV32;

#[derive(Debug, Default, Clone, Deserialize, Serialize, TypeFingerprint, Versioned)]
pub struct AllocatorInfoV32 {
    /// Holds the set of layer file object_id for the LSM tree (newest first).
    pub layers: Vec<u64>,
    /// Maps from owner_object_id to bytes allocated.
    pub allocated_bytes: BTreeMap<u64, u64>,
    /// Set of owner_object_id that we should ignore if found in layer files.  For now, this should
    /// always be empty on-disk because we always do full compactions.
    pub marked_for_deletion: HashSet<u64>,
    /// The limit for the number of allocates bytes per `owner_object_id` whereas the value. If
    /// there is no limit present here for an `owner_object_id` assume it is max u64.
    pub limit_bytes: BTreeMap<u64, u64>,
}

const MAX_ALLOCATOR_INFO_SERIALIZED_SIZE: usize = 131_072;

/// Computes the target maximum extent size based on the block size of the allocator.
pub fn max_extent_size_for_block_size(block_size: BlockSize) -> u64 {
    // Each block in an extent contains an 8-byte checksum (which due to varint encoding is 9
    // bytes), and a given extent record must be no larger DEFAULT_MAX_SERIALIZED_RECORD_SIZE.  We
    // also need to leave a bit of room (arbitrarily, 64 bytes) for the rest of the extent's
    // metadata.
    block_size * ((DEFAULT_MAX_SERIALIZED_RECORD_SIZE - 64) / 9)
}

#[derive(Default)]
struct AllocatorCounters {
    num_flushes: u64,
    last_flush_time: Option<std::time::SystemTime>,
}

pub struct Allocator {
    filesystem: Weak<FxFilesystem>,
    block_size: BlockSize,
    device_size: u64,
    object_id: u64,
    max_extent_size_bytes: u64,
    tree: LSMTree<AllocatorKey, AllocatorValue>,
    // A list of allocations which are temporary; i.e. they are not actually stored in the LSM tree,
    // but we still don't want to be able to allocate over them.  This layer is merged into the
    // allocator's LSM tree whilst reading it, so the allocations appear to exist in the LSM tree.
    // This is used in a few places, for example to hold back allocations that have been added to a
    // transaction but are not yet committed yet, or to prevent the allocation of a deleted extent
    // until the device is flushed.
    temporary_allocations: Arc<SkipListLayer<AllocatorKey, AllocatorValue>>,
    inner: Mutex<Inner>,
    allocation_mutex: futures::lock::Mutex<()>,
    counters: Mutex<AllocatorCounters>,
    maximum_offset: AtomicU64,
    allocations_allowed: AtomicBool,
}

/// Tracks the different stages of byte allocations for an individual owner.
#[derive(Debug, Default, PartialEq)]
struct ByteTracking {
    /// This value is the up-to-date count of the number of allocated bytes per owner_object_id
    /// whereas the value in `Info::allocated_bytes` is the value as it was when we last flushed.
    /// This field should be regarded as *untrusted*; it can be invalid due to filesystem
    /// inconsistencies, and this is why it is `Saturating<u64>` rather than just `u64`.  Any
    /// computations that use this value should typically propagate `Saturating` so that it is clear
    /// the value is *untrusted*.
    allocated_bytes: Saturating<u64>,

    /// This value is the number of bytes allocated to uncommitted allocations.
    /// (Bytes allocated, but not yet persisted to disk)
    uncommitted_allocated_bytes: u64,

    /// This value is the number of bytes allocated to reservations.
    reserved_bytes: u64,

    /// Committed deallocations that we cannot use until they are flushed to the device.  Each entry
    /// in this list is the log file offset at which it was committed and an array of deallocations
    /// that occurred at that time.
    committed_deallocated_bytes: u64,
}

impl ByteTracking {
    // Returns the total number of bytes that are taken either from reservations, allocations or
    // uncommitted allocations.
    fn used_bytes(&self) -> Saturating<u64> {
        self.allocated_bytes + Saturating(self.uncommitted_allocated_bytes + self.reserved_bytes)
    }

    // Returns the amount that is not available to be allocated, which includes actually allocated
    // bytes, bytes that have been allocated for a transaction but the transaction hasn't committed
    // yet, and bytes that have been deallocated, but the device hasn't been flushed yet so we can't
    // reuse those bytes yet.
    fn unavailable_bytes(&self) -> Saturating<u64> {
        self.allocated_bytes
            + Saturating(self.uncommitted_allocated_bytes)
            + Saturating(self.committed_deallocated_bytes)
    }

    // Like `unavailable_bytes`, but treats as available the bytes which have been deallocated and
    // require a device flush to be reused.
    fn unavailable_after_sync_bytes(&self) -> Saturating<u64> {
        self.allocated_bytes + Saturating(self.uncommitted_allocated_bytes)
    }
}

#[derive(Debug)]
struct CommittedDeallocation {
    // The offset at which this deallocation was committed.
    log_file_offset: u64,
    // The device range being deallocated.
    range: Range<u64>,
    // The owning object id which originally allocated it.
    owner_object_id: u64,
}

/// Tracks the allocator-side lifecycle of an active or draining [`Preallocation`].
#[derive(Debug)]
struct PreallocationState {
    range: Range<u64>,
    owner_object_id: u64,
    /// Number of uncommitted `Allocate` mutations carved from `range`.
    in_flight_takes: usize,
    /// Set once `Preallocation` has been dropped; the `temporary_allocations` entry is queued for
    /// erasure once `in_flight_takes` also reaches 0.
    dropped: bool,
}

struct Inner {
    info: AllocatorInfo,

    /// The allocator can only be opened if there have been no allocations and it has not already
    /// been opened or initialized.
    opened: bool,

    /// When we allocate a range from RAM, we add it to Allocator::temporary_allocations.
    /// This layer is added to layer_set in rebuild_strategy and take_for trim so we don't assume
    /// the range is available.
    /// If we apply the mutation, we move it from `temporary_allocations` to the LSMTree.
    /// If we drop the mutation, we just delete it from `temporary_allocations` and call `free`.
    ///
    /// We need to be very careful about races when we move the allocation into the LSMTree.
    /// A layer_set is a snapshot in time of a set of layers. Say:
    ///  1. `rebuild_strategy` takes a layer_set that includes the mutable layer.
    ///  2. A compaction operation seals that mutable layer and creates a new mutable layer.
    ///     Note that the layer_set in rebuild_strategy doesn't have this new mutable layer.
    ///  3. An apply_mutation operation adds the allocation to the new mutable layer.
    ///     It then removes the allocation from `temporary_allocations`.
    ///  4. `rebuild_strategy` iterates the layer_set, missing both the temporary mutation AND
    ///     the record in the new mutable layer, making the allocated range available for double
    ///     allocation and future filesystem corruption.
    ///
    /// We don't have (or want) a means to lock compctions during rebuild_strategy operations so
    /// to avoid this scenario, we can't remove from temporary_allocations when we apply a mutation.
    /// Instead we add them to dropped_temporary_allocations and make the actual removals from
    /// `temporary_allocations` while holding the allocator lock. Because rebuild_strategy only
    /// runs with this lock held, this guarantees that we don't remove entries from temporary
    /// mutations while also iterating over it.
    ///
    /// A related issue happens when we deallocate a range. We use temporary_allocations this time
    /// to prevent reuse until after the deallocation has been successfully flushed.
    /// In this scenario we don't want to call 'free()' until the range is ready to use again.
    dropped_temporary_allocations: Vec<Range<u64>>,

    /// Active and draining [`Preallocation`] entries, sorted by `range.start`.  A single entry
    /// stays in `temporary_allocations` for the full `range` until both the `Preallocation` has
    /// been dropped and every `Allocate` mutation carved from it has been applied or dropped, so
    /// sub-range `Allocate` mutations check this list to avoid touching `temporary_allocations`.
    preallocations: Vec<PreallocationState>,

    /// The per-owner counters for bytes at various stages of the data life-cycle. From initial
    /// reservation through until the bytes are unallocated and eventually uncommitted.
    owner_bytes: BTreeMap<u64, ByteTracking>,

    /// This value is the number of bytes allocated to reservations but not tracked as part of a
    /// particular volume.
    unattributed_reserved_bytes: u64,

    /// Committed deallocations that we cannot use until they are flushed to the device.
    committed_deallocated: VecDeque<CommittedDeallocation>,

    /// Bytes which are currently being trimmed.  These can still be allocated from, but the
    /// allocation will block until the current batch of trimming is finished.
    trim_reserved_bytes: u64,

    /// While a trim is being performed, this listener is set.  When the current batch of extents
    /// being trimmed have been released (and trim_reserved_bytes is 0), this is signaled.
    /// This should only be listened to while the allocation_mutex is held.
    trim_listener: Option<EventListener>,

    /// This controls how we allocate our free space to manage fragmentation.
    strategy: strategy::BestFit,

    /// Tracks the number of allocations of size 1,2,...63,>=64.
    allocation_size_histogram: [u64; 64],
    /// Tracks which size bucket triggers rebuild_strategy.
    rebuild_strategy_trigger_histogram: [u64; 64],

    /// This is distinct from the set contained in `info`.  New entries are inserted *after* a
    /// device has been flushed (it is not safe to reuse the space taken by a deleted volume prior
    /// to this) and are removed after a major compaction.
    marked_for_deletion: HashSet<u64>,

    /// The set of volumes deleted that are waiting for a sync.
    volumes_deleted_pending_sync: HashSet<u64>,
}

impl Inner {
    fn allocated_bytes(&self) -> Saturating<u64> {
        let mut total = Saturating(0);
        for (_, bytes) in &self.owner_bytes {
            total += bytes.allocated_bytes;
        }
        total
    }

    fn uncommitted_allocated_bytes(&self) -> u64 {
        self.owner_bytes.values().map(|x| &x.uncommitted_allocated_bytes).sum()
    }

    fn reserved_bytes(&self) -> u64 {
        self.owner_bytes.values().map(|x| &x.reserved_bytes).sum::<u64>()
            + self.unattributed_reserved_bytes
    }

    fn owner_id_limit_bytes(&self, owner_object_id: u64) -> u64 {
        match self.info.limit_bytes.get(&owner_object_id) {
            Some(v) => *v,
            None => u64::MAX,
        }
    }

    fn owner_id_bytes_left(&self, owner_object_id: u64) -> u64 {
        let limit = self.owner_id_limit_bytes(owner_object_id);
        let used = self.owner_bytes.get(&owner_object_id).map_or(Saturating(0), |b| b.used_bytes());
        (Saturating(limit) - used).0
    }

    // Returns the amount that is not available to be allocated, which includes actually allocated
    // bytes, bytes that have been allocated for a transaction but the transaction hasn't committed
    // yet, and bytes that have been deallocated, but the device hasn't been flushed yet so we can't
    // reuse those bytes yet.
    fn unavailable_bytes(&self) -> Saturating<u64> {
        let mut total = Saturating(0);
        for (_, bytes) in &self.owner_bytes {
            total += bytes.unavailable_bytes();
        }
        total
    }

    // Returns the total number of bytes that are taken either from reservations, allocations or
    // uncommitted allocations.
    fn used_bytes(&self) -> Saturating<u64> {
        let mut total = Saturating(0);
        for (_, bytes) in &self.owner_bytes {
            total += bytes.used_bytes();
        }
        total + Saturating(self.unattributed_reserved_bytes)
    }

    // Like `unavailable_bytes`, but treats as available the bytes which have been deallocated and
    // require a device flush to be reused.
    fn unavailable_after_sync_bytes(&self) -> Saturating<u64> {
        let mut total = Saturating(0);
        for (_, bytes) in &self.owner_bytes {
            total += bytes.unavailable_after_sync_bytes();
        }
        total
    }

    // Returns the number of bytes which are not tied up in allocations or the current batch of
    // trimming (treating deallocated bytes pending a sync as available).
    fn bytes_available_not_being_trimmed(&self, device_size: u64) -> Result<u64, Error> {
        device_size
            .checked_sub(
                (self.unavailable_after_sync_bytes() + Saturating(self.trim_reserved_bytes)).0,
            )
            .ok_or_else(|| anyhow!(FxfsError::Inconsistent))
    }

    fn add_reservation(&mut self, owner_object_id: Option<u64>, amount: u64) {
        match owner_object_id {
            Some(owner) => self.owner_bytes.entry(owner).or_default().reserved_bytes += amount,
            None => self.unattributed_reserved_bytes += amount,
        };
    }

    /// Returns the index in `self.preallocations` whose `range` encloses `sub_range`, or `None` if
    /// `sub_range` does not belong to any active or draining [`Preallocation`].
    fn find_preallocation(&self, sub_range: &Range<u64>) -> Option<usize> {
        let idx = self.preallocations.partition_point(|p| p.range.end <= sub_range.start);
        let entry = self.preallocations.get(idx)?;
        if sub_range.start >= entry.range.start && sub_range.end <= entry.range.end {
            Some(idx)
        } else {
            debug_assert!(
                sub_range.end <= entry.range.start,
                "allocation {sub_range:?} straddles preallocation {:?}",
                entry.range
            );
            None
        }
    }

    /// Records that one in-flight `Allocate` mutation carved from `self.preallocations[idx]` has
    /// been applied or dropped, queueing `temporary_allocations` cleanup once the [`Preallocation`]
    /// itself has also been dropped and no in-flight takes remain.
    fn finish_preallocation_take(&mut self, idx: usize) {
        let entry = &mut self.preallocations[idx];
        entry.in_flight_takes -= 1;
        if entry.dropped && entry.in_flight_takes == 0 {
            let entry = self.preallocations.remove(idx);
            self.dropped_temporary_allocations.push(entry.range);
        }
    }

    fn remove_reservation(&mut self, owner_object_id: Option<u64>, amount: u64) {
        match owner_object_id {
            Some(owner) => {
                let owner_entry = self.owner_bytes.entry(owner).or_default();
                assert!(
                    owner_entry.reserved_bytes >= amount,
                    "{} >= {}",
                    owner_entry.reserved_bytes,
                    amount
                );
                owner_entry.reserved_bytes -= amount;
            }
            None => {
                assert!(
                    self.unattributed_reserved_bytes >= amount,
                    "{} >= {}",
                    self.unattributed_reserved_bytes,
                    amount
                );
                self.unattributed_reserved_bytes -= amount
            }
        };
    }
}

/// A container for a set of extents which are known to be free and can be trimmed.  Returned by
/// `take_for_trimming`.
pub struct TrimmableExtents<'a> {
    allocator: &'a Allocator,
    extents: Vec<Range<u64>>,
    // The allocator can subscribe to this event to wait until these extents are dropped.  This way,
    // we don't fail an allocation attempt if blocks are tied up for trimming; rather, we just wait
    // until the batch is finished with and then proceed.
    _drop_event: DropEvent,
}

impl<'a> TrimmableExtents<'a> {
    pub fn extents(&self) -> &Vec<Range<u64>> {
        &self.extents
    }

    // Also returns an EventListener which is signaled when this is dropped.
    fn new(allocator: &'a Allocator) -> (Self, EventListener) {
        let drop_event = DropEvent::new();
        let listener = drop_event.listen();
        (Self { allocator, extents: vec![], _drop_event: drop_event }, listener)
    }

    fn add_extent(&mut self, extent: Range<u64>) {
        self.extents.push(extent);
    }
}

impl<'a> Drop for TrimmableExtents<'a> {
    fn drop(&mut self) {
        let mut inner = self.allocator.inner.lock();
        for device_range in std::mem::take(&mut self.extents) {
            inner.strategy.free(device_range.clone()).expect("drop trim extent");
            self.allocator
                .temporary_allocations
                .erase(&AllocatorKey { device_range: Extent(device_range) });
        }
        inner.trim_reserved_bytes = 0;
    }
}

/// A single contiguous device range preallocated in memory via [`Allocator::preallocate`].
///
/// The range is tracked in `temporary_allocations` and covered by a byte reservation for
/// `owner_object_id`, without writing anything to the journal.  Callers carve block-aligned chunks
/// off the front with [`Preallocation::take`], which records normal `AllocatorMutation::Allocate`
/// mutations on `transaction`.  Dropping `Preallocation` releases any unused tail back to the
/// in-memory strategy and returns its reservation.
///
/// Callers must not deallocate any range taken from a `Preallocation` while the `Preallocation`
/// (or any uncommitted transaction that took from it) is still alive.
pub struct Preallocation {
    allocator: Arc<Allocator>,
    owner_object_id: u64,
    range: Range<u64>,
    cursor: u64,
}

impl std::fmt::Debug for Preallocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preallocation")
            .field("owner_object_id", &self.owner_object_id)
            .field("range", &self.range)
            .field("cursor", &self.cursor)
            .finish()
    }
}

impl Preallocation {
    /// Returns the remaining bytes available to [`Self::take`].
    pub fn remaining(&self) -> u64 {
        self.range.end - self.cursor
    }

    /// Returns true if no bytes remain in this preallocation.
    pub fn is_empty(&self) -> bool {
        self.cursor >= self.range.end
    }

    /// Carves up to `len` block-aligned bytes off the front of the preallocation and adds an
    /// `AllocatorMutation::Allocate` for them to `transaction`.  Returns `None` if `len == 0` or
    /// the preallocation is empty.
    ///
    /// `transaction` must not carry an explicit allocator reservation (`ReservationOptions::Hold`
    /// or `Borrowed`), since `Preallocation` already holds the byte reservation for
    /// `owner_object_id`.
    pub fn take(&mut self, transaction: &mut Transaction<'_>, len: u64) -> Option<Range<u64>> {
        assert!(self.allocator.block_size.is_aligned(len));
        if len == 0 || self.is_empty() {
            return None;
        }
        assert!(
            transaction.allocator_reservation(self.owner_object_id).is_none(),
            "Preallocation::take cannot be used with a transaction carrying its own reservation"
        );
        let take_len = std::cmp::min(len, self.remaining());
        let taken = self.cursor..self.cursor + take_len;
        self.cursor = taken.end;
        {
            let mut inner = self.allocator.inner.lock();
            let idx = inner
                .find_preallocation(&taken)
                .expect("Preallocation missing from allocator preallocations");
            debug_assert_eq!(inner.preallocations[idx].owner_object_id, self.owner_object_id);
            inner.preallocations[idx].in_flight_takes += 1;
            inner.remove_reservation(Some(self.owner_object_id), take_len);
            inner
                .owner_bytes
                .entry(self.owner_object_id)
                .or_default()
                .uncommitted_allocated_bytes += take_len;
        }
        let mutation = AllocatorMutation::Allocate {
            device_range: taken.clone().into(),
            owner_object_id: self.owner_object_id,
        };
        assert!(
            transaction.add(self.allocator.object_id(), Mutation::Allocator(mutation)).is_none()
        );
        Some(taken)
    }
}

impl Drop for Preallocation {
    fn drop(&mut self) {
        if self.range.start >= self.range.end {
            return;
        }
        let unused = self.cursor..self.range.end;
        let unused_len = unused.end - unused.start;
        let mut inner = self.allocator.inner.lock();
        if unused_len > 0 {
            inner.strategy.free(unused).expect("drop preallocation");
            inner.remove_reservation(Some(self.owner_object_id), unused_len);
        }
        let full = self.range.clone();
        let idx = inner
            .preallocations
            .binary_search_by_key(&full.start, |p| p.range.start)
            .expect("Preallocation missing from allocator preallocations");
        if inner.preallocations[idx].in_flight_takes == 0 {
            inner.preallocations.remove(idx);
            // Defer removal from `temporary_allocations` until `allocation_mutex` is held.
            inner.dropped_temporary_allocations.push(full);
        } else {
            inner.preallocations[idx].dropped = true;
        }
    }
}

impl Allocator {
    pub fn new(filesystem: Arc<FxFilesystem>, object_id: u64) -> Allocator {
        let block_size = filesystem.block_size();
        // We expect device size to be a multiple of block size. Throw away any tail.
        let device_size = block_size.align_down(filesystem.device().size());
        if device_size != filesystem.device().size() {
            warn!("Device size is not block aligned. Rounding down.");
        }
        let max_extent_size_bytes = max_extent_size_for_block_size(block_size);
        let mut strategy = strategy::BestFit::default();
        strategy.free(0..device_size).expect("new fs");
        Allocator {
            filesystem: Arc::downgrade(&filesystem),
            block_size,
            device_size,
            object_id,
            max_extent_size_bytes,
            tree: LSMTree::new(merge, None),
            temporary_allocations: SkipListLayer::new(1024),
            inner: Mutex::new(Inner {
                info: AllocatorInfo::default(),
                opened: false,
                dropped_temporary_allocations: Vec::new(),
                preallocations: Vec::new(),
                owner_bytes: BTreeMap::new(),
                unattributed_reserved_bytes: 0,
                committed_deallocated: VecDeque::new(),
                trim_reserved_bytes: 0,
                trim_listener: None,
                strategy,
                allocation_size_histogram: [0; 64],
                rebuild_strategy_trigger_histogram: [0; 64],
                marked_for_deletion: HashSet::new(),
                volumes_deleted_pending_sync: HashSet::new(),
            }),
            allocation_mutex: futures::lock::Mutex::new(()),
            counters: Mutex::new(AllocatorCounters::default()),
            maximum_offset: AtomicU64::new(0),
            allocations_allowed: AtomicBool::new(filesystem.options().image_builder_mode.is_none()),
        }
    }

    pub fn tree(&self) -> &LSMTree<AllocatorKey, AllocatorValue> {
        &self.tree
    }

    /// Enables allocations.
    /// This is only valid to call if the allocator was created in image builder mode.
    pub fn enable_allocations(&self) {
        self.allocations_allowed.store(true, Ordering::SeqCst);
    }

    /// Returns an iterator that yields all allocations, filtering out tombstones and any
    /// owner_object_id that have been marked as deleted.  If `committed_marked_for_deletion` is
    /// true, then filter using the committed volumes marked for deletion rather than the in-memory
    /// copy which excludes volumes that have been deleted but there hasn't been a sync yet.
    pub async fn filter<'a>(
        &self,
        iter: impl LayerIterator<AllocatorKey, AllocatorValue> + 'a,
        committed_marked_for_deletion: bool,
    ) -> Result<impl LayerIterator<AllocatorKey, AllocatorValue> + 'a, Error> {
        let marked_for_deletion = {
            let inner = self.inner.lock();
            if committed_marked_for_deletion {
                &inner.info.marked_for_deletion
            } else {
                &inner.marked_for_deletion
            }
            .clone()
        };
        let iter =
            filter_marked_for_deletion(filter_tombstones(iter).await?, marked_for_deletion).await?;
        Ok(iter)
    }

    /// A histogram of allocation request sizes.
    /// The index into the array is 'number of blocks'.
    /// The last bucket is a catch-all for larger allocation requests.
    pub fn allocation_size_histogram(&self) -> [u64; 64] {
        self.inner.lock().allocation_size_histogram
    }

    /// Creates a new (empty) allocator.
    pub async fn create(&self, transaction: &mut Transaction<'_>) -> Result<(), Error> {
        // Mark the allocator as opened before creating the file because creating a new
        // transaction requires a reservation.
        assert_eq!(std::mem::replace(&mut self.inner.lock().opened, true), false);

        let filesystem = self.filesystem.upgrade().unwrap();
        let root_store = filesystem.root_store();
        ObjectStore::create_object_with_id(
            &root_store,
            transaction,
            ReservedId::new(&root_store, NonZero::new(self.object_id()).unwrap()),
            HandleOptions::default(),
            None,
        )?;
        root_store.update_last_object_id(self.object_id());
        Ok(())
    }

    // Opens the allocator.  This is not thread-safe; this should be called prior to
    // replaying the allocator's mutations.
    pub async fn open(self: &Arc<Self>) -> Result<(), Error> {
        let filesystem = self.filesystem.upgrade().unwrap();
        let root_store = filesystem.root_store();

        self.inner.lock().strategy = strategy::BestFit::default();

        let handle =
            ObjectStore::open_object(&root_store, self.object_id, HandleOptions::default(), None)
                .await
                .context("Failed to open allocator object")?;

        if handle.get_size() > 0 {
            let serialized_info = handle
                .contents(MAX_ALLOCATOR_INFO_SERIALIZED_SIZE)
                .await
                .context("Failed to read AllocatorInfo")?;
            let mut cursor = std::io::Cursor::new(serialized_info);
            let (info, _version) = AllocatorInfo::deserialize_with_version(&mut cursor)
                .context("Failed to deserialize AllocatorInfo")?;

            let (layers, total_size) = open_layers(&root_store, info.layers.iter().cloned(), None)
                .await
                .context("Failed to open allocator layer file")?;

            {
                let mut inner = self.inner.lock();

                // Check allocated_bytes fits within the device.
                let mut device_bytes = self.device_size;
                for (&owner_object_id, &bytes) in &info.allocated_bytes {
                    ensure!(
                        bytes <= device_bytes,
                        anyhow!(FxfsError::Inconsistent).context(format!(
                            "Allocated bytes exceeds device size: {:?}",
                            info.allocated_bytes
                        ))
                    );
                    device_bytes -= bytes;

                    inner.owner_bytes.entry(owner_object_id).or_default().allocated_bytes =
                        Saturating(bytes);
                }

                inner.info = info;
            }

            self.tree.append_open_layers(layers);
            self.filesystem
                .upgrade()
                .unwrap()
                .object_manager()
                .update_layer_size(self.object_id, total_size);
        }

        Ok(())
    }

    pub async fn on_replay_complete(self: &Arc<Self>) -> Result<(), Error> {
        // When opened writable, the device is flushed before any writes.
        {
            let mut inner = self.inner.lock();
            inner.volumes_deleted_pending_sync.clear();
            inner.marked_for_deletion = inner.info.marked_for_deletion.clone();
        }

        // Build free extent structure from disk.
        if !self.rebuild_strategy().await.context("Build free extents")? {
            info!("Device contains no free space.");
        }

        assert_eq!(std::mem::replace(&mut self.inner.lock().opened, true), false);
        Ok(())
    }

    /// Returns an error if the in-memory free extents are empty, which is only authoritative right
    /// after they have been rebuilt (e.g. after replay).  For now, we expect writable filesystems
    /// to have *some* free space at all times.
    pub fn ensure_free_space(&self) -> Result<(), Error> {
        if !self.inner.lock().strategy.has_free_space() {
            return Err(FxfsError::Inconsistent).context("Device appears to contain no free space");
        }
        Ok(())
    }

    /// Walk all allocations to generate the set of free regions between allocations.
    /// It is safe to re-run this on live filesystems but it should not be called concurrently
    /// with allocations, trims or other rebuild_strategy invocations -- use allocation_mutex.
    /// Returns true if the rebuild made changes to either the free ranges or overflow markers.
    async fn rebuild_strategy(self: &Arc<Self>) -> Result<bool, Error> {
        let mut changed = false;
        let mut layer_set = self.tree.empty_layer_set();
        layer_set.layers.push((self.temporary_allocations.clone() as Arc<dyn Layer<_, _>>).into());
        self.tree.add_all_layers_to_layer_set(&mut layer_set);

        let overflow_markers = self.inner.lock().strategy.overflow_markers();
        self.inner.lock().strategy.reset_overflow_markers();

        let mut to_add = Vec::new();
        let mut merger = layer_set.merger();
        let mut iter = self.filter(merger.query(Query::FullScan).await?, false).await?;
        let mut last_offset = 0;
        while last_offset < self.device_size {
            let next_range = match iter.get() {
                None => {
                    assert!(last_offset <= self.device_size);
                    let range = last_offset..self.device_size;
                    last_offset = self.device_size;
                    iter.advance().await?;
                    range
                }
                Some(ItemRef { key, .. }) => {
                    let device_range = &key.device_range;
                    if device_range.end < last_offset {
                        iter.advance().await?;
                        continue;
                    }
                    if device_range.start <= last_offset {
                        last_offset = device_range.end;
                        iter.advance().await?;
                        continue;
                    }
                    let range = last_offset..device_range.start;
                    last_offset = device_range.end;
                    iter.advance().await?;
                    range
                }
            };
            to_add.push(next_range);
            // Avoid taking a lock on inner for every free range.
            if to_add.len() > 100 {
                let mut inner = self.inner.lock();
                for range in to_add.drain(..) {
                    changed |= inner.strategy.force_free(range)?;
                }
            }
        }
        let mut inner = self.inner.lock();
        for range in to_add {
            changed |= inner.strategy.force_free(range)?;
        }
        if overflow_markers != inner.strategy.overflow_markers() {
            changed = true;
        }
        Ok(changed)
    }

    /// Collects up to `extents_per_batch` free extents of size up to `max_extent_size` from
    /// `offset`.  The extents will be reserved.
    /// Note that only one `FreeExtents` can exist for the allocator at any time.
    pub async fn take_for_trimming(
        &self,
        offset: u64,
        max_extent_size: usize,
        extents_per_batch: usize,
    ) -> Result<TrimmableExtents<'_>, Error> {
        let _guard = self.allocation_mutex.lock().await;

        let (mut result, listener) = TrimmableExtents::new(self);
        if offset >= self.device_size {
            return Ok(result);
        }
        let mut bytes = 0;

        // We can't just use self.strategy here because it doesn't necessarily hold all
        // free extent metadata in RAM.
        let mut layer_set = self.tree.empty_layer_set();
        layer_set.layers.push((self.temporary_allocations.clone() as Arc<dyn Layer<_, _>>).into());
        self.tree.add_all_layers_to_layer_set(&mut layer_set);
        let mut merger = layer_set.merger();
        let mut iter = self
            .filter(
                merger
                    .query(Query::FullRange(&AllocatorKey {
                        device_range: Extent::search_key_from_offset(offset),
                    }))
                    .await?,
                false,
            )
            .await?;
        let mut last_offset = offset;
        'outer: while last_offset < self.device_size {
            let mut range = match iter.get() {
                None => {
                    assert!(last_offset <= self.device_size);
                    let range = last_offset..self.device_size;
                    last_offset = self.device_size;
                    iter.advance().await?;
                    range
                }
                Some(ItemRef { key: AllocatorKey { device_range, .. }, .. }) => {
                    if device_range.end <= last_offset {
                        iter.advance().await?;
                        continue;
                    }
                    if device_range.start <= last_offset {
                        last_offset = device_range.end;
                        iter.advance().await?;
                        continue;
                    }
                    let range = last_offset..device_range.start;
                    last_offset = device_range.end;
                    iter.advance().await?;
                    range
                }
            };
            if range.start < offset {
                continue;
            }

            // 'range' is based on the on-disk LSM tree. We need to check against any uncommitted
            // allocations and remove temporarily allocate the range for ourselves.
            let mut inner = self.inner.lock();

            while range.start < range.end {
                let prefix =
                    range.start..std::cmp::min(range.start + max_extent_size as u64, range.end);
                range = prefix.end..range.end;
                bytes += prefix.length()?;
                // We can assume the following are safe because we've checked both LSM tree and
                // temporary_allocations while holding the lock.
                inner.strategy.remove(prefix.clone());
                self.temporary_allocations.insert(AllocatorItem {
                    key: AllocatorKey { device_range: Extent(prefix.clone()) },
                    value: AllocatorValue::Abs { owner_object_id: INVALID_OBJECT_ID, count: 1 },
                })?;
                result.add_extent(prefix);
                if result.extents.len() == extents_per_batch {
                    break 'outer;
                }
            }
            if result.extents.len() == extents_per_batch {
                break 'outer;
            }
        }
        {
            let mut inner = self.inner.lock();

            ensure!(inner.trim_reserved_bytes == 0, FxfsError::AlreadyBound);
            inner.trim_listener = Some(listener);
            inner.trim_reserved_bytes = bytes;
            debug_assert!(
                (Saturating(inner.trim_reserved_bytes) + inner.unavailable_bytes()).0
                    <= self.device_size
            );
        }
        Ok(result)
    }

    /// Returns all objects that exist in the parent store that pertain to this allocator.
    pub fn parent_objects(&self) -> Vec<u64> {
        // The allocator tree needs to store a file for each of the layers in the tree, so we return
        // those, since nothing else references them.
        self.inner.lock().info.layers.clone()
    }

    /// Returns all the current owner byte limits (in pairs of `(owner_object_id, bytes)`).
    pub fn owner_byte_limits(&self) -> Vec<(u64, u64)> {
        self.inner.lock().info.limit_bytes.iter().map(|(k, v)| (*k, *v)).collect()
    }

    /// Returns (allocated_bytes, byte_limit) for the given owner.
    pub fn owner_allocation_info(&self, owner_object_id: u64) -> (u64, Option<u64>) {
        let inner = self.inner.lock();
        (
            inner.owner_bytes.get(&owner_object_id).map(|b| b.used_bytes().0).unwrap_or(0u64),
            inner.info.limit_bytes.get(&owner_object_id).copied(),
        )
    }

    /// Returns owner bytes debug information.
    pub fn owner_bytes_debug(&self) -> String {
        format!("{:?}", self.inner.lock().owner_bytes)
    }

    fn needs_sync(&self) -> bool {
        // TODO(https://fxbug.dev/42178048): This will only trigger if *all* free space is taken up with
        // committed deallocated bytes, but we might want to trigger a sync if we're low and there
        // happens to be a lot of deallocated bytes as that might mean we can fully satisfy
        // allocation requests.
        let inner = self.inner.lock();
        inner.unavailable_bytes().0 >= self.device_size
    }

    fn is_system_store(&self, owner_object_id: u64) -> bool {
        self.filesystem.upgrade().unwrap().object_manager().is_system_store(owner_object_id)
    }

    /// Updates the accounting to track that a byte reservation has been moved out of an owner to
    /// the unattributed pool.
    pub fn disown_reservation(&self, old_owner_object_id: Option<u64>, amount: u64) {
        if old_owner_object_id.is_none() || amount == 0 {
            return;
        }
        // These 2 mutations should behave as though they're a single atomic mutation.
        let mut inner = self.inner.lock();
        inner.remove_reservation(old_owner_object_id, amount);
        inner.add_reservation(None, amount);
    }

    /// Creates a lazy inspect node named `str` under `parent` which will yield statistics for the
    /// allocator when queried.
    pub fn track_statistics(self: &Arc<Self>, parent: &fuchsia_inspect::Node, name: &str) {
        let this = Arc::downgrade(self);
        parent.record_lazy_child(name, move || {
            let this_clone = this.clone();
            async move {
                let inspector = fuchsia_inspect::Inspector::default();
                if let Some(this) = this_clone.upgrade() {
                    let counters = this.counters.lock();
                    let root = inspector.root();
                    root.record_uint("max_extent_size_bytes", this.max_extent_size_bytes);
                    root.record_uint("bytes_total", this.device_size);
                    let (allocated, reserved, used, unavailable) = {
                        // TODO(https://fxbug.dev/42069513): Push-back or rate-limit to prevent DoS.
                        let inner = this.inner.lock();
                        (
                            inner.allocated_bytes().0,
                            inner.reserved_bytes(),
                            inner.used_bytes().0,
                            inner.unavailable_bytes().0,
                        )
                    };
                    root.record_uint("bytes_allocated", allocated);
                    root.record_uint("bytes_reserved", reserved);
                    root.record_uint("bytes_used", used);
                    root.record_uint("bytes_unavailable", unavailable);

                    // TODO(https://fxbug.dev/42068224): Post-compute rather than manually computing
                    // metrics.
                    if let Some(x) = round_div(100 * allocated, this.device_size) {
                        root.record_uint("bytes_allocated_percent", x);
                    }
                    if let Some(x) = round_div(100 * reserved, this.device_size) {
                        root.record_uint("bytes_reserved_percent", x);
                    }
                    if let Some(x) = round_div(100 * used, this.device_size) {
                        root.record_uint("bytes_used_percent", x);
                    }
                    if let Some(x) = round_div(100 * unavailable, this.device_size) {
                        root.record_uint("bytes_unavailable_percent", x);
                    }

                    root.record_uint("num_flushes", counters.num_flushes);
                    if let Some(last_flush_time) = counters.last_flush_time.as_ref() {
                        root.record_uint(
                            "last_flush_time_ms",
                            last_flush_time
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or(std::time::Duration::ZERO)
                                .as_millis()
                                .try_into()
                                .unwrap_or(0u64),
                        );
                    }

                    let data = this.allocation_size_histogram();
                    let alloc_sizes = root.create_uint_linear_histogram(
                        "allocation_size_histogram",
                        fuchsia_inspect::LinearHistogramParams {
                            floor: 1,
                            step_size: 1,
                            buckets: 64,
                        },
                    );
                    for (i, count) in data.iter().enumerate() {
                        if i != 0 {
                            alloc_sizes.insert_multiple(i as u64, *count as usize);
                        }
                    }
                    root.record(alloc_sizes);

                    let data = this.inner.lock().rebuild_strategy_trigger_histogram;
                    let triggers = root.create_uint_linear_histogram(
                        "rebuild_strategy_triggers",
                        fuchsia_inspect::LinearHistogramParams {
                            floor: 1,
                            step_size: 1,
                            buckets: 64,
                        },
                    );
                    for (i, count) in data.iter().enumerate() {
                        if i != 0 {
                            triggers.insert_multiple(i as u64, *count as usize);
                        }
                    }
                    root.record(triggers);

                    let allocator_ref = this.clone();
                    root.record_child("lsm_tree", move |node| {
                        allocator_ref.tree.record_inspect_data(node);
                    });
                }
                Ok(inspector)
            }
            .boxed()
        });
    }

    /// Returns the offset of the first byte which has not been used by the allocator since its
    /// creation.
    /// NB: This does *not* take into account existing allocations.  This is only reliable when the
    /// allocator was created from scratch, without any pre-existing allocations.
    pub fn maximum_offset(&self) -> u64 {
        self.maximum_offset.load(Ordering::Relaxed)
    }
}

impl Drop for Allocator {
    fn drop(&mut self) {
        let inner = self.inner.lock();
        // Uncommitted and reserved should be released back using RAII, so they should be zero.
        assert_eq!(inner.uncommitted_allocated_bytes(), 0);
        assert_eq!(inner.reserved_bytes(), 0);
    }
}

#[fxfs_trace::trace]
impl Allocator {
    /// Returns the object ID for the allocator.
    pub fn object_id(&self) -> u64 {
        self.object_id
    }

    /// Returns information about the allocator such as the layer files storing persisted
    /// allocations.
    pub fn info(&self) -> AllocatorInfo {
        self.inner.lock().info.clone()
    }

    fn reservation_description(&self, reservation: &Reservation) -> String {
        let metadata_info = self.filesystem.upgrade().and_then(|fs| {
            let om = fs.object_manager();
            if om.is_metadata_reservation(reservation) {
                Some((om.borrowed_metadata_space(), om.max_store_reservation()))
            } else {
                None
            }
        });
        match metadata_info {
            Some((borrowed, max_store_reservation)) => {
                format!(
                    "reservation with {} bytes available (borrowed metadata space: {}, max store \
                    reservation: {})",
                    reservation.amount(),
                    borrowed,
                    max_store_reservation
                )
            }
            None => {
                format!("reservation with {} bytes available", reservation.amount())
            }
        }
    }

    /// Reserves up to `len` block-aligned bytes for `owner_object_id` from remaining owner/device
    /// capacity, returning `(reserved_len, reservation)`.
    fn reserve_for_owner(
        &self,
        owner_object_id: u64,
        len: u64,
    ) -> (u64, ReservationImpl<&Self, Self>) {
        let mut inner = self.inner.lock();
        assert!(inner.opened);
        // Do not exceed the limit for the owner or the device.
        let device_used = inner.used_bytes();
        let owner_bytes_left = inner.owner_id_bytes_left(owner_object_id);
        // We must take care not to use up space that might be reserved.
        let limit = std::cmp::min(owner_bytes_left, (Saturating(self.device_size) - device_used).0);
        let reserved = self.block_size.align_down(std::cmp::min(len, limit));
        inner.owner_bytes.entry(owner_object_id).or_default().reserved_bytes += reserved;
        (reserved, ReservationImpl::<_, Self>::new(self, Some(owner_object_id), reserved))
    }

    /// Takes a single contiguous run of up to `len` bytes from `strategy`, rebuilding the in-memory
    /// strategy from the LSM tree if a bucket overflowed.  `allocation_mutex` must be held by the
    /// caller (`_guard`).
    async fn allocate_from_strategy(
        self: &Arc<Self>,
        _guard: &futures::lock::MutexGuard<'_, ()>,
        len: u64,
    ) -> Result<Option<Range<u64>>, Error> {
        loop {
            {
                let mut inner = self.inner.lock();

                // While we know rebuild_strategy and take_for_trim are not running (we hold
                // `_guard`), apply deferred `temporary_allocations` removals.
                for device_range in inner.dropped_temporary_allocations.drain(..) {
                    self.temporary_allocations
                        .erase(&AllocatorKey { device_range: Extent(device_range) });
                }

                match inner.strategy.allocate(len) {
                    Err(FxfsError::NotFound) => {
                        // Overflow. Fall through and rebuild.
                        inner.rebuild_strategy_trigger_histogram
                            [std::cmp::min(63, (len / self.block_size) as usize)] += 1;
                    }
                    Err(FxfsError::NoSpace) => return Ok(None),
                    Err(err) => {
                        error!(err:%; "Likely filesystem corruption.");
                        return Err(err.into());
                    }
                    Ok(range) => return Ok(Some(range)),
                }
            }
            // We've run out of extents of the requested length in RAM but there exists more of this
            // size on device. Rescan device and circle back.  We already hold `allocation_mutex`,
            // so exclusive access is guaranteed.
            if !self.rebuild_strategy().await? {
                return Ok(None);
            }
        }
    }

    /// Tries to allocate enough space for |object_range| in the specified object and returns the
    /// device range allocated.
    /// The allocated range may be short (e.g. due to fragmentation), in which case the caller can
    /// simply call allocate again until they have enough blocks.
    ///
    /// We also store the object store ID of the store that the allocation should be assigned to so
    /// that we have a means to delete encrypted stores without needing the encryption key.
    #[trace]
    pub async fn allocate(
        self: &Arc<Self>,
        transaction: &mut Transaction<'_>,
        owner_object_id: u64,
        mut len: u64,
    ) -> Result<Range<u64>, Error> {
        ensure!(self.allocations_allowed.load(Ordering::SeqCst), FxfsError::Unavailable);
        assert!(self.block_size.is_aligned(len));
        len = std::cmp::min(len, self.max_extent_size_bytes);
        debug_assert_ne!(owner_object_id, INVALID_OBJECT_ID);

        let requested_len = len;

        // Make sure we have space reserved before we try and find the space.
        let reservation = if let Some(reservation) =
            transaction.allocator_reservation(owner_object_id)
        {
            match reservation.owner_object_id {
                // If there is no owner, this must be a system store that we're allocating for.
                None => assert!(self.is_system_store(owner_object_id)),
                // If it has an owner, it should not be different than the allocating owner.
                Some(res_owner_object_id) => assert_eq!(owner_object_id, res_owner_object_id),
            };
            // Reservation limits aren't necessarily a multiple of the block size.
            let r = reservation
                .reserve_with(|limit| std::cmp::min(len, self.block_size.align_down(limit)));
            len = r.amount();
            Left(r)
        } else {
            assert!(
                !transaction.metadata_reservation.is_borrowed()
                    || self.filesystem.upgrade().unwrap().options().image_builder_mode.is_some(),
                "Allocations for non-system stores are not allowed in Borrowed transactions"
            );
            let (reserved_len, r) = self.reserve_for_owner(owner_object_id, len);
            len = reserved_len;
            Right(r)
        };

        if len == 0 {
            if let Some(reservation) = transaction.allocator_reservation(owner_object_id) {
                bail!(anyhow!(FxfsError::NoSpace).context(format!(
                    "Failed to allocate {} bytes for owner {} from {}",
                    requested_len,
                    owner_object_id,
                    self.reservation_description(reservation),
                )));
            } else {
                let inner = self.inner.lock();
                bail!(anyhow!(FxfsError::NoSpace).context(format!(
                    "Failed to allocate {} bytes for owner {} (owner_bytes_left: {}, device_used: \
                    {}, device_size: {})",
                    requested_len,
                    owner_object_id,
                    inner.owner_id_bytes_left(owner_object_id),
                    inner.used_bytes(),
                    self.device_size
                )));
            }
        }

        // If volumes have been deleted, flush the device so that we can use any of the freed space.
        let volumes_deleted = {
            let inner = self.inner.lock();
            (!inner.volumes_deleted_pending_sync.is_empty())
                .then(|| inner.volumes_deleted_pending_sync.clone())
        };

        if let Some(volumes_deleted) = volumes_deleted {
            // No locks are held here, so in theory, there could be unnecessary syncs, but it
            // should be sufficiently rare that it won't matter.
            self.filesystem
                .upgrade()
                .unwrap()
                .sync(SyncOptions {
                    flush_device: true,
                    precondition: Some(Box::new(|| {
                        !self.inner.lock().volumes_deleted_pending_sync.is_empty()
                    })),
                    ..Default::default()
                })
                .await?;

            {
                let mut inner = self.inner.lock();
                for owner_id in volumes_deleted {
                    inner.volumes_deleted_pending_sync.remove(&owner_id);
                    inner.marked_for_deletion.insert(owner_id);
                }
            }

            let _guard = self.allocation_mutex.lock().await;
            self.rebuild_strategy().await?;
        }

        #[allow(clippy::never_loop)] // Loop used as a for {} else {}.
        let guard = 'sync: loop {
            // Cap number of sync attempts before giving up on finding free space.
            for _ in 0..10 {
                {
                    let guard = self.allocation_mutex.lock().await;

                    if !self.needs_sync() {
                        break 'sync guard;
                    }
                }

                // All the free space is currently tied up with deallocations, so we need to sync
                // and flush the device to free that up.
                //
                // We can't hold the allocation lock whilst we sync here because the allocation lock
                // is also taken in apply_mutations, which is called when journal locks are held,
                // and we call sync here which takes those same locks, so it would have the
                // potential to result in a deadlock.  Sync holds its own lock to guard against
                // multiple syncs occurring at the same time, and we can supply a precondition that
                // is evaluated under that lock to ensure we don't sync twice if we don't need to.
                self.filesystem
                    .upgrade()
                    .unwrap()
                    .sync(SyncOptions {
                        flush_device: true,
                        precondition: Some(Box::new(|| self.needs_sync())),
                        ..Default::default()
                    })
                    .await?;
            }
            bail!(
                anyhow!(FxfsError::NoSpace).context("Sync failed to yield sufficient free space.")
            );
        };

        let mut trim_listener = None;
        {
            let mut inner = self.inner.lock();
            inner.allocation_size_histogram[std::cmp::min(63, len / self.block_size) as usize] += 1;

            // If trimming would be the reason that this allocation gets cut short, wait for
            // trimming to complete before proceeding.
            let avail = self
                .device_size
                .checked_sub(inner.unavailable_bytes().0)
                .ok_or(FxfsError::Inconsistent)?;
            let free_and_not_being_trimmed =
                inner.bytes_available_not_being_trimmed(self.device_size)?;
            if free_and_not_being_trimmed < std::cmp::min(len, avail) {
                debug_assert!(inner.trim_reserved_bytes > 0);
                trim_listener = std::mem::take(&mut inner.trim_listener);
            }
        }

        if let Some(listener) = trim_listener {
            listener.await;
        }

        let Some(result) = self.allocate_from_strategy(&guard, len).await? else {
            error!("Cannot find additional free space. Corruption?");
            return Err(FxfsError::Inconsistent.into());
        };

        debug!(device_range:? = result; "allocate");

        let len = result.length().unwrap();
        let reservation_owner = reservation.either(
            // Left means we got an outside reservation.
            |l| {
                l.forget_some(len);
                l.owner_object_id()
            },
            |r| {
                r.forget_some(len);
                r.owner_object_id()
            },
        );

        {
            let mut inner = self.inner.lock();
            let owner_entry = inner.owner_bytes.entry(owner_object_id).or_default();
            owner_entry.uncommitted_allocated_bytes += len;
            // If the reservation has an owner, ensure they are the same.
            assert_eq!(owner_object_id, reservation_owner.unwrap_or(owner_object_id));
            inner.remove_reservation(reservation_owner, len);
            self.temporary_allocations.insert(AllocatorItem {
                key: AllocatorKey { device_range: Extent(result.clone()) },
                value: AllocatorValue::Abs { owner_object_id, count: 1 },
            })?;
        }

        let mutation =
            AllocatorMutation::Allocate { device_range: result.clone().into(), owner_object_id };
        assert!(transaction.add(self.object_id(), Mutation::Allocator(mutation)).is_none());

        Ok(result)
    }

    /// Preallocates a single contiguous extent of up to `len` bytes for `owner_object_id`.  The
    /// preallocation exists only in memory and is not immediately recorded into the journal;
    /// allocations from this preallocation only become durable once they are persisted in a
    /// transaction.
    ///
    /// The returned [`Preallocation`] may be shorter than `len` (for example due to fragmentation,
    /// `max_extent_size_bytes`, owner byte limits, or low free space), or empty if no space is
    /// currently available; unlike [`Self::allocate`], lack of space returns an empty
    /// `Preallocation` rather than `FxfsError::NoSpace` unless allocations are disallowed or the
    /// allocator detects corruption.
    #[trace]
    pub async fn preallocate(
        self: &Arc<Self>,
        owner_object_id: u64,
        mut len: u64,
    ) -> Result<Preallocation, Error> {
        ensure!(self.allocations_allowed.load(Ordering::SeqCst), FxfsError::Unavailable);
        assert!(self.block_size.is_aligned(len));
        len = std::cmp::min(len, self.max_extent_size_bytes);
        debug_assert_ne!(owner_object_id, INVALID_OBJECT_ID);

        let empty =
            || Preallocation { allocator: self.clone(), owner_object_id, range: 0..0, cursor: 0 };

        let (len, reservation) = self.reserve_for_owner(owner_object_id, len);
        if len == 0 {
            return Ok(empty());
        }

        // Avoid forcing a device sync or waiting on trim for a speculative preallocation:
        // `strategy` and `temporary_allocations` already exclude uncommitted deallocations and
        // extents being trimmed, so `allocate_from_strategy` only returns immediately usable
        // space.
        let guard = self.allocation_mutex.lock().await;
        if self.needs_sync() {
            return Ok(empty());
        }

        let Some(result) = self.allocate_from_strategy(&guard, len).await? else {
            return Ok(empty());
        };

        let preallocated_len = result.length().unwrap();
        {
            let mut inner = self.inner.lock();
            self.temporary_allocations.insert(AllocatorItem {
                key: AllocatorKey { device_range: Extent(result.clone()) },
                value: AllocatorValue::Abs { owner_object_id, count: 1 },
            })?;
            let idx = inner
                .preallocations
                .binary_search_by_key(&result.start, |p| p.range.start)
                .expect_err("duplicate preallocation range start");
            inner.preallocations.insert(
                idx,
                PreallocationState {
                    range: result.clone(),
                    owner_object_id,
                    in_flight_takes: 0,
                    dropped: false,
                },
            );
        }

        // Keep `preallocated_len` reserved for `owner_object_id` now that `Preallocation` creation
        // cannot fail; `reservation`'s Drop returns any excess.
        reservation.forget_some(preallocated_len);
        std::mem::drop(reservation);

        debug!(device_range:? = result; "preallocate");
        Ok(Preallocation {
            allocator: self.clone(),
            owner_object_id,
            cursor: result.start,
            range: result,
        })
    }

    /// Marks the given device range as allocated.  The main use case for this at this time is for
    /// the super-block which needs to be at a fixed location on the device.
    #[trace]
    pub fn mark_allocated(
        &self,
        transaction: &mut Transaction<'_>,
        owner_object_id: u64,
        device_range: Range<u64>,
    ) -> Result<(), Error> {
        debug_assert_ne!(owner_object_id, INVALID_OBJECT_ID);
        {
            let len = device_range.length().map_err(|_| FxfsError::InvalidArgs)?;
            let reservation = transaction.allocator_reservation(owner_object_id);

            let mut inner = self.inner.lock();
            let device_used = inner.used_bytes();
            let owner_id_bytes_left = inner.owner_id_bytes_left(owner_object_id);
            let owner_entry = inner.owner_bytes.entry(owner_object_id).or_default();
            ensure!(
                device_range.end <= self.device_size
                    && (Saturating(self.device_size) - device_used).0 >= len
                    && owner_id_bytes_left >= len,
                anyhow!(FxfsError::NoSpace).context(format!(
                    "Failed to allocate {} bytes at {:?} for owner {} (owner_bytes_left: {}, \
                    device_used: {}, device_size: {})",
                    len,
                    device_range,
                    owner_object_id,
                    owner_id_bytes_left,
                    device_used,
                    self.device_size
                ))
            );
            if let Some(reservation) = reservation {
                // The transaction takes ownership of this hold.
                reservation
                    .reserve(len)
                    .ok_or_else(|| {
                        anyhow!(FxfsError::NoSpace).context(format!(
                            "Failed to reserve {} bytes at {:?} from {}",
                            len,
                            device_range,
                            self.reservation_description(reservation),
                        ))
                    })?
                    .forget();
            } else {
                assert!(
                    !transaction.metadata_reservation.is_borrowed()
                        || self
                            .filesystem
                            .upgrade()
                            .unwrap()
                            .options()
                            .image_builder_mode
                            .is_some(),
                    "Allocations for non-system stores are not allowed in Borrowed transactions"
                );
            }
            owner_entry.uncommitted_allocated_bytes += len;
            inner.strategy.remove(device_range.clone());
            self.temporary_allocations.insert(AllocatorItem {
                key: AllocatorKey { device_range: Extent(device_range.clone()) },
                value: AllocatorValue::Abs { owner_object_id, count: 1 },
            })?;
        }
        let mutation =
            AllocatorMutation::Allocate { device_range: device_range.into(), owner_object_id };
        transaction.add(self.object_id(), Mutation::Allocator(mutation));
        Ok(())
    }

    /// Sets the limits for an owner object in terms of usage.
    pub fn set_bytes_limit(
        &self,
        transaction: &mut Transaction<'_>,
        owner_object_id: u64,
        bytes: u64,
    ) -> Result<(), Error> {
        // System stores cannot be given limits.
        assert!(!self.is_system_store(owner_object_id));
        transaction.add(
            self.object_id(),
            Mutation::Allocator(AllocatorMutation::SetLimit { owner_object_id, bytes }),
        );
        Ok(())
    }

    /// Gets the bytes limit for an owner object.
    pub fn get_owner_bytes_limit(&self, owner_object_id: u64) -> Option<u64> {
        self.inner.lock().info.limit_bytes.get(&owner_object_id).copied()
    }

    /// Gets the number of bytes currently used by allocations, reservations or uncommitted
    /// allocations.
    pub fn get_owner_bytes_used(&self, owner_object_id: u64) -> u64 {
        self.inner.lock().owner_bytes.get(&owner_object_id).map_or(0, |info| info.used_bytes().0)
    }

    /// Deallocates the given device range for the specified object.
    #[trace]
    pub async fn deallocate(
        &self,
        transaction: &mut Transaction<'_>,
        owner_object_id: u64,
        dealloc_range: Range<u64>,
    ) -> Result<u64, Error> {
        debug!(device_range:? = dealloc_range; "deallocate");
        ensure!(dealloc_range.is_valid(), FxfsError::InvalidArgs);
        // We don't currently support sharing of allocations (value.count always equals 1), so as
        // long as we can assume the deallocated range is actually allocated, we can avoid device
        // access.
        let deallocated = dealloc_range.end - dealloc_range.start;
        let mutation = AllocatorMutation::Deallocate {
            device_range: dealloc_range.clone().into(),
            owner_object_id,
        };
        transaction.add(self.object_id(), Mutation::Allocator(mutation));

        let _guard = self.allocation_mutex.lock().await;

        // We use `dropped_temporary_allocations` to defer removals from `temporary_allocations` in
        // places where we can't execute async code or take locks.
        //
        // It's important we don't ever remove entries from `temporary_allocations` without
        // holding the `allocation_mutex` lock or else we may end up with an inconsistent view of
        // available disk space when we combine temporary_allocations with the LSMTree.
        // This is normally done in `allocate()` but we also need to apply these here because
        // `temporary_allocations` is also used to track deallocated space until it has been
        // flushed (see comment below). A user may allocate and then deallocate space before calling
        // allocate() a second time, so if we do not clean up here, we may end up with the same
        // range in temporary_allocations twice (once for allocate, once for deallocate).
        let mut inner = self.inner.lock();
        debug_assert!(
            inner.find_preallocation(&dealloc_range).is_none(),
            "deallocate {dealloc_range:?} overlaps an active Preallocation"
        );
        for device_range in inner.dropped_temporary_allocations.drain(..) {
            self.temporary_allocations.erase(&AllocatorKey { device_range: Extent(device_range) });
        }

        // We can't reuse deallocated space immediately because failure to successfully flush will
        // mean that on next mount, we may find this space is still assigned to the deallocated
        // region. To avoid immediate reuse, we hold these regions in 'temporary_allocations' until
        // after a successful flush so we know the region is safe to reuse.
        self.temporary_allocations
            .insert(AllocatorItem {
                key: AllocatorKey { device_range: Extent(dealloc_range.clone()) },
                value: AllocatorValue::Abs { owner_object_id, count: 1 },
            })
            .context("tracking deallocated")?;

        Ok(deallocated)
    }

    /// Marks allocations associated with a given |owner_object_id| for deletion.
    ///
    /// This is used as part of deleting encrypted volumes (ObjectStore) without having the keys.
    ///
    /// MarkForDeletion mutations eventually manipulates allocator metadata (AllocatorInfo) instead
    /// of the mutable layer but we must be careful not to do this too early and risk premature
    /// reuse of extents.
    ///
    /// Replay is not guaranteed until the *device* gets flushed, so we cannot reuse the deleted
    /// extents until we've flushed the device.
    ///
    /// TODO(b/316827348): Consider removing the use of mark_for_deletion in AllocatorInfo and
    /// just compacting?
    ///
    /// After an allocator.flush() (i.e. a major compaction), we know that there is no data left
    /// in the layer files for this owner_object_id and we are able to clear `marked_for_deletion`.
    pub fn mark_for_deletion(&self, transaction: &mut Transaction<'_>, owner_object_id: u64) {
        // Note that because the actual time of deletion (the next major compaction) is undefined,
        // |owner_object_id| should not be reused after this call.
        transaction.add(
            self.object_id(),
            Mutation::Allocator(AllocatorMutation::MarkForDeletion(owner_object_id)),
        );
    }

    /// Called when the device has been flushed and indicates what the journal log offset was when
    /// that happened.
    #[trace]
    pub async fn did_flush_device(&self, flush_log_offset: u64) {
        // Free in bounded batches and yield between them so that releasing many extents neither
        // holds `self.inner` for long nor barges back into it ahead of waiting tasks.
        while self.free_some_flushed_deallocations(flush_log_offset) {
            fasync::yield_now().await;
        }
    }

    /// Frees up to a batch of deallocations committed before `flush_log_offset`. Returns true if
    /// more flushed deallocations remain.
    fn free_some_flushed_deallocations(&self, flush_log_offset: u64) -> bool {
        const BATCH_SIZE: usize = 64;
        let is_flushed =
            |dealloc: &CommittedDeallocation| dealloc.log_file_offset < flush_log_offset;

        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        // The list is maintained in order, so we can stop on the first entry that we find that
        // should not be unreserved yet.
        let count = inner
            .committed_deallocated
            .iter()
            .take(BATCH_SIZE)
            .take_while(|d| is_flushed(d))
            .count();
        let mut totals = BTreeMap::<u64, u64>::new();
        for dealloc in inner.committed_deallocated.drain(..count) {
            *(totals.entry(dealloc.owner_object_id).or_default()) +=
                dealloc.range.length().unwrap();
            inner.strategy.free(dealloc.range.clone()).expect("dealloced ranges");
            self.temporary_allocations.erase(&AllocatorKey { device_range: Extent(dealloc.range) });
        }

        // This *must* come after we've removed the records from reserved reservations because the
        // allocator uses this value to decide whether or not a device-flush is required and it must
        // be possible to find free space if it thinks no device-flush is required.
        for (owner_object_id, total) in totals {
            match inner.owner_bytes.get_mut(&owner_object_id) {
                Some(counters) => counters.committed_deallocated_bytes -= total,
                None => {
                    // This should only be possible if the volume has been deleted and a sync is
                    // pending.
                    assert!(inner.volumes_deleted_pending_sync.contains(&owner_object_id));
                }
            }
        }

        inner.committed_deallocated.front().is_some_and(is_flushed)
    }

    /// Returns a reservation that can be used later, or None if there is insufficient space. The
    /// |owner_object_id| indicates which object in the root object store the reservation is for.
    pub fn reserve(
        self: Arc<Self>,
        owner_object_id: Option<u64>,
        amount: u64,
    ) -> Option<Reservation> {
        {
            let mut inner = self.inner.lock();

            let device_free = (Saturating(self.device_size) - inner.used_bytes()).0;

            let limit = match owner_object_id {
                Some(id) => std::cmp::min(inner.owner_id_bytes_left(id), device_free),
                None => device_free,
            };
            if limit < amount {
                return None;
            }
            inner.add_reservation(owner_object_id, amount);
        }
        Some(Reservation::new(self, owner_object_id, amount))
    }

    /// Like reserve, but takes a callback is passed the |limit| and should return the amount,
    /// which can be zero.
    pub fn reserve_with(
        self: Arc<Self>,
        owner_object_id: Option<u64>,
        amount: impl FnOnce(u64) -> u64,
    ) -> Reservation {
        let amount = {
            let mut inner = self.inner.lock();

            let device_free = (Saturating(self.device_size) - inner.used_bytes()).0;

            let amount = amount(match owner_object_id {
                Some(id) => std::cmp::min(inner.owner_id_bytes_left(id), device_free),
                None => device_free,
            });

            inner.add_reservation(owner_object_id, amount);

            amount
        };

        Reservation::new(self, owner_object_id, amount)
    }

    /// Returns the total number of allocated bytes.
    pub fn get_allocated_bytes(&self) -> u64 {
        self.inner.lock().allocated_bytes().0
    }

    /// Returns the size of bytes available to allocate.
    pub fn get_disk_bytes(&self) -> u64 {
        self.device_size
    }

    /// Returns the total number of allocated bytes per owner_object_id.
    /// Note that this is quite an expensive operation as it copies the collection.
    /// This is intended for use in fsck() and friends, not general use code.
    pub fn get_owner_allocated_bytes(&self) -> BTreeMap<u64, u64> {
        self.inner.lock().owner_bytes.iter().map(|(k, v)| (*k, v.allocated_bytes.0)).collect()
    }

    /// Returns the number of allocated and reserved bytes.
    pub fn get_used_bytes(&self) -> Saturating<u64> {
        let inner = self.inner.lock();
        inner.used_bytes()
    }

    pub async fn flush(&self) -> Result<Version, Error> {
        let filesystem = self.filesystem.upgrade().unwrap();
        let object_manager = filesystem.object_manager();
        let earliest_version = self.tree.get_earliest_version();
        if !object_manager.needs_flush(self.object_id()) && earliest_version == LATEST_VERSION {
            // Early exit, but still return the earliest version used by a struct in the tree
            return Ok(earliest_version);
        }

        let fs = self.filesystem.upgrade().unwrap();
        let mut flusher = Flusher::new(self, &fs).await;
        let (new_layer_file, info) = flusher.start().await?;
        flusher.finish(new_layer_file, info).await
    }
}

impl ReservationOwner for Allocator {
    fn release_reservation(&self, owner_object_id: Option<u64>, amount: u64) {
        self.inner.lock().remove_reservation(owner_object_id, amount);
    }
}

#[async_trait]
impl JournalingObject for Allocator {
    fn apply_mutation(
        &self,
        mutation: Mutation,
        context: &ApplyContext<'_, '_>,
        _assoc_obj: AssocObj<'_>,
    ) -> Result<(), Error> {
        match mutation {
            Mutation::Allocator(AllocatorMutation::MarkForDeletion(owner_object_id)) => {
                let mut inner = self.inner.lock();
                inner.owner_bytes.remove(&owner_object_id);

                // We use `info.marked_for_deletion` to track the committed state and
                // `inner.marked_for_deletion` to track volumes marked for deletion *after* we have
                // flushed the device.  It is not safe to use extents belonging to deleted volumes
                // until after we have flushed the device.
                inner.info.marked_for_deletion.insert(owner_object_id);
                inner.volumes_deleted_pending_sync.insert(owner_object_id);

                inner.info.limit_bytes.remove(&owner_object_id);
            }
            Mutation::Allocator(AllocatorMutation::Allocate { device_range, owner_object_id }) => {
                self.maximum_offset.fetch_max(device_range.end, Ordering::Relaxed);
                let item = AllocatorItem {
                    key: AllocatorKey { device_range: Extent(device_range.0.clone()) },
                    value: AllocatorValue::Abs { count: 1, owner_object_id },
                };
                let len = item.key.device_range.length().unwrap();
                let lower_bound = item.key.lower_bound_for_merge_into();
                self.tree.merge_into(item, &lower_bound);
                let reservation = if let ApplyMode::Live(transaction) = context.mode {
                    transaction.allocator_reservation(owner_object_id)
                } else {
                    None
                };
                let mut inner = self.inner.lock();
                let entry = inner.owner_bytes.entry(owner_object_id).or_default();
                entry.allocated_bytes += len;
                if context.mode.is_live() {
                    entry.uncommitted_allocated_bytes -= len;
                    if let Some(idx) = inner.find_preallocation(&device_range.0) {
                        // The full preallocated range remains in `temporary_allocations` until the
                        // `Preallocation` is dropped and all of its takes have resolved.
                        inner.finish_preallocation_take(idx);
                    } else {
                        // Note that we cannot drop entries from temporary_allocations without
                        // holding the allocation_mutex as it may introduce races. We instead add
                        // the range to a Vec that can be applied later when we hold the lock (See
                        // comment on `dropped_temporary_allocations` above).
                        inner.dropped_temporary_allocations.push(device_range.0);
                    }
                    if let Some(reservation) = reservation {
                        reservation.commit(len);
                    }
                }
            }
            Mutation::Allocator(AllocatorMutation::Deallocate {
                device_range,
                owner_object_id,
            }) => {
                let item = AllocatorItem {
                    key: AllocatorKey { device_range: Extent(device_range.0) },
                    value: AllocatorValue::None,
                };
                let len = item.key.device_range.length().unwrap();
                let reservation = if let ApplyMode::Live(transaction) = context.mode {
                    transaction.allocator_reservation(owner_object_id)
                } else {
                    None
                };

                {
                    let mut inner = self.inner.lock();
                    {
                        let entry = inner.owner_bytes.entry(owner_object_id).or_default();
                        entry.allocated_bytes -= len;
                        if context.mode.is_live() {
                            entry.committed_deallocated_bytes += len;
                        }
                    }
                    if context.mode.is_live() {
                        inner.committed_deallocated.push_back(CommittedDeallocation {
                            log_file_offset: context.checkpoint.file_offset,
                            range: (*item.key.device_range).clone(),
                            owner_object_id,
                        });
                    }
                    if let Some(reservation) = reservation {
                        inner.add_reservation(reservation.owner_object_id(), len);
                        reservation.add(len);
                    }
                }
                let lower_bound = item.key.lower_bound_for_merge_into();
                self.tree.merge_into(item, &lower_bound);
            }
            Mutation::Allocator(AllocatorMutation::SetLimit { owner_object_id, bytes }) => {
                // Journal replay is ordered and each of these calls is idempotent. So the last one
                // will be respected, it doesn't matter if the value is already set, or gets changed
                // multiple times during replay. When it gets opened it will be merged in with the
                // snapshot.
                self.inner.lock().info.limit_bytes.insert(owner_object_id, bytes);
            }
            Mutation::BeginFlush => {
                self.tree.seal();
                // Transfer our running count for allocated_bytes so that it gets written to the new
                // info file when flush completes.
                let mut inner = self.inner.lock();
                let allocated_bytes =
                    inner.owner_bytes.iter().map(|(k, v)| (*k, v.allocated_bytes.0)).collect();
                inner.info.allocated_bytes = allocated_bytes;
            }
            Mutation::EndFlush(_) => {}
            _ => bail!("unexpected mutation: {:?}", mutation),
        }
        Ok(())
    }

    fn drop_mutation(&self, mutation: Mutation, transaction: &Transaction<'_>) {
        match mutation {
            Mutation::Allocator(AllocatorMutation::Allocate { device_range, owner_object_id }) => {
                let len = device_range.length().unwrap();
                let reservation = transaction.allocator_reservation(owner_object_id);
                let mut inner = self.inner.lock();
                inner
                    .owner_bytes
                    .entry(owner_object_id)
                    .or_default()
                    .uncommitted_allocated_bytes -= len;
                if let Some(idx) = inner.find_preallocation(&device_range.0) {
                    // `Preallocation::Drop` frees only `[cursor, end)`, so dropping a taken
                    // transaction must free `device_range` itself while leaving the single
                    // `temporary_allocations` entry intact until the preallocation (and all its
                    // takes) finish.
                    debug_assert!(reservation.is_none());
                    inner.strategy.free(device_range.0.clone()).expect("drop preallocation take");
                    inner.finish_preallocation_take(idx);
                } else {
                    if let Some(reservation) = reservation {
                        let res_owner = reservation.owner_object_id();
                        inner.add_reservation(res_owner, len);
                        reservation.release_reservation(res_owner, len);
                    }
                    inner.strategy.free(device_range.0.clone()).expect("drop mutaton");
                    self.temporary_allocations
                        .erase(&AllocatorKey { device_range: Extent(device_range.0) });
                }
            }
            Mutation::Allocator(AllocatorMutation::Deallocate { device_range, .. }) => {
                self.temporary_allocations
                    .erase(&AllocatorKey { device_range: Extent(device_range.0) });
            }
            _ => {}
        }
    }

    async fn flush(&self, _reason: crate::filesystem::FlushReason) -> Result<Version, Error> {
        self.flush().await
    }
}

impl Coalesce<AllocatorValue> for AllocatorKey {
    fn may_coalesce(_item: ItemRef<'_, Self, AllocatorValue>) -> bool {
        true
    }

    fn coalesce(
        left: ItemRef<'_, Self, AllocatorValue>,
        right: ItemRef<'_, Self, AllocatorValue>,
    ) -> Result<Option<Item<Self, AllocatorValue>>, Error> {
        // Adjacent records from a merged stream cannot overlap.
        ensure!(left.key.device_range.end <= right.key.device_range.start, FxfsError::Inconsistent);
        // We can only coalesce records if they are touching and have the same value.
        if left.key.device_range.end < right.key.device_range.start || left.value != right.value {
            return Ok(None);
        }
        Ok(Some(Item::new(
            AllocatorKey {
                device_range: Extent(left.key.device_range.start..right.key.device_range.end),
            },
            left.value.clone(),
        )))
    }
}

struct Flusher<'a> {
    allocator: &'a Allocator,
    fs: &'a Arc<FxFilesystem>,
    _guard: WriteGuard<'a>,
}

impl<'a> Flusher<'a> {
    async fn new(allocator: &'a Allocator, fs: &'a Arc<FxFilesystem>) -> Self {
        let keys = lock_keys![LockKey::flush(allocator.object_id())];
        Self { allocator, fs, _guard: fs.lock_manager().write_lock(keys).await }
    }

    fn txn_options() -> Options<'static> {
        Options {
            skip_journal_checks: true,
            reservation: ReservationOptions::Borrowed,
            ..Default::default()
        }
    }

    async fn start(&mut self) -> Result<(DataObjectHandle<ObjectStore>, AllocatorInfo), Error> {
        let root_store = self.fs.root_store();
        let mut transaction = root_store.new_transaction(lock_keys![], Self::txn_options()).await?;
        let layer_object_handle = ObjectStore::create_object(
            &root_store,
            &mut transaction,
            HandleOptions { skip_journal_checks: true, ..Default::default() },
            None,
        )
        .await?;
        root_store.add_to_graveyard(&mut transaction, layer_object_handle.object_id());
        // It's important that this transaction does not include any allocations because we use
        // BeginFlush as a snapshot point for mutations to the tree: other allocator mutations
        // within this transaction might get applied before seal (which would be OK), but they could
        // equally get applied afterwards (since Transaction makes no guarantees about the order in
        // which mutations are applied whilst committing), in which case they'd get lost on replay
        // because the journal will only send mutations that follow this transaction.
        transaction.add(self.allocator.object_id(), Mutation::BeginFlush);
        let info = transaction
            .commit_with_callback(|_| {
                // We must capture `info` as it is when we start flushing. Subsequent transactions
                // can end up modifying `info` and we shouldn't capture those here.
                self.allocator.inner.lock().info.clone()
            })
            .await?;
        Ok((layer_object_handle, info))
    }

    async fn finish(
        self,
        layer_object_handle: DataObjectHandle<ObjectStore>,
        mut info: AllocatorInfo,
    ) -> Result<Version, Error> {
        let txn_options = Self::txn_options();

        let layer_set = self.allocator.tree.immutable_layer_set();
        {
            let start_time = std::time::Instant::now();
            let merged_layer_count = layer_set.layers.len();
            let mut merger = layer_set.merger();
            let iter = self.allocator.filter(merger.query(Query::FullScan).await?, true).await?;
            let iter = CoalescingIterator::new(iter).await?;
            // The allocator never consults bloom filters, so omit them.
            let bytes_written = compact_with_iterator(
                iter,
                None,
                DirectWriter::new(&layer_object_handle, txn_options).await,
                layer_object_handle.block_size(),
                Some(self.fs.journal().get_compaction_yielder()),
            )
            .await?;

            self.allocator.tree.report_compaction_metrics(
                bytes_written,
                start_time.elapsed(),
                merged_layer_count,
            );
        }

        let root_store = self.fs.root_store();

        // Both of these forward-declared variables need to outlive the transaction.
        let object_handle;
        let reservation_update;
        let mut transaction = root_store
            .new_transaction(
                lock_keys![LockKey::object(
                    root_store.store_object_id(),
                    self.allocator.object_id()
                )],
                txn_options,
            )
            .await?;
        let mut serialized_info = Vec::new();

        debug!(oid = layer_object_handle.object_id(); "new allocator layer file");
        object_handle = ObjectStore::open_object(
            &root_store,
            self.allocator.object_id(),
            HandleOptions::default(),
            None,
        )
        .await?;

        // Move all the existing layers to the graveyard.
        for object_id in &info.layers {
            root_store.add_to_graveyard(&mut transaction, *object_id);
        }

        // Write out updated info.

        // After successfully flushing, all the stores that were marked for deletion at the time of
        // the BeginFlush transaction, no longer need to be marked for deletion.  There can be
        // stores that have been deleted since the BeginFlush transaction, but they will be covered
        // by a MarkForDeletion mutation.
        let marked_for_deletion = std::mem::take(&mut info.marked_for_deletion);

        info.layers = vec![layer_object_handle.object_id()];

        info.serialize_with_version(&mut serialized_info)?;

        let mut buf = object_handle.allocate_buffer(serialized_info.len()).await;
        buf.copy_from_slice(&serialized_info);
        object_handle.txn_write(&mut transaction, 0u64, buf.as_ref()).await?;

        reservation_update = ReservationUpdate::new(layer_object_handle.get_size());

        // It's important that EndFlush is in the same transaction that we write AllocatorInfo,
        // because we use EndFlush to make the required adjustments to allocated_bytes.
        transaction.add_with_object(
            self.allocator.object_id(),
            Mutation::EndFlush(0),
            AssocObj::Borrowed(&reservation_update),
        );
        root_store.remove_from_graveyard(&mut transaction, layer_object_handle.object_id());

        let layers = vec![layer_from_handle(layer_object_handle, None).await?];
        transaction
            .commit_with_callback(|_| {
                self.allocator.tree.set_layers(layers);

                // At this point we've committed the new layers to disk so we can start using them.
                // This means we can also switch to the new AllocatorInfo which clears
                // marked_for_deletion.
                let mut inner = self.allocator.inner.lock();
                inner.info.layers = info.layers;
                for owner_id in marked_for_deletion {
                    inner.marked_for_deletion.remove(&owner_id);
                    inner.info.marked_for_deletion.remove(&owner_id);
                }
            })
            .await?;

        // Now close the layers and purge them.
        for layer in layer_set.layers {
            let object_id = layer.handle().map(|h| h.object_id());
            layer.close_layer().await;
            if let Some(object_id) = object_id {
                root_store.tombstone_object(object_id, txn_options, None).await?;
            }
        }

        let mut counters = self.allocator.counters.lock();
        counters.num_flushes += 1;
        counters.last_flush_time = Some(std::time::SystemTime::now());
        // Return the earliest version used by a struct in the tree
        Ok(self.allocator.tree.get_earliest_version())
    }
}

#[cfg(test)]
mod tests {
    use crate::filesystem::{FxFilesystem, FxFilesystemBuilder, OpenFxFilesystem, SyncOptions};
    use crate::fsck::fsck;
    use crate::lsm_tree::skip_list_layer::SkipListLayer;
    use crate::lsm_tree::types::{
        CoalescingIterator, FuzzyHash as _, Item, ItemRef, LayerIterator, LayerKey as _,
    };
    use crate::lsm_tree::{
        BloomFilterOptions, LSMTree, Query, compact_with_iterator, layers_from_handles,
    };
    use crate::object_handle::{ObjectHandle, WriteObjectHandle};
    use crate::object_store::allocator::merge::merge;
    use crate::object_store::allocator::{Allocator, AllocatorKey, AllocatorValue};
    use crate::object_store::extent::MIN_BLOCK_SIZE;
    use crate::object_store::journal::CompactionYielder;
    use crate::object_store::transaction::{
        Options, ReservationOptions, TRANSACTION_METADATA_MAX_AMOUNT, lock_keys,
    };
    use crate::object_store::volume::root_volume;
    use crate::object_store::{
        Directory, FxfsError, HandleOptions, LockKey, NewChildStoreOptions, ObjectStore,
        StoreOptions,
    };
    use crate::range::RangeExt;
    use crate::serialized_types::{LATEST_VERSION, Versioned};
    use crate::testing::fake_object::{FakeObject, FakeObjectHandle};
    use crate::testing::writer::Writer;
    use crate::testing::{self};
    use anyhow::bail;
    use bincode::Options as _;
    use fuchsia_async as fasync;
    use fuchsia_sync::Mutex;
    use futures::poll;
    use std::cmp::{max, min};
    use std::ops::{Bound, Range};
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use storage_device::DeviceHolder;
    use storage_device::fake_device::{FakeDevice, Op};

    #[test]
    fn test_allocator_key_is_range_based() {
        // Make sure we disallow using allocator keys with point queries.
        assert!(AllocatorKey { device_range: (0..100).into() }.is_range_key());
    }

    #[fuchsia::test]
    async fn test_allocator_layers_have_no_bloom_filter() {
        let key = AllocatorKey { device_range: (0..512).into() };
        let mut iter = key.fuzzy_hash();
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);

        let (fs, allocator) = test_fs().await;
        allocator.flush().await.expect("flush failed");
        let legacy_layer_object = Arc::new(FakeObject::new());
        {
            let layer_set = allocator.tree().immutable_layer_set();
            assert!(!layer_set.layers.is_empty());
            for layer in &layer_set.layers {
                assert!(!layer.has_bloom_filter());
            }

            // Simulate a pre-existing on-disk allocator layer file written with a bloom filter
            // before this change and verify that `fsck` still succeeds.
            let legacy_handle =
                FakeObjectHandle::new_with_block_size(legacy_layer_object.clone(), fs.block_size());
            let mut merger = layer_set.merger();
            let iter = merger.query(Query::FullScan).await.expect("query failed");
            compact_with_iterator(
                iter,
                Some(BloomFilterOptions { num_items: layer_set.sum_len() }),
                Writer::new(&legacy_handle).await,
                fs.block_size(),
                None::<CompactionYielder<'_>>,
            )
            .await
            .expect("compact_with_iterator failed");
        }

        let legacy_layers = layers_from_handles([FakeObjectHandle::new_with_block_size(
            legacy_layer_object,
            fs.block_size(),
        )])
        .await
        .expect("layers_from_handles failed");
        assert!(legacy_layers[0].has_bloom_filter());
        allocator.tree().set_layers(legacy_layers);
        fsck(fs.clone()).await.expect("fsck failed");

        fs.close().await.expect("close failed");
    }

    #[test]
    fn test_allocator_key_search_key() {
        let key = AllocatorKey { device_range: (MIN_BLOCK_SIZE.get()..3 * MIN_BLOCK_SIZE).into() };
        assert!(!key.is_search_key());
        let search_key = key.search_key().unwrap();
        assert!(search_key.is_search_key());
        assert_eq!(search_key.device_range, (MIN_BLOCK_SIZE.get()..2 * MIN_BLOCK_SIZE).into());
    }

    #[test]
    fn test_allocator_key_serialization_compatibility() {
        let range = 1024..2048;
        let key = AllocatorKey { device_range: range.clone().into() };

        // 1. Serialize the range directly using bincode (which is what the manual
        //    Versioned implementation did)
        let options = bincode::DefaultOptions::new().allow_trailing_bytes();
        let expected_bytes = options.serialize(&range).unwrap();

        // 2. Serialize AllocatorKey via our derived Versioned implementation
        let mut key_bytes = Vec::new();
        key.serialize_into(&mut key_bytes).unwrap();

        // Verify they are byte-compatible
        assert_eq!(key_bytes, expected_bytes);

        // 3. Deserialize a Range<u64> as AllocatorKey via our derived Versioned
        //    implementation
        let deserialized_key =
            AllocatorKey::deserialize_from(&mut &expected_bytes[..], LATEST_VERSION).unwrap();
        assert_eq!(deserialized_key, key);
    }

    #[fuchsia::test]
    async fn test_coalescing_iterator() {
        let skip_list = SkipListLayer::new(100);
        let items = [
            Item::new(
                AllocatorKey { device_range: (0..100 * 512).into() },
                AllocatorValue::Abs { count: 1, owner_object_id: 99 },
            ),
            Item::new(
                AllocatorKey { device_range: (100 * 512..200 * 512).into() },
                AllocatorValue::Abs { count: 1, owner_object_id: 99 },
            ),
        ];
        skip_list.insert(items[1].clone()).expect("insert error");
        skip_list.insert(items[0].clone()).expect("insert error");
        let mut iter =
            CoalescingIterator::new(skip_list.seek(Bound::Unbounded)).await.expect("new failed");
        let ItemRef { key, value, .. } = iter.get().expect("get failed");
        assert_eq!(
            (key, value),
            (
                &AllocatorKey { device_range: (0..200 * 512).into() },
                &AllocatorValue::Abs { count: 1, owner_object_id: 99 }
            )
        );
        iter.advance().await.expect("advance failed");
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_merge_and_coalesce_across_three_layers() {
        let lsm_tree = LSMTree::new(merge, None);
        lsm_tree
            .insert(Item::new(
                AllocatorKey { device_range: (100 * 512..200 * 512).into() },
                AllocatorValue::Abs { count: 1, owner_object_id: 99 },
            ))
            .expect("insert error");
        lsm_tree.seal();
        lsm_tree
            .insert(Item::new(
                AllocatorKey { device_range: (0..100 * 512).into() },
                AllocatorValue::Abs { count: 1, owner_object_id: 99 },
            ))
            .expect("insert error");

        let layer_set = lsm_tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter =
            CoalescingIterator::new(merger.query(Query::FullScan).await.expect("seek failed"))
                .await
                .expect("new failed");
        let ItemRef { key, value, .. } = iter.get().expect("get failed");
        assert_eq!(
            (key, value),
            (
                &AllocatorKey { device_range: (0..200 * 512).into() },
                &AllocatorValue::Abs { count: 1, owner_object_id: 99 }
            )
        );
        iter.advance().await.expect("advance failed");
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_merge_and_coalesce_wont_merge_across_object_id() {
        let lsm_tree = LSMTree::new(merge, None);
        lsm_tree
            .insert(Item::new(
                AllocatorKey { device_range: (100 * 512..200 * 512).into() },
                AllocatorValue::Abs { count: 1, owner_object_id: 99 },
            ))
            .expect("insert error");
        lsm_tree.seal();
        lsm_tree
            .insert(Item::new(
                AllocatorKey { device_range: (0..100 * 512).into() },
                AllocatorValue::Abs { count: 1, owner_object_id: 98 },
            ))
            .expect("insert error");

        let layer_set = lsm_tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter =
            CoalescingIterator::new(merger.query(Query::FullScan).await.expect("seek failed"))
                .await
                .expect("new failed");
        let ItemRef { key, value, .. } = iter.get().expect("get failed");
        assert_eq!(
            (key, value),
            (
                &AllocatorKey { device_range: (0..100 * 512).into() },
                &AllocatorValue::Abs { count: 1, owner_object_id: 98 },
            )
        );
        iter.advance().await.expect("advance failed");
        let ItemRef { key, value, .. } = iter.get().expect("get failed");
        assert_eq!(
            (key, value),
            (
                &AllocatorKey { device_range: (100 * 512..200 * 512).into() },
                &AllocatorValue::Abs { count: 1, owner_object_id: 99 }
            )
        );
        iter.advance().await.expect("advance failed");
        assert!(iter.get().is_none());
    }

    fn overlap(a: &Range<u64>, b: &Range<u64>) -> u64 {
        if a.end > b.start && a.start < b.end {
            min(a.end, b.end) - max(a.start, b.start)
        } else {
            0
        }
    }

    async fn collect_allocations(allocator: &Allocator) -> Vec<Range<u64>> {
        let layer_set = allocator.tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = CoalescingIterator::new(
            allocator
                .filter(merger.query(Query::FullScan).await.expect("seek failed"), false)
                .await
                .expect("build iterator"),
        )
        .await
        .expect("coalesce failed");
        let mut allocations: Vec<Range<u64>> = Vec::new();
        while let Some(ItemRef { key: AllocatorKey { device_range }, .. }) = iter.get() {
            if let Some(r) = allocations.last() {
                assert!(device_range.start >= r.end);
            }
            allocations.push(device_range.clone().into());
            iter.advance().await.expect("advance failed");
        }
        allocations
    }

    async fn check_allocations(allocator: &Allocator, expected_allocations: &[Range<u64>]) {
        let layer_set = allocator.tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = allocator
            .filter(merger.query(Query::FullScan).await.expect("seek failed"), false)
            .await
            .expect("build iterator");
        let mut found = 0;
        while let Some(ItemRef { key: AllocatorKey { device_range }, .. }) = iter.get() {
            let mut l = device_range.length().expect("Invalid range");
            found += l;
            // Make sure that the entire range we have found completely overlaps with all the
            // allocations we expect to find.
            for range in expected_allocations {
                l -= overlap(range, device_range);
                if l == 0 {
                    break;
                }
            }
            assert_eq!(l, 0, "range {:?} not covered by expectations", device_range);
            iter.advance().await.expect("advance failed");
        }
        // Make sure the total we found adds up to what we expect.
        assert_eq!(found, expected_allocations.iter().map(|r| r.length().unwrap()).sum::<u64>());
    }

    async fn test_fs() -> (OpenFxFilesystem, Arc<Allocator>) {
        let device = DeviceHolder::new(FakeDevice::new(4096, 4096));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let allocator = fs.allocator();
        (fs, allocator)
    }

    #[fuchsia::test]
    async fn test_allocations() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        let mut device_ranges = collect_allocations(&allocator).await;

        // Expected extents:
        let expected = vec![
            0..135168,      // Superblock A (4k) + Superblock A & B extensions (64k each)
            135168..401408, // Initial Journal (128k), StoreInfo (4k), and Journal extension (128k)
            524288..528384, // Superblock B (4k)
        ];
        assert_eq!(device_ranges, expected);
        let first_new = device_ranges.len();
        device_ranges.push(
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                .await
                .expect("allocate failed"),
        );
        assert_eq!(device_ranges.last().unwrap().length().expect("Invalid range"), fs.block_size());
        device_ranges.push(
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                .await
                .expect("allocate failed"),
        );
        assert_eq!(device_ranges.last().unwrap().length().expect("Invalid range"), fs.block_size());
        assert_eq!(overlap(&device_ranges[0], &device_ranges[1]), 0);
        transaction.commit().await.expect("commit failed");
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        device_ranges.push(
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                .await
                .expect("allocate failed"),
        );
        assert_eq!(device_ranges[first_new + 2].length().expect("Invalid range"), fs.block_size());
        assert_eq!(overlap(&device_ranges[first_new], &device_ranges[first_new + 2]), 0);
        assert_eq!(overlap(&device_ranges[first_new + 1], &device_ranges[first_new + 2]), 0);
        transaction.commit().await.expect("commit failed");

        check_allocations(&allocator, &device_ranges).await;
    }

    #[fuchsia::test]
    async fn test_allocate_more_than_max_size() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        let mut device_ranges = collect_allocations(&allocator).await;
        device_ranges.push(
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, fs.device().size())
                .await
                .expect("allocate failed"),
        );
        assert_eq!(
            device_ranges.last().unwrap().length().expect("Invalid range"),
            allocator.max_extent_size_bytes
        );
        transaction.commit().await.expect("commit failed");

        check_allocations(&allocator, &device_ranges).await;
    }

    #[fuchsia::test]
    async fn test_deallocations() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let initial_allocations = collect_allocations(&allocator).await;

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        let device_range1 = allocator
            .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
            .await
            .expect("allocate failed");
        assert_eq!(device_range1.length().expect("Invalid range"), fs.block_size());
        transaction.commit().await.expect("commit failed");

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        allocator
            .deallocate(&mut transaction, STORE_OBJECT_ID, device_range1)
            .await
            .expect("deallocate failed");
        transaction.commit().await.expect("commit failed");

        check_allocations(&allocator, &initial_allocations).await;
    }

    #[fuchsia::test]
    async fn test_mark_allocated() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let mut device_ranges = collect_allocations(&allocator).await;
        let range = {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new failed");
            // First, allocate 2 blocks.
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, 2 * fs.block_size())
                .await
                .expect("allocate failed")
            // Let the transaction drop.
        };

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");

        // If we allocate 1 block, the two blocks that were allocated earlier should be available,
        // and this should return the first of them.
        device_ranges.push(
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                .await
                .expect("allocate failed"),
        );

        assert_eq!(device_ranges.last().unwrap().start, range.start);

        // Mark the second block as allocated.
        let mut range2 = range.clone();
        range2.start += fs.block_size();
        allocator
            .mark_allocated(&mut transaction, STORE_OBJECT_ID, range2.clone())
            .expect("mark_allocated failed");
        device_ranges.push(range2);

        // This should avoid the range we marked as allocated.
        device_ranges.push(
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                .await
                .expect("allocate failed"),
        );
        let last_range = device_ranges.last().unwrap();
        assert_eq!(last_range.length().expect("Invalid range"), fs.block_size());
        assert_eq!(overlap(last_range, &range), 0);
        transaction.commit().await.expect("commit failed");

        check_allocations(&allocator, &device_ranges).await;
    }

    #[fuchsia::test]
    async fn test_mark_for_deletion() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;

        // Allocate some stuff.
        let initial_allocated_bytes = allocator.get_allocated_bytes();
        let mut device_ranges = collect_allocations(&allocator).await;
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        // Note we have a cap on individual allocation length so we allocate over multiple mutation.
        for _ in 0..15 {
            device_ranges.push(
                allocator
                    .allocate(&mut transaction, STORE_OBJECT_ID, 100 * fs.block_size())
                    .await
                    .expect("allocate failed"),
            );
            device_ranges.push(
                allocator
                    .allocate(&mut transaction, STORE_OBJECT_ID, 100 * fs.block_size())
                    .await
                    .expect("allocate2 failed"),
            );
        }
        transaction.commit().await.expect("commit failed");
        check_allocations(&allocator, &device_ranges).await;

        assert_eq!(
            allocator.get_allocated_bytes(),
            initial_allocated_bytes + fs.block_size() * 3000
        );

        // Mark for deletion.
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        allocator.mark_for_deletion(&mut transaction, STORE_OBJECT_ID);
        transaction.commit().await.expect("commit failed");

        // Expect that allocated bytes is updated immediately but device ranges are still allocated.
        assert_eq!(allocator.get_allocated_bytes(), initial_allocated_bytes);
        check_allocations(&allocator, &device_ranges).await;

        // Allocate more space than we have until we deallocate the mark_for_deletion space.
        // This should force a flush on allocate(). (1500 * 3 > test_fs size of 4096 blocks).
        device_ranges.clear();

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        let target_bytes = 1500 * fs.block_size();
        while device_ranges.iter().map(|x| x.length().unwrap()).sum::<u64>() != target_bytes {
            let len = std::cmp::min(
                target_bytes - device_ranges.iter().map(|x| x.length().unwrap()).sum::<u64>(),
                100 * fs.block_size(),
            );
            device_ranges.push(
                allocator.allocate(&mut transaction, 100, len).await.expect("allocate failed"),
            );
        }
        transaction.commit().await.expect("commit failed");

        // Have the deleted ranges cleaned up.
        allocator.flush().await.expect("flush failed");

        // The flush above seems to trigger an allocation for the allocator itself.
        // We will just check that we have the right size for the owner we care about.

        assert!(!allocator.get_owner_allocated_bytes().contains_key(&STORE_OBJECT_ID));
        assert_eq!(*allocator.get_owner_allocated_bytes().get(&100).unwrap(), target_bytes,);
    }

    async fn create_file(store: &Arc<ObjectStore>, size: usize) {
        let root_directory =
            Directory::open(store, store.root_directory_object_id()).await.expect("open failed");

        let mut transaction = store
            .filesystem()
            .ensure_local()
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(
                    store.store_object_id(),
                    store.root_directory_object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let file = root_directory
            .create_child_file(&mut transaction, &format!("foo {}", size))
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        let buffer = file.allocate_buffer(size).await;

        // Append some data to it.
        const CHUNK_SIZE: usize = 1_048_576;
        for offset in (0..size).step_by(CHUNK_SIZE) {
            let len = std::cmp::min(CHUNK_SIZE, size - offset);
            let mut transaction = file.new_transaction().await.expect("new_transaction failed");
            file.txn_write(&mut transaction, offset as u64, buffer.subslice(offset..offset + len))
                .await
                .expect("txn_write failed");
            transaction.commit().await.expect("commit failed");
        }
    }

    #[fuchsia::test]
    async fn test_replay_with_deleted_store_and_compaction() {
        let (fs, _) = test_fs().await;

        const FILE_SIZE: usize = 10_000_000;

        let mut store_id = {
            let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
            let store = root_vol
                .new_volume("vol", NewChildStoreOptions::default())
                .await
                .expect("new_volume failed");

            create_file(&store, FILE_SIZE).await;
            store.store_object_id()
        };

        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);

        let mut fs = FxFilesystemBuilder::new().open(device).await.expect("open failed");

        // Compact so that when we replay the transaction to delete the store won't find any
        // mutations.
        fs.journal().force_compact().await.expect("compact failed");

        for _ in 0..2 {
            {
                let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");

                let transaction = fs
                    .root_store()
                    .new_transaction(
                        lock_keys![
                            LockKey::object(
                                root_vol.volume_directory().store().store_object_id(),
                                root_vol.volume_directory().object_id(),
                            ),
                            LockKey::flush(store_id)
                        ],
                        Options { reservation: ReservationOptions::Borrowed, ..Default::default() },
                    )
                    .await
                    .expect("new_transaction failed");
                root_vol
                    .delete_volume("vol", transaction, || {})
                    .await
                    .expect("delete_volume failed");

                let store = root_vol
                    .new_volume("vol", NewChildStoreOptions::default())
                    .await
                    .expect("new_volume failed");
                create_file(&store, FILE_SIZE).await;
                store_id = store.store_object_id();
            }

            fs.close().await.expect("close failed");
            let device = fs.take_device().await;
            device.reopen(false);

            fs = FxFilesystemBuilder::new().open(device).await.expect("open failed");
        }

        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("close failed");
    }

    #[fuchsia::test(threads = 4)]
    async fn test_compaction_delete_race() {
        let (fs, _allocator) = test_fs().await;

        {
            const FILE_SIZE: usize = 10_000_000;

            let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
            let store = root_vol
                .new_volume("vol", NewChildStoreOptions::default())
                .await
                .expect("new_volume failed");

            create_file(&store, FILE_SIZE).await;

            // Race compaction with deleting a store.
            let fs_clone = fs.clone();

            // Even though the executor has 4 threads, it's hard to get it to run with
            // multiple threads.
            let executor_tasks = testing::force_executor_threads_to_run(4).await;

            let task = fasync::Task::spawn(async move {
                fs_clone.journal().force_compact().await.expect("compact failed");
            });

            // We don't need the executor tasks any more.
            drop(executor_tasks);

            // This range is chosen such that it caused this test to fail after quite a low number
            // of iterations for the bug that this test was introduced for.
            let sleep = rand::random_range(3000..6000);
            std::thread::sleep(std::time::Duration::from_micros(sleep));
            log::info!("sleep {sleep}us");

            let transaction = fs
                .root_store()
                .new_transaction(
                    lock_keys![
                        LockKey::object(
                            root_vol.volume_directory().store().store_object_id(),
                            root_vol.volume_directory().object_id(),
                        ),
                        LockKey::flush(store.store_object_id())
                    ],
                    Options { reservation: ReservationOptions::Borrowed, ..Default::default() },
                )
                .await
                .expect("new_transaction failed");
            root_vol.delete_volume("vol", transaction, || {}).await.expect("delete_volume failed");

            task.await;
        }

        fs.journal().force_compact().await.expect("compact failed");
        fs.close().await.expect("close failed");

        let device = fs.take_device().await;
        device.reopen(false);

        let fs = FxFilesystemBuilder::new().open(device).await.expect("open failed");
        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_delete_multiple_volumes() {
        let (mut fs, _) = test_fs().await;

        for _ in 0..50 {
            {
                let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
                let store = root_vol
                    .new_volume("vol", NewChildStoreOptions::default())
                    .await
                    .expect("new_volume failed");

                create_file(&store, 1_000_000).await;

                let transaction = fs
                    .root_store()
                    .new_transaction(
                        lock_keys![
                            LockKey::object(
                                root_vol.volume_directory().store().store_object_id(),
                                root_vol.volume_directory().object_id(),
                            ),
                            LockKey::flush(store.store_object_id())
                        ],
                        Options { reservation: ReservationOptions::Borrowed, ..Default::default() },
                    )
                    .await
                    .expect("new_transaction failed");
                root_vol
                    .delete_volume("vol", transaction, || {})
                    .await
                    .expect("delete_volume failed");

                fs.allocator().flush().await.expect("flush failed");
            }

            fs.close().await.expect("close failed");
            let device = fs.take_device().await;
            device.reopen(false);

            fs = FxFilesystemBuilder::new().open(device).await.expect("open failed");
        }

        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_allocate_free_reallocate() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;

        // Allocate some stuff.
        let mut device_ranges = Vec::new();
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        for _ in 0..30 {
            device_ranges.push(
                allocator
                    .allocate(&mut transaction, STORE_OBJECT_ID, 100 * fs.block_size())
                    .await
                    .expect("allocate failed"),
            );
        }
        transaction.commit().await.expect("commit failed");

        assert_eq!(
            fs.block_size() * 3000,
            *allocator.get_owner_allocated_bytes().entry(STORE_OBJECT_ID).or_default()
        );

        // Delete it all.
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        for range in std::mem::replace(&mut device_ranges, Vec::new()) {
            allocator.deallocate(&mut transaction, STORE_OBJECT_ID, range).await.expect("dealloc");
        }
        transaction.commit().await.expect("commit failed");

        assert_eq!(0, *allocator.get_owner_allocated_bytes().entry(STORE_OBJECT_ID).or_default());

        // Allocate some more stuff. Due to storage pressure, this requires us to flush device
        // before reusing the above space
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        let target_len = 1500 * fs.block_size();
        while device_ranges.iter().map(|i| i.length().unwrap()).sum::<u64>() != target_len {
            let len = target_len - device_ranges.iter().map(|i| i.length().unwrap()).sum::<u64>();
            device_ranges.push(
                allocator
                    .allocate(&mut transaction, STORE_OBJECT_ID, len)
                    .await
                    .expect("allocate failed"),
            );
        }
        transaction.commit().await.expect("commit failed");

        assert_eq!(
            fs.block_size() * 1500,
            *allocator.get_owner_allocated_bytes().entry(STORE_OBJECT_ID).or_default()
        );
    }

    #[fuchsia::test]
    async fn test_flush() {
        const STORE_OBJECT_ID: u64 = 99;

        let mut device_ranges = Vec::new();
        let device = {
            let (fs, allocator) = test_fs().await;
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new failed");
            device_ranges.push(
                allocator
                    .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                    .await
                    .expect("allocate failed"),
            );
            device_ranges.push(
                allocator
                    .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                    .await
                    .expect("allocate failed"),
            );
            device_ranges.push(
                allocator
                    .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                    .await
                    .expect("allocate failed"),
            );
            transaction.commit().await.expect("commit failed");

            allocator.flush().await.expect("flush failed");

            fs.close().await.expect("close failed");
            fs.take_device().await
        };

        device.reopen(false);
        let fs = FxFilesystemBuilder::new().open(device).await.expect("open failed");
        let allocator = fs.allocator();

        let allocated = collect_allocations(&allocator).await;

        // Make sure the ranges we allocated earlier are still allocated.
        for i in &device_ranges {
            let mut overlapping = 0;
            for j in &allocated {
                overlapping += overlap(i, j);
            }
            assert_eq!(overlapping, i.length().unwrap(), "Range {i:?} not allocated");
        }

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        let range = allocator
            .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
            .await
            .expect("allocate failed");

        // Make sure the range just allocated doesn't overlap any other allocated ranges.
        for r in &allocated {
            assert_eq!(overlap(r, &range), 0);
        }
        transaction.commit().await.expect("commit failed");
    }

    #[fuchsia::test]
    async fn test_dropped_transaction() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let allocated_range = {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                .await
                .expect("allocate failed")
        };
        // After dropping the transaction and attempting to allocate again, we should end up with
        // the same range because the reservation should have been released.
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        assert_eq!(
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                .await
                .expect("allocate failed"),
            allocated_range
        );
    }

    #[fuchsia::test]
    async fn test_cleanup_removed_owner() {
        const STORE_OBJECT_ID: u64 = 99;
        let device = {
            let (fs, allocator) = test_fs().await;

            assert!(!allocator.get_owner_allocated_bytes().contains_key(&STORE_OBJECT_ID));
            {
                let mut transaction = fs
                    .root_store()
                    .new_transaction(lock_keys![], Options::default())
                    .await
                    .unwrap();
                allocator
                    .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                    .await
                    .expect("Allocating");
                transaction.commit().await.expect("Committing.");
            }
            allocator.flush().await.expect("Flushing");
            assert!(allocator.get_owner_allocated_bytes().contains_key(&STORE_OBJECT_ID));
            {
                let mut transaction = fs
                    .root_store()
                    .new_transaction(lock_keys![], Options::default())
                    .await
                    .unwrap();
                allocator.mark_for_deletion(&mut transaction, STORE_OBJECT_ID);
                transaction.commit().await.expect("Committing.");
            }
            assert!(!allocator.get_owner_allocated_bytes().contains_key(&STORE_OBJECT_ID));
            fs.close().await.expect("Closing");
            fs.take_device().await
        };

        device.reopen(false);
        let fs = FxFilesystemBuilder::new().open(device).await.expect("open failed");
        let allocator = fs.allocator();
        assert!(!allocator.get_owner_allocated_bytes().contains_key(&STORE_OBJECT_ID));
    }

    #[fuchsia::test]
    async fn test_allocated_bytes() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;

        let initial_allocated_bytes = allocator.get_allocated_bytes();

        // Verify allocated_bytes reflects allocation changes.
        let allocated_bytes = initial_allocated_bytes + fs.block_size();
        let allocated_range = {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            let range = allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                .await
                .expect("allocate failed");
            transaction.commit().await.expect("commit failed");
            assert_eq!(allocator.get_allocated_bytes(), allocated_bytes);
            range
        };

        {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, fs.block_size().get())
                .await
                .expect("allocate failed");

            // Prior to committing, the count of allocated bytes shouldn't change.
            assert_eq!(allocator.get_allocated_bytes(), allocated_bytes);
        }

        // After dropping the prior transaction, the allocated bytes still shouldn't have changed.
        assert_eq!(allocator.get_allocated_bytes(), allocated_bytes);

        // Verify allocated_bytes reflects deallocations.
        let deallocate_range = allocated_range.start + 20..allocated_range.end - 20;
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");
        allocator
            .deallocate(&mut transaction, STORE_OBJECT_ID, deallocate_range)
            .await
            .expect("deallocate failed");

        // Before committing, there should be no change.
        assert_eq!(allocator.get_allocated_bytes(), allocated_bytes);

        transaction.commit().await.expect("commit failed");

        // After committing, all but 40 bytes should remain allocated.
        assert_eq!(allocator.get_allocated_bytes(), initial_allocated_bytes + 40);
    }

    #[fuchsia::test]
    async fn test_persist_bytes_limit() {
        const LIMIT: u64 = 12345;
        const OWNER_ID: u64 = 12;

        let (fs, allocator) = test_fs().await;
        {
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            allocator
                .set_bytes_limit(&mut transaction, OWNER_ID, LIMIT)
                .expect("Failed to set limit.");
            assert!(allocator.inner.lock().info.limit_bytes.get(&OWNER_ID).is_none());
            transaction.commit().await.expect("Failed to commit transaction");
            let bytes: u64 = *allocator
                .inner
                .lock()
                .info
                .limit_bytes
                .get(&OWNER_ID)
                .expect("Failed to find limit");
            assert_eq!(LIMIT, bytes);
        }
    }

    /// Given a sorted list of non-overlapping ranges, this will coalesce adjacent ranges.
    /// This allows comparison of equivalent sets of ranges which may occur due to differences
    /// across allocator strategies.
    fn coalesce_ranges(ranges: Vec<Range<u64>>) -> Vec<Range<u64>> {
        let mut coalesced = Vec::new();
        let mut prev: Option<Range<u64>> = None;
        for range in ranges {
            if let Some(prev_range) = &mut prev {
                if range.start == prev_range.end {
                    prev_range.end = range.end;
                } else {
                    coalesced.push(prev_range.clone());
                    prev = Some(range);
                }
            } else {
                prev = Some(range);
            }
        }
        if let Some(prev_range) = prev {
            coalesced.push(prev_range);
        }
        coalesced
    }

    #[fuchsia::test]
    async fn test_take_for_trimming() {
        const STORE_OBJECT_ID: u64 = 99;

        // Allocate a large chunk, then free a few bits of it, so we have free chunks interleaved
        // with allocated chunks.
        let allocated_range;
        let expected_free_ranges;
        let device = {
            let (fs, allocator) = test_fs().await;
            let bs = fs.block_size();
            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new failed");
            allocated_range = allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, 32 * bs)
                .await
                .expect("allocate failed");
            transaction.commit().await.expect("commit failed");

            let mut transaction = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new failed");
            let base = allocated_range.start;
            expected_free_ranges = vec![
                base..(base + (bs * 1)),
                (base + (bs * 2))..(base + (bs * 3)),
                // Note that the next three ranges are adjacent and will be treated as one free
                // range once applied.  We separate them here to exercise the handling of "large"
                // free ranges.
                (base + (bs * 4))..(base + (bs * 8)),
                (base + (bs * 8))..(base + (bs * 12)),
                (base + (bs * 12))..(base + (bs * 13)),
                (base + (bs * 29))..(base + (bs * 30)),
            ];
            for range in &expected_free_ranges {
                allocator
                    .deallocate(&mut transaction, STORE_OBJECT_ID, range.clone())
                    .await
                    .expect("deallocate failed");
            }
            transaction.commit().await.expect("commit failed");

            allocator.flush().await.expect("flush failed");

            fs.close().await.expect("close failed");
            fs.take_device().await
        };

        device.reopen(false);
        let fs = FxFilesystemBuilder::new().open(device).await.expect("open failed");
        let allocator = fs.allocator();

        // These values were picked so that each of them would be the reason why
        // collect_free_extents finished, and so we would return after partially processing one of
        // the free extents.
        let max_extent_size = (fs.block_size() * 4) as usize;
        const EXTENTS_PER_BATCH: usize = 2;
        let mut free_ranges = vec![];
        let mut offset = allocated_range.start;
        while offset < allocated_range.end {
            let free = allocator
                .take_for_trimming(offset, max_extent_size, EXTENTS_PER_BATCH)
                .await
                .expect("take_for_trimming failed");
            free_ranges.extend(
                free.extents().iter().filter(|range| range.end <= allocated_range.end).cloned(),
            );
            offset = free.extents().last().expect("Unexpectedly hit the end of free extents").end;
        }
        // Coalesce adjacent free ranges because the allocator may return smaller aligned chunks
        // but the overall range should always be equivalent.
        let coalesced_free_ranges = coalesce_ranges(free_ranges);
        let coalesced_expected_free_ranges = coalesce_ranges(expected_free_ranges);

        assert_eq!(coalesced_free_ranges, coalesced_expected_free_ranges);
    }

    #[fuchsia::test]
    async fn test_allocations_wait_for_free_extents() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let allocator_clone = allocator.clone();

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new failed");

        // Tie up all of the free extents on the device, and make sure allocations block.
        let max_extent_size = fs.device().size() as usize;
        const EXTENTS_PER_BATCH: usize = usize::MAX;

        // HACK: Treat `trimmable_extents` as being locked by `trim_done` (i.e. it should only be
        // accessed whilst `trim_done` is locked). We can't combine them into the same mutex,
        // because the inner type would be "poisoned" by the lifetime parameter of
        // `trimmable_extents` (which is in the lifetime of `allocator`), and then we can't move it
        // into `alloc_task` which would require a `'static` lifetime.
        let trim_done = Arc::new(Mutex::new(false));
        let trimmable_extents = allocator
            .take_for_trimming(0, max_extent_size, EXTENTS_PER_BATCH)
            .await
            .expect("take_for_trimming failed");

        let trim_done_clone = trim_done.clone();
        let bs = fs.block_size();
        let alloc_task = fasync::Task::spawn(async move {
            allocator_clone
                .allocate(&mut transaction, STORE_OBJECT_ID, bs.get())
                .await
                .expect("allocate should fail");
            {
                assert!(*trim_done_clone.lock(), "Allocation finished before trim completed");
            }
            transaction.commit().await.expect("commit failed");
        });

        // Add a small delay to simulate the trim taking some nonzero amount of time.  Otherwise,
        // this will almost certainly always beat the allocation attempt.
        fasync::Timer::new(std::time::Duration::from_millis(100)).await;

        // Once the free extents are released, the task should unblock.
        {
            let mut trim_done = trim_done.lock();
            std::mem::drop(trimmable_extents);
            *trim_done = true;
        }

        alloc_task.await;
    }

    #[fuchsia::test]
    async fn test_allocation_with_reservation_not_multiple_of_block_size() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;

        // Reserve an amount that isn't a multiple of the block size.
        const RESERVATION_AMOUNT: u64 = TRANSACTION_METADATA_MAX_AMOUNT + 5000;
        let reservation =
            allocator.clone().reserve(Some(STORE_OBJECT_ID), RESERVATION_AMOUNT).unwrap();

        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![],
                Options {
                    reservation: ReservationOptions::Hold(&reservation),
                    ..Options::default()
                },
            )
            .await
            .expect("new failed");

        let range = allocator
            .allocate(
                &mut transaction,
                STORE_OBJECT_ID,
                fs.block_size().align_up(RESERVATION_AMOUNT).unwrap(),
            )
            .await
            .expect("allocate faiiled");
        assert!(fs.block_size().is_aligned(range.end - range.start));

        println!("{}", range.end - range.start);
    }

    #[fuchsia::test]
    async fn test_concurrent_take_for_trimming_returns_error() {
        let (fs, allocator) = test_fs().await;
        let max_extent_size = fs.device().size() as usize;
        const EXTENTS_PER_BATCH: usize = usize::MAX;

        {
            let _trimmable_extents = allocator
                .take_for_trimming(0, max_extent_size, EXTENTS_PER_BATCH)
                .await
                .expect("take_for_trimming failed");

            let res = allocator.take_for_trimming(0, max_extent_size, EXTENTS_PER_BATCH).await;
            assert!(matches!(res, Err(e) if FxfsError::AlreadyBound.matches(&e)));
        }

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_preallocate_and_drop_releases_all_space() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let bs = fs.block_size().get();
        let used_before = allocator.get_used_bytes().0;

        let held_range = {
            let hold = allocator.preallocate(STORE_OBJECT_ID, 8 * bs).await.expect("hold failed");
            assert_eq!(hold.remaining(), 8 * bs);
            assert_eq!(allocator.get_used_bytes().0, used_before + 8 * bs);
            assert_eq!(allocator.owner_allocation_info(STORE_OBJECT_ID).0, 8 * bs);

            // A concurrent allocation must not overlap the held range.
            let mut txn =
                fs.root_store().new_transaction(lock_keys![], Options::default()).await.unwrap();
            let other = allocator.allocate(&mut txn, STORE_OBJECT_ID, bs).await.unwrap();
            assert!(other.end <= hold.range.start || other.start >= hold.range.end);
            // Drop `txn` without committing so `used_before` stays unchanged.
            hold.range.clone()
        };

        assert_eq!(allocator.get_used_bytes().0, used_before);
        assert_eq!(allocator.owner_allocation_info(STORE_OBJECT_ID).0, 0);

        // Once dropped, the exact held range can be allocated again.
        let mut txn =
            fs.root_store().new_transaction(lock_keys![], Options::default()).await.unwrap();
        let reallocated = allocator.allocate(&mut txn, STORE_OBJECT_ID, 8 * bs).await.unwrap();
        assert_eq!(reallocated, held_range);
        std::mem::drop(txn);
        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_preallocate_partial_take_and_drop_releases_tail() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let bs = fs.block_size().get();
        let allocated_before = allocator.get_allocated_bytes();

        let mut hold = allocator.preallocate(STORE_OBJECT_ID, 8 * bs).await.expect("hold failed");
        let full_range = hold.range.clone();
        assert_eq!(full_range.end - full_range.start, 8 * bs);

        // Take two contiguous chunks across two committed transactions, plus one aborted take.
        let mut txn1 =
            fs.root_store().new_transaction(lock_keys![], Options::default()).await.unwrap();
        let r1 = hold.take(&mut txn1, 2 * bs).unwrap();
        assert_eq!(r1, full_range.start..full_range.start + 2 * bs);
        txn1.commit().await.expect("commit 1");

        let mut txn2 =
            fs.root_store().new_transaction(lock_keys![], Options::default()).await.unwrap();
        let r2 = hold.take(&mut txn2, 3 * bs).unwrap();
        assert_eq!(r2, full_range.start + 2 * bs..full_range.start + 5 * bs);
        txn2.commit().await.expect("commit 2");

        // Aborted take: its 1 block must be freed back when `txn3` drops.
        let mut txn3 =
            fs.root_store().new_transaction(lock_keys![], Options::default()).await.unwrap();
        let r3 = hold.take(&mut txn3, bs).unwrap();
        assert_eq!(r3, full_range.start + 5 * bs..full_range.start + 6 * bs);
        std::mem::drop(txn3);

        assert_eq!(hold.remaining(), 2 * bs);
        std::mem::drop(hold);

        // Per-owner tracking for `STORE_OBJECT_ID` reflects only the 5 committed blocks (not the
        // 1 aborted block or 2 unused tail blocks).
        assert_eq!(allocator.owner_allocation_info(STORE_OBJECT_ID).0, 5 * bs);
        assert_eq!(allocator.get_allocated_bytes(), allocated_before + 5 * bs);

        // Both the aborted 1-block gap and the 2-block unused tail (which coalesce into 3 blocks)
        // are immediately reusable.
        let mut txn4 =
            fs.root_store().new_transaction(lock_keys![], Options::default()).await.unwrap();
        let tail = allocator.allocate(&mut txn4, STORE_OBJECT_ID, 3 * bs).await.unwrap();
        assert_eq!(tail, full_range.start + 5 * bs..full_range.end);
        txn4.commit().await.expect("commit 4");

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_preallocate_survives_rebuild_strategy_trim_and_unclean_replay() {
        let device = DeviceHolder::new(FakeDevice::new(4096, 4096));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_vol = root_volume(fs.clone()).await.expect("root_volume");
        let store =
            root_vol.new_volume("vol", NewChildStoreOptions::default()).await.expect("new_volume");
        let store_id = store.store_object_id();
        let allocator = fs.allocator();
        let bs = fs.block_size().get();

        let mut hold = allocator.preallocate(store_id, 6 * bs).await.expect("hold failed");
        let held_range = hold.range.clone();

        // Commit the first 2 blocks to an actual object so fsck is happy after replay.
        let mut txn = store
            .new_transaction(
                lock_keys![LockKey::object(store_id, store.root_directory_object_id())],
                Options::default(),
            )
            .await
            .unwrap();
        let obj = ObjectStore::create_object(&store, &mut txn, Default::default(), None)
            .await
            .expect("create_object");
        store.add_to_graveyard(&mut txn, obj.object_id());
        let taken = hold.take(&mut txn, 2 * bs).unwrap();
        txn.add(
            store_id,
            crate::object_store::transaction::Mutation::merge_object(
                crate::object_store::ObjectKey::extent(
                    obj.object_id(),
                    crate::object_store::AttributeId::DATA,
                    0..2 * bs,
                ),
                crate::object_store::ObjectValue::Extent(
                    crate::object_store::extent_record::ExtentValue::new_raw(
                        taken.start,
                        crate::object_store::VOLUME_DATA_KEY_ID,
                    ),
                ),
            ),
        );
        txn.commit().await.expect("commit");

        // Force a strategy rebuild and a trim scan while `hold` overlaps the committed 2 blocks.
        {
            let _guard = allocator.allocation_mutex.lock().await;
            allocator.rebuild_strategy().await.expect("rebuild_strategy");
        }
        {
            let trim = allocator
                .take_for_trimming(0, fs.device().size() as usize, usize::MAX)
                .await
                .expect("take_for_trimming");
            for ext in trim.extents() {
                assert!(
                    ext.end <= held_range.start || ext.start >= held_range.end,
                    "trim extent {ext:?} overlapped held range {held_range:?}"
                );
            }
        }

        // Flush the journal, snapshot the device without dropping `hold` (simulating a crash mid-
        // hold), and verify replay only accounts for the 2 committed blocks.
        fs.sync(crate::filesystem::SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect("sync");
        let crashed_device = fs.device().snapshot().expect("snapshot");

        std::mem::drop(hold);
        fs.close().await.expect("close");

        let fs2 = FxFilesystem::open(crashed_device).await.expect("reopen");
        fsck(fs2.clone()).await.expect("fsck after crash");
        assert_eq!(fs2.allocator().owner_allocation_info(store_id).0, 2 * bs);
        fs2.close().await.expect("close 2");
    }

    #[fuchsia::test]
    async fn test_preallocate_respects_owner_byte_limit() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let bs = fs.block_size().get();

        let mut txn =
            fs.root_store().new_transaction(lock_keys![], Options::default()).await.unwrap();
        allocator.set_bytes_limit(&mut txn, STORE_OBJECT_ID, 3 * bs).expect("set_bytes_limit");
        txn.commit().await.unwrap();

        let hold = allocator.preallocate(STORE_OBJECT_ID, 10 * bs).await.expect("hold");
        assert_eq!(hold.remaining(), 3 * bs);

        // With all 3 blocks held, a second hold returns empty rather than failing.
        let hold2 = allocator.preallocate(STORE_OBJECT_ID, bs).await.expect("hold 2");
        assert!(hold2.is_empty());

        std::mem::drop(hold);
        std::mem::drop(hold2);
        fs.close().await.expect("close");
    }

    #[fuchsia::test]
    async fn test_preallocate_does_not_block_on_trim() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let bs = fs.block_size().get();
        let max_extent_size = fs.device().size() as usize;

        // Tie up all free extents on the device in an in-flight trim batch; `preallocate` must
        // return an empty `Preallocation` immediately without waiting for `trimmable_extents` to be
        // dropped.
        let trimmable_extents =
            allocator.take_for_trimming(0, max_extent_size, usize::MAX).await.expect("trim");
        let hold = allocator.preallocate(STORE_OBJECT_ID, 4 * bs).await.expect("preallocate");
        assert!(hold.is_empty());
        std::mem::drop(hold);

        std::mem::drop(trimmable_extents);
        let hold = allocator.preallocate(STORE_OBJECT_ID, 4 * bs).await.expect("preallocate 2");
        assert_eq!(hold.remaining(), 4 * bs);
        std::mem::drop(hold);
        fs.close().await.expect("close");
    }

    #[fuchsia::test]
    async fn test_did_flush_device_chunked() {
        const STORE_OBJECT_ID: u64 = 99;
        let (fs, allocator) = test_fs().await;
        let bs = fs.block_size().get();

        // Allocate 300 blocks so we can deallocate 150 non-adjacent blocks (> 2 batches of 64).
        let mut allocated = Vec::new();
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        for _ in 0..300 {
            allocated.push(
                allocator
                    .allocate(&mut transaction, STORE_OBJECT_ID, bs)
                    .await
                    .expect("allocate failed"),
            );
        }
        transaction.commit().await.expect("commit failed");
        let expected_when_all_allocated = collect_allocations(&allocator).await;

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        for range in allocated.iter().step_by(2) {
            allocator
                .deallocate(&mut transaction, STORE_OBJECT_ID, range.clone())
                .await
                .expect("deallocate failed");
        }
        transaction.commit().await.expect("commit failed");

        // Drive a device-flushing sync step-by-step until the first batch of 64 deallocated
        // blocks becomes allocatable while `sync` is suspended at the inter-batch yield point,
        // then drive `sync_fut` to completion.
        let mut sync_fut = pin!(fs.sync(SyncOptions { flush_device: true, ..Default::default() }));
        loop {
            assert!(
                poll!(sync_fut.as_mut()).is_pending(),
                "sync should suspend between did_flush_device batches"
            );
            let mut probe_txn = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            let first_probe = allocator
                .allocate(&mut probe_txn, STORE_OBJECT_ID, bs)
                .await
                .expect("probe allocate failed");
            if first_probe == allocated[0] {
                // Verify that only the first batch of 64 blocks was released at the first yield:
                // blocks 1..64 match `allocated[2], allocated[4], ..., allocated[126]` while the
                // 65th block must come from past `allocated[299]`.
                for i in 1..64 {
                    let next_probe = allocator
                        .allocate(&mut probe_txn, STORE_OBJECT_ID, bs)
                        .await
                        .expect("probe allocate failed");
                    assert_eq!(next_probe, allocated[i * 2]);
                }
                let batch2_probe = allocator
                    .allocate(&mut probe_txn, STORE_OBJECT_ID, bs)
                    .await
                    .expect("probe allocate failed");
                assert!(batch2_probe.start >= allocated[299].end);
                break;
            }
            drop(probe_txn);
            fasync::yield_now().await;
        }
        sync_fut.await.expect("sync failed");

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        for _ in 0..150 {
            allocator
                .allocate(&mut transaction, STORE_OBJECT_ID, bs)
                .await
                .expect("reallocate failed");
        }
        transaction.commit().await.expect("commit failed");

        check_allocations(&allocator, &expected_when_all_allocated).await;
        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_deallocated_not_reused_before_device_flush() {
        let fail_flushes = Arc::new(AtomicBool::new(false));
        let fail_flushes_clone = fail_flushes.clone();
        let write_count = Arc::new(AtomicUsize::new(0));
        let write_count_clone = write_count.clone();
        let flush_count = Arc::new(AtomicUsize::new(0));
        let flush_count_clone = flush_count.clone();
        let mut fake_device = FakeDevice::new(4096, 4096);
        fake_device.set_op_callback(move |op| match op {
            Op::Write => {
                write_count_clone.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Op::Flush => {
                if fail_flushes_clone.load(Ordering::Relaxed) {
                    bail!("Injected flush error");
                }
                flush_count_clone.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Op::Read => Ok(()),
        });
        let fs = FxFilesystem::new_empty(DeviceHolder::new(fake_device))
            .await
            .expect("new_empty failed");
        let allocator = fs.allocator();
        let bs = fs.block_size().get();

        // Persist a file with `0xaa` so its extent is durable on disk.
        let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
        let vol = root_vol.new_volume("vol", NewChildStoreOptions::default()).await.expect("vol");
        let store_object_id = vol.store_object_id();
        let dir = Directory::open(&vol, vol.root_directory_object_id()).await.expect("open dir");
        let (file_a, file_b) = {
            let mut transaction = fs
                .root_store()
                .new_transaction(
                    lock_keys![LockKey::object(store_object_id, vol.root_directory_object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let file_a = dir.create_child_file(&mut transaction, "a").await.expect("create a");
            let file_b = dir.create_child_file(&mut transaction, "b").await.expect("create b");
            transaction.commit().await.expect("commit failed");
            (file_a, file_b)
        };
        let mut buf = file_a.allocate_buffer(bs as usize).await;
        buf.fill(0xaa);
        file_a.write_or_append(Some(0), buf.as_ref()).await.expect("write a failed");
        fs.sync(SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect("initial sync failed");

        // Overwrite `file_a` (CoW-deallocating its `0xaa` extent) without flushing the journal yet,
        // then write `0xcc` to `file_b`. Because `file_a`'s old extent has not been device-flushed,
        // `file_b`'s data write must not clobber `file_a`'s durable `0xaa` block if power is lost
        // before the journal writes land.
        buf.fill(0xbb);
        file_a.write_or_append(Some(0), buf.as_ref()).await.expect("overwrite a failed");
        buf.fill(0xcc);
        file_b.write_or_append(Some(0), buf.as_ref()).await.expect("write b failed");

        let snapshot = fs.device().snapshot().expect("snapshot failed");
        let fs_reopened = FxFilesystem::open(snapshot).await.expect("open snapshot failed");
        fsck(fs_reopened.clone()).await.expect("fsck failed");
        let reopened_vol = root_volume(fs_reopened.clone())
            .await
            .expect("root_volume failed")
            .volume("vol", StoreOptions::default())
            .await
            .expect("open vol failed");
        let reopened_dir = Directory::open(&reopened_vol, reopened_vol.root_directory_object_id())
            .await
            .expect("open reopened dir failed");
        let (reopened_a_id, _, _) =
            reopened_dir.lookup("a").await.expect("lookup a failed").expect("a missing");
        let reopened_a =
            ObjectStore::open_object(&reopened_vol, reopened_a_id, HandleOptions::default(), None)
                .await
                .expect("open a failed");
        let mut read_buf = reopened_a.allocate_buffer(bs as usize).await;
        assert_eq!(
            reopened_a.read_aligned(0, read_buf.as_mut()).await.expect("read a"),
            bs as usize
        );
        let mut contents = vec![0u8; bs as usize];
        read_buf.copy_to_slice(&mut contents);
        assert_eq!(contents, vec![0xaa; bs as usize]);
        fs_reopened.close().await.expect("close reopened failed");

        // Sync first to clear earlier deallocations, then verify directly that neither a
        // journal-only sync (`flush_device: false`) nor a failed device flush releases a
        // newly deallocated extent for reuse.
        fs.sync(SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect("sync failed");

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        let deallocated_range = allocator
            .allocate(&mut transaction, store_object_id, bs)
            .await
            .expect("allocate failed");
        transaction.commit().await.expect("commit failed");

        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        allocator
            .deallocate(&mut transaction, store_object_id, deallocated_range.clone())
            .await
            .expect("deallocate failed");
        transaction.commit().await.expect("commit failed");

        // Flushing only the journal to the block device (`flush_device: false`) must not release
        // `deallocated_range`, even once the background journal write completes on the device.
        let writes_before = write_count.load(Ordering::Relaxed);
        let flushes_before = flush_count.load(Ordering::Relaxed);
        fs.sync(SyncOptions { flush_device: false, ..Default::default() })
            .await
            .expect("journal-only sync failed");
        while write_count.load(Ordering::Relaxed) == writes_before {
            fasync::yield_now().await;
        }
        fasync::yield_now().await;
        assert_eq!(flush_count.load(Ordering::Relaxed), flushes_before);
        {
            let mut probe_txn = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            let probe = allocator
                .allocate(&mut probe_txn, store_object_id, bs)
                .await
                .expect("probe allocate failed");
            assert!(probe.start >= deallocated_range.end);
        }

        // A failing device flush must also keep `deallocated_range` held back.
        fail_flushes.store(true, Ordering::Relaxed);
        fs.sync(SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect_err("sync should fail when device flush fails");
        {
            let mut probe_txn = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            let probe = allocator
                .allocate(&mut probe_txn, store_object_id, bs)
                .await
                .expect("probe allocate failed");
            assert!(probe.start >= deallocated_range.end);
        }

        // Once the device flush succeeds, `deallocated_range` becomes the lowest free offset and
        // is immediately reused.
        fail_flushes.store(false, Ordering::Relaxed);
        fs.sync(SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect("device-flushing sync failed");
        {
            let mut probe_txn = fs
                .root_store()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            let reused = allocator
                .allocate(&mut probe_txn, store_object_id, bs)
                .await
                .expect("reuse allocate failed");
            assert_eq!(reused, deallocated_range);
        }

        fs.close().await.expect("close failed");
    }
}
