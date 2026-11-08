// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/anonymous_page_request.h"

#include <vm/anonymous_page_request_ffi.h>

// AnonymousPageRequest is implemented in Rust (anonymous_page_request.rs); these methods forward to
// it.

zx::result<> AnonymousPageRequest::Allocate(bool suspendable) {
  return zx::make_result(rust_anonymous_page_request_allocate(this, suspendable));
}

void AnonymousPageRequest::FreePage() { rust_anonymous_page_request_free_page(this); }
