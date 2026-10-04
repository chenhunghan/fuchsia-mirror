// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// A synthetic dispatcher used to test babeldiff. It is not part of Zircon.

#include <kernel/ffi.h>
#include <object/doorbell_dispatcher.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_doorbell_dispatcher_log(const DoorbellDispatcher* doorbell,
                                                   uint32_t kind) {
  if (kind == 1) {
    doorbell->LogChime();
  } else {
    doorbell->LogBuzzer();
  }
}

}  // extern "C"
