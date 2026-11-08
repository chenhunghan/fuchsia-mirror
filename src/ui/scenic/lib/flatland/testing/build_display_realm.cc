// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/testing/build_display_realm.h"

#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>

namespace flatland::testing {

component_testing::RealmRoot BuildFakeDisplayRealm(async_dispatcher_t* dispatcher,
                                                   const DisplayRealmConfig& config) {
  component_testing::RealmBuilder builder = component_testing::RealmBuilder::Create();
  static const std::string kFakeDisplayStackHostChildName = "fake-display-stack-host";
  builder.AddChild(std::string(kFakeDisplayStackHostChildName), "#meta/fake-display-stack-host.cm");

  // Route capabilities from the test to the child component.
  builder.AddRoute(component_testing::Route{
      .capabilities = {component_testing::Protocol{.name = "fuchsia.sysmem2.Allocator"},
                       component_testing::Protocol{.name = "fuchsia.tracing.provider.Registry"}},
      .source = component_testing::ParentRef(),
      .targets = {component_testing::ChildRef{.name = kFakeDisplayStackHostChildName}},
  });

  // Route capabilities from the child component to the test.
  builder.AddRoute(component_testing::Route{
      .capabilities = {component_testing::Service{.name = "fuchsia.hardware.display.Service"}},
      .source = component_testing::ChildRef{.name = kFakeDisplayStackHostChildName},
      .targets = {component_testing::ParentRef()},
  });

  // Route configurations to fake-display-stack-host.
  std::vector<component_testing::ConfigCapability> configurations;
  std::vector<component_testing::Capability> capabilities;
  if (config.active_width_px != 0) {
    configurations.push_back({
        .name = "fuchsia.display.testing.ActiveWidthPx",
        .value = component_testing::ConfigValue::Uint32(config.active_width_px),
    });
    capabilities.push_back(
        component_testing::Config{.name = "fuchsia.display.testing.ActiveWidthPx"});
  }
  if (config.active_height_px != 0) {
    configurations.push_back({
        .name = "fuchsia.display.testing.ActiveHeightPx",
        .value = component_testing::ConfigValue::Uint32(config.active_height_px),
    });
    capabilities.push_back(
        component_testing::Config{.name = "fuchsia.display.testing.ActiveHeightPx"});
  }
  if (config.refresh_rate_millihertz != 0) {
    configurations.push_back({
        .name = "fuchsia.display.testing.RefreshRateMillihertz",
        .value = component_testing::ConfigValue::Uint32(config.refresh_rate_millihertz),
    });
    capabilities.push_back(
        component_testing::Config{.name = "fuchsia.display.testing.RefreshRateMillihertz"});
  }
  if (config.max_layer_count != 0) {
    configurations.push_back({
        .name = "fuchsia.display.testing.MaxLayerCount",
        .value = component_testing::ConfigValue::Uint32(config.max_layer_count),
    });
    capabilities.push_back(
        component_testing::Config{.name = "fuchsia.display.testing.MaxLayerCount"});
  }
  if (!configurations.empty()) {
    builder.AddConfiguration(std::move(configurations));
    builder.AddRoute(component_testing::Route{
        .capabilities = std::move(capabilities),
        .source = component_testing::SelfRef(),
        .targets = {component_testing::ChildRef{.name = kFakeDisplayStackHostChildName}},
    });
  }
  return builder.Build(dispatcher);
}

}  // namespace flatland::testing
