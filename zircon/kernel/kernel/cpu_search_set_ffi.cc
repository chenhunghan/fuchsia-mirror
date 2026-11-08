// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/cpu_search_set.h>
#include <kernel/ffi.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_search_set_init(ffi::Uninitialized<CpuSearchSet>* out) {
  DEBUG_ASSERT(out != nullptr);
  out->Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_search_set_set_perf_scale(cpu_num_t cpu, int64_t perf_scale) {
  CpuSearchSet::SetPerfScale(cpu, perf_scale);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_search_set_auto_cluster(size_t cpu_count) {
  CpuSearchSet::AutoCluster(cpu_count);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_search_set_dump_clusters() { CpuSearchSet::DumpClusters(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_search_set_initialize(CpuSearchSet* set, cpu_num_t this_cpu,
                                                     size_t cpu_count) {
  DEBUG_ASSERT(set != nullptr);
  set->Initialize(this_cpu, cpu_count);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_search_set_dump(const CpuSearchSet* set) {
  DEBUG_ASSERT(set != nullptr);
  set->Dump();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_cpu_search_set_cluster(const CpuSearchSet* set) {
  DEBUG_ASSERT(set != nullptr);
  return set->cluster();
}

}  // extern "C"
