// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARCH_OPS_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARCH_OPS_H_

#ifndef __ASSEMBLER__

#include <stdint.h>

#include <arch/arm64/feature.h>
#include <arch/arm64/interrupt.h>
#include <arch/arm64/mp.h>
#include <ktl/atomic.h>

inline uint32_t arch_cpu_features() { return arm64_isa_features; }

inline uint32_t arch_cpu_sve_features() {
  return ktl::atomic_ref(arm64_sve_features).load(ktl::memory_order_relaxed);
}

inline uint32_t arch_dcache_line_size() { return arm64_cache_features.dcache_size; }

inline uint32_t arch_icache_line_size() { return arm64_cache_features.icache_size; }

constexpr uint32_t arch_vm_features() { return 0; }

#endif  // __ASSEMBLER__

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARCH_OPS_H_
