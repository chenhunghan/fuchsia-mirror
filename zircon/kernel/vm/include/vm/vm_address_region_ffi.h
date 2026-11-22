// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_FFI_H_

#include <lib/btree.h>
#include <stddef.h>
#include <sys/types.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <vm/arch_vm_aspace.h>
#include <vm/vm_address_region.h>
#include <vm/vm_address_region_observer.h>
#include <vm/vm_constants.h>

using VmAddressRegionSubregions =
    btree::BTree<vaddr_t, fbl::RefPtr<VmAddressRegionOrMapping>, VmAddressRegionObserver>;
static_assert(sizeof(VmAddressRegionSubregions) == kVmAddressRegionSubregionsSize);
static_assert(alignof(VmAddressRegionSubregions) == kVmAddressRegionSubregionsAlign);
using VmAddressRegionSubregionsCursor = VmAddressRegionSubregions::const_iterator;
static_assert(sizeof(VmAddressRegionSubregionsCursor) == kVmAddressRegionSubregionsCursorSize);
static_assert(alignof(VmAddressRegionSubregionsCursor) == kVmAddressRegionSubregionsCursorAlign);
using VmAddressRegionObserverState = VmAddressRegionObserver::State;

using VmMappingProtectionRanges = btree::BTree<vaddr_t, arch_mmu_flags_t>;
static_assert(sizeof(VmMappingProtectionRanges) == kVmMappingProtectionRangesSize);
static_assert(alignof(VmMappingProtectionRanges) == kVmMappingProtectionRangesAlign);
using VmMappingProtectionRangesCursor = VmMappingProtectionRanges::const_iterator;
static_assert(sizeof(VmMappingProtectionRangesCursor) == kVmMappingProtectionRangesCursorSize);
static_assert(alignof(VmMappingProtectionRangesCursor) == kVmMappingProtectionRangesCursorAlign);

__BEGIN_CDECLS

void cpp_vm_address_region_subregions_construct(
    ffi::Uninitialized<VmAddressRegionSubregions>* subregions);
void cpp_vm_address_region_subregions_destroy(VmAddressRegionSubregions* subregions);
bool cpp_vm_address_region_subregions_is_empty(const VmAddressRegionSubregions* subregions);
void cpp_vm_address_region_subregions_begin(
    const VmAddressRegionSubregions* subregions,
    ffi::Uninitialized<VmAddressRegionSubregionsCursor>* out_cursor);
void cpp_vm_address_region_subregions_upper_bound(
    const VmAddressRegionSubregions* subregions, vaddr_t key,
    ffi::Uninitialized<VmAddressRegionSubregionsCursor>* out_cursor);
void cpp_vm_address_region_subregions_find(
    const VmAddressRegionSubregions* subregions, vaddr_t key,
    ffi::Uninitialized<VmAddressRegionSubregionsCursor>* out_cursor);
void cpp_vm_address_region_subregions_insert(
    VmAddressRegionSubregions* subregions, vaddr_t key, VmAddressRegionOrMapping* region,
    ffi::Uninitialized<VmAddressRegionSubregionsCursor>* out_cursor);
void cpp_vm_address_region_subregions_update(VmAddressRegionSubregions* subregions,
                                             const VmAddressRegionSubregionsCursor* cursor,
                                             VmAddressRegionOrMapping* region);
void cpp_vm_address_region_subregions_erase(VmAddressRegionSubregions* subregions,
                                            const VmAddressRegionSubregionsCursor* cursor);
size_t cpp_vm_address_region_subregions_calculate_utilization_slow(
    const VmAddressRegionSubregions* subregions);
// TODO(https://fxbug.dev/528405969): Remove cfi_unchecked_callee once cross-language
// CFI between Clang C++ and rustc is supported in the kernel.
zx_status_t cpp_vm_address_region_subregions_walk(
    const VmAddressRegionSubregions* subregions,
    zx_status_t (*subtree)(const VmAddressRegionObserverState* state, void* ctx)
        [[clang::cfi_unchecked_callee]],
    zx_status_t (*leaf)(const VmAddressRegionObserverState* state,
                        const VmAddressRegionSubregionsCursor* first,
                        const VmAddressRegionSubregionsCursor* last, void* ctx)
        [[clang::cfi_unchecked_callee]],
    void* ctx);
bool cpp_vm_address_region_subregions_cursor_is_valid(
    const VmAddressRegionSubregionsCursor* cursor);
const VmAddressRegionOrMapping* cpp_vm_address_region_subregions_cursor_get(
    const VmAddressRegionSubregionsCursor* cursor);
void cpp_vm_address_region_subregions_cursor_next(VmAddressRegionSubregionsCursor* cursor);
void cpp_vm_address_region_subregions_cursor_prev(VmAddressRegionSubregionsCursor* cursor);
bool cpp_vm_address_region_subregions_cursor_eq(const VmAddressRegionSubregionsCursor* cursor,
                                                const VmAddressRegionSubregionsCursor* other);
vaddr_t cpp_vm_address_region_observer_state_min_addr(const VmAddressRegionObserverState* state);
bool cpp_vm_address_region_observer_state_max_gap(const VmAddressRegionObserverState* state,
                                                  ffi::Uninitialized<size_t>* out_max_gap);
vaddr_t cpp_vm_address_region_observer_state_max_addr(const VmAddressRegionObserverState* state);

void cpp_vm_mapping_protection_ranges_construct(
    ffi::Uninitialized<VmMappingProtectionRanges>* ranges);
void cpp_vm_mapping_protection_ranges_destroy(VmMappingProtectionRanges* ranges);
bool cpp_vm_mapping_protection_ranges_is_empty(const VmMappingProtectionRanges* ranges);
void cpp_vm_mapping_protection_ranges_clear(VmMappingProtectionRanges* ranges);
void cpp_vm_mapping_protection_ranges_move_assign(VmMappingProtectionRanges* ranges,
                                                  VmMappingProtectionRanges* other);
void cpp_vm_mapping_protection_ranges_begin(
    const VmMappingProtectionRanges* ranges,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor);
void cpp_vm_mapping_protection_ranges_end(
    const VmMappingProtectionRanges* ranges,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor);
void cpp_vm_mapping_protection_ranges_upper_bound(
    const VmMappingProtectionRanges* ranges, vaddr_t key,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor);
void cpp_vm_mapping_protection_ranges_lower_bound(
    const VmMappingProtectionRanges* ranges, vaddr_t key,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor);
void cpp_vm_mapping_protection_ranges_find(
    const VmMappingProtectionRanges* ranges, vaddr_t key,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor);
void cpp_vm_mapping_protection_ranges_insert(
    VmMappingProtectionRanges* ranges, vaddr_t key, arch_mmu_flags_t flags,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor);
void cpp_vm_mapping_protection_ranges_insert_hint(
    VmMappingProtectionRanges* ranges, const VmMappingProtectionRangesCursor* hint, vaddr_t key,
    arch_mmu_flags_t flags, ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor);
void cpp_vm_mapping_protection_ranges_update(VmMappingProtectionRanges* ranges,
                                             const VmMappingProtectionRangesCursor* cursor,
                                             arch_mmu_flags_t flags);
void cpp_vm_mapping_protection_ranges_erase(
    VmMappingProtectionRanges* ranges, const VmMappingProtectionRangesCursor* cursor,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor);
bool cpp_vm_mapping_protection_ranges_cursor_is_valid(
    const VmMappingProtectionRangesCursor* cursor);
void cpp_vm_mapping_protection_ranges_cursor_get(const VmMappingProtectionRangesCursor* cursor,
                                                 vaddr_t* out_key, arch_mmu_flags_t* out_flags);
void cpp_vm_mapping_protection_ranges_cursor_next(VmMappingProtectionRangesCursor* cursor);
void cpp_vm_mapping_protection_ranges_cursor_prev(VmMappingProtectionRangesCursor* cursor);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_FFI_H_
