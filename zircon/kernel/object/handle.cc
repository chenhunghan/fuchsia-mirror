// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/handle.h"

#include <object/dispatcher.h>

// KernelHandle<Dispatcher> is passed by pointer across the FFI boundary and mirrored by the Rust
// `KernelHandle<Dispatcher>` in handle.rs.
static_assert(sizeof(KernelHandle<Dispatcher>) == 8);
static_assert(alignof(KernelHandle<Dispatcher>) == 8);

HandleOwner Handle::Make(KernelHandle<Dispatcher> kernel_handle, zx_rights_t rights) {
  Handle* h = rust_handle_make(kernel_handle.dispatcher().get(), rights);
  if (!h) {
    return nullptr;
  }
  fbl::RefPtr<Dispatcher> disp = kernel_handle.release();
  Dispatcher* raw = fbl::ExportToRawPtr(&disp);
  (void)raw;
  return HandleOwner(h);
}
