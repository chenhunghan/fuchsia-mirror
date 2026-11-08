// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/vm_cow_pages_ffi.h"

#include <assert.h>
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

FFI_ALWAYS_INLINE VmCowPages* cpp_vm_cow_pages_debug_get_parent(VmCowPages* cow) {
  auto parent = cow->DebugGetParent();
  return fbl::ExportToRawPtr(&parent);
}

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

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_debug_lookup_readable(
    VmCowPages* cow, VmCowRange range, void* ctx, cpp_vm_cow_pages_lookup_readable_fn callback) {
  return cow->DebugLookupReadable(
      range, [ctx, callback](uint64_t offset, paddr_t pa) { return callback(ctx, offset, pa); });
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

FFI_ALWAYS_INLINE void cpp_vm_cow_pages_deferred_ops_init_page_source_lock(
    ffi::Uninitialized<DeferredPageSourceLock>* lock_storage, VmCowPages* cow) {
  cow->InitDeferredPageSourceLock(lock_storage);
}

FFI_ALWAYS_INLINE void cpp_vm_cow_pages_deferred_ops_destroy_page_source_lock(
    DeferredPageSourceLock* lock_storage) {
  ktl::destroy_at(lock_storage);
}

// The range is passed as (offset, len) rather than a VmCowRange by value: Clang lowers the 2-field
// struct to {i64, i64} while rustc emits [2 x i64], and the mismatch stops ThinLTO from inlining
// this shim on x86_64.
FFI_ALWAYS_INLINE void cpp_vm_cow_pages_range_change_update_cow_children(
    VmCowPages* cow, uint64_t offset, uint64_t len,
    VmObject::RangeChangeOp op) TA_NO_THREAD_SAFETY_ANALYSIS {
  VmCowPages::RangeChangeUpdateCowChildren(VmCowPages::LockedPtr(cow), VmCowRange(offset, len), op);
}

FFI_ALWAYS_INLINE void cpp_vm_cow_pages_lookup_cursor_destroy(
    VmCowPagesLookupCursorResult* cursor) {
  ktl::destroy_at(cursor);
}

FFI_ALWAYS_INLINE VmCowPages::LookupCursor* cpp_vm_cow_pages_lookup_cursor_result_value(
    VmCowPagesLookupCursorResult* cursor) {
  DEBUG_ASSERT(cursor->result.is_ok());
  if (!cursor->result.is_ok()) {
    __builtin_unreachable();
  }
  return &*cursor->result;
}

FFI_ALWAYS_INLINE bool cpp_vm_cow_pages_lookup_cursor_lock_is_held(
    const VmCowPagesLookupCursorResult* cursor) {
  DEBUG_ASSERT(cursor->result.is_ok());
  if (!cursor->result.is_ok()) {
    __builtin_unreachable();
  }
  return cursor->result->lock_ref().lock().IsHeld();
}

FFI_ALWAYS_INLINE void cpp_vm_cow_pages_lookup_cursor_disable_mark_accessed(
    VmCowPages::LookupCursor* cursor) {
  cursor->DisableMarkAccessed();
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_lookup_cursor_require_owned_page(
    VmCowPages::LookupCursor* cursor, bool will_write, uint64_t max_request_pages,
    VmCowPages::DeferredOps* deferred, MultiPageRequest* page_request, vm_page_t** out_page,
    bool* out_writable) TA_NO_THREAD_SAFETY_ANALYSIS {
  auto result = cursor->RequireOwnedPage(will_write, max_request_pages, *deferred, page_request);
  if (unlikely(result.is_error())) {
    return result.error_value();
  }
  *out_page = result->page;
  *out_writable = result->writable;
  return ZX_OK;
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_lookup_cursor_require_page(
    VmCowPages::LookupCursor* cursor, bool will_write, uint64_t max_request_pages,
    VmCowPages::DeferredOps* deferred, MultiPageRequest* page_request, vm_page_t** out_page,
    bool* out_writable) TA_NO_THREAD_SAFETY_ANALYSIS {
  __UNINITIALIZED auto result =
      cursor->RequirePage(will_write, max_request_pages, *deferred, page_request);
  if (unlikely(result.is_error())) {
    return result.error_value();
  }
  *out_page = result->page;
  *out_writable = result->writable;
  return ZX_OK;
}

FFI_ALWAYS_INLINE vm_page_t* cpp_vm_cow_pages_lookup_cursor_maybe_page(
    VmCowPages::LookupCursor* cursor, bool will_write) TA_NO_THREAD_SAFETY_ANALYSIS {
  return cursor->MaybePage(will_write);
}

FFI_ALWAYS_INLINE uint64_t cpp_vm_cow_pages_lookup_cursor_skip_missing_pages(
    VmCowPages::LookupCursor* cursor) TA_NO_THREAD_SAFETY_ANALYSIS {
  return cursor->SkipMissingPages();
}

FFI_ALWAYS_INLINE uint64_t cpp_vm_cow_pages_lookup_cursor_if_exist_pages(
    VmCowPages::LookupCursor* cursor, bool will_write, uint64_t max_pages,
    paddr_t* paddrs) TA_NO_THREAD_SAFETY_ANALYSIS {
  return cursor->IfExistPages(will_write, max_pages, paddrs);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_add_new_pages_locked(
    VmCowPages* cow, uint64_t start_offset, VmPageDoublyLinkedList* pages,
    VmCowPages::CanOverwriteSlot overwrite, bool zero,
    VmCowPages::DeferredOps* deferred) TA_NO_THREAD_SAFETY_ANALYSIS {
  return cow->AddNewPagesLocked(start_offset, pages, overwrite, zero, deferred);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_zero_pages_locked(
    VmCowPages* cow, VmCowRange range, bool dirty_track, VmCowPages::DeferredOps* deferred,
    MultiPageRequest* page_request, uint64_t* out_zeroed_bytes) TA_NO_THREAD_SAFETY_ANALYSIS {
  auto [status, zeroed_bytes] = cow->ZeroPagesLocked(range, dirty_track, *deferred, page_request);
  *out_zeroed_bytes = zeroed_bytes;
  return status;
}

FFI_ALWAYS_INLINE zx_status_t
cpp_vm_cow_pages_supply_pages_locked(VmCowPages* cow, VmCowRange range, VmPageSpliceList* pages,
                                     SupplyOptions options, VmCowPages::DeferredOps* deferred,
                                     MultiPageRequest* page_request) TA_NO_THREAD_SAFETY_ANALYSIS {
  return cow->SupplyPagesLocked(range, pages, options, *deferred, page_request);
}

FFI_ALWAYS_INLINE bool cpp_vm_cow_pages_can_borrow(const VmCowPages* cow) {
  return cow->can_borrow();
}

FFI_ALWAYS_INLINE bool cpp_vm_cow_pages_can_evict(const VmCowPages* cow) {
  return cow->can_evict();
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

FFI_ALWAYS_INLINE PageSourceType cpp_vm_cow_pages_page_source_type(const VmCowPages* cow) {
  return cow->page_source_type();
}

FFI_ALWAYS_INLINE PageSource* cpp_vm_cow_pages_page_source(const VmCowPages* cow) {
  return cow->page_source_.get();
}

FFI_ALWAYS_INLINE VmObjectPaged* cpp_vm_cow_pages_get_paged_backlink_locked(const VmCowPages* cow)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return cow->get_paged_backlink_locked();
}

FFI_ALWAYS_INLINE void* cpp_vm_cow_pages_state(VmCowPages* cow) { return &cow->opaque_storage_; }

}  // extern "C"
