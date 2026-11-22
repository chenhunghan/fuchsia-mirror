// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <stddef.h>

#include <kernel/cpu.h>
#include <kernel/cpu_distance_map.h>
#include <kernel/ffi.h>

#ifdef UNITTESTS_ENABLED
#include <fbl/alloc_checker.h>
#include <ktl/utility.h>
#endif

#include <ktl/enforce.h>

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

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE CpuDistanceMap::Distance cpp_cpu_distance_map_distance_threshold(
    const CpuDistanceMap* map) {
  DEBUG_ASSERT(map != nullptr);
  return map->distance_threshold();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE CpuDistanceMap::Distance cpp_cpu_distance_map_index(const CpuDistanceMap* map,
                                                                      cpu_num_t i, cpu_num_t j) {
  DEBUG_ASSERT(map != nullptr);
  return (*map)[{i, j}];
}

#ifdef UNITTESTS_ENABLED
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_distance_map_set_threshold_for_test(
    CpuDistanceMap* map, CpuDistanceMap::Distance threshold) {
  DEBUG_ASSERT(map != nullptr);
  map->set_distance_threshold(threshold);
}

CpuDistanceMap* cpp_cpu_distance_map_create_for_test(size_t cpu_count, CpuDistanceCallback cb,
                                                     void* ctx) {
  auto result = CpuDistanceMap::Create(
      cpu_count, [cb, ctx](cpu_num_t from_id, cpu_num_t to_id) { return cb(from_id, to_id, ctx); });
  if (!result) {
    return nullptr;
  }
  fbl::AllocChecker ac;
  CpuDistanceMap* map = new (&ac) CpuDistanceMap{ktl::move(*result)};
  if (!ac.check()) {
    return nullptr;
  }
  return map;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_distance_map_destroy_for_test(CpuDistanceMap* map) { delete map; }
#endif

}  // extern "C"
