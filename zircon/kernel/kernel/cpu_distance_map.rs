// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::{SMP_MAX_CPUS, cpu_num_t};
use core::ffi::c_void;
#[cfg(ktest)]
use core::ops::Deref;
#[cfg(ktest)]
use core::ptr::NonNull;
use zr::OpaqueFacade;

// Ensures `cpu_count <= SMP_MAX_CPUS` implies `cpu_count <= usize::from(u16::MAX)`, preventing
// 32-bit `index.i * index.i + index.i` overflow in C++ `CpuDistanceMap::LinearIndex`.
zr::static_assert!(SMP_MAX_CPUS <= u16::MAX as usize);

/// The value type for metric distances.
pub type Distance = u32;

/// The signature of the distance callback `cpp_cpu_distance_map_init` takes.
type CpuDistanceCallback =
    unsafe extern "C" fn(from_id: cpu_num_t, to_id: cpu_num_t, ctx: *mut c_void) -> Distance;

/// Shared C callback trampoline forwarding `(from_id, to_id)` to a Rust closure `F`.
///
/// # Safety
///
/// The caller must ensure that `ctx` is non-null, properly aligned for `F`, and points to a live,
/// initialized `F` with exclusive mutable access for the duration of this call (in particular, `F`
/// must not re-entrantly invoke a callback using the same `ctx`).
unsafe extern "C" fn distance_trampoline<F>(
    from_id: cpu_num_t,
    to_id: cpu_num_t,
    ctx: *mut c_void,
) -> Distance
where
    F: FnMut(cpu_num_t, cpu_num_t) -> Distance,
{
    // SAFETY: The caller guarantees `ctx` is non-null, properly aligned for `F`, and points to an
    // initialized `F` with exclusive mutable access for the duration of this call.
    let cb = unsafe { &mut *ctx.cast::<F>() };
    cb(from_id, to_id)
}

/// Index pair that sorts the index elements so that `i <= j`.
pub struct Index {
    // Safety invariant: `self.i <= self.j`, established by `Index::new` (the sole constructor).
    /// Lower logical CPU index (`min(i, j)`).
    i: cpu_num_t,
    /// Higher logical CPU index (`max(i, j)`).
    j: cpu_num_t,
}

impl Index {
    /// Creates an index pair sorting `(i, j)` so that `self.i <= self.j`.
    #[inline]
    pub fn new(i: cpu_num_t, j: cpu_num_t) -> Self {
        Self { i: i.min(j), j: i.max(j) }
    }
}

impl From<(cpu_num_t, cpu_num_t)> for Index {
    #[inline]
    fn from((i, j): (cpu_num_t, cpu_num_t)) -> Self {
        Self::new(i, j)
    }
}

/// A compact distance matrix storing the metric distance between CPUs.
// Safety invariant: Every valid `&CpuDistanceMap` refers to an initialized C++ `CpuDistanceMap`
// with `cpu_count <= usize::from(u16::MAX)` (enforced by `CpuDistanceMap::initialize`,
// `CpuDistanceMap::get`, and `TestCpuDistanceMap::create`), so 32-bit `index.i * index.i + index.i`
// in C++ `CpuDistanceMap::LinearIndex` cannot overflow for `index.i < index.j < cpu_count`.
#[repr(C)]
pub struct CpuDistanceMap {
    _facade: OpaqueFacade,
}

impl CpuDistanceMap {
    /// Initializes the global [`CpuDistanceMap`] with distances computed by `f`, which is called
    /// once for each unique pair of the `cpu_count` CPUs.
    ///
    /// # Panics
    ///
    /// Panics if `cpu_count > SMP_MAX_CPUS`.
    ///
    /// # Safety
    ///
    /// Must be called at most once, on the boot CPU before it starts any secondary CPU, and before
    /// anything else uses the global map.
    pub unsafe fn initialize<F>(cpu_count: usize, mut f: F)
    where
        F: FnMut(cpu_num_t, cpu_num_t) -> Distance,
    {
        assert!(cpu_count <= SMP_MAX_CPUS);
        let ctx = core::ptr::from_mut(&mut f).cast::<c_void>();
        // SAFETY: `cpu_count <= SMP_MAX_CPUS <= u16::MAX` prevents 32-bit index overflow in C++
        // `LinearIndex`. `ctx` is derived from local `f` via `core::ptr::from_mut` and does not
        // escape, and C++ `CpuDistanceMap::Initialize` invokes `distance_trampoline::<F>`
        // synchronously and sequentially with that `ctx`, giving each callback invocation exclusive
        // mutable access to `f`. The caller guarantees nothing else is using the global map.
        unsafe {
            cpp_cpu_distance_map_init(cpu_count, distance_trampoline::<F>, ctx);
        }
    }

    /// Returns the global [`CpuDistanceMap`] instance.
    ///
    /// # Panics
    ///
    /// Panics if the global map's `cpu_count` exceeds `SMP_MAX_CPUS`.
    ///
    /// # Safety
    ///
    /// `CpuDistanceMap::Initialize` must have succeeded before calling this function so that
    /// `g_cpu_distance_map` holds an initialized instance.
    #[inline]
    pub unsafe fn get() -> &'static Self {
        // SAFETY: Caller guarantees `CpuDistanceMap::Initialize` has succeeded, so
        // `cpp_cpu_distance_map_get()` returns a valid pointer to an initialized `'static`
        // `CpuDistanceMap`. Asserting `cpu_count <= SMP_MAX_CPUS` (and `SMP_MAX_CPUS <= u16::MAX`)
        // before forming `&Self` upholds `CpuDistanceMap`'s `cpu_count <= u16::MAX` safety
        // invariant even when C++ `CpuDistanceMap::Initialize` was called directly.
        unsafe {
            let ptr = cpp_cpu_distance_map_get();
            assert!(cpp_cpu_distance_map_cpu_count(ptr) <= SMP_MAX_CPUS);
            &*ptr
        }
    }

    /// Returns the distance for the given index pair `(i, j)`.
    ///
    /// # Panics
    ///
    /// Panics if `i != j` and either CPU index is `>= self.cpu_count()`.
    #[inline]
    pub fn distance(&self, index: impl Into<Index>) -> Distance {
        let index = index.into();
        if index.i != index.j {
            let cpu_count = self.cpu_count();
            // Unconditional check because C++ `CpuDistanceMap::operator[]` only `DEBUG_ASSERT`s
            // bounds and indexes `entries_` unchecked when `LK_DEBUGLEVEL == 0`. When
            // `index.i == index.j`, C++ `operator[]` returns `0` immediately before calling
            // `LinearIndex` or indexing `entries_`, so out-of-range equal indices are harmless.
            assert!((index.i as usize) < cpu_count && (index.j as usize) < cpu_count);
        }
        // SAFETY:
        // - `self` is a valid reference to an initialized `CpuDistanceMap`, which by type
        //   invariant has `cpu_count <= usize::from(u16::MAX)`.
        // - When `index.i == index.j`, C++ `CpuDistanceMap::operator[]` returns `0` immediately
        //   without calling `LinearIndex` or accessing `entries_`.
        // - When `index.i != index.j`, `Index`'s safety invariant (`index.i <= index.j`) plus the
        //   `assert!` above guarantees `index.i < index.j < cpu_count <= usize::from(u16::MAX)`.
        //   Consequently `index.i < u16::MAX`, so the 32-bit `index.i * index.i + index.i` in C++
        //   `LinearIndex` cannot wrap, and the resulting linear index is in bounds of `entries_`.
        unsafe { cpp_cpu_distance_map_index(self, index.i, index.j) }
    }

    /// Returns the number of CPUs in the distance map.
    #[inline]
    pub fn cpu_count(&self) -> usize {
        // SAFETY: `self` is a valid reference to an initialized `CpuDistanceMap`.
        unsafe { cpp_cpu_distance_map_cpu_count(self) }
    }

    /// Returns the metric distance representing the first significant distance in the map.
    #[inline]
    pub fn distance_threshold(&self) -> Distance {
        // SAFETY: `self` is a valid reference to an initialized `CpuDistanceMap`.
        unsafe { cpp_cpu_distance_map_distance_threshold(self) }
    }

    /// Sets the metric distance representing the first significant distance in the global map.
    /// The value is not used directly by this class. Instead, it is provided as a convenience for
    /// reference by consumers when processing map values.
    ///
    /// For example, this value may be used to communicate the threshold for auto clustering from
    /// the producer of the map to the clustering logic.
    ///
    /// # Safety
    ///
    /// [`Self::initialize`] must have been called, and nothing else may be using the global map.
    /// `CpuDistanceMap::Get()` does not check that the allocation in `initialize` succeeded.
    #[inline]
    pub unsafe fn set_distance_threshold(distance_threshold: Distance) {
        // SAFETY: The callee takes a plain integer, and the caller meets the requirements of the
        // `CpuDistanceMap::Get().set_distance_threshold()` it makes.
        unsafe { cpp_cpu_distance_map_set_distance_threshold(distance_threshold) };
    }

    /// Prints the global map.
    ///
    /// # Safety
    ///
    /// [`Self::initialize`] must have been called, as for [`Self::set_distance_threshold`].
    #[inline]
    pub unsafe fn dump() {
        // SAFETY: The callee takes no arguments, and the caller meets the requirements of the
        // `CpuDistanceMap::Get().Dump()` it makes.
        unsafe { cpp_cpu_distance_map_dump() };
    }
}

/// Owned test-only [`CpuDistanceMap`] instance for hermetic unit tests.
#[cfg(ktest)]
pub struct TestCpuDistanceMap(
    // Safety invariant: Points to a live, initialized, exclusively owned `CpuDistanceMap`
    // with `cpu_count <= usize::from(u16::MAX)` allocated by `cpp_cpu_distance_map_create_for_test`
    // until dropped in `Drop::drop`.
    NonNull<CpuDistanceMap>,
);

#[cfg(ktest)]
impl TestCpuDistanceMap {
    /// Creates a local [`CpuDistanceMap`] with `cpu_count` CPUs and distances computed by `f`.
    pub fn create<F>(cpu_count: usize, mut f: F) -> Option<Self>
    where
        F: FnMut(cpu_num_t, cpu_num_t) -> Distance,
    {
        // Bound `cpu_count` so C++ `CpuDistanceMap::LinearIndex` cannot overflow 32-bit
        // `index.i * index.i` arithmetic when `LK_DEBUGLEVEL == 0`.
        if cpu_count > usize::from(u16::MAX) {
            return None;
        }

        let ctx = core::ptr::from_mut(&mut f).cast::<c_void>();
        // SAFETY: `cpu_count <= u16::MAX` prevents 32-bit index overflow in `LinearIndex`. `ctx`
        // is derived from local `f` via `core::ptr::from_mut` and does not escape, and C++
        // `CpuDistanceMap::Create` invokes `distance_trampoline::<F>` synchronously and
        // sequentially on its own local `(cb, ctx)`, giving each callback invocation exclusive
        // mutable access to `f`.
        let ptr = unsafe {
            cpp_cpu_distance_map_create_for_test(cpu_count, distance_trampoline::<F>, ctx)
        };
        NonNull::new(ptr).map(Self)
    }

    /// Sets the significant distance threshold on this map instance.
    #[inline]
    pub fn set_distance_threshold(&mut self, distance_threshold: Distance) {
        // SAFETY: `self.0` points to a valid, exclusively owned `CpuDistanceMap`.
        unsafe { cpp_cpu_distance_map_set_threshold_for_test(self.0.as_ptr(), distance_threshold) };
    }
}

#[cfg(ktest)]
impl Deref for TestCpuDistanceMap {
    type Target = CpuDistanceMap;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `self.0` points to a valid, initialized `CpuDistanceMap` with
        // `cpu_count <= usize::from(u16::MAX)` owned by `self`.
        unsafe { self.0.as_ref() }
    }
}

#[cfg(ktest)]
impl Drop for TestCpuDistanceMap {
    fn drop(&mut self) {
        // SAFETY: `self.0` was allocated by `cpp_cpu_distance_map_create_for_test` and is dropped
        // at most once here.
        unsafe { cpp_cpu_distance_map_destroy_for_test(self.0.as_ptr()) };
    }
}

unsafe extern "C" {
    fn cpp_cpu_distance_map_init(cpu_count: usize, cb: CpuDistanceCallback, ctx: *mut c_void);
    fn cpp_cpu_distance_map_set_distance_threshold(threshold: Distance);
    fn cpp_cpu_distance_map_dump();
    fn cpp_cpu_distance_map_get() -> *const CpuDistanceMap;
    fn cpp_cpu_distance_map_cpu_count(map: *const CpuDistanceMap) -> usize;
    fn cpp_cpu_distance_map_distance_threshold(map: *const CpuDistanceMap) -> Distance;
    fn cpp_cpu_distance_map_index(
        map: *const CpuDistanceMap,
        i: cpu_num_t,
        j: cpu_num_t,
    ) -> Distance;
    #[cfg(ktest)]
    fn cpp_cpu_distance_map_set_threshold_for_test(map: *mut CpuDistanceMap, threshold: Distance);
    #[cfg(ktest)]
    fn cpp_cpu_distance_map_create_for_test(
        cpu_count: usize,
        cb: CpuDistanceCallback,
        ctx: *mut c_void,
    ) -> *mut CpuDistanceMap;
    #[cfg(ktest)]
    fn cpp_cpu_distance_map_destroy_for_test(map: *mut CpuDistanceMap);
}
