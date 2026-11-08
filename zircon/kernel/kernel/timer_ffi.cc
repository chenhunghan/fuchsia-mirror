// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/timer.h>

#ifdef UNITTESTS_ENABLED
namespace {

// Timers whose storage comes from the constexpr Timer constructor (constinit rules out runtime
// initialization), for the timer_rust suite's constinit_timer test.
constinit Timer gConstinitMonotonicTimer{ZX_CLOCK_MONOTONIC};
constinit Timer gConstinitBootTimer{ZX_CLOCK_BOOT};

}  // namespace

extern "C" {

// Return never-set, constant-initialized Timers, on the monotonic and boot clocks respectively.
const Timer* cpp_timer_constinit_monotonic_timer_for_test() { return &gConstinitMonotonicTimer; }
const Timer* cpp_timer_constinit_boot_timer_for_test() { return &gConstinitBootTimer; }

}  // extern "C"
#endif
