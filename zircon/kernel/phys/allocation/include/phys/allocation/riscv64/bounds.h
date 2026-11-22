// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_ALLOCATION_INCLUDE_PHYS_ALLOCATION_RISCV64_BOUNDS_H_
#define ZIRCON_KERNEL_PHYS_ALLOCATION_INCLUDE_PHYS_ALLOCATION_RISCV64_BOUNDS_H_

#include <stdint.h>

#include <ktl/optional.h>

constexpr ktl::optional<uint64_t> kArchAllocationMinAddr = ktl::nullopt;
constexpr ktl::optional<uint64_t> kArchAllocationMaxAddr = ktl::nullopt;

#endif  // ZIRCON_KERNEL_PHYS_ALLOCATION_INCLUDE_PHYS_ALLOCATION_RISCV64_BOUNDS_H_
