// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/bin/health_inspector.h"

#include <lib/fpromise/single_threaded_executor.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/reader.h>

#include <memory>
#include <optional>
#include <string>
#include <type_traits>

#include <gtest/gtest.h>

namespace scenic_impl {
namespace {

// Non-movable because `HealthInspector` registers `[this]` with `CreateLazyValues()`.
static_assert(!std::is_move_constructible_v<HealthInspector>);

class HealthInspectorTest : public ::testing::Test {
 protected:
  std::optional<inspect::Hierarchy> ReadHealth() const {
    auto result = fpromise::run_single_threaded(inspect::ReadFromInspector(inspector_));
    if (!result.is_ok()) {
      ADD_FAILURE() << "Failed to read Inspect hierarchy";
      return std::nullopt;
    }
    for (inspect::Hierarchy& child : result.take_value().take_children()) {
      if (child.name() == "health") {
        return std::move(child);
      }
    }
    ADD_FAILURE() << "Missing Inspect child node: health";
    return std::nullopt;
  }

  static std::optional<std::string> GetString(const inspect::Hierarchy& node,
                                              const std::string& name) {
    const auto* prop = node.node().get_property<inspect::StringPropertyValue>(name);
    if (!prop) {
      ADD_FAILURE() << "Missing string property: " << name;
      return std::nullopt;
    }
    return prop->value();
  }

  static std::optional<uint64_t> GetUint(const inspect::Hierarchy& node, const std::string& name) {
    const auto* prop = node.node().get_property<inspect::UintPropertyValue>(name);
    if (!prop) {
      ADD_FAILURE() << "Missing uint property: " << name;
      return std::nullopt;
    }
    return prop->value();
  }

  static std::optional<int64_t> GetInt(const inspect::Hierarchy& node, const std::string& name) {
    const auto* prop = node.node().get_property<inspect::IntPropertyValue>(name);
    if (!prop) {
      ADD_FAILURE() << "Missing int property: " << name;
      return std::nullopt;
    }
    return prop->value();
  }

  void EnableStalledVsyncDisplay() {
    display_manager_.emplace([] {});
    display_ = std::make_shared<display::Display>(display::WireDisplayId{.value = 1}, 640, 480, 1);
    display_->OnVsync(zx::time_monotonic(zx_clock_get_monotonic()) - zx::sec(10), {});
    display_manager_->SetDefaultDisplayForTests(display_);
    display_power_manager_.emplace(inspector_.GetRoot(),
                                   [](fuchsia_hardware_display_types::PowerMode) { return ZX_OK; });
  }

  inspect::Inspector inspector_;
  std::shared_ptr<display::Display> display_;
  std::optional<display::DisplayManager> display_manager_;
  std::optional<display::DisplayPowerManager> display_power_manager_;
};

TEST_F(HealthInspectorTest, CleanWhenNoTrackerGetter) {
  HealthInspector health_inspector(display_manager_, display_power_manager_, inspector_.GetRoot(),
                                   {});

  const std::optional<inspect::Hierarchy> health = ReadHealth();
  ASSERT_TRUE(health.has_value());
  EXPECT_EQ(GetString(*health, "overall_health"), "NOT_UNHEALTHY");
  EXPECT_EQ(health->GetByPath({"ERROR_omitted_layers"}), nullptr);
  EXPECT_EQ(health->GetByPath({"omitted_layers"}), nullptr);
}

TEST_F(HealthInspectorTest, CleanWhenTrackerIsNull) {
  HealthInspector health_inspector(
      display_manager_, display_power_manager_, inspector_.GetRoot(),
      [] { return static_cast<const flatland::OmittedLayerTracker*>(nullptr); });

  const std::optional<inspect::Hierarchy> health = ReadHealth();
  ASSERT_TRUE(health.has_value());
  EXPECT_EQ(GetString(*health, "overall_health"), "NOT_UNHEALTHY");
  EXPECT_EQ(health->GetByPath({"ERROR_omitted_layers"}), nullptr);
  EXPECT_EQ(health->GetByPath({"omitted_layers"}), nullptr);
}

TEST_F(HealthInspectorTest, CleanWhenNoOmittedLayers) {
  flatland::OmittedLayerTracker tracker;
  HealthInspector health_inspector(display_manager_, display_power_manager_, inspector_.GetRoot(),
                                   [&tracker] { return &tracker; });

  const std::optional<inspect::Hierarchy> health = ReadHealth();
  ASSERT_TRUE(health.has_value());
  EXPECT_EQ(GetString(*health, "overall_health"), "NOT_UNHEALTHY");
  EXPECT_EQ(health->GetByPath({"ERROR_omitted_layers"}), nullptr);
  EXPECT_EQ(health->GetByPath({"omitted_layers"}), nullptr);
}

TEST_F(HealthInspectorTest, ReportsUnhealthyWhenVsyncsStall) {
  EnableStalledVsyncDisplay();
  flatland::OmittedLayerTracker tracker;
  HealthInspector health_inspector(display_manager_, display_power_manager_, inspector_.GetRoot(),
                                   [&tracker] { return &tracker; });

  const std::optional<inspect::Hierarchy> health = ReadHealth();
  ASSERT_TRUE(health.has_value());
  EXPECT_EQ(GetString(*health, "overall_health"), "UNHEALTHY");
  const inspect::Hierarchy* vsync_error =
      health->GetByPath({"ERROR_not_receiving_vsyncs_when_display_is_on"});
  ASSERT_NE(vsync_error, nullptr);
  const std::optional<int64_t> time_since_last_vsync_ms =
      GetInt(*vsync_error, "time_since_last_vsync_ms");
  ASSERT_TRUE(time_since_last_vsync_ms.has_value());
  EXPECT_GE(*time_since_last_vsync_ms, 10'000);
  EXPECT_EQ(health->GetByPath({"ERROR_omitted_layers"}), nullptr);

  display_->OnVsync(zx::time_monotonic(zx_clock_get_monotonic()), {});
  {
    const std::optional<inspect::Hierarchy> recovered = ReadHealth();
    ASSERT_TRUE(recovered.has_value());
    EXPECT_EQ(GetString(*recovered, "overall_health"), "NOT_UNHEALTHY");
    EXPECT_EQ(recovered->GetByPath({"ERROR_not_receiving_vsyncs_when_display_is_on"}), nullptr);
  }
}

TEST_F(HealthInspectorTest, ReportsBothFailuresInOneSnapshot) {
  EnableStalledVsyncDisplay();
  flatland::OmittedLayerTracker tracker;
  HealthInspector health_inspector(display_manager_, display_power_manager_, inspector_.GetRoot(),
                                   [&tracker] { return &tracker; });

  flatland::OmittedLayerTracker::ReasonCounts one_failed{};
  one_failed[flatland::OmittedLayerTracker::ReasonIndex(
      flatland::OmittedLayerReason::kGpuImportFailed)] = 1;
  tracker.RecordFrame(one_failed, zx::time_monotonic(1234));

  const std::optional<inspect::Hierarchy> health = ReadHealth();
  ASSERT_TRUE(health.has_value());
  EXPECT_EQ(GetString(*health, "overall_health"), "UNHEALTHY");
  const inspect::Hierarchy* vsync_error =
      health->GetByPath({"ERROR_not_receiving_vsyncs_when_display_is_on"});
  ASSERT_NE(vsync_error, nullptr);
  const std::optional<int64_t> time_since_last_vsync_ms =
      GetInt(*vsync_error, "time_since_last_vsync_ms");
  ASSERT_TRUE(time_since_last_vsync_ms.has_value());
  EXPECT_GE(*time_since_last_vsync_ms, 10'000);
  EXPECT_NE(health->GetByPath({"ERROR_omitted_layers"}), nullptr);
  EXPECT_NE(health->GetByPath({"omitted_layers"}), nullptr);
}

TEST_F(HealthInspectorTest, ReportsUnhealthyForOmittedLayersAndRecoversAfterTenCleanFrames) {
  flatland::OmittedLayerTracker tracker;
  HealthInspector health_inspector(display_manager_, display_power_manager_, inspector_.GetRoot(),
                                   [&tracker] { return &tracker; });

  flatland::OmittedLayerTracker::ReasonCounts two_failed{};
  two_failed[flatland::OmittedLayerTracker::ReasonIndex(
      flatland::OmittedLayerReason::kGpuImportFailed)] = 2;
  flatland::OmittedLayerTracker::ReasonCounts one_failed{};
  one_failed[flatland::OmittedLayerTracker::ReasonIndex(
      flatland::OmittedLayerReason::kGpuImportFailed)] = 1;

  // Record 8 unhealthy frames and 2 clean frames across 10 frames.
  for (int i = 0; i < 7; ++i) {
    tracker.RecordFrame(one_failed, zx::time_monotonic(1000 + i));
  }
  tracker.RecordCleanFrame();
  tracker.RecordFrame(two_failed, zx::time_monotonic(5555));
  tracker.RecordCleanFrame();

  {
    const std::optional<inspect::Hierarchy> health = ReadHealth();
    ASSERT_TRUE(health.has_value());
    EXPECT_EQ(GetString(*health, "overall_health"), "UNHEALTHY");

    const inspect::Hierarchy* error_node = health->GetByPath({"ERROR_omitted_layers"});
    ASSERT_NE(error_node, nullptr);
    EXPECT_EQ(GetUint(*error_node, "recent_frames"), 8u);
    EXPECT_EQ(GetUint(*error_node, "window_size_frames"), 10u);

    const inspect::Hierarchy* recent_reasons = error_node->GetByPath({"recent_layers_by_reason"});
    ASSERT_NE(recent_reasons, nullptr);
    EXPECT_EQ(GetUint(*recent_reasons, "gpu_import_failed"), 9u);

    const inspect::Hierarchy* history = health->GetByPath({"omitted_layers"});
    ASSERT_NE(history, nullptr);
    EXPECT_EQ(GetUint(*history, "total_frames"), 8u);
    EXPECT_EQ(GetInt(*history, "last_omitted_frame_mono_ns"), 5555);

    const inspect::Hierarchy* total_by_reason = history->GetByPath({"total_layers_by_reason"});
    ASSERT_NE(total_by_reason, nullptr);
    EXPECT_EQ(GetUint(*total_by_reason, "gpu_import_failed"), 9u);
  }

  // After 8 more clean frames, only the `two_failed` frame (time 5555) is left in the window.
  for (int i = 0; i < 8; ++i) {
    tracker.RecordCleanFrame();
  }
  {
    const std::optional<inspect::Hierarchy> health = ReadHealth();
    ASSERT_TRUE(health.has_value());
    EXPECT_EQ(GetString(*health, "overall_health"), "UNHEALTHY");

    const inspect::Hierarchy* error_node = health->GetByPath({"ERROR_omitted_layers"});
    ASSERT_NE(error_node, nullptr);
    EXPECT_EQ(GetUint(*error_node, "recent_frames"), 1u);
    const inspect::Hierarchy* recent_reasons = error_node->GetByPath({"recent_layers_by_reason"});
    ASSERT_NE(recent_reasons, nullptr);
    EXPECT_EQ(GetUint(*recent_reasons, "gpu_import_failed"), 2u);

    const inspect::Hierarchy* history = health->GetByPath({"omitted_layers"});
    ASSERT_NE(history, nullptr);
    EXPECT_EQ(GetUint(*history, "total_frames"), 8u);
    EXPECT_EQ(GetInt(*history, "last_omitted_frame_mono_ns"), 5555);
    const inspect::Hierarchy* total_by_reason = history->GetByPath({"total_layers_by_reason"});
    ASSERT_NE(total_by_reason, nullptr);
    EXPECT_EQ(GetUint(*total_by_reason, "gpu_import_failed"), 9u);
  }

  // The 10th clean frame after the last omitted frame clears the unhealthy state while retaining
  // cumulative counts and the last omitted frame timestamp.
  tracker.RecordCleanFrame();
  {
    const std::optional<inspect::Hierarchy> health = ReadHealth();
    ASSERT_TRUE(health.has_value());
    EXPECT_EQ(GetString(*health, "overall_health"), "NOT_UNHEALTHY");
    EXPECT_EQ(health->GetByPath({"ERROR_omitted_layers"}), nullptr);

    const inspect::Hierarchy* history = health->GetByPath({"omitted_layers"});
    ASSERT_NE(history, nullptr);
    EXPECT_EQ(GetUint(*history, "total_frames"), 8u);
    EXPECT_EQ(GetInt(*history, "last_omitted_frame_mono_ns"), 5555);

    const inspect::Hierarchy* total_by_reason = history->GetByPath({"total_layers_by_reason"});
    ASSERT_NE(total_by_reason, nullptr);
    EXPECT_EQ(GetUint(*total_by_reason, "gpu_import_failed"), 9u);
  }
}

}  // namespace
}  // namespace scenic_impl
