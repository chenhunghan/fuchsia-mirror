// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/evictor_ffi.h"

#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <vm/vm_cow_pages.h>

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE void cpp_evictor_disable_eviction(Evictor* evictor) {
  evictor->DisableEviction();
}

FFI_ALWAYS_INLINE void cpp_evictor_init(ffi::Uninitialized<Evictor>* evictor) {
  evictor->Initialize();
}

FFI_ALWAYS_INLINE void cpp_evictor_enable_eviction(Evictor* evictor, bool use_compression) {
  evictor->EnableEviction(use_compression);
}

FFI_ALWAYS_INLINE void cpp_evictor_evict_from_external_target(Evictor* evictor,
                                                              const Evictor::EvictionTarget* target,
                                                              Evictor::EvictionResult* out_result) {
  *out_result = evictor->EvictFromExternalTarget(*target);
}

FFI_ALWAYS_INLINE void cpp_evictor_evict_synchronous(Evictor* evictor, uint64_t min_mem_to_free,
                                                     uint64_t free_mem_target,
                                                     Evictor::EvictionLevel eviction_level,
                                                     Evictor::Output output,
                                                     Evictor::TriggerReason reason,
                                                     Evictor::EvictionResult* out_result) {
  *out_result =
      evictor->EvictSynchronous(min_mem_to_free, free_mem_target, eviction_level, output, reason);
}

FFI_ALWAYS_INLINE void cpp_evictor_evict_asynchronous(Evictor* evictor, uint64_t min_mem_to_free,
                                                      uint64_t free_mem_target,
                                                      Evictor::EvictionLevel eviction_level,
                                                      Evictor::Output output) {
  evictor->EvictAsynchronous(min_mem_to_free, free_mem_target, eviction_level, output);
}

FFI_ALWAYS_INLINE bool cpp_evictor_is_eviction_enabled(const Evictor* evictor) {
  return evictor->IsEvictionEnabled();
}

FFI_ALWAYS_INLINE bool cpp_evictor_is_compression_enabled(const Evictor* evictor) {
  return evictor->IsCompressionEnabled();
}

FFI_ALWAYS_INLINE void cpp_evictor_get_global_stats(Evictor::EvictorStats* out_stats) {
  *out_stats = Evictor::GetGlobalStats();
}

FFI_ALWAYS_INLINE Thread* cpp_evictor_debug_get_evictor_thread(Evictor* evictor) {
  return evictor->DebugGetEvictorThread();
}

}  // extern "C"
