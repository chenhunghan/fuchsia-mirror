// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "power-supply-visitor.h"

#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_properties.h>
#include <lib/driver/devicetree/visitors/common-types.h>
#include <lib/driver/devicetree/visitors/registration.h>
#include <lib/driver/logging/cpp/logger.h>

#include <string>
#include <vector>

#include <bind/fuchsia/cpp/bind.h>

namespace power_supply_visitor_dt {
namespace {

// Maps a `power-supply-type` string to the FIDL service the supply serves. The
// types mirror `fuchsia.hardware.power.source/SourceType`.
std::optional<std::string_view> ServiceForType(std::string_view type) {
  if (type == "usb") {
    return "fuchsia.hardware.power.usb.Service";
  }
  if (type == "battery") {
    return "fuchsia.hardware.power.battery.Service";
  }
  if (type == "ac") {
    return "fuchsia.hardware.power.source.Service";
  }
  return std::nullopt;
}

}  // namespace

PowerSupplyVisitor::PowerSupplyVisitor() {
  fdf_devicetree::Properties properties = {};
  properties.emplace_back(std::make_unique<fdf_devicetree::ReferenceProperty>(
      kPowerSupplies, 0u, /* required */ false));
  properties.emplace_back(std::make_unique<fdf_devicetree::StringListProperty>(
      kPowerSupplyNames, /* required */ false));
  parser_ = std::make_unique<fdf_devicetree::PropertyParser>(std::move(properties));
}

zx::result<> PowerSupplyVisitor::Visit(fdf_devicetree::Node& node,
                                       const devicetree::PropertyDecoder& decoder) {
  zx::result parser_output = parser_->Parse(node);
  if (parser_output.is_error()) {
    fdf::error("Power supply visitor failed for node '{}' : {}", node.name(), parser_output);
    return parser_output.take_error();
  }

  std::optional<fdf_devicetree::References> supplies =
      parser_output->Get<fdf_devicetree::References>(kPowerSupplies);
  if (!supplies.has_value() || supplies->empty()) {
    return zx::ok();
  }

  std::optional<std::vector<std::string>> supply_names =
      parser_output->Get<std::vector<std::string>>(kPowerSupplyNames);
  if (!supply_names && supplies->size() != 1u) {
    fdf::error(
        "Node '{}' references {} power supplies but has no '{}' property. A name is required to distinguish the bind parents.",
        node.name(), supplies->size(), kPowerSupplyNames);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  if (supply_names && supply_names->size() != supplies->size()) {
    fdf::error("Node '{}' has {} '{}' entries but references {} power supplies.", node.name(),
               supply_names->size(), kPowerSupplyNames, supplies->size());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  for (size_t index = 0; index < supplies->size(); index++) {
    auto& supply = (*supplies)[index];
    if (!supply.reference_node()) {
      fdf::error("Node '{}' has an invalid '{}' reference.", node.name(), kPowerSupplies);
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    std::optional<std::string_view> parent_name;
    if (supply_names) {
      parent_name = (*supply_names)[index];
    }
    if (zx::result result = ParseReferenceChild(node, supply.reference_node(), parent_name);
        result.is_error()) {
      return result.take_error();
    }
  }
  return zx::ok();
}

zx::result<> PowerSupplyVisitor::ParseReferenceChild(fdf_devicetree::Node& child,
                                                     fdf_devicetree::ReferenceNode& supply,
                                                     std::optional<std::string_view> parent_name) {
  zx::result type = supply.GetProperty<std::string>(kPowerSupplyType);
  if (type.is_error()) {
    fdf::error("Power supply node '{}' referenced by '{}' has no '{}' property: {}", supply.name(),
               child.name(), kPowerSupplyType, type);
    return type.take_error();
  }

  std::optional<std::string_view> service = ServiceForType(*type);
  if (!service) {
    fdf::error("Power supply node '{}' has unsupported {} '{}'.", supply.name(), kPowerSupplyType,
               *type);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  std::vector bind_rules = {
      fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, *service),
  };
  std::vector bind_properties = {
      fdf::MakeProperty2(bind_fuchsia::SERVICE, *service),
  };

  // Identifies which supply, when a provider exposes several of one type.
  if (zx::result name = supply.GetProperty<std::string>(kPowerSupplyOutputName); name.is_ok()) {
    bind_rules.emplace_back(fdf::MakeAcceptBindRule(bind_fuchsia::NAME, *name));
  } else if (name.error_value() != ZX_ERR_NOT_FOUND) {
    fdf::error("Power supply node '{}' has an invalid '{}' property: {}", supply.name(),
               kPowerSupplyOutputName, name);
    return name.take_error();
  }

  // Names the bind parent; driver-index requires this to match the parent name used in the
  // consumer's bind rules (see `node_matches_composite_driver`).
  if (parent_name) {
    bind_properties.emplace_back(fdf::MakeProperty2(bind_fuchsia::NAME, *parent_name));
  }

  child.AddNodeSpec(fuchsia_driver_framework::ParentSpec2{{
      .bind_rules = std::move(bind_rules),
      .properties = std::move(bind_properties),
  }});
  fdf::debug("Added '{}' power supply parent '{}' to node '{}'.", *type, supply.name(),
             child.name());
  return zx::ok();
}

}  // namespace power_supply_visitor_dt

REGISTER_DEVICETREE_VISITOR(power_supply_visitor_dt::PowerSupplyVisitor);
