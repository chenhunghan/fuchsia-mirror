// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/hypervisor.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <object/port_dispatcher.h>
#include <vm/vm_address_region.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_guest_create(Guest** guest_out) {
  auto guest = Guest::Create();
  if (guest.is_error()) {
    return guest.status_value();
  }
  *guest_out = (*guest).release();
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE VmAddressRegion* cpp_guest_root_vmar(const Guest* guest) {
  fbl::RefPtr<VmAddressRegion> vmar = guest->RootVmar();
  return fbl::ExportToRawPtr(&vmar);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_guest_destroy(Guest* guest) { delete guest; }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_guest_set_trap(Guest* guest, uint32_t kind, zx_vaddr_t addr,
                                                 size_t len, PortDispatcher* port, uint64_t key) {
  return guest->SetTrap(kind, addr, len, fbl::ImportFromRawPtr(port), key).status_value();
}

}  // extern "C"
