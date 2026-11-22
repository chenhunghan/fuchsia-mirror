// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_FEATURE_FFI_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_FEATURE_FFI_H_

#include <stdint.h>
#include <zircon/compiler.h>

struct cpuid_leaf;

// TODO(https://fxbug.dev/540178114): Remove once all C++ callers are ported to Rust.
__BEGIN_CDECLS

bool rust_x86_get_cpuid_subleaf(uint32_t num, uint32_t subleaf, struct cpuid_leaf* leaf);

__END_CDECLS

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_FEATURE_FFI_H_
