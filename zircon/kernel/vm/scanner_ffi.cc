// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/scanner_ffi.h"

#include <zircon/types.h>

#include <kernel/ffi.h>

#include "vm/scanner.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE void cpp_scanner_push_disable_count(void) { scanner_push_disable_count(); }

FFI_ALWAYS_INLINE void cpp_scanner_pop_disable_count(void) { scanner_pop_disable_count(); }

}  // extern "C"
