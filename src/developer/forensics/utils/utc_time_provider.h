// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_UTILS_UTC_TIME_PROVIDER_H_
#define SRC_DEVELOPER_FORENSICS_UTILS_UTC_TIME_PROVIDER_H_

#include <optional>
#include <string>

#include "src/developer/forensics/utils/utc_clock_ready_watcher.h"
#include "src/lib/timekeeper/clock.h"

namespace forensics {

// Provides the UTC time only if the device's UTC clock has achieved logging quality.
//
// Can be configured to record the UTC-boot difference of the current boot and read the one from the
// previous boot by providing |utc_boot_difference_paths|.
class UtcTimeProvider {
 public:
  struct UtcBootDifferencePaths {
    // Where the UTC-boot difference of the current boot is written.
    std::string current_boot;

    // Where the UTC-boot difference of the previous boot is read from.
    std::string previous_boot;
  };

  UtcTimeProvider(UtcClockReadyWatcherBase* utc_clock_ready_watcher, timekeeper::Clock* clock);
  UtcTimeProvider(UtcClockReadyWatcherBase* utc_clock_ready_watcher, timekeeper::Clock* clock,
                  UtcBootDifferencePaths utc_boot_difference_paths);

  // Returns the current UTC time if the device's UTC time is accurate, std::nullopt otherwise.
  std::optional<timekeeper::time_utc> CurrentTime() const;

  // Returns the difference between the UTC clock and the device's boot time if the device's UTC
  // time is accurate, std::nullopt otherwise.
  //
  // This value can be added to a boot time to convert it to a UTC time.
  std::optional<zx::duration> CurrentUtcBootDifference() const;
  std::optional<zx::duration> PreviousBootUtcBootDifference() const;

 private:
  UtcTimeProvider(UtcClockReadyWatcherBase* utc_clock_ready_watcher, timekeeper::Clock* clock,
                  std::optional<UtcBootDifferencePaths> utc_boot_difference_paths);

  // Keep waiting on the clock handle until the clock has achieved logging quality.
  void OnClockLoggingQuality();

  timekeeper::Clock* clock_;

  std::optional<UtcBootDifferencePaths> utc_boot_difference_paths_;

  // The last difference between the UTC and boot clocks in the previous boot.
  std::optional<zx::duration> previous_boot_utc_boot_difference_;

  UtcClockReadyWatcherBase* utc_clock_ready_watcher_;
};

}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_UTILS_UTC_TIME_PROVIDER_H_
