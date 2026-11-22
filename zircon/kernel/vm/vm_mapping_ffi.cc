// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/types.h>

#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <ktl/tuple.h>
#include <ktl/type_traits.h>
#include <ktl/utility.h>
#include <vm/vm_address_region_ffi.h>

static_assert(ktl::is_trivially_copy_constructible_v<VmMappingProtectionRangesCursor>);
static_assert(ktl::is_trivially_move_constructible_v<VmMappingProtectionRangesCursor>);
static_assert(ktl::is_trivially_copy_assignable_v<VmMappingProtectionRangesCursor>);
static_assert(ktl::is_trivially_move_assignable_v<VmMappingProtectionRangesCursor>);
static_assert(ktl::is_trivially_destructible_v<VmMappingProtectionRangesCursor>);
static_assert(sizeof(ffi::Uninitialized<VmMappingProtectionRangesCursor>) ==
              sizeof(VmMappingProtectionRangesCursor));
static_assert(alignof(ffi::Uninitialized<VmMappingProtectionRangesCursor>) ==
              alignof(VmMappingProtectionRangesCursor));
static_assert(sizeof(ffi::Uninitialized<VmMappingProtectionRanges>) ==
              sizeof(VmMappingProtectionRanges));
static_assert(alignof(ffi::Uninitialized<VmMappingProtectionRanges>) ==
              alignof(VmMappingProtectionRanges));

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_construct(
    ffi::Uninitialized<VmMappingProtectionRanges>* ranges) {
  ranges->Initialize();
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_destroy(VmMappingProtectionRanges* ranges) {
  ktl::destroy_at(ranges);
}
FFI_ALWAYS_INLINE bool cpp_vm_mapping_protection_ranges_is_empty(
    const VmMappingProtectionRanges* ranges) {
  return ranges->is_empty();
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_clear(VmMappingProtectionRanges* ranges) {
  ranges->clear();
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_move_assign(
    VmMappingProtectionRanges* ranges, VmMappingProtectionRanges* other) {
  *ranges = ktl::move(*other);
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_begin(
    const VmMappingProtectionRanges* ranges,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor) {
  out_cursor->Initialize(ranges->begin());
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_end(
    const VmMappingProtectionRanges* ranges,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor) {
  out_cursor->Initialize(ranges->end());
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_upper_bound(
    const VmMappingProtectionRanges* ranges, vaddr_t key,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor) {
  out_cursor->Initialize(ranges->upper_bound(key));
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_lower_bound(
    const VmMappingProtectionRanges* ranges, vaddr_t key,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor) {
  out_cursor->Initialize(ranges->lower_bound(key));
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_find(
    const VmMappingProtectionRanges* ranges, vaddr_t key,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor) {
  out_cursor->Initialize(ranges->find(key));
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_insert(
    VmMappingProtectionRanges* ranges, vaddr_t key, arch_mmu_flags_t flags,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor) {
  out_cursor->Initialize(VmMappingProtectionRanges::const_iterator(ranges->insert(key, flags)));
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_insert_hint(
    VmMappingProtectionRanges* ranges, const VmMappingProtectionRangesCursor* hint, vaddr_t key,
    arch_mmu_flags_t flags, ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor) {
  out_cursor->Initialize(VmMappingProtectionRanges::const_iterator(
      ranges->insert(VmMappingProtectionRanges::iterator(*hint), key, flags)));
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_update(
    VmMappingProtectionRanges* ranges, const VmMappingProtectionRangesCursor* cursor,
    arch_mmu_flags_t flags) {
  ranges->update(VmMappingProtectionRanges::iterator(*cursor), flags);
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_erase(
    VmMappingProtectionRanges* ranges, const VmMappingProtectionRangesCursor* cursor,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor) {
  out_cursor->Initialize(VmMappingProtectionRanges::const_iterator(
      ranges->erase(VmMappingProtectionRanges::iterator(*cursor))));
}
FFI_ALWAYS_INLINE bool cpp_vm_mapping_protection_ranges_cursor_is_valid(
    const VmMappingProtectionRangesCursor* cursor) {
  return cursor->IsValid();
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_cursor_get(
    const VmMappingProtectionRangesCursor* cursor, vaddr_t* out_key, arch_mmu_flags_t* out_flags) {
  ktl::tie(*out_key, *out_flags) = **cursor;
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_cursor_next(
    VmMappingProtectionRangesCursor* cursor) {
  ++*cursor;
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_cursor_prev(
    VmMappingProtectionRangesCursor* cursor) {
  --*cursor;
}
}  // extern "C"
