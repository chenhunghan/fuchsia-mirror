// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <vm/page.h>
#include <vm/page_source.h>

__BEGIN_CDECLS

// The PageProvider virtual interface, reached through `PageProviderFfi` in page_source_ffi.cc.
// This header is also the bindgen entry point for the page source types.
void* cpp_page_provider_get_ref_counted(PageProvider* provider);
void cpp_page_provider_free(PageProvider* provider);
PageSourceProperties cpp_page_provider_properties(const PageProvider* provider);
void cpp_page_provider_send_async_request(PageProvider* provider, PageRequest* request);
void cpp_page_provider_clear_async_request(PageProvider* provider, PageRequest* request);
void cpp_page_provider_swap_async_request(PageProvider* provider, PageRequest* old,
                                          PageRequest* new_req);
void cpp_page_provider_free_pages(PageProvider* provider, VmPageDoublyLinkedList* pages);
bool cpp_page_provider_debug_is_page_ok(PageProvider* provider, vm_page_t* page, uint64_t offset);
void cpp_page_provider_on_detach(PageProvider* provider);
void cpp_page_provider_on_close(PageProvider* provider);
zx_status_t cpp_page_provider_wait_on_event(PageProvider* provider, Event* event, bool suspendable);
void cpp_page_provider_dump(PageProvider* provider, uint32_t depth, uint32_t max_items);
zx_koid_t cpp_page_provider_get_koid(const PageProvider* provider);
void cpp_page_provider_get_debug_name(const PageProvider* provider, char (*name)[ZX_MAX_NAME_LEN]);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_FFI_H_
