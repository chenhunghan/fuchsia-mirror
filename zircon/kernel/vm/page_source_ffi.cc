// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/page_source_ffi.h"

#include <kernel/ffi.h>

#include "vm/page_source.h"

// The C++ shell is only storage for the Rust object, which lives at offset 0.
static_assert(sizeof(MultiPageRequest) == kMultiPageRequestSize);
static_assert(alignof(MultiPageRequest) == kMultiPageRequestAlign);

// `PageProviderFfi` forwards the private `PageProvider` virtual interface to the
// `cpp_page_provider_*` shims below. Friendship does not follow Rust callers across the FFI
// boundary, so this one struct is `PageProvider`'s friend instead of each shim.
struct PageProviderFfi {
  static PageSourceProperties properties(const PageProvider* provider) {
    return provider->properties();
  }
  static void SendAsyncRequest(PageProvider* provider, PageRequest* request) {
    provider->SendAsyncRequest(request);
  }
  static void ClearAsyncRequest(PageProvider* provider, PageRequest* request) {
    provider->ClearAsyncRequest(request);
  }
  static void SwapAsyncRequest(PageProvider* provider, PageRequest* old, PageRequest* new_req) {
    provider->SwapAsyncRequest(old, new_req);
  }
  static void FreePages(PageProvider* provider, VmPageDoublyLinkedList* pages) {
    provider->FreePages(pages);
  }
  static bool DebugIsPageOk(PageProvider* provider, vm_page_t* page, uint64_t offset) {
    return provider->DebugIsPageOk(page, offset);
  }
  static void OnDetach(PageProvider* provider) { provider->OnDetach(); }
  static void OnClose(PageProvider* provider) { provider->OnClose(); }
  static zx_status_t WaitOnEvent(PageProvider* provider, Event* event, bool suspendable) {
    return provider->WaitOnEvent(event, suspendable);
  }
  static void Dump(PageProvider* provider, uint depth, uint max_items) {
    provider->Dump(depth, max_items);
  }
  static ktl::optional<uint64_t> GetKoid(const PageProvider* provider) {
    return provider->GetKoid();
  }
  static void GetDebugName(const PageProvider* provider, char (&name)[ZX_MAX_NAME_LEN]) {
    provider->GetDebugName(name);
  }
};

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.

FFI_ALWAYS_INLINE void* cpp_page_provider_get_ref_counted(PageProvider* provider) {
  return static_cast<fbl::RefCounted<PageProvider>*>(provider);
}

FFI_ALWAYS_INLINE void cpp_page_provider_free(PageProvider* provider) { delete provider; }

FFI_ALWAYS_INLINE PageSourceProperties cpp_page_provider_properties(const PageProvider* provider) {
  return PageProviderFfi::properties(provider);
}

FFI_ALWAYS_INLINE void cpp_page_provider_send_async_request(PageProvider* provider,
                                                            PageRequest* request) {
  PageProviderFfi::SendAsyncRequest(provider, request);
}

FFI_ALWAYS_INLINE void cpp_page_provider_clear_async_request(PageProvider* provider,
                                                             PageRequest* request) {
  PageProviderFfi::ClearAsyncRequest(provider, request);
}

FFI_ALWAYS_INLINE void cpp_page_provider_swap_async_request(PageProvider* provider,
                                                            PageRequest* old,
                                                            PageRequest* new_req) {
  PageProviderFfi::SwapAsyncRequest(provider, old, new_req);
}

FFI_ALWAYS_INLINE void cpp_page_provider_free_pages(PageProvider* provider,
                                                    VmPageDoublyLinkedList* pages) {
  PageProviderFfi::FreePages(provider, pages);
}

FFI_ALWAYS_INLINE bool cpp_page_provider_debug_is_page_ok(PageProvider* provider, vm_page_t* page,
                                                          uint64_t offset) {
  return PageProviderFfi::DebugIsPageOk(provider, page, offset);
}

FFI_ALWAYS_INLINE void cpp_page_provider_on_detach(PageProvider* provider) {
  PageProviderFfi::OnDetach(provider);
}

FFI_ALWAYS_INLINE void cpp_page_provider_on_close(PageProvider* provider) {
  PageProviderFfi::OnClose(provider);
}

FFI_ALWAYS_INLINE zx_status_t cpp_page_provider_wait_on_event(PageProvider* provider, Event* event,
                                                              bool suspendable) {
  return PageProviderFfi::WaitOnEvent(provider, event, suspendable);
}

FFI_ALWAYS_INLINE void cpp_page_provider_dump(PageProvider* provider, uint32_t depth,
                                              uint32_t max_items) {
  PageProviderFfi::Dump(provider, depth, max_items);
}

FFI_ALWAYS_INLINE zx_koid_t cpp_page_provider_get_koid(const PageProvider* provider) {
  return PageProviderFfi::GetKoid(provider).value_or(ZX_KOID_INVALID);
}

FFI_ALWAYS_INLINE void cpp_page_provider_get_debug_name(const PageProvider* provider,
                                                        char (*name)[ZX_MAX_NAME_LEN]) {
  PageProviderFfi::GetDebugName(provider, *name);
}
}  // extern "C"
