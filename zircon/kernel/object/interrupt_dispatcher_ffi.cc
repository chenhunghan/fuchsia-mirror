// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <object/interrupt_dispatcher.h>

// Immediately after construction, virtual interrupt objects assert
// ZX_VIRTUAL_INTERRUPT_UNTRIGGERED.
InterruptDispatcher::InterruptDispatcher(uint32_t flags, uint32_t options)
    // NOLINTNEXTLINE(bugprone-signed-bitwise)
    : Dispatcher((options & ZX_INTERRUPT_VIRTUAL) ? ZX_VIRTUAL_INTERRUPT_UNTRIGGERED : 0u) {
  DISPATCHER_VERIFY_OFFSET(InterruptDispatcher, kInterruptDispatcherStateOffset);
  rust_interrupt_dispatcher_state_init(&opaque_storage_, this, flags, options);
}

InterruptDispatcher::~InterruptDispatcher() {
  // Drop `kind` and unlink `wake_event` from the global/pending wake-vector lists while this
  // dispatcher is still accessed only through shared references, before
  // `rust_interrupt_dispatcher_state_destroy` requires exclusive access to `opaque_storage_`.
  rust_interrupt_dispatcher_teardown(this);
  rust_interrupt_dispatcher_state_destroy(&opaque_storage_);
}

Lock<CriticalMutex>* InterruptDispatcher::get_lock() const {
  return rust_interrupt_dispatcher_state_get_lock(&opaque_storage_);
}

extern "C" {

InterruptDispatcher* cpp_interrupt_dispatcher_create(uint32_t flags, uint32_t options) {
  fbl::AllocChecker ac;
  auto disp = fbl::AdoptRef(new (&ac) InterruptDispatcher(flags, options));
  if (!ac.check()) {
    return nullptr;
  }
  return fbl::ExportToRawPtr(&disp);
}

}  // extern "C"
