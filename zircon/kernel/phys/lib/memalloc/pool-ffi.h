// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_LIB_MEMALLOC_POOL_FFI_H_
#define ZIRCON_KERNEL_PHYS_LIB_MEMALLOC_POOL_FFI_H_

#include <lib/memalloc/pool.h>
#include <lib/memalloc/range.h>

#include <cstddef>
#include <cstdint>

namespace memalloc {

struct PoolIteratorState {
  Pool::iterator it;
};

extern "C" {

void cpp_new_pool(Pool* pool);

void cpp_destroy_pool(Pool* pool);

void cpp_new_range_iteration_context(Range* ranges, size_t size,
                                     internal::RangeIterationContext* out);

bool cpp_pool_init(Pool* pool, internal::RangeIterationContext* state, size_t state_count,
                   uint64_t default_min_addr, uint64_t default_max_addr);

bool cpp_pool_empty(const Pool* pool);

size_t cpp_pool_size(const Pool* pool);

void cpp_pool_begin(const Pool* pool, PoolIteratorState* out);

const Range* cpp_pool_iter_next(const Pool* pool, PoolIteratorState* state);

bool cpp_pool_allocate(Pool* pool, Type type, uint64_t size, uint64_t alignment,
                       const uint64_t* min_addr, const uint64_t* max_addr, uint64_t* out_addr);

bool cpp_pool_free(Pool* pool, uint64_t addr, uint64_t size);

bool cpp_pool_resize(Pool* pool, const Range* original, uint64_t new_size, uint64_t min_alignment,
                     uint64_t* out_addr);

const Range* cpp_pool_find_containing_range(const Pool* pool, uint64_t addr);

bool cpp_pool_update_ram_subranges(Pool* pool, Type type, uint64_t addr, uint64_t size);

bool cpp_pool_truncate_total_ram(Pool* pool, uint64_t new_capacity_bytes);

bool cpp_pool_mark_as_peripheral(Pool* pool, const Range* range);

bool cpp_pool_coalesce_peripherals(Pool* pool, const size_t* alignments, size_t count);

}  // extern "C"

}  // namespace memalloc

#endif  // ZIRCON_KERNEL_PHYS_LIB_MEMALLOC_POOL_FFI_H_
