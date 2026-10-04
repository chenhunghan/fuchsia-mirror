// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_ANONYMOUS_PAGE_REQUEST_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_ANONYMOUS_PAGE_REQUEST_FFI_H_

#include <zircon/compiler.h>

#include <kernel/ffi.h>
#include <vm/anonymous_page_request.h>

__BEGIN_CDECLS

void cpp_anonymous_page_request_construct(ffi::Uninitialized<AnonymousPageRequest>* req);
void cpp_anonymous_page_request_destroy(AnonymousPageRequest* req);
void cpp_anonymous_page_request_cancel(AnonymousPageRequest* req);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_ANONYMOUS_PAGE_REQUEST_FFI_H_
