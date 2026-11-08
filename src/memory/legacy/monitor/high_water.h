// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEMORY_LEGACY_MONITOR_HIGH_WATER_H_
#define SRC_MEMORY_LEGACY_MONITOR_HIGH_WATER_H_

#include <lib/fit/function.h>

#include <string>

#include "src/lib/fxl/macros.h"
#include "src/memory/legacy/metrics/capture.h"
#include "src/memory/legacy/metrics/digest.h"
#include "src/memory/legacy/metrics/summary.h"
#include "src/memory/legacy/metrics/watcher.h"
namespace monitor {

class HighWater {
 public:
  using DigestCb = fit::function<void(const memory::Capture&, memory::Digest*)>;

  HighWater(const std::string& dir, zx::duration poll_frequency, uint64_t high_water_threshold,
            async_dispatcher_t* dispatcher, memory::CaptureFn capture_cb, DigestCb digest_cb);
  ~HighWater() = default;

  void RecordHighWater(const memory::Capture& capture);
  void RecordHighWaterDigest(const memory::Digest& digest);
  std::string GetHighWater();
  std::string GetPreviousHighWater();
  std::string GetHighWaterDigest();
  std::string GetPreviousHighWaterDigest();

 private:
  std::string GetFile(const char* filename);
  const std::string dir_;
  memory::Watcher watcher_;
  memory::Namer namer_;
  DigestCb digest_cb_;
  std::mutex mutex_;
  FXL_DISALLOW_COPY_AND_ASSIGN(HighWater);
};

}  // namespace monitor

#endif  // SRC_MEMORY_LEGACY_MONITOR_HIGH_WATER_H_
