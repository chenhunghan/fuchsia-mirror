// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_EVICTOR_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_EVICTOR_FFI_H_

#include <zircon/compiler.h>

#include <kernel/ffi.h>
#include <vm/evictor.h>

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
__BEGIN_CDECLS

using EvictorTestReclaimFn = bool (*)(void* ctx, VmCompression* compression,
                                      Evictor::EvictionLevel eviction_level, bool* out_is_ok,
                                      VmCowReclaimSuccess* out_success,
                                      VmCowReclaimFailure* out_failure);
using EvictorTestFreePagesFn = uint64_t (*)(void* ctx);

FFI_ALWAYS_INLINE void cpp_evictor_disable_eviction(Evictor* evictor);
FFI_ALWAYS_INLINE void cpp_evictor_init(ffi::Uninitialized<Evictor>* evictor);
FFI_ALWAYS_INLINE void cpp_evictor_enable_eviction(Evictor* evictor, bool use_compression);
FFI_ALWAYS_INLINE void cpp_evictor_evict_from_external_target(Evictor* evictor,
                                                              const Evictor::EvictionTarget* target,
                                                              Evictor::EvictionResult* out_result);
FFI_ALWAYS_INLINE void cpp_evictor_evict_synchronous(Evictor* evictor, uint64_t min_mem_to_free,
                                                     uint64_t free_mem_target,
                                                     Evictor::EvictionLevel eviction_level,
                                                     Evictor::Output output,
                                                     Evictor::TriggerReason reason,
                                                     Evictor::EvictionResult* out_result);
FFI_ALWAYS_INLINE void cpp_evictor_evict_asynchronous(Evictor* evictor, uint64_t min_mem_to_free,
                                                      uint64_t free_mem_target,
                                                      Evictor::EvictionLevel eviction_level,
                                                      Evictor::Output output);
FFI_ALWAYS_INLINE bool cpp_evictor_is_eviction_enabled(const Evictor* evictor);
FFI_ALWAYS_INLINE bool cpp_evictor_is_compression_enabled(const Evictor* evictor);
FFI_ALWAYS_INLINE void cpp_evictor_get_global_stats(Evictor::EvictorStats* out_stats);
FFI_ALWAYS_INLINE Thread* cpp_evictor_debug_get_evictor_thread(Evictor* evictor);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_EVICTOR_FFI_H_
