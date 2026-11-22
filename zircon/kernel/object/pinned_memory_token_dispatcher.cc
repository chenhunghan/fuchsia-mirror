// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/pinned_memory_token_dispatcher.h"

#include <lib/object-constants.h>

PinnedMemoryTokenDispatcher::PinnedMemoryTokenDispatcher(const void* bti) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(PinnedMemoryTokenDispatcher, kPinnedMemoryTokenDispatcherStateOffset);
  rust_pinned_memory_token_dispatcher_state_init(&opaque_storage_, this, bti);
}

IMPLEMENT_DISPATCHER_RUST_STATE(PinnedMemoryTokenDispatcher,
                                rust_pinned_memory_token_dispatcher_state_get_lock,
                                rust_pinned_memory_token_dispatcher_state_destroy)

void PinnedMemoryTokenDispatcher::on_zero_handles() {
  rust_pinned_memory_token_dispatcher_on_zero_handles(this);
}
