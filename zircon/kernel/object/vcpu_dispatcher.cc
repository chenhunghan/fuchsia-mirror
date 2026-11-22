// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/vcpu_dispatcher.h"

#include <lib/object-constants.h>

#include <kernel/ffi.h>

VcpuDispatcher::VcpuDispatcher(GuestDispatcher* guest_dispatcher, Vcpu* vcpu) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(VcpuDispatcher, kVcpuDispatcherStateOffset);
  rust_vcpu_dispatcher_state_init(&opaque_storage_, this, guest_dispatcher, vcpu);
}

IMPLEMENT_DISPATCHER_RUST_STATE(VcpuDispatcher, rust_vcpu_dispatcher_state_get_lock,
                                rust_vcpu_dispatcher_state_destroy)
