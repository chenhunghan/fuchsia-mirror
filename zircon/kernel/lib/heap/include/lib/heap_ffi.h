// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_HEAP_INCLUDE_LIB_HEAP_FFI_H_
#define ZIRCON_KERNEL_LIB_HEAP_INCLUDE_LIB_HEAP_FFI_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>

// TODO(https://fxbug.dev/565806419): Remove once callers of heap_init, profile_track_alloc, and
// profile_track_free are migrated to Rust.
__BEGIN_CDECLS

void rust_heap_init(void);
uint32_t rust_profile_track_alloc(size_t size);
void rust_profile_track_free(uint32_t handle, size_t size);

__END_CDECLS

#endif  // ZIRCON_KERNEL_LIB_HEAP_INCLUDE_LIB_HEAP_FFI_H_
