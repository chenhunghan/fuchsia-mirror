// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// TODO(https://fxbug.dev/540178114): Remove once all C++ callers are ported to Rust.

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_USER_COPY_FFI_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_USER_COPY_FFI_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

__BEGIN_CDECLS
// Typically we would not use structs as function return values, but in this case it enables us to
// very efficiently use the 2 registers for return values to encode the optional flags and va
// page fault values.

typedef struct X64CopyToFromUserRet {
  zx_status_t status;
  uint32_t pf_flags;
  uintptr_t pf_va;
} X64CopyToFromUserRet;  // NOLINT(modernize-use-using)

X64CopyToFromUserRet rust_arch_copy_from_user_capture_faults(void* dst, const void* src,
                                                             size_t len);
X64CopyToFromUserRet rust_arch_copy_to_user_capture_faults(void* dst, const void* src, size_t len);

__END_CDECLS

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_USER_COPY_FFI_H_
