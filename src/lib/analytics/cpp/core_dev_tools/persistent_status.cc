// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/analytics/cpp/core_dev_tools/persistent_status.h"

#include "src/lib/analytics/cpp/metric_properties/metric_properties.h"
#include "src/lib/uuid/uuid.h"

namespace analytics::core_dev_tools::internal {

namespace {

constexpr char kEnabledProperty[] = "analytics-status";
constexpr char kInternalEnabledProperty[] = "analytics-status-internal";
constexpr char kUuidProperty[] = "uuid";

}  // namespace

PersistentStatus::PersistentStatus(std::string_view tool_name)
    : tool_name_(tool_name), launched_property_(tool_name_ + "-launched") {}

void PersistentStatus::Enable() {
  metric_properties::SetBool(kEnabledProperty, true);
  if (auto internal_status = metric_properties::Get(kInternalEnabledProperty);
      internal_status.has_value() && *internal_status != "1" && *internal_status != "2") {
    metric_properties::Set(kInternalEnabledProperty, "1");
  }
  metric_properties::Set(kUuidProperty, uuid::Uuid::Generate().ToString());
}

void PersistentStatus::Disable() {
  metric_properties::SetBool(kEnabledProperty, false);
  if (metric_properties::Exists(kInternalEnabledProperty)) {
    metric_properties::Set(kInternalEnabledProperty, "0");
  }
  metric_properties::Delete(kUuidProperty);
}

bool PersistentStatus::IsEnabled() {
  if (auto internal_status = metric_properties::Get(kInternalEnabledProperty);
      internal_status.has_value()) {
    return *internal_status == "1" || *internal_status == "2";
  }
  return metric_properties::GetBool(kEnabledProperty).value_or(false);
}

bool PersistentStatus::IsFirstLaunchOfFirstTool() {
  return !metric_properties::Exists(kInternalEnabledProperty) &&
         !metric_properties::Exists(kEnabledProperty);
}

void PersistentStatus::MarkAsDirectlyLaunched() { metric_properties::Set(launched_property_, ""); }

bool PersistentStatus::IsFirstDirectLaunch() const {
  return !metric_properties::Exists(launched_property_);
}

std::string PersistentStatus::GetUuid() {
  auto uuid = metric_properties::Get(kUuidProperty);
  if (uuid.has_value() && uuid::IsValid(*uuid)) {
    return *uuid;
  }
  std::string generated = uuid::Uuid::Generate().ToString();
  metric_properties::Set(kUuidProperty, generated);
  return generated;
}

}  // namespace analytics::core_dev_tools::internal
