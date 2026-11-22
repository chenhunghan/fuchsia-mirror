// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// Kernel Address Sanitizer (KASAN) is a tool to detect use-after-free, use-out-of-bounds, and
// other common memory errors in kernel.
// See: //zircon/kernel/lib/instrumentation/asan/README.md for more context.

#ifndef ZIRCON_KERNEL_LIB_INSTRUMENTATION_INCLUDE_LIB_INSTRUMENTATION_ASAN_H_
#define ZIRCON_KERNEL_LIB_INSTRUMENTATION_INCLUDE_LIB_INSTRUMENTATION_ASAN_H_

#include <zircon/compiler.h>

#include <arch/kernel_aspace.h>

#define KASAN_SHADOW_OFFSET (ASAN_MAPPING_OFFSET + (KERNEL_ASPACE_BASE >> ASAN_MAPPING_SCALE))

#ifdef __x86_64__

#define X86_KERNEL_KASAN_PDP_ENTRIES (64)

#endif  // __x86_64__

#define NO_ASAN __attribute__((no_sanitize("address", "hwaddress")))

#endif  // ZIRCON_KERNEL_LIB_INSTRUMENTATION_INCLUDE_LIB_INSTRUMENTATION_ASAN_H_
