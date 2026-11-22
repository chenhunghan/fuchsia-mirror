// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/pdev/power/power.cc

#include <stdint.h>
#include <sys/types.h>
#include <zircon/types.h>

#include <dev/power.h>
#include <kernel/ffi.h>
#include <pdev/power.h>

extern "C" {

zx_status_t rust_power_cpu_on(uint64_t hw_cpu_id, paddr_t entry, uint64_t context);

void rust_pdev_register_power(const pdev_power_ops* ops);

}  // extern "C"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t power_cpu_on(uint64_t hw_cpu_id, paddr_t entry, uint64_t context) {
  return rust_power_cpu_on(hw_cpu_id, entry, context);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void pdev_register_power(const pdev_power_ops* ops) {
  rust_pdev_register_power(ops);
}
