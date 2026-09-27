// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/display_power_manager.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/reader.h>

#include <cstdint>
#include <string>
#include <string_view>
#include <vector>

#include <gtest/gtest.h>

namespace display::test {

namespace {

using PowerMode = fuchsia_ui_display_singleton::PowerMode;

// The newest entry of the `display_power_events` inspect node.
struct InspectedPowerEvent {
  // The power mode, plus "_ERROR_<status>" if the request failed.
  std::string name;
  int64_t boot_ns = 0;
  int64_t mono_ns = 0;
};

struct DisplayPowerInfo {
  PowerMode power_mode = PowerMode::kOn;
  int64_t mono_ns = 0;
};

class DisplayPowerManagerTest : public ::testing::Test {
 public:
  DisplayPowerManagerTest()
      : display_power_manager_(inspector_.GetRoot(),
                               [this](fuchsia_hardware_display_types::PowerMode mode) {
                                 requested_modes_.push_back(mode);
                                 return next_status_;
                               }) {}

  DisplayPowerManager* display_power_manager() { return &display_power_manager_; }

  InspectedPowerEvent GetLastInspectedPowerEvent() {
    auto result = inspect::ReadFromVmo(inspector_.DuplicateVmo());
    EXPECT_TRUE(result.is_ok());
    auto hierarchy = result.take_value();
    const auto& power_node = hierarchy.children()[0];
    EXPECT_EQ("display_power_events", power_node.name());

    InspectedPowerEvent event;
    if (power_node.children().empty()) {
      return event;
    }
    const auto& node = power_node.children().back().node();

    // The name comes from the "<name>_mono_ns" property, not from the unsuffixed "<name>"
    // property, which names no timeline and is slated for removal.
    constexpr std::string_view kMonoSuffix = "_mono_ns";
    for (const auto& property : node.properties()) {
      const std::string& property_name = property.name();
      if (property_name.ends_with(kMonoSuffix)) {
        event.name = property_name.substr(0, property_name.size() - kMonoSuffix.size());
        event.mono_ns = property.Get<inspect::IntPropertyValue>().value();
        break;
      }
    }
    EXPECT_FALSE(event.name.empty());
    EXPECT_NE(event.mono_ns, 0);

    const auto* boot = node.get_property<inspect::IntPropertyValue>(event.name + "_boot_ns");
    EXPECT_NE(boot, nullptr);
    if (boot) {
      event.boot_ns = boot->value();
    }
    EXPECT_NE(event.boot_ns, 0);

    return event;
  }

  // Piggybacks on `GetLastInspectedPowerEvent()`.
  DisplayPowerInfo GetLastDisplayPowerInfo() {
    InspectedPowerEvent event = GetLastInspectedPowerEvent();
    DisplayPowerInfo info;
    info.mono_ns = event.mono_ns;
    EXPECT_TRUE(event.name == "on" || event.name == "off" || event.name == "doze" ||
                event.name == "doze_suspend");
    if (event.name == "on") {
      info.power_mode = PowerMode::kOn;
    } else if (event.name == "off") {
      info.power_mode = PowerMode::kOff;
    } else if (event.name == "doze") {
      info.power_mode = PowerMode::kDoze;
    } else if (event.name == "doze_suspend") {
      info.power_mode = PowerMode::kDozeSuspend;
    }
    return info;
  }

 protected:
  // What the closure received, and what it returns next.
  std::vector<fuchsia_hardware_display_types::PowerMode> requested_modes_;
  zx_status_t next_status_ = ZX_OK;

 private:
  inspect::Inspector inspector_;
  DisplayPowerManager display_power_manager_;
};

TEST_F(DisplayPowerManagerTest, PowerModeGeneratesVsyncs) {
  EXPECT_TRUE(PowerModeGeneratesVsyncs(PowerMode::kOn));
  EXPECT_TRUE(PowerModeGeneratesVsyncs(PowerMode::kDoze));
  EXPECT_TRUE(PowerModeGeneratesVsyncs(PowerMode::kDozeSuspend));
  EXPECT_FALSE(PowerModeGeneratesVsyncs(PowerMode::kOff));
}

TEST_F(DisplayPowerManagerTest, Ok) {
  EXPECT_EQ(display_power_manager()->current_power_mode(), PowerMode::kOn);

  bool callback_1_executed = false;
  display_power_manager()->SetPowerMode(PowerMode::kOff,
                                        [&callback_1_executed](fit::result<zx_status_t> result) {
                                          callback_1_executed = true;
                                          EXPECT_TRUE(result.is_ok());
                                        });
  EXPECT_TRUE(callback_1_executed);
  EXPECT_EQ(requested_modes_, std::vector{fuchsia_hardware_display_types::PowerMode::kOff});
  EXPECT_EQ(display_power_manager()->current_power_mode(), PowerMode::kOff);
  auto power_info_1 = GetLastDisplayPowerInfo();
  EXPECT_EQ(power_info_1.power_mode, PowerMode::kOff);
  const auto last_power_mono_ns = power_info_1.mono_ns;

  bool callback_2_executed = false;
  display_power_manager()->SetPowerMode(PowerMode::kOn,
                                        [&callback_2_executed](fit::result<zx_status_t> result) {
                                          callback_2_executed = true;
                                          EXPECT_TRUE(result.is_ok());
                                        });
  EXPECT_TRUE(callback_2_executed);
  EXPECT_EQ(requested_modes_, (std::vector{fuchsia_hardware_display_types::PowerMode::kOff,
                                           fuchsia_hardware_display_types::PowerMode::kOn}));
  EXPECT_EQ(display_power_manager()->current_power_mode(), PowerMode::kOn);
  auto power_info_2 = GetLastDisplayPowerInfo();
  EXPECT_EQ(power_info_2.power_mode, PowerMode::kOn);
  EXPECT_GT(power_info_2.mono_ns, last_power_mono_ns);
}

TEST_F(DisplayPowerManagerTest, NoDisplay) {
  next_status_ = ZX_ERR_NOT_FOUND;

  bool callback_executed = false;
  display_power_manager()->SetPowerMode(PowerMode::kOff,
                                        [&callback_executed](fit::result<zx_status_t> result) {
                                          callback_executed = true;
                                          ASSERT_TRUE(result.is_error());
                                          EXPECT_EQ(result.error_value(), ZX_ERR_NOT_FOUND);
                                        });
  EXPECT_TRUE(callback_executed);
  EXPECT_EQ(requested_modes_, std::vector{fuchsia_hardware_display_types::PowerMode::kOff});
  EXPECT_EQ(display_power_manager()->current_power_mode(), PowerMode::kOn);
  auto power_event = GetLastInspectedPowerEvent();
  EXPECT_EQ(power_event.name, "off_ERROR_ZX_ERR_NOT_FOUND");
}

TEST_F(DisplayPowerManagerTest, NotSupported) {
  next_status_ = ZX_ERR_NOT_SUPPORTED;

  bool callback_executed = false;
  display_power_manager()->SetPowerMode(PowerMode::kOff,
                                        [&callback_executed](fit::result<zx_status_t> result) {
                                          callback_executed = true;
                                          ASSERT_TRUE(result.is_error());
                                          EXPECT_EQ(result.error_value(), ZX_ERR_NOT_SUPPORTED);
                                        });
  EXPECT_TRUE(callback_executed);
  EXPECT_EQ(requested_modes_, std::vector{fuchsia_hardware_display_types::PowerMode::kOff});
  EXPECT_EQ(display_power_manager()->current_power_mode(), PowerMode::kOn);
  auto power_event = GetLastInspectedPowerEvent();
  EXPECT_EQ(power_event.name, "off_ERROR_ZX_ERR_NOT_SUPPORTED");
}

}  // namespace

}  // namespace display::test
