// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_ANONYMOUS_PAGE_REQUEST_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_ANONYMOUS_PAGE_REQUEST_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <vm/anonymous_page_request.h>

__BEGIN_CDECLS

zx_status_t rust_anonymous_page_request_allocate(AnonymousPageRequest* req, bool suspendable);
void rust_anonymous_page_request_free_page(AnonymousPageRequest* req);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_ANONYMOUS_PAGE_REQUEST_FFI_H_
