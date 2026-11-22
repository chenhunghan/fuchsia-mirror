// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/errors.h>
#include <zircon/types.h>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>
#include <object/handle.h>
#include <object/vcpu_dispatcher.h>

extern "C" {

zx_status_t cpp_vcpu_dispatcher_create(
    GuestDispatcher* guest_dispatcher_raw, Vcpu* vcpu_raw,
    ffi::Uninitialized<KernelHandle<VcpuDispatcher>>* handle_out) {
  // Ownership of `guest_dispatcher_raw` and `vcpu_raw` passes to the dispatcher only on success.
  fbl::AllocChecker ac;
  KernelHandle new_handle(fbl::AdoptRef(new (&ac) VcpuDispatcher(guest_dispatcher_raw, vcpu_raw)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  handle_out->Initialize(ktl::move(new_handle));
  return ZX_OK;
}

}  // extern "C"
