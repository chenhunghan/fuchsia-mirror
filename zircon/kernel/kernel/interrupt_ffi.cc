// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <kernel/interrupt.h>

// LINT.IfChange(int_handler_saved_state_t)
static_assert(sizeof(int_handler_saved_state_t) == 1);
static_assert(alignof(int_handler_saved_state_t) == 1);
// LINT.ThenChange(//zircon/kernel/kernel/interrupt.rs:IntHandlerSavedState)

extern "C" {

void cpp_int_handler_start(int_handler_saved_state_t* state);
bool cpp_int_handler_finish(int_handler_saved_state_t* state);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_int_handler_start(int_handler_saved_state_t* state) {
  int_handler_start(state);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_int_handler_finish(int_handler_saved_state_t* state) {
  return int_handler_finish(state);
}

}  // extern "C"
