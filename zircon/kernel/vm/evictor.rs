// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::event::AutounsignalEvent;
use crate::kernel::thread::Thread;
use core::cell::UnsafeCell;
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use core::sync::atomic::AtomicBool;
use evictor_bindings as bindings;
use ksync::{KMutex, RawMonitoredSpinlock, RawMutex, guarded};
use pin_init::{PinInit, pin_data};
use zr::Opaque;

#[repr(u8)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
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

/// Eviction target state is grouped together behind a lock to allow different threads to safely
/// trigger and perform the eviction.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
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
#[derive(Default, Debug, Clone)]
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
}
