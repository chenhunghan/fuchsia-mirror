// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use zr::OpaqueFacade;

/// A compact distance matrix storing the metric distance between CPUs.
#[repr(C)]
pub struct CpuDistanceMap {
    _facade: OpaqueFacade,
}

impl CpuDistanceMap {
    /// Returns the global [`CpuDistanceMap`] instance.
    ///
    /// # Safety
    ///
    /// `CpuDistanceMap::Initialize` must have succeeded before calling this function so that
    /// `g_cpu_distance_map` holds an initialized instance.
    #[inline]
    pub unsafe fn get() -> &'static Self {
        // SAFETY: Caller guarantees `CpuDistanceMap::Initialize` has succeeded, so
        // `CpuDistanceMap::Get()` returns a valid reference living for `'static`.
        unsafe { &*cpp_cpu_distance_map_get() }
    }

    /// Returns the number of CPUs in the distance map.
    #[inline]
    pub fn cpu_count(&self) -> usize {
        // SAFETY: `self` is a valid reference to an initialized `CpuDistanceMap`.
        unsafe { cpp_cpu_distance_map_cpu_count(self) }
    }
}

unsafe extern "C" {
    fn cpp_cpu_distance_map_get() -> *const CpuDistanceMap;
    fn cpp_cpu_distance_map_cpu_count(map: *const CpuDistanceMap) -> usize;
}
