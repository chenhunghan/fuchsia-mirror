// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::event::AutounsignalEvent;
use crate::kernel::thread::{Thread, ThreadPtr};
use core::cell::UnsafeCell;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::AtomicBool;
use evictor_bindings as bindings;
use ksync::{KMutex, RawMonitoredSpinlock, RawMutex, guarded};
use pin_init::{PinInit, pin_data};
use zr::Opaque;

#[repr(u8)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvictionLevel {
    OnlyOldest = 0,
    IncludeNewest = 1,
}

// Compile-time layout assertions against C++ EvictionLevel
zr::static_assert!(
    core::mem::size_of::<EvictionLevel>()
        == core::mem::size_of::<bindings::Evictor_EvictionLevel>()
);
zr::static_assert!(
    EvictionLevel::OnlyOldest as u8 == bindings::Evictor_EvictionLevel::OnlyOldest as u8
);
zr::static_assert!(
    EvictionLevel::IncludeNewest as u8 == bindings::Evictor_EvictionLevel::IncludeNewest as u8
);

#[repr(u8)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum Output {
    NoPrint = 0,
    Print = 1,
}

// Compile-time layout assertions against C++ Output
zr::static_assert!(
    core::mem::size_of::<Output>() == core::mem::size_of::<bindings::Evictor_Output>()
);
zr::static_assert!(Output::Print as u8 == bindings::Evictor_Output::Print as u8);
zr::static_assert!(Output::NoPrint as u8 == bindings::Evictor_Output::NoPrint as u8);

#[repr(u8)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum TriggerReason {
    Other = 0,
    OOM = 1,
}

// Compile-time layout assertions against C++ TriggerReason
zr::static_assert!(
    core::mem::size_of::<TriggerReason>()
        == core::mem::size_of::<bindings::Evictor_TriggerReason>()
);
zr::static_assert!(TriggerReason::OOM as u8 == bindings::Evictor_TriggerReason::OOM as u8);
zr::static_assert!(TriggerReason::Other as u8 == bindings::Evictor_TriggerReason::Other as u8);

/// Eviction target state is grouped together behind a lock to allow different threads to safely
/// trigger and perform the eviction.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct EvictionTarget {
    pub pending: bool,
    /// The desired value to get |pmm_node_|'s free page count to
    pub free_pages_target: u64,
    /// A minimum amount of pages we want to evict, regardless of how much free memory is
    /// available.
    pub min_pages_to_free: u64,
    pub level: EvictionLevel,
    pub print_counts: bool,
    pub oom_trigger: bool,
}

impl Default for EvictionTarget {
    fn default() -> Self {
        Self {
            pending: false,
            free_pages_target: 0,
            min_pages_to_free: 0,
            level: EvictionLevel::OnlyOldest,
            print_counts: false,
            oom_trigger: false,
        }
    }
}

// Compile-time layout assertions against C++ EvictionTarget
zr::static_assert!(
    core::mem::size_of::<EvictionTarget>()
        == core::mem::size_of::<bindings::Evictor_EvictionTarget>()
);
zr::static_assert!(
    core::mem::align_of::<EvictionTarget>()
        == core::mem::align_of::<bindings::Evictor_EvictionTarget>()
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, pending)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, pending)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, free_pages_target)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, free_pages_target)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, min_pages_to_free)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, min_pages_to_free)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, level)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, level)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, print_counts)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, print_counts)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, oom_trigger)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, oom_trigger)
);

/// We count non-loaned and loaned evicted pages separately since the eviction goal is set in terms
/// of non-loaned pages (for now) so in order to verify expected behavior in tests we keep separate
/// counts.
#[repr(C)]
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct EvictedPageCounts {
    /// The pager_backed and pager_backed_loaned counts are exclusive; any given evicted page is
    /// counted as pager_backed or pager_backed_loaned depending on whether it's non-loaned or
    /// loaned respectively.
    ///
    /// evicted from pager-backed VMO non-loaned page count
    pub pager_backed: u64,
    /// evicted from pager-backed VMO loaned page count
    pub pager_backed_loaned: u64,
    /// evicted from/via discardable VMO page count
    pub discardable: u64,
    /// evicted from an anonymous VMO via compression
    pub compressed: u64,
}

impl EvictedPageCounts {
    pub const fn non_loaned_total(&self) -> u64 {
        self.pager_backed + self.discardable + self.compressed
    }
}

impl core::ops::AddAssign for EvictedPageCounts {
    fn add_assign(&mut self, rhs: Self) {
        self.pager_backed += rhs.pager_backed;
        self.pager_backed_loaned += rhs.pager_backed_loaned;
        self.discardable += rhs.discardable;
        self.compressed += rhs.compressed;
    }
}

impl core::ops::Sub for EvictedPageCounts {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            pager_backed: self.pager_backed - rhs.pager_backed,
            pager_backed_loaned: self.pager_backed_loaned - rhs.pager_backed_loaned,
            discardable: self.discardable - rhs.discardable,
            compressed: self.compressed - rhs.compressed,
        }
    }
}

// Compile-time layout assertions against C++ EvictedPageCounts
zr::static_assert!(
    core::mem::size_of::<EvictedPageCounts>()
        == core::mem::size_of::<bindings::Evictor_EvictedPageCounts>()
);
zr::static_assert!(
    core::mem::align_of::<EvictedPageCounts>()
        == core::mem::align_of::<bindings::Evictor_EvictedPageCounts>()
);
zr::static_assert!(
    core::mem::offset_of!(EvictedPageCounts, pager_backed)
        == core::mem::offset_of!(bindings::Evictor_EvictedPageCounts, pager_backed)
);
zr::static_assert!(
    core::mem::offset_of!(EvictedPageCounts, pager_backed_loaned)
        == core::mem::offset_of!(bindings::Evictor_EvictedPageCounts, pager_backed_loaned)
);
zr::static_assert!(
    core::mem::offset_of!(EvictedPageCounts, discardable)
        == core::mem::offset_of!(bindings::Evictor_EvictedPageCounts, discardable)
);
zr::static_assert!(
    core::mem::offset_of!(EvictedPageCounts, compressed)
        == core::mem::offset_of!(bindings::Evictor_EvictedPageCounts, compressed)
);

/// Result returned from eviction methods indicating what they performed and why they returned.
/// Provides by the count of how many different items were evicted, which could be more or less
/// than requested, as well as whether any free memory target was achieved.
#[repr(C)]
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct EvictionResult {
    pub counts: EvictedPageCounts,
    pub free_target_reached: bool,
}

// Compile-time layout assertions against C++ EvictionResult
zr::static_assert!(
    core::mem::size_of::<EvictionResult>()
        == core::mem::size_of::<bindings::Evictor_EvictionResult>()
);
zr::static_assert!(
    core::mem::align_of::<EvictionResult>()
        == core::mem::align_of::<bindings::Evictor_EvictionResult>()
);
zr::static_assert!(
    core::mem::offset_of!(EvictionResult, counts)
        == core::mem::offset_of!(bindings::Evictor_EvictionResult, counts)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionResult, free_target_reached)
        == core::mem::offset_of!(bindings::Evictor_EvictionResult, free_target_reached)
);

#[repr(C)]
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct EvictorStats {
    pub pager_backed_oom: u64,
    pub pager_backed_other: u64,
    pub compression_oom: u64,
    pub compression_other: u64,
    pub discarded_oom: u64,
    pub discarded_other: u64,
}

// Compile-time layout assertions against C++ EvictorStats
zr::static_assert!(
    core::mem::size_of::<EvictorStats>() == core::mem::size_of::<bindings::Evictor_EvictorStats>()
);
zr::static_assert!(
    core::mem::align_of::<EvictorStats>()
        == core::mem::align_of::<bindings::Evictor_EvictorStats>()
);
zr::static_assert!(
    core::mem::offset_of!(EvictorStats, pager_backed_oom)
        == core::mem::offset_of!(bindings::Evictor_EvictorStats, pager_backed_oom)
);
zr::static_assert!(
    core::mem::offset_of!(EvictorStats, pager_backed_other)
        == core::mem::offset_of!(bindings::Evictor_EvictorStats, pager_backed_other)
);
zr::static_assert!(
    core::mem::offset_of!(EvictorStats, compression_oom)
        == core::mem::offset_of!(bindings::Evictor_EvictorStats, compression_oom)
);
zr::static_assert!(
    core::mem::offset_of!(EvictorStats, compression_other)
        == core::mem::offset_of!(bindings::Evictor_EvictorStats, compression_other)
);
zr::static_assert!(
    core::mem::offset_of!(EvictorStats, discarded_oom)
        == core::mem::offset_of!(bindings::Evictor_EvictorStats, discarded_oom)
);
zr::static_assert!(
    core::mem::offset_of!(EvictorStats, discarded_other)
        == core::mem::offset_of!(bindings::Evictor_EvictorStats, discarded_other)
);

/// Implements page evictor logic to free pages belonging to a PmmNode under memory pressure.
/// Eviction in this context is both direct eviction of pager backed memory and discardable VMO
/// memory, as well as by performing compression.
/// This class is thread-safe.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Evictor {
    /// Target for eviction.
    #[guarded_by(lock)]
    eviction_target: EvictionTarget,

    /// Mutex that enforces only one eviction attempt to be active at any time. This prevents us
    /// from overshooting the free memory targets required by various simultaneous eviction
    /// requests.
    #[mutex]
    eviction_lock: KMutex<RawMutex>,

    /// Tracks the total amount of evictions performed by EvictUntilTargetsMet. This allows
    /// parallel evictions to combine their eviction progress instead of acting serially.
    #[guarded_by(eviction_lock)]
    total_evicted: EvictedPageCounts,

    /// Use MonitoredSpinLock to provide lockup detector diagnostics for the critical sections
    /// protected by this lock.
    #[mutex]
    lock: KMutex<RawMonitoredSpinlock>,

    /// The eviction thread used to process asynchronous requests.
    /// Created only if eviction is enabled i.e. |eviction_enabled_| is set to true.
    eviction_thread: UnsafeCell<*const Thread>,
    eviction_thread_exiting: AtomicBool,

    /// Used by the eviction thread to wait for eviction requests.
    #[pin]
    eviction_signal: AutounsignalEvent,

    /// Optionally specified methods to allow for tests to fake interactions with the pmm. To avoid
    /// virtual dispatch in non test scenarios these are empty/null methods when not set.
    test_reclaim_function: Opaque<bindings::Evictor_ReclaimFunction>,
    test_free_pages_function: Opaque<bindings::Evictor_FreePagesFunction>,

    // These parameters are initialized later from kernel cmdline options.
    /// Whether eviction is enabled.
    #[guarded_by(lock)]
    eviction_enabled: bool,
    /// Whether eviction should attempt compression.
    #[guarded_by(lock)]
    use_compression: bool,

    phantom: PhantomData<PhantomPinned>,
}

// Compile-time layout assertions against the C++ Evictor type via bindgen.
zr::static_assert!(core::mem::size_of::<Evictor>() == core::mem::size_of::<bindings::Evictor>());
zr::static_assert!(core::mem::align_of::<Evictor>() == core::mem::align_of::<bindings::Evictor>());
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_target)
        == core::mem::offset_of!(bindings::Evictor, eviction_target_)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_lock)
        == core::mem::offset_of!(bindings::Evictor, eviction_lock_)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, total_evicted)
        == core::mem::offset_of!(bindings::Evictor, total_evicted_)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, lock) == core::mem::offset_of!(bindings::Evictor, lock_)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_thread)
        == core::mem::offset_of!(bindings::Evictor, eviction_thread_)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_thread_exiting)
        == core::mem::offset_of!(bindings::Evictor, eviction_thread_exiting_)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_signal)
        == core::mem::offset_of!(bindings::Evictor, eviction_signal_)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, test_reclaim_function)
        == core::mem::offset_of!(bindings::Evictor, test_reclaim_function_)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, test_free_pages_function)
        == core::mem::offset_of!(bindings::Evictor, test_free_pages_function_)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_enabled)
        == core::mem::offset_of!(bindings::Evictor, eviction_enabled_)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, use_compression)
        == core::mem::offset_of!(bindings::Evictor, use_compression_)
);

// SAFETY: `Evictor` is internally synchronized via its own `lock` and `eviction_lock`, and the
// pointers it holds only reference objects that outlive it.
unsafe impl Send for Evictor {}
// SAFETY: `Evictor` methods operate on shared references `&self` concurrently across threads,
// with all mutable state protected by its internal locks or held in atomics.
unsafe impl Sync for Evictor {}

#[pin_init::pinned_drop]
impl PinnedDrop for Evictor {
    fn drop(self: Pin<&mut Self>) {
        // The remainder of the C++ destructor, i.e. tearing down the locks and the event, is
        // performed by the drop glue of the individual members.
        // SAFETY: `self` is a live, pinned `Evictor` for the duration of this call.
        unsafe { bindings::cpp_evictor_disable_eviction(self.as_raw()) };
    }
}

impl Evictor {
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(bindings::cpp_evictor_init)
    }

    /// Domain-specific conversion: returns raw pointer for `Evictor`.
    fn as_raw(&self) -> *mut bindings::Evictor {
        (self as *const Self).cast_mut().cast()
    }

    /// Called from the scanner to enable eviction if required. Creates an eviction thread to
    /// process asynchronous eviction requests.
    /// By default this only enables user pager based eviction and `use_compression` can be used to
    /// also perform compression.
    pub fn enable_eviction(&self, use_compression: bool) {
        // SAFETY: `self.as_raw()` returns a valid `Evictor` pointer.
        unsafe { bindings::cpp_evictor_enable_eviction(self.as_raw(), use_compression) }
    }

    /// Called from the scanner to disable all eviction if needed, will shut down any in existing
    /// eviction thread. It is a responsibility of the scanner to not have multiple concurrent calls
    /// to this and `enable_eviction`.
    pub fn disable_eviction(&self) {
        // SAFETY: `self.as_raw()` returns a valid `Evictor` pointer.
        unsafe { bindings::cpp_evictor_disable_eviction(self.as_raw()) }
    }

    /// Evict from a user specified external `target` which is only used for this eviction attempt
    /// and does not interfere with the `eviction_target`.
    pub fn evict_from_external_target(&self, target: EvictionTarget) -> EvictionResult {
        let mut result = MaybeUninit::<EvictionResult>::uninit();
        // SAFETY: `self.as_raw()` returns a valid `Evictor` pointer, and `EvictionTarget` and
        // `EvictionResult` have identical memory layout to their C++ counterparts.
        unsafe {
            bindings::cpp_evictor_evict_from_external_target(
                self.as_raw(),
                (&target as *const EvictionTarget).cast(),
                result.as_mut_ptr().cast(),
            );
            result.assume_init()
        }
    }

    /// Performs a synchronous request to evict until free memory equals |free_mem_start| (in bytes)
    /// and at least |min_mem_to_free| (in bytes) has been reclaimed. The return value is an
    /// EvictionResult detailing exactly what was evicted and whether the free_mem_target was
    /// achieved. The |eviction_level| is a rough control that maps to how old a page needs to
    /// be for being considered for eviction. This may acquire arbitrary vmo and aspace locks.
    pub fn evict_synchronous(
        &self,
        min_mem_to_free: u64,
        free_mem_target: u64,
        eviction_level: EvictionLevel,
        output: Output,
        reason: TriggerReason,
    ) -> EvictionResult {
        let mut result = MaybeUninit::<EvictionResult>::uninit();
        // SAFETY: `self.as_raw()` returns a valid `Evictor` pointer, and `EvictionResult` and the
        // enums have identical memory representations to their C++ counterparts.
        unsafe {
            bindings::cpp_evictor_evict_synchronous(
                self.as_raw(),
                min_mem_to_free,
                free_mem_target,
                core::mem::transmute::<EvictionLevel, bindings::Evictor_EvictionLevel>(
                    eviction_level,
                ),
                core::mem::transmute::<Output, bindings::Evictor_Output>(output),
                core::mem::transmute::<TriggerReason, bindings::Evictor_TriggerReason>(reason),
                result.as_mut_ptr().cast(),
            );
            result.assume_init()
        }
    }

    /// Reclaim memory until free memory equals the `free_mem_target` (in bytes) and at least
    /// `min_mem_to_free` (in bytes) has been reclaimed. Reclamation will happen asynchronously on
    /// the eviction thread and this function returns immediately. Once the target is reached,
    /// or there is no more memory that can be reclaimed, this process will stop and the free
    /// memory target will be cleared. The `eviction_level` is a rough control on how hard to
    /// try and evict. Multiple calls to `evict_asynchronous` will cause all the targets to get
    /// merged by adding together `min_mem_to_free`, taking the max of `free_mem_target` and the
    /// highest or most aggressive of any `eviction_level`.
    pub fn evict_asynchronous(
        &self,
        min_mem_to_free: u64,
        free_mem_target: u64,
        eviction_level: EvictionLevel,
        output: Output,
    ) {
        // SAFETY: `self.as_raw()` returns a valid `Evictor` pointer.
        unsafe {
            bindings::cpp_evictor_evict_asynchronous(
                self.as_raw(),
                min_mem_to_free,
                free_mem_target,
                core::mem::transmute::<EvictionLevel, bindings::Evictor_EvictionLevel>(
                    eviction_level,
                ),
                core::mem::transmute::<Output, bindings::Evictor_Output>(output),
            )
        }
    }

    /// Whether any eviction can occur.
    pub fn is_eviction_enabled(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `Evictor` pointer.
        unsafe { bindings::cpp_evictor_is_eviction_enabled(self.as_raw()) }
    }

    /// Whether eviction should attempt to use compression.
    pub fn is_compression_enabled(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `Evictor` pointer.
        unsafe { bindings::cpp_evictor_is_compression_enabled(self.as_raw()) }
    }

    /// Return global eviction stats from all instantiations of the Evictor.
    pub fn get_global_stats() -> EvictorStats {
        let mut stats = MaybeUninit::<EvictorStats>::uninit();
        // SAFETY: `EvictorStats` has identical memory layout to `bindings::Evictor_EvictorStats`.
        unsafe {
            bindings::cpp_evictor_get_global_stats(stats.as_mut_ptr().cast());
            stats.assume_init()
        }
    }

    /// Debug method to retrieve any current eviction thread. Only to be used for testing /
    /// debugging purposes. It is up to the caller to know if this objects is alive or not.
    pub fn debug_get_evictor_thread(&self) -> Option<ThreadPtr> {
        // SAFETY: `self.as_raw()` returns a valid `Evictor` pointer.
        let raw = unsafe { bindings::cpp_evictor_debug_get_evictor_thread(self.as_raw()) };
        // SAFETY: If non-null, `raw` points to a live kernel thread.
        unsafe { ThreadPtr::from_raw(raw.cast()) }
    }
}
