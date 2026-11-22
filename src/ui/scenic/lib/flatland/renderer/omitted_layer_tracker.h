// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_RENDERER_OMITTED_LAYER_TRACKER_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_RENDERER_OMITTED_LAYER_TRACKER_H_

#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>
#include <zircon/compiler.h>

#include <array>
#include <cstddef>
#include <cstdint>
#include <utility>

namespace flatland {

// Reasons why a composed frame left out a scene-graph layer. Layers dropped
// after an explicit `ReleaseImageImmediately()` call do not count as omitted,
// because the client asked Scenic to stop using the image at once.
enum class OmittedLayerReason : uint8_t {
  kGpuImportFailed = 0,
  kCount,
};

constexpr const char* OmittedLayerReasonName(OmittedLayerReason reason) {
  switch (reason) {
    case OmittedLayerReason::kGpuImportFailed:
      return "gpu_import_failed";
    case OmittedLayerReason::kCount:
      break;
  }
  FX_CHECK(false) << "Invalid OmittedLayerReason: "
                  << static_cast<uint32_t>(std::to_underlying(reason));
  __UNREACHABLE;
}

// Main-thread `kWindowSize`-frame ring and cumulative counters for omitted layers, read by
// `scenic_impl::HealthInspector` when Inspect is queried. The recent window
// covers the last `kWindowSize` composed frames, however old, so an idle screen after an
// omitted frame stays in the window until `kWindowSize` more frames are composed.
class OmittedLayerTracker {
 public:
  // Retains the last 10 composed frames so transient skips stay visible briefly after recovery.
  static constexpr size_t kWindowSize = 10;
  static constexpr size_t kReasonCount = std::to_underlying(OmittedLayerReason::kCount);

  // Plain `std::array` alias; callers must value-initialize (`ReasonCounts{}`) before setting
  // per-reason counts via `ReasonIndex()`.
  using ReasonCounts = std::array<uint32_t, kReasonCount>;

  static constexpr size_t ReasonIndex(OmittedLayerReason reason) {
    const size_t index = std::to_underlying(reason);
    FX_CHECK(index < kReasonCount);
    return index;
  }

  // Records one composed frame. An all-zero `counts` advances the `kWindowSize`-frame
  // window so the health check recovers `kWindowSize` clean frames after a transient skip.
  // `now` is ignored when `counts` is all zero.
  void RecordFrame(const ReasonCounts& counts, zx::time_monotonic now) {
    ring_[next_slot_] = counts;
    next_slot_ = (next_slot_ + 1) % kWindowSize;

    bool any_omitted = false;
    for (size_t i = 0; i < kReasonCount; ++i) {
      if (counts[i] > 0) {
        any_omitted = true;
        total_omitted_layers_by_reason_[i] += counts[i];
      }
    }
    if (any_omitted) {
      ++total_frames_with_omitted_layers_;
      last_omitted_frame_time_ = now;
    }
  }

  void RecordCleanFrame() { RecordFrame({}, zx::time_monotonic{0}); }

  uint32_t RecentFramesWithOmittedLayers() const {
    uint32_t frames = 0;
    for (const ReasonCounts& slot : ring_) {
      for (uint32_t count : slot) {
        if (count > 0) {
          ++frames;
          break;
        }
      }
    }
    return frames;
  }

  uint64_t RecentOmittedLayers(OmittedLayerReason reason) const {
    const size_t idx = ReasonIndex(reason);
    uint64_t sum = 0;
    for (const ReasonCounts& slot : ring_) {
      sum += slot[idx];
    }
    return sum;
  }

  uint64_t total_frames_with_omitted_layers() const { return total_frames_with_omitted_layers_; }

  uint64_t TotalOmittedLayers(OmittedLayerReason reason) const {
    return total_omitted_layers_by_reason_[ReasonIndex(reason)];
  }

  zx::time_monotonic last_omitted_frame_time() const { return last_omitted_frame_time_; }

 private:
  std::array<ReasonCounts, kWindowSize> ring_{};
  size_t next_slot_ = 0;
  uint64_t total_frames_with_omitted_layers_ = 0;
  std::array<uint64_t, kReasonCount> total_omitted_layers_by_reason_{};
  zx::time_monotonic last_omitted_frame_time_{0};
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_RENDERER_OMITTED_LAYER_TRACKER_H_
