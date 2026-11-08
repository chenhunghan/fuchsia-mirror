// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/page_source_ffi.h"

#include <new>

#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <ktl/type_traits.h>
#include <ktl/utility.h>

#include "vm/page_source.h"

namespace {
// `PageProviderHelper` gives local access to the static protected methods of `PageProvider`.
struct PageProviderHelper : public PageProvider {
  using PageProvider::GetRequestLen;
  using PageProvider::GetRequestOffset;
  using PageProvider::GetRequestType;
  using PageProvider::GetRequestVmoId;
};
}  // namespace

// Rust views a `PageProviderTag` node state as an `fbl::DoublyLinkedListNode`, which is a
// `#[repr(C)]` pair of `next` and `prev` pointers.
using PageProviderNodeState = fbl::DoublyLinkedListNodeState<PageRequest*>;
static_assert(
    ktl::is_same_v<PageProviderNodeState&,
                   decltype(fbl::DefaultDoublyLinkedListTraits<PageRequest*, PageProviderTag>::
                                node_state(ktl::declval<PageRequest&>()))>);
static_assert(ktl::is_standard_layout_v<PageProviderNodeState>);
static_assert(sizeof(PageProviderNodeState) == 2 * sizeof(void*));
static_assert(alignof(PageProviderNodeState) == alignof(void*));

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.

FFI_ALWAYS_INLINE void cpp_multi_page_request_construct(MultiPageRequest* req) {
  // Intentional default initialization
  new (req) MultiPageRequest;
}

FFI_ALWAYS_INLINE void cpp_multi_page_request_destroy(MultiPageRequest* req) {
  ktl::destroy_at(req);
}

FFI_ALWAYS_INLINE void cpp_multi_page_request_cancel_requests(MultiPageRequest* req) {
  req->CancelRequests();
}

FFI_ALWAYS_INLINE zx_status_t cpp_multi_page_request_wait(MultiPageRequest* req, bool suspendable) {
  return req->Wait(suspendable);
}

// PageRequest accessors
FFI_ALWAYS_INLINE uint32_t cpp_page_request_get_type(const PageRequest* request) {
  return static_cast<uint32_t>(PageProviderHelper::GetRequestType(request));
}

FFI_ALWAYS_INLINE uint64_t cpp_page_request_get_offset(const PageRequest* request) {
  return PageProviderHelper::GetRequestOffset(request);
}

FFI_ALWAYS_INLINE uint64_t cpp_page_request_get_len(const PageRequest* request) {
  return PageProviderHelper::GetRequestLen(request);
}

FFI_ALWAYS_INLINE void* cpp_page_request_provider_node(PageRequest* request) {
  return &fbl::DefaultDoublyLinkedListTraits<PageRequest*, PageProviderTag>::node_state(*request);
}

// PageSource failure & helper shims
FFI_ALWAYS_INLINE void cpp_page_source_free(PageSource* src) { delete src; }

FFI_ALWAYS_INLINE void* cpp_page_source_get_ref_counted(PageSource* src) {
  return static_cast<fbl::RefCounted<PageRequestInterface>*>(src);
}

FFI_ALWAYS_INLINE void cpp_page_source_on_pages_failed(PageSource* page_source, uint64_t offset,
                                                       uint64_t len, zx_status_t error_status) {
  page_source->OnPagesFailed(offset, len, error_status);
}

FFI_ALWAYS_INLINE bool cpp_page_source_is_valid_external_failure_code(zx_status_t error_status) {
  return PageSource::IsValidExternalFailureCode(error_status);
}

FFI_ALWAYS_INLINE bool cpp_page_source_is_valid_internal_failure_code(zx_status_t status) {
  return PageSource::IsValidInternalFailureCode(status);
}

FFI_ALWAYS_INLINE bool cpp_page_source_paged_vmo_lock_is_held(PageSource* page_source) {
  return page_source->paged_vmo_lock()->lock().IsHeld();
}

void cpp_page_source_dump(const PageSource* page_source, uint32_t depth, uint32_t max_items) {
  page_source->Dump(depth, max_items);
}

FFI_ALWAYS_INLINE AnonymousPageRequest* cpp_multi_page_request_get_anonymous(
    MultiPageRequest* req) {
  return req->GetAnonymous();
}

FFI_ALWAYS_INLINE PageRequest* cpp_multi_page_request_get_read_request(MultiPageRequest* req) {
  return req->GetReadRequest();
}

FFI_ALWAYS_INLINE LazyPageRequest* cpp_multi_page_request_get_lazy_dirty_request(
    MultiPageRequest* req) {
  return req->GetLazyDirtyRequest();
}

FFI_ALWAYS_INLINE void cpp_multi_page_request_made_dirty_request(MultiPageRequest* req) {
  req->MadeDirtyRequest();
}

FFI_ALWAYS_INLINE PageRequest* cpp_lazy_page_request_get(LazyPageRequest* req) {
  return req->get();
}

__PURE FFI_ALWAYS_INLINE bool cpp_lazy_page_request_is_initialized(const LazyPageRequest* req) {
  return req->is_initialized();
}

__PURE FFI_ALWAYS_INLINE PageSourceProperties
cpp_page_source_properties(const PageSource* page_source) {
  return page_source->properties();
}

__PURE FFI_ALWAYS_INLINE bool cpp_page_source_should_trap_dirty_transitions(
    const PageSource* page_source) {
  return page_source->ShouldTrapDirtyTransitions();
}

FFI_ALWAYS_INLINE zx_status_t cpp_page_source_get_pages(PageSource* page_source, uint64_t offset,
                                                        uint64_t len, PageRequest* req,
                                                        VmoDebugInfo vmo_debug_info) {
  return page_source->GetPages(offset, len, req, vmo_debug_info);
}

FFI_ALWAYS_INLINE zx_status_t
cpp_page_source_request_dirty_transition(PageSource* page_source, PageRequest* req, uint64_t offset,
                                         uint64_t len, VmoDebugInfo vmo_debug_info) {
  return page_source->RequestDirtyTransition(req, offset, len, vmo_debug_info);
}

FFI_ALWAYS_INLINE void cpp_page_source_on_pages_supplied(PageSource* page_source, uint64_t offset,
                                                         uint64_t len) {
  page_source->OnPagesSupplied(offset, len);
}

FFI_ALWAYS_INLINE void cpp_page_source_on_pages_dirtied(PageSource* page_source, uint64_t offset,
                                                        uint64_t len) {
  page_source->OnPagesDirtied(offset, len);
}

FFI_ALWAYS_INLINE void cpp_page_source_free_pages(PageSource* page_source,
                                                  VmPageDoublyLinkedList* pages) {
  page_source->FreePages(pages);
}
}  // extern "C"
