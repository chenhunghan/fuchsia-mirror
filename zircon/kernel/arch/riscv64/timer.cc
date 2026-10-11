// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/affine/ratio.h>
#include <lib/zbi-format/driver-config.h>
#include <platform.h>
#include <trace.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <platform/timer.h>

extern "C" {

FFI_ALWAYS_INLINE void cpp_timer_tick() { timer_tick(); }

FFI_ALWAYS_INLINE void cpp_timer_set_conversion(uint32_t cntfrq, uint64_t initial_ticks) {
  affine::Ratio cntpct_to_nsec = {ZX_SEC(1), cntfrq};
  dprintf(SPEW, "riscv generic timer cntpct_per_nsec: %u/%u\n", cntpct_to_nsec.numerator(),
          cntpct_to_nsec.denominator());
  timer_set_ticks_to_time_ratio(cntpct_to_nsec);
  timer_set_initial_ticks(initial_ticks);
}

}  // extern "C"
