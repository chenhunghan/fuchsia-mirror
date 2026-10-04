// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// Synthetic syscalls used to test babeldiff. They are not part of Zircon.

#include <lib/syscalls/forward.h>
#include <trace.h>

#include <object/doorbell_dispatcher.h>
#include <object/process_dispatcher.h>

#define LOCAL_TRACE 0

// zx_status_t zx_doorbell_ring
zx_status_t sys_doorbell_ring(zx_handle_t handle, uint32_t tone,
                              user_out_ptr<uint64_t> out_count) {
  LTRACEF("handle %x tone %u\n", handle, tone);

  zx_status_t status;
  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<DoorbellDispatcher> doorbell;
  status = up->handle_table().GetDispatcherWithRights(*up, handle, ZX_RIGHT_SIGNAL, &doorbell);
  if (status != ZX_OK) {
    return status;
  }

  status = doorbell->Ring(tone);
  if (status == ZX_OK && out_count) {
    status = out_count.copy_to_user(doorbell->RingCount());
  }

  return status;
}

// zx_status_t zx_doorbell_create
zx_status_t sys_doorbell_create(uint32_t options, uint32_t arg, zx_handle_t* out) {
  KernelHandle<DoorbellDispatcher> handle;
  zx_rights_t rights;
  zx_status_t status;
  if (options & ZX_DOORBELL_BUZZER) {
    status = BuzzerDoorbellDispatcher::Create(arg, &handle, &rights);
  } else {
    status = ChimeDoorbellDispatcher::Create(arg, &handle, &rights);
  }
  if (status != ZX_OK) {
    return status;
  }
  return ProcessDispatcher::GetCurrent()->MakeAndAddHandle(ktl::move(handle), rights, out);
}
