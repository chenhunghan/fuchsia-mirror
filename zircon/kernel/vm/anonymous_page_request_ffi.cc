// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/anonymous_page_request_ffi.h"

#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <ktl/type_traits.h>

#include "vm/anonymous_page_request.h"

static_assert(sizeof(ffi::Uninitialized<AnonymousPageRequest>) == sizeof(AnonymousPageRequest));
static_assert(alignof(ffi::Uninitialized<AnonymousPageRequest>) == alignof(AnonymousPageRequest));
static_assert(ktl::is_standard_layout_v<AnonymousPageRequest>);
static_assert(ktl::is_standard_layout_v<ffi::Uninitialized<AnonymousPageRequest>>);

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.

FFI_ALWAYS_INLINE void cpp_anonymous_page_request_construct(
    ffi::Uninitialized<AnonymousPageRequest>* req) {
  req->Initialize();
}

FFI_ALWAYS_INLINE void cpp_anonymous_page_request_destroy(AnonymousPageRequest* req) {
  ktl::destroy_at(req);
}

FFI_ALWAYS_INLINE void cpp_anonymous_page_request_cancel(AnonymousPageRequest* req) {
  req->Cancel();
}

}  // extern "C"
