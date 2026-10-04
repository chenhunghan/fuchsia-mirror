// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/channel_dispatcher.h"

extern "C" {
void rust_channel_dispatcher_state_init(void* state, void* holder);
void rust_channel_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_channel_dispatcher_state_get_lock(const void* state);
}  // extern "C"

ChannelDispatcher::ChannelDispatcher(void* holder) : Dispatcher(ZX_CHANNEL_WRITABLE) {
  DISPATCHER_VERIFY_OFFSET(ChannelDispatcher, kChannelDispatcherStateOffset);
  rust_channel_dispatcher_state_init(&opaque_storage_, holder);
}

IMPLEMENT_DISPATCHER_RUST_STATE(ChannelDispatcher, rust_channel_dispatcher_state_get_lock,
                                rust_channel_dispatcher_state_destroy)
