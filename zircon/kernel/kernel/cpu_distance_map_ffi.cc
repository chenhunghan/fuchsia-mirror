// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/cpu_distance_map.h>
#include <kernel/ffi.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const CpuDistanceMap* cpp_cpu_distance_map_get() {
  return &CpuDistanceMap::Get();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_cpu_distance_map_cpu_count(const CpuDistanceMap* map) {
  DEBUG_ASSERT(map != nullptr);
  return map->cpu_count();
}

}  // extern "C"
