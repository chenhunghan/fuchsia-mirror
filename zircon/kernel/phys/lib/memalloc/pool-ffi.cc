// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "pool-ffi.h"

#include <new>
#include <optional>
#include <span>
#include <type_traits>

namespace memalloc {

static_assert(std::is_trivially_destructible_v<PoolIteratorState>);

extern "C" {

void cpp_new_pool(Pool* pool) { new (pool) Pool(); }

void cpp_destroy_pool(Pool* pool) { pool->~Pool(); }

void cpp_new_range_iteration_context(Range* ranges, size_t size,
                                     internal::RangeIterationContext* out) {
  new (out) internal::RangeIterationContext(std::span<Range>(ranges, size));
}

bool cpp_pool_init(Pool* pool, internal::RangeIterationContext* state, size_t state_count,
                   uint64_t default_min_addr, uint64_t default_max_addr) {
  return pool
      ->InitInternal(std::span<internal::RangeIterationContext>(state, state_count),
                     default_min_addr, default_max_addr)
      .is_ok();
}

bool cpp_pool_empty(const Pool* pool) { return pool->empty(); }

size_t cpp_pool_size(const Pool* pool) { return pool->size(); }

void cpp_pool_begin(const Pool* pool, PoolIteratorState* out) {
  new (out) PoolIteratorState{pool->begin()};
}

const Range* cpp_pool_iter_next(const Pool* pool, PoolIteratorState* state) {
  return state->it == pool->end() ? nullptr : &*state->it++;
}

bool cpp_pool_allocate(Pool* pool, Type type, uint64_t size, uint64_t alignment,
                       const uint64_t* min_addr, const uint64_t* max_addr, uint64_t* out_addr) {
  std::optional<uint64_t> min = min_addr ? std::optional(*min_addr) : std::nullopt;
  std::optional<uint64_t> max = max_addr ? std::optional(*max_addr) : std::nullopt;
  auto result = pool->Allocate(type, size, alignment, min, max);
  if (result.is_error()) {
    return false;
  }
  *out_addr = result.value();
  return true;
}

bool cpp_pool_free(Pool* pool, uint64_t addr, uint64_t size) {
  return pool->Free(addr, size).is_ok();
}

bool cpp_pool_resize(Pool* pool, const Range* original, uint64_t new_size, uint64_t min_alignment,
                     uint64_t* out_addr) {
  auto result = pool->Resize(*original, new_size, min_alignment);
  if (result.is_error()) {
    return false;
  }
  *out_addr = result.value();
  return true;
}

const Range* cpp_pool_find_containing_range(const Pool* pool, uint64_t addr) {
  auto it = pool->FindContainingRange(addr);
  return it == pool->end() ? nullptr : &*it;
}

bool cpp_pool_update_ram_subranges(Pool* pool, Type type, uint64_t addr, uint64_t size) {
  return pool->UpdateRamSubranges(type, addr, size).is_ok();
}

bool cpp_pool_truncate_total_ram(Pool* pool, uint64_t new_capacity_bytes) {
  return pool->TruncateTotalRam(new_capacity_bytes).is_ok();
}

bool cpp_pool_mark_as_peripheral(Pool* pool, const Range* range) {
  return pool->MarkAsPeripheral(*range).is_ok();
}

bool cpp_pool_coalesce_peripherals(Pool* pool, const size_t* alignments, size_t count) {
  return pool->CoalescePeripherals(std::span<const size_t>(alignments, count)).is_ok();
}

}  // extern "C"

}  // namespace memalloc
