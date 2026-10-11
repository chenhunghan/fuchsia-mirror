// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/vcpu_dispatcher.h"

#include <lib/object-constants.h>

#include <arch/hypervisor.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/unique_ptr.h>
#include <ktl/utility.h>
#include <object/guest_dispatcher.h>

VcpuDispatcher::VcpuDispatcher(fbl::RefPtr<GuestDispatcher> guest_dispatcher,
                               ktl::unique_ptr<Vcpu> vcpu)
    : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(VcpuDispatcher, kVcpuDispatcherStateOffset);
  rust_vcpu_dispatcher_state_init(&opaque_storage_, this, fbl::ExportToRawPtr(&guest_dispatcher),
                                  vcpu.release());
}

IMPLEMENT_DISPATCHER_RUST_STATE(VcpuDispatcher, rust_vcpu_dispatcher_state_get_lock,
                                rust_vcpu_dispatcher_state_destroy)
