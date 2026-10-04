// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/hypervisor.h>
#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/unique_ptr.h>
#include <ktl/utility.h>
#include <object/guest_dispatcher.h>
#include <object/handle.h>

extern "C" {

zx_status_t cpp_guest_dispatcher_create(
    Guest* guest_raw, ffi::Uninitialized<KernelHandle<GuestDispatcher>>* guest_handle_out) {
  ktl::unique_ptr<Guest> guest(guest_raw);
  fbl::AllocChecker ac;
  KernelHandle new_guest_handle(fbl::AdoptRef(new (&ac) GuestDispatcher(ktl::move(guest))));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  guest_handle_out->Initialize(ktl::move(new_guest_handle));
  return ZX_OK;
}

}  // extern "C"
