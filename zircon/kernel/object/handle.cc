// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/handle.h"

#include <object/dispatcher.h>

// Handles are constructed and destroyed by Rust (rust_handle_make, rust_handle_destroy), so every
// Handle the C++ accessors in handle.h touch was laid out by the Rust `Handle` in
// //zircon/kernel/object/handle.rs, which pins its own layout with a matching set of assertions.
// The assertions below pin the C++ view of the very same bytes, so that the two can never drift
// apart silently.  They live here rather than in the header because offsetof() on Handle's private
// members needs friend access.
struct HandleLayoutCheck {
  static_assert(sizeof(Handle) == 40);
  static_assert(alignof(Handle) == 8);
  static_assert(offsetof(Handle, handle_table_id_) == 0);
  static_assert(offsetof(Handle, dispatcher_) == 8);
  static_assert(offsetof(Handle, rights_) == 16);
  static_assert(offsetof(Handle, base_value_) == 20);
  static_assert(offsetof(Handle, node_state_) == 24);

  // KernelHandle<Dispatcher> is passed by pointer across the FFI boundary and mirrored by the Rust
  // `KernelHandle<Dispatcher>` in handle.rs.
  static_assert(sizeof(KernelHandle<Dispatcher>) == 8);
  static_assert(alignof(KernelHandle<Dispatcher>) == 8);
};

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

uint32_t Handle::Count(const Dispatcher& dispatcher) { return dispatcher.current_handle_count(); }
