// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/vm_address_region_ffi.h"

#include <stddef.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <ktl/optional.h>
#include <ktl/type_traits.h>
#include <vm/vm_address_region.h>

// Rust and C++ use the same pointer for a VmAddressRegionOrMapping, VmAddressRegion or VmMapping
// and for its fbl::RefCounted base, which needs that base at offset 0. Standard layout makes the
// base pointer-interconvertible with the object.
static_assert(ktl::is_standard_layout_v<VmAddressRegionOrMapping>);
static_assert(ktl::is_standard_layout_v<VmAddressRegion>);
static_assert(ktl::is_standard_layout_v<VmMapping>);

static_assert(ktl::is_trivially_copy_constructible_v<VmAddressRegionSubregionsCursor>);
static_assert(ktl::is_trivially_move_constructible_v<VmAddressRegionSubregionsCursor>);
static_assert(ktl::is_trivially_copy_assignable_v<VmAddressRegionSubregionsCursor>);
static_assert(ktl::is_trivially_move_assignable_v<VmAddressRegionSubregionsCursor>);
static_assert(ktl::is_trivially_destructible_v<VmAddressRegionSubregionsCursor>);
static_assert(sizeof(ffi::Uninitialized<VmAddressRegionSubregionsCursor>) ==
              sizeof(VmAddressRegionSubregionsCursor));
static_assert(alignof(ffi::Uninitialized<VmAddressRegionSubregionsCursor>) ==
              alignof(VmAddressRegionSubregionsCursor));
static_assert(sizeof(ffi::Uninitialized<VmAddressRegionSubregions>) ==
              sizeof(VmAddressRegionSubregions));
static_assert(alignof(ffi::Uninitialized<VmAddressRegionSubregions>) ==
              alignof(VmAddressRegionSubregions));

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_construct(
    ffi::Uninitialized<VmAddressRegionSubregions>* subregions) {
  subregions->Initialize();
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_destroy(
    VmAddressRegionSubregions* subregions) {
  ktl::destroy_at(subregions);
}
FFI_ALWAYS_INLINE bool cpp_vm_address_region_subregions_is_empty(
    const VmAddressRegionSubregions* subregions) {
  return subregions->is_empty();
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_begin(
    const VmAddressRegionSubregions* subregions,
    ffi::Uninitialized<VmAddressRegionSubregionsCursor>* out_cursor) {
  out_cursor->Initialize(subregions->begin());
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_upper_bound(
    const VmAddressRegionSubregions* subregions, vaddr_t key,
    ffi::Uninitialized<VmAddressRegionSubregionsCursor>* out_cursor) {
  out_cursor->Initialize(subregions->upper_bound(key));
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_find(
    const VmAddressRegionSubregions* subregions, vaddr_t key,
    ffi::Uninitialized<VmAddressRegionSubregionsCursor>* out_cursor) {
  out_cursor->Initialize(subregions->find(key));
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_insert(
    VmAddressRegionSubregions* subregions, vaddr_t key, VmAddressRegionOrMapping* region,
    ffi::Uninitialized<VmAddressRegionSubregionsCursor>* out_cursor) {
  out_cursor->Initialize(
      VmAddressRegionSubregionsCursor(subregions->insert(key, fbl::RefPtr(region))));
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_update(
    VmAddressRegionSubregions* subregions, const VmAddressRegionSubregionsCursor* cursor,
    VmAddressRegionOrMapping* region) {
  subregions->update(VmAddressRegionSubregions::iterator(*cursor), fbl::RefPtr(region));
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_erase(
    VmAddressRegionSubregions* subregions, const VmAddressRegionSubregionsCursor* cursor) {
  subregions->erase(VmAddressRegionSubregions::iterator(*cursor));
}
FFI_ALWAYS_INLINE size_t cpp_vm_address_region_subregions_calculate_utilization_slow(
    const VmAddressRegionSubregions* subregions) {
  return subregions->calculate_utilization_slow().stored_values;
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_address_region_subregions_walk(
    const VmAddressRegionSubregions* subregions,
    zx_status_t (*subtree)(const VmAddressRegionObserverState* state, void* ctx)
        [[clang::cfi_unchecked_callee]],
    zx_status_t (*leaf)(const VmAddressRegionObserverState* state,
                        const VmAddressRegionSubregionsCursor* first,
                        const VmAddressRegionSubregionsCursor* last, void* ctx)
        [[clang::cfi_unchecked_callee]],
    void* ctx) {
  return subregions->walk(
      [subtree, ctx](const VmAddressRegionObserverState& state) { return subtree(&state, ctx); },
      [leaf, ctx](const VmAddressRegionObserverState& state, VmAddressRegionSubregionsCursor first,
                  VmAddressRegionSubregionsCursor last) {
        return leaf(&state, &first, &last, ctx);
      });
}
FFI_ALWAYS_INLINE bool cpp_vm_address_region_subregions_cursor_is_valid(
    const VmAddressRegionSubregionsCursor* cursor) {
  return cursor->IsValid();
}
FFI_ALWAYS_INLINE const VmAddressRegionOrMapping* cpp_vm_address_region_subregions_cursor_get(
    const VmAddressRegionSubregionsCursor* cursor) {
  const VmAddressRegionOrMapping* region = (**cursor).second;
  __builtin_assume(region != nullptr);
  return static_cast<const VmAddressRegionOrMapping*>(
      __builtin_assume_aligned(region, alignof(VmAddressRegionOrMapping)));
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_cursor_next(
    VmAddressRegionSubregionsCursor* cursor) {
  ++*cursor;
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_cursor_prev(
    VmAddressRegionSubregionsCursor* cursor) {
  --*cursor;
}
FFI_ALWAYS_INLINE bool cpp_vm_address_region_subregions_cursor_eq(
    const VmAddressRegionSubregionsCursor* cursor, const VmAddressRegionSubregionsCursor* other) {
  return *cursor == *other;
}
FFI_ALWAYS_INLINE vaddr_t
cpp_vm_address_region_observer_state_min_addr(const VmAddressRegionObserverState* state) {
  return state->min_addr();
}
FFI_ALWAYS_INLINE bool cpp_vm_address_region_observer_state_max_gap(
    const VmAddressRegionObserverState* state, ffi::Uninitialized<size_t>* out_max_gap) {
  ktl::optional<size_t> gap = state->max_gap();
  if (!gap) {
    return false;
  }
  out_max_gap->Initialize(*gap);
  return true;
}
FFI_ALWAYS_INLINE vaddr_t
cpp_vm_address_region_observer_state_max_addr(const VmAddressRegionObserverState* state) {
  return state->max_addr;
}
}  // extern "C"
