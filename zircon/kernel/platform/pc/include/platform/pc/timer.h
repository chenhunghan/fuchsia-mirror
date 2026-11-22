// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PLATFORM_PC_INCLUDE_PLATFORM_PC_TIMER_H_
#define ZIRCON_KERNEL_PLATFORM_PC_INCLUDE_PLATFORM_PC_TIMER_H_

#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/time.h>

__BEGIN_CDECLS

zx_duration_t rust_convert_raw_tsc_duration_to_nanoseconds(int64_t duration);
zx_instant_mono_t rust_convert_raw_tsc_timestamp_to_clock_monotonic(int64_t ts);

__END_CDECLS

inline zx_duration_t convert_raw_tsc_duration_to_nanoseconds(int64_t duration) {
  return rust_convert_raw_tsc_duration_to_nanoseconds(duration);
}

inline zx_instant_mono_t convert_raw_tsc_timestamp_to_clock_monotonic(int64_t ts) {
  return rust_convert_raw_tsc_timestamp_to_clock_monotonic(ts);
}

#endif  // ZIRCON_KERNEL_PLATFORM_PC_INCLUDE_PLATFORM_PC_TIMER_H_
