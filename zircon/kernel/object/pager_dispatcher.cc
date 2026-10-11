// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/object-constants.h>

#include <object/pager_dispatcher.h>

static_assert(PagerProxy::kTrapDirty == kPagerProxyTrapDirty);

PagerDispatcher::PagerDispatcher() : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(PagerDispatcher, kPagerDispatcherStateOffset);
  rust_pager_dispatcher_state_init(&opaque_storage_, this);
}

IMPLEMENT_DISPATCHER_RUST_STATE(PagerDispatcher, rust_pager_dispatcher_state_get_lock,
                                rust_pager_dispatcher_state_destroy)
