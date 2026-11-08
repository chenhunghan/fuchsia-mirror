// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_HEAP_INCLUDE_LIB_HEAP_H_
#define ZIRCON_KERNEL_LIB_HEAP_INCLUDE_LIB_HEAP_H_

#include <lib/heap_ffi.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
#include <zircon/compiler.h>

__BEGIN_CDECLS

// The underlying cmpctmalloc allocator defaults to 8 byte alignment.
#define HEAP_DEFAULT_ALIGNMENT 8

#ifdef _KERNEL

// Enables collection of all unique call sites with unique sizes.
// KERNEL_MEMORY_PROFILER and HEAP_COLLECT_STATS are mutually exclusive.
#define HEAP_COLLECT_STATS 0

// standard heap definitions
void* malloc(size_t size) __MALLOC __ALLOC_SIZE(1);
void* memalign(size_t alignment, size_t size) __MALLOC __ALLOC_SIZE(2);
void* calloc(size_t count, size_t size) __MALLOC __ALLOC_SIZE(1, 2);
void free(void* ptr);
void sized_free(void* ptr, size_t s);

// called once at kernel initialization
#ifdef __cplusplus
inline void heap_init() { rust_heap_init(); }
#else
static inline void heap_init(void) { rust_heap_init(); }
#endif

// Tracks an allocation of the given |size| in the heap profiler. This can be used to make visible
// allocations that, while not from the general kernel heap, are notionally heap-like or from
// separate slab / other non-general heap allocators.
// The return value is a cookie that, along with |size| can be passed to |profile_track_free| to
// indicate deallocation.
static inline uint32_t profile_track_alloc(size_t size) { return rust_profile_track_alloc(size); }
static inline void profile_track_free(uint32_t handle, size_t size) {
  rust_profile_track_free(handle, size);
}

#endif  // _KERNEL

__END_CDECLS

#endif  // ZIRCON_KERNEL_LIB_HEAP_INCLUDE_LIB_HEAP_H_
