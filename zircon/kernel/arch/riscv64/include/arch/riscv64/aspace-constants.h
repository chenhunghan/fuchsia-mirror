// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_RISCV64_INCLUDE_ARCH_RISCV64_ASPACE_CONSTANTS_H_
#define ZIRCON_KERNEL_ARCH_RISCV64_INCLUDE_ARCH_RISCV64_ASPACE_CONSTANTS_H_

#include <stddef.h>

// Size and alignment of the Rust `Riscv64ArchVmAspace` that the C++
// `Riscv64ArchVmAspace` stores inline in an `OpaqueStorage`.  The size is an
// upper bound; a static_assert on the Rust side checks the real struct fits.
//
// The Rust struct contains a `ksync::KMutex`, whose raw lock grows by a lock
// class ID pointer when WITH_LOCK_DEP is enabled, and by another word when lock
// name tracing is on, so the lockdep bound leaves room for both.
#if WITH_LOCK_DEP
constexpr size_t kRiscv64ArchVmAspaceStateSize = 128;
#else
constexpr size_t kRiscv64ArchVmAspaceStateSize = 112;
#endif
constexpr size_t kRiscv64ArchVmAspaceStateAlign = 8;

#endif  // ZIRCON_KERNEL_ARCH_RISCV64_INCLUDE_ARCH_RISCV64_ASPACE_CONSTANTS_H_
