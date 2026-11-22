// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_RESTRICTED_STATE_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_RESTRICTED_STATE_H_

#include <ktl/unique_ptr.h>

// Encapsulates a thread's restricted mode state.
//
// The underlying state is allocated and managed entirely in Rust
// (`zircon/kernel/kernel/restricted_state.rs`). In C++, `RestrictedState` is an incomplete struct
// type serving as an opaque handle so that C++ code can safely pass around strongly-typed
// `RestrictedState*` pointers without exposing Rust-side layout details. Lifecycle management is
// handled via `RestrictedStatePtr` (`ktl::unique_ptr<RestrictedState, RestrictedStateDeleter>`),
// which dispatches to `rust_restricted_state_destroy` upon destruction.
struct RestrictedState;

extern "C" {

void rust_restricted_state_destroy(RestrictedState* ptr);

}  // extern "C"

// Custom deleter for Rust-allocated RestrictedState.
struct RestrictedStateDeleter {
  void operator()(RestrictedState* ptr) const {
    if (ptr) {
      rust_restricted_state_destroy(ptr);
    }
  }
};

using RestrictedStatePtr = ktl::unique_ptr<RestrictedState, RestrictedStateDeleter>;

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_RESTRICTED_STATE_H_
