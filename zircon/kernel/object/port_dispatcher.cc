// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/port_dispatcher.h"

#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <fbl/alloc_checker.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>

static_assert(sizeof(PortDispatcher) == kPortDispatcherStateSize + kPortDispatcherStateOffset,
              "PortDispatcher size mismatch");
static_assert(alignof(PortDispatcher) == kPortDispatcherStateAlign,
              "PortDispatcher align mismatch");

static_assert(sizeof(PortPacket) == kPortPacketSize, "PortPacket size mismatch");
static_assert(alignof(PortPacket) == kPortPacketAlign, "PortPacket align mismatch");

// Rust reads and writes every PortPacket field directly, so pin each field offset in addition to
// the overall size. PortPacket is not standard-layout (it derives from DoublyLinkedListable), so
// offsetof needs -Winvalid-offsetof suppressed.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Winvalid-offsetof"
static_assert(__builtin_offsetof(PortPacket, packet) == kPortPacketPacketOffset,
              "PortPacket::packet offset mismatch");
static_assert(__builtin_offsetof(PortPacket, handle) == kPortPacketHandleOffset,
              "PortPacket::handle offset mismatch");
static_assert(__builtin_offsetof(PortPacket, observer) == kPortPacketObserverOffset,
              "PortPacket::observer offset mismatch");
static_assert(__builtin_offsetof(PortPacket, allocator) == kPortPacketAllocatorOffset,
              "PortPacket::allocator offset mismatch");
#pragma clang diagnostic pop
// The list node is the DoublyLinkedListable base subobject: it must precede |packet| and is
// exactly as large as the gap before it, so it occupies [kPortPacketNodeOffset, packet).
static_assert(kPortPacketNodeOffset == 0, "PortPacket list node must be the first subobject");
static_assert(sizeof(fbl::DoublyLinkedListable<PortPacket*>) ==
                  kPortPacketPacketOffset - kPortPacketNodeOffset,
              "PortPacket list node size mismatch");
static_assert(sizeof(PortAllocator*) == sizeof(void*), "PortAllocator* must be pointer-sized");

extern "C" {

zx_status_t rust_port_dispatcher_make_observer(const PortDispatcher* disp, uint32_t options,
                                               const Handle* handle, uint64_t key,
                                               zx_signals_t signals, SignalObserver** out_observer);

}  // extern "C"

PortDispatcher::PortDispatcher(uint32_t options) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(PortDispatcher, kPortDispatcherStateOffset);
  rust_port_dispatcher_state_init(&opaque_storage_, this, options);
}

IMPLEMENT_DISPATCHER_RUST_STATE(PortDispatcher, rust_port_dispatcher_state_get_lock,
                                rust_port_dispatcher_state_destroy)

zx::result<SignalObserver*> PortDispatcher::MakeObserver(uint32_t options, const Handle* handle,
                                                         uint64_t key, zx_signals_t signals) const {
  SignalObserver* observer = nullptr;
  zx_status_t status =
      rust_port_dispatcher_make_observer(this, options, handle, key, signals, &observer);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(observer);
}

extern "C" {

zx_status_t cpp_port_dispatcher_create(
    uint32_t options, ffi::Uninitialized<KernelHandle<PortDispatcher>>* handle_out) {
  fbl::AllocChecker ac;
  KernelHandle new_handle(fbl::AdoptRef(new (&ac) PortDispatcher(options)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  handle_out->Initialize(ktl::move(new_handle));
  return ZX_OK;
}

}  // extern "C"
