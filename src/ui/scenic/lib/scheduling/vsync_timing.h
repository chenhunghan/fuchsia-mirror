// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_SCHEDULING_VSYNC_TIMING_H_
#define SRC_UI_SCENIC_LIB_SCHEDULING_VSYNC_TIMING_H_

#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <atomic>

#include "src/ui/scenic/lib/scheduling/median_duration_predictor.h"

namespace scheduling {

class VsyncTiming {
 public:
  VsyncTiming();

  // Obtain the time of the last Vsync, in nanoseconds.
  zx::time last_vsync_time() const {
    return zx::time(last_vsync_time_ns_.load(std::memory_order_relaxed));
  }

  void set_last_vsync_time(zx::time last_vsync_time) {
    last_vsync_time_ns_.store(last_vsync_time.get(), std::memory_order_relaxed);
  }

  // Obtain the current VSync interval, in nanoseconds.
  zx::duration vsync_interval() const {
    return zx::duration(vsync_interval_ns_.load(std::memory_order_relaxed));
  }

  // Set an authoritative display VSync interval directly and reset the empirical predictor history.
  void set_vsync_interval(zx::duration vsync_interval) {
    FX_DCHECK(vsync_interval.get() > 0);
    vsync_interval_predictor_.Reset(vsync_interval);
    vsync_interval_ns_.store(vsync_interval.get(), std::memory_order_relaxed);
  }

  // Feed an empirical inter-VSync measurement into the median filter (for fixed-rate and emulator
  // displays).
  void AddVsyncInterval(zx::duration vsync_interval) {
    FX_DCHECK(vsync_interval.get() > 0);
    vsync_interval_predictor_.InsertNewMeasurement(vsync_interval);
    vsync_interval_ns_.store(vsync_interval_predictor_.GetPrediction().get(),
                             std::memory_order_relaxed);
  }

 private:
  // Used as a default value before real timings arrive.
  static constexpr zx::duration kNsecsFor60fps = zx::nsec(16'666'667);  // 16.666667ms

  std::atomic<zx_time_t> last_vsync_time_ns_{0};
  std::atomic<zx_duration_t> vsync_interval_ns_{kNsecsFor60fps.get()};
  MedianDurationPredictor vsync_interval_predictor_{kNsecsFor60fps};
};

}  // namespace scheduling

#endif  // SRC_UI_SCENIC_LIB_SCHEDULING_VSYNC_TIMING_H_
