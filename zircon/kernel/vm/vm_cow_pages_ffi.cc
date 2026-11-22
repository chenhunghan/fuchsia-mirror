// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/vm_cow_pages_ffi.h"

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <ktl/memory.h>

#include "vm/vm_cow_pages.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_replace_page_with_loaned(VmCowPages* cow,
                                                                        vm_page_t* before_page,
                                                                        uint64_t offset) {
  return cow->ReplacePageWithLoaned(before_page, offset);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_replace_page(VmCowPages* cow, vm_page_t* before_page,
                                                            uint64_t offset, bool with_loaned,
                                                            vm_page_t** after_page,
                                                            AnonymousPageRequest* page_request) {
  return cow->ReplacePage(before_page, offset, with_loaned, after_page, page_request);
}

FFI_ALWAYS_INLINE void* cpp_vm_cow_pages_get_ref_counted(const VmCowPages* cow) {
  return const_cast<fbl::RefCountedUpgradeable<VmCowPages>*>(
      static_cast<const fbl::RefCountedUpgradeable<VmCowPages>*>(cow));
}

FFI_ALWAYS_INLINE void cpp_vm_cow_pages_free(VmCowPages* cow) { delete cow; }

FFI_ALWAYS_INLINE bool cpp_vm_cow_pages_dedup_zero_page(VmCowPages* cow, vm_page_t* page,
                                                        uint64_t offset) {
  return cow->DedupZeroPage(page, offset);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_evict_loaned_page(VmCowPages* cow, vm_page_t* page,
                                                                 uint64_t offset) {
  return cow->EvictLoanedPage(page, offset);
}

FFI_ALWAYS_INLINE bool cpp_vm_cow_pages_lock_is_held(const VmCowPages* cow) {
  return cow->lock_ref().lock().IsHeld();
}

FFI_ALWAYS_INLINE bool cpp_vm_cow_pages_reclaim_page(
    VmCowPages* cow, vm_page_t* page, uint64_t offset, VmCowPages::EvictionAction eviction_action,
    VmCompressor* compressor, VmCowReclaimSuccess* out_success, VmCowReclaimFailure* out_failure) {
  auto result = cow->ReclaimPage(page, offset, eviction_action, compressor);
  if (result.is_ok()) {
    *out_success = result.value();
    return true;
  }
  *out_failure = result.error_value();
  return false;
}

FFI_ALWAYS_INLINE DiscardableVmoTracker* cpp_vm_cow_pages_discardable_tracker(
    const VmCowPages* cow) {
  return cow->discardable_tracker_.get();
}

FFI_ALWAYS_INLINE void* cpp_vm_cow_pages_get_lock(const VmCowPages* cow) { return cow->lock(); }

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_call_lookup_fn(void* lookup_fn, uint64_t offset,
                                                              paddr_t pa) {
  return (*static_cast<VmObject::LookupFunction*>(lookup_fn))(offset, pa);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_call_dirty_range_fn(void* dirty_range_fn,
                                                                   uint64_t range_offset,
                                                                   uint64_t range_len,
                                                                   bool range_is_zero) {
  return (*static_cast<VmCowPages::DirtyRangeEnumerateFunction*>(dirty_range_fn))(
      range_offset, range_len, range_is_zero);
}

// The range is passed as (offset, len) rather than a VmCowRange by value: Clang lowers the 2-field
// struct to {i64, i64} while rustc emits [2 x i64], and the mismatch stops ThinLTO from inlining
// this shim on x86_64.
FFI_ALWAYS_INLINE void cpp_vm_cow_pages_range_change_update_cow_children(
    VmCowPages* cow, uint64_t offset, uint64_t len,
    VmObject::RangeChangeOp op) TA_NO_THREAD_SAFETY_ANALYSIS {
  VmCowPages::RangeChangeUpdateCowChildren(VmCowPages::LockedPtr(cow), VmCowRange(offset, len), op);
}

FFI_ALWAYS_INLINE void cpp_vm_cow_pages_resize_update_child_limits(VmCowPages* cow, uint64_t s) {
  cow->ResizeUpdateChildLimits(s);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_replace_page_locked(
    VmCowPages* cow, vm_page_t* before_page, uint64_t offset, bool with_loaned,
    vm_page_t** after_page, void* deferred,
    AnonymousPageRequest* page_request) TA_NO_THREAD_SAFETY_ANALYSIS {
  return cow->ReplacePageLocked(before_page, offset, with_loaned, after_page,
                                *static_cast<VmCowPages::DeferredOps*>(deferred), page_request);
}

FFI_ALWAYS_INLINE zx_status_t
cpp_vm_cow_pages_supply_pages_locked(VmCowPages* cow, VmCowRange range, VmPageSpliceList* pages,
                                     SupplyOptions options, VmCowPages::DeferredOps* deferred,
                                     MultiPageRequest* page_request) TA_NO_THREAD_SAFETY_ANALYSIS {
  return cow->SupplyPagesLocked(range, pages, options, *deferred, page_request);
}

FFI_ALWAYS_INLINE VmCowPages* cpp_vm_cow_pages_upgrade_from_raw(VmCowPages* cow) {
  if (!cow->AddRefMaybeInDestructor()) {
    return nullptr;
  }
  return cow;
}

FFI_ALWAYS_INLINE void cpp_priority_changer_construct(ffi::Uninitialized<PriorityChanger>* out,
                                                      VmCowPages* cow, int64_t delta) {
  out->Initialize(delta, cow);
}

FFI_ALWAYS_INLINE void cpp_priority_changer_destroy(PriorityChanger* pc) { ktl::destroy_at(pc); }

FFI_ALWAYS_INLINE void cpp_priority_changer_prepare_may_not_already_be_high_priority(
    PriorityChanger* pc) TA_NO_THREAD_SAFETY_ANALYSIS {
  pc->PrepareMayNotAlreadyBeHighPriority();
}

FFI_ALWAYS_INLINE void cpp_priority_changer_prepare_is_already_high_priority_locked(
    PriorityChanger* pc) TA_NO_THREAD_SAFETY_ANALYSIS {
  pc->PrepareIsAlreadyHighPriorityLocked();
}

FFI_ALWAYS_INLINE void cpp_priority_changer_change_high_priority_count_locked(PriorityChanger* pc)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  pc->ChangeHighPriorityCountLocked();
}

FFI_ALWAYS_INLINE void* cpp_priority_changer_lock(const PriorityChanger* pc)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return pc->lock();
}

FFI_ALWAYS_INLINE void cpp_vm_cow_pages_debug_dump_reclaim_counters() {
  VmCowPages::DebugDumpReclaimCounters();
}

FFI_ALWAYS_INLINE VmObjectPaged* cpp_vm_cow_pages_get_paged_backlink_locked(const VmCowPages* cow)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return cow->get_paged_backlink_locked();
}

FFI_ALWAYS_INLINE void* cpp_vm_cow_pages_state(VmCowPages* cow) { return &cow->opaque_storage_; }

}  // extern "C"
