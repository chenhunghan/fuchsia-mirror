// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "kernel/timer.h"

#include <assert.h>
#include <lib/affine/ratio.h>
#include <lib/arch/intrin.h>
#include <lib/zircon-internal/macros.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <kernel/spinlock.h>
#include <platform/timer.h>

namespace {

affine::Ratio gTicksToTime;
uint64_t gTicksPerSecond;

}  // anonymous namespace

void timer_set_ticks_to_time_ratio(const affine::Ratio& ticks_to_time) {
  // ASSERT that we are not calling this function twice.  Once set, this ratio
  // may not change.
  DEBUG_ASSERT(gTicksPerSecond == 0);
  DEBUG_ASSERT(ticks_to_time.numerator() != 0);
  DEBUG_ASSERT(ticks_to_time.denominator() != 0);
  gTicksToTime = ticks_to_time;
  gTicksPerSecond = gTicksToTime.Inverse().Scale(ZX_SEC(1));
}

const affine::Ratio& timer_get_ticks_to_time_ratio(void) { return gTicksToTime; }

zx_instant_mono_t current_mono_time(void) { return gTicksToTime.Scale(current_mono_ticks()); }

zx_instant_boot_t current_boot_time(void) { return gTicksToTime.Scale(current_boot_ticks()); }

InstantMonoAndBootTime current_mono_and_boot_time() {
  CurrentTicksObservation observation = timer_current_mono_and_boot_ticks();
  return {.mono_time = gTicksToTime.Scale(observation.mono_now),
          .boot_time = gTicksToTime.Scale(observation.boot_now)};
}

InstantMonoTimeAndBootTicks current_mono_time_and_boot_ticks() {
  CurrentTicksObservation observation = timer_current_mono_and_boot_ticks();
  return {.mono_time = gTicksToTime.Scale(observation.mono_now),
          .boot_ticks = observation.boot_now};
}

zx_ticks_t ticks_per_second(void) { return gTicksPerSecond; }

// This overload stays in C++ because |lock| is a raw C++ MonitoredSpinLock (without the lockdep
// wrapper), which has no Rust counterpart. It reads the cancel flag through the Rust Timer.
zx_status_t Timer::TrylockOrCancel(MonitoredSpinLock* lock) {
  // spin trylocking on the passed in spinlock either waiting for it
  // to grab or the passed in timer to be canceled.
  while (unlikely(lock->TryAcquire(SOURCE_TAG))) {
    // we failed to grab it, check for cancel
    if (rust_timer_cancel_pending(this)) {
      // we were canceled, so bail immediately
      return ZX_ERR_TIMED_OUT;
    }
    // tell the arch to wait
    arch::Yield();
  }

  return ZX_OK;
}
