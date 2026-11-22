// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_ALLOCATION_INCLUDE_PHYS_ALLOCATION_BOUNDS_H_
#define ZIRCON_KERNEL_PHYS_ALLOCATION_INCLUDE_PHYS_ALLOCATION_BOUNDS_H_

// TODO(https://fxbug.dev/581892793): Use #elifdef when possible.
#if defined(__aarch64__)
#include <phys/allocation/arm64/bounds.h>
#elif defined(__riscv)
#include <phys/allocation/riscv64/bounds.h>
#elif defined(__x86_64__)
#include <phys/allocation/x86/bounds.h>
#else
#error "unsupported machine"
#endif

#endif  // ZIRCON_KERNEL_PHYS_ALLOCATION_INCLUDE_PHYS_ALLOCATION_BOUNDS_H_
