// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/bin/health_inspector.h"

#include <lib/fpromise/promise.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>

namespace scenic_impl {

HealthInspector::HealthInspector(
    const std::optional<display::DisplayManager>& display_manager,
    const std::optional<display::DisplayPowerManager>& display_power_manager,
    inspect::Node& parent_node, GetOmittedLayerTrackerFn get_omitted_layer_tracker)
    : display_manager_(display_manager),
      display_power_manager_(display_power_manager),
      get_omitted_layer_tracker_(std::move(get_omitted_layer_tracker)) {
  InitializeInspectObjects(parent_node);
}

void HealthInspector::InitializeInspectObjects(inspect::Node& parent_node) {
  // The user prefers "health" to be at the top, potentially as a peer to "subsystems".
  health_node_ = parent_node.CreateChild("health");
  inspect_lazy_node_ = health_node_.CreateLazyValues("metrics", [this] {
    // Precautionary: Scenic serves Inspect on the thread that owns these objects. If that ever
    // changes, guard these reads with a lock instead of relaxing this check.
    FX_CHECK(thread_checker_.is_thread_valid());
    TRACE_DURATION("gfx", "HealthInspectorLazyValues");
    inspect::Inspector inspector;
    auto& root = inspector.GetRoot();

    const zx::time_monotonic now(zx_clock_get_monotonic());
    // Evaluate every check before combining so simultaneous failures all record
    // their error child nodes in the same Inspect snapshot.
    const bool vsync_healthy = CheckReceivingVsyncsWhenDisplayIsOn(root, now, inspector);
    const bool layers_healthy = CheckNoOmittedLayers(root, inspector);
    const bool healthy = vsync_healthy && layers_healthy;
    root.CreateString("overall_health", healthy ? "NOT_UNHEALTHY" : "UNHEALTHY", &inspector);

    return fpromise::make_ok_promise(std::move(inspector));
  });
}

bool HealthInspector::CheckReceivingVsyncsWhenDisplayIsOn(inspect::Node& node,
                                                          zx::time_monotonic now,
                                                          inspect::Inspector& inspector) {
  if (!display_power_manager_ || !display_manager_) {
    return true;  // Can't check, assume OK.
  }

  const auto power_mode = display_power_manager_->current_power_mode();
  // Only `kOff` stops VSync events; the doze modes still emit them.
  if (!display::PowerModeGeneratesVsyncs(power_mode)) {
    return true;
  }

  auto default_display = display_manager_->default_display();
  if (!default_display) {
    return true;  // No display, assume OK.
  }

  // Vsyncs are expected at 60fps, but we use a longer grace period to avoid false positives.
  constexpr zx::duration kGracePeriod = zx::sec(2);

  const zx::time_monotonic last_power_change = display_power_manager_->last_power_change_time();
  if (now - last_power_change < kGracePeriod) {
    return true;  // Still within grace period since display was powered on.
  }

  auto vsync_timing = default_display->vsync_timing();
  if (!vsync_timing) {
    return true;  // No vsync timing, assume OK.
  }
  const zx::time_monotonic last_vsync = vsync_timing->last_vsync_time();
  const zx::duration delta = now - last_vsync;

  const bool healthy = delta < kGracePeriod;
  if (!healthy) {
    // Record details in a specific error child node.
    inspect::Node error_node = node.CreateChild("ERROR_not_receiving_vsyncs_when_display_is_on");
    error_node.CreateInt("time_since_last_vsync_ms", delta.to_msecs(), &inspector);
    inspector.emplace(std::move(error_node));
  }

  return healthy;
}

bool HealthInspector::CheckNoOmittedLayers(inspect::Node& node,
                                           inspect::Inspector& inspector) const {
  const flatland::OmittedLayerTracker* tracker =
      get_omitted_layer_tracker_ ? get_omitted_layer_tracker_() : nullptr;
  if (!tracker) {
    return true;
  }

  const uint64_t total_frames = tracker->total_frames_with_omitted_layers();
  if (total_frames == 0) {
    return true;
  }

  const auto populate_reasons = [&inspector](inspect::Node& parent, const char* child_name,
                                             auto&& count_for_reason) {
    inspect::Node child = parent.CreateChild(child_name);
    for (size_t i = 0; i < flatland::OmittedLayerTracker::kReasonCount; ++i) {
      const auto reason = static_cast<flatland::OmittedLayerReason>(i);
      child.CreateUint(flatland::OmittedLayerReasonName(reason), count_for_reason(reason),
                       &inspector);
    }
    inspector.emplace(std::move(child));
  };

  inspect::Node history_node = node.CreateChild("omitted_layers");
  history_node.CreateUint("total_frames", total_frames, &inspector);
  history_node.CreateInt("last_omitted_frame_mono_ns", tracker->last_omitted_frame_time().get(),
                         &inspector);
  populate_reasons(
      history_node, "total_layers_by_reason",
      [tracker](flatland::OmittedLayerReason r) { return tracker->TotalOmittedLayers(r); });
  inspector.emplace(std::move(history_node));

  const uint32_t recent_frames = tracker->RecentFramesWithOmittedLayers();
  if (recent_frames == 0) {
    return true;
  }

  inspect::Node error_node = node.CreateChild("ERROR_omitted_layers");
  error_node.CreateUint("recent_frames", recent_frames, &inspector);
  error_node.CreateUint("window_size_frames", flatland::OmittedLayerTracker::kWindowSize,
                        &inspector);
  populate_reasons(
      error_node, "recent_layers_by_reason",
      [tracker](flatland::OmittedLayerReason r) { return tracker->RecentOmittedLayers(r); });
  inspector.emplace(std::move(error_node));
  return false;
}

}  // namespace scenic_impl
