// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <kernel/idle_power_thread.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_idle_power_thread_init(ffi::Uninitialized<IdlePowerThread>* out) {
  DEBUG_ASSERT(out != nullptr);
  out->Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_resume_timer_wake_vector_init() {
  IdlePowerThread::resume_timer_wake_vector_.Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const wake_vector::WakeEvent* cpp_resume_timer_wake_vector_get_wake_event() {
  return &IdlePowerThread::resume_timer_wake_vector_->wake_event;
}

}  // extern "C"
