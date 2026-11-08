// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::MaybeUninit;

use percpu_bindings as bindings;
use pin_init::PinInit;

use crate::kernel::types::{SMP_MAX_CPUS, cpu_num_t};

/// CpuSearchSet is a cache/cluster-aware search list that minimizes cache
/// crossings and maximizes remote CPU access distribution when searching for a
/// target CPU to place a task.
// Backed by `zr::Opaque` (`UnsafeCell` + `PhantomPinned`) so secondary CPU init can populate each
// CPU's search set through shared `&PerCpu` references without ever forming `&mut PerCpu`.
// TODO(https://fxbug.dev/575987745): Consolidate `scheduler.rs`'s `CpuSearchSetEntry` and
// `cpp_get_cpu_search_set_entries` onto `CpuSearchSet`.
#[repr(transparent)]
pub struct CpuSearchSet(zr::Opaque<bindings::CpuSearchSet>);

impl CpuSearchSet {
    /// Constructs a default `CpuSearchSet` in place.
    #[inline]
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_cpu_search_set_init)
    }

    #[inline]
    fn as_raw_mut(&self) -> *mut Self {
        // `Self` is `#[repr(transparent)]` over `zr::Opaque<bindings::CpuSearchSet>`.
        self.0.get().cast::<Self>()
    }

    /// Sets the relative performance scale for the given CPU.
    ///
    /// # Safety
    ///
    /// Must only be called during single-threaded boot initialization before concurrent access to
    /// `CpuSearchSet::perf_scales_`.
    #[inline]
    pub unsafe fn set_perf_scale(cpu: cpu_num_t, perf_scale: i64) {
        // Unconditional check because `CpuSearchSet::perf_scales_[cpu]` indexes unchecked when
        // `LK_DEBUGLEVEL == 0`.
        assert!((cpu as usize) < SMP_MAX_CPUS);
        // SAFETY: `cpu < SMP_MAX_CPUS` is checked above and the caller guarantees exclusive
        // single-threaded boot access to `CpuSearchSet::perf_scales_`.
        unsafe { cpp_cpu_search_set_set_perf_scale(cpu, perf_scale) };
    }

    /// Called once at percpu secondary init to compute the logical clusters from
    /// the topology-derived distance map.
    ///
    /// # Safety
    ///
    /// Must only be called once during single-threaded secondary CPU initialization after
    /// `CpuDistanceMap::Initialize` has succeeded and before concurrent reads of
    /// `CpuSearchSet::cluster_set_`. `cpu_count` must satisfy
    /// `cpu_count <= CpuDistanceMap::get().cpu_count()` and `cpu_count <= SMP_MAX_CPUS`.
    #[inline]
    pub unsafe fn auto_cluster(cpu_count: usize) {
        assert!(cpu_count <= SMP_MAX_CPUS);
        // SAFETY: `cpu_count <= SMP_MAX_CPUS` is checked above, and the caller guarantees
        // `CpuDistanceMap::Initialize` succeeded with
        // `cpu_count <= CpuDistanceMap::get().cpu_count()` and exclusive single-threaded access to
        // `cluster_set_` during secondary CPU initialization.
        unsafe { cpp_cpu_search_set_auto_cluster(cpu_count) };
    }

    /// Dumps the CPU clusters to the debug log.
    #[inline]
    pub fn dump_clusters() {
        // SAFETY: Reads the global cluster table and formats to debug output.
        unsafe { cpp_cpu_search_set_dump_clusters() };
    }

    /// Called once per CPU at percpu secondary init to compute the unique, cache-
    /// aware CPU search order for the CPUs.
    ///
    /// # Safety
    ///
    /// Must only be called after [`Self::auto_cluster`] during single-threaded secondary CPU
    /// initialization while no other CPU reads or mutates `self`. `(this_cpu as usize)` must be
    /// `< cpu_count`, `cpu_count` must be `<= SMP_MAX_CPUS`, and `cpu_count` must not exceed the
    /// count passed to [`Self::auto_cluster`].
    #[inline]
    pub unsafe fn initialize(&self, this_cpu: cpu_num_t, cpu_count: usize) {
        assert!((this_cpu as usize) < cpu_count && cpu_count <= SMP_MAX_CPUS);
        // SAFETY: `this_cpu < cpu_count <= SMP_MAX_CPUS` is checked above, `self.as_raw_mut()`
        // derives a mutable pointer via `zr::Opaque` (`UnsafeCell`), and the caller guarantees
        // exclusive access to `self` after `auto_cluster` with a `cpu_count` not exceeding the
        // clustered CPU count.
        unsafe { cpp_cpu_search_set_initialize(self.as_raw_mut(), this_cpu, cpu_count) };
    }

    /// Dumps the CPU search list for this set to the debug log.
    #[inline]
    pub fn dump(&self) {
        // SAFETY: `self` points to a valid, initialized `CpuSearchSet`.
        unsafe { cpp_cpu_search_set_dump(self) };
    }

    /// Returns the logical cluster index for this CPU's search set.
    ///
    /// # Safety
    ///
    /// [`Self::auto_cluster`] must have been called for a CPU count covering `self.this_cpu_`.
    #[inline]
    pub unsafe fn cluster(&self) -> usize {
        // SAFETY: `self` is initialized and the caller guarantees `auto_cluster` has populated
        // `cluster_set_.cpu_to_cluster_map` for `self.this_cpu_`.
        unsafe { cpp_cpu_search_set_cluster(self) }
    }
}

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_cpu_search_set_init(out: *mut MaybeUninit<CpuSearchSet>);
    fn cpp_cpu_search_set_set_perf_scale(cpu: cpu_num_t, perf_scale: i64);
    fn cpp_cpu_search_set_auto_cluster(cpu_count: usize);
    fn cpp_cpu_search_set_dump_clusters();
    fn cpp_cpu_search_set_initialize(set: *mut CpuSearchSet, this_cpu: cpu_num_t, cpu_count: usize);
    fn cpp_cpu_search_set_dump(set: *const CpuSearchSet);
    fn cpp_cpu_search_set_cluster(set: *const CpuSearchSet) -> usize;
}
