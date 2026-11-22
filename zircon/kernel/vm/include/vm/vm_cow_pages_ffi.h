// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_COW_PAGES_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_COW_PAGES_FFI_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <vm/anonymous_page_request.h>

#include "vm/discardable_vmo_tracker.h"
#include "vm/vm_cow_pages.h"
__BEGIN_CDECLS

FFI_ALWAYS_INLINE void* cpp_vm_cow_pages_state(VmCowPages* cow);

// The Rust `VmCowRange` in vm_cow_pages.rs mirrors this struct.
static_assert(sizeof(VmCowRange) == 16);
static_assert(alignof(VmCowRange) == 8);
static_assert(offsetof(VmCowRange, offset) == 0);
static_assert(offsetof(VmCowRange, len) == 8);

zx_status_t cpp_vm_cow_pages_replace_page_with_loaned(VmCowPages* cow, vm_page_t* before_page,
                                                      uint64_t offset);
zx_status_t cpp_vm_cow_pages_replace_page(VmCowPages* cow, vm_page_t* before_page, uint64_t offset,
                                          bool with_loaned, vm_page_t** after_page,
                                          AnonymousPageRequest* page_request);
void* cpp_vm_cow_pages_get_ref_counted(const VmCowPages* cow);
void cpp_vm_cow_pages_free(VmCowPages* cow);
bool cpp_vm_cow_pages_dedup_zero_page(VmCowPages* cow, vm_page_t* page, uint64_t offset);
zx_status_t cpp_vm_cow_pages_evict_loaned_page(VmCowPages* cow, vm_page_t* page, uint64_t offset);
bool cpp_vm_cow_pages_lock_is_held(const VmCowPages* cow);
__PURE DiscardableVmoTracker* cpp_vm_cow_pages_discardable_tracker(const VmCowPages* cow);
bool cpp_vm_cow_pages_reclaim_page(VmCowPages* cow, vm_page_t* page, uint64_t offset,
                                   VmCowPages::EvictionAction eviction_action,
                                   VmCompressor* compressor, VmCowReclaimSuccess* out_success,
                                   VmCowReclaimFailure* out_failure);
void* cpp_vm_cow_pages_get_lock(const VmCowPages* cow);
// Invoke the VmObject::LookupFunction (VmCowPages::LookupReadableFunction) /
// VmCowPages::DirtyRangeEnumerateFunction that |lookup_fn| / |dirty_range_fn| point at.
zx_status_t cpp_vm_cow_pages_call_lookup_fn(void* lookup_fn, uint64_t offset, paddr_t pa);
zx_status_t cpp_vm_cow_pages_call_dirty_range_fn(void* dirty_range_fn, uint64_t range_offset,
                                                 uint64_t range_len, bool range_is_zero);
void cpp_vm_cow_pages_range_change_update_cow_children(VmCowPages* cow, uint64_t offset,
                                                       uint64_t len, VmObject::RangeChangeOp op);
void cpp_vm_cow_pages_resize_update_child_limits(VmCowPages* cow, uint64_t s);
// |deferred| is a VmCowPages::DeferredOps* (see the declaration in vm_cow_pages.h).
zx_status_t cpp_vm_cow_pages_replace_page_locked(VmCowPages* cow, vm_page_t* before_page,
                                                 uint64_t offset, bool with_loaned,
                                                 vm_page_t** after_page, void* deferred,
                                                 AnonymousPageRequest* page_request);
zx_status_t cpp_vm_cow_pages_supply_pages_locked(VmCowPages* cow, VmCowRange range,
                                                 VmPageSpliceList* pages, SupplyOptions options,
                                                 VmCowPages::DeferredOps* deferred,
                                                 MultiPageRequest* page_request);
VmCowPages* cpp_vm_cow_pages_upgrade_from_raw(VmCowPages* cow);
void cpp_vm_cow_pages_debug_dump_reclaim_counters();
__PURE VmObjectPaged* cpp_vm_cow_pages_get_paged_backlink_locked(const VmCowPages* cow);

void cpp_priority_changer_construct(ffi::Uninitialized<PriorityChanger>* out, VmCowPages* cow,
                                    int64_t delta);
void cpp_priority_changer_destroy(PriorityChanger* pc);
void cpp_priority_changer_prepare_may_not_already_be_high_priority(PriorityChanger* pc);
void cpp_priority_changer_prepare_is_already_high_priority_locked(PriorityChanger* pc);
void cpp_priority_changer_change_high_priority_count_locked(PriorityChanger* pc);
void* cpp_priority_changer_lock(const PriorityChanger* pc);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_COW_PAGES_FFI_H_
