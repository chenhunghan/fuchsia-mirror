// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_POWER_POWER_SUPPLY_POWER_SUPPLY_VISITOR_H_
#define LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_POWER_POWER_SUPPLY_POWER_SUPPLY_VISITOR_H_

#include <lib/driver/devicetree/manager/node.h>
#include <lib/driver/devicetree/manager/visitor.h>
#include <lib/driver/devicetree/visitors/property-parser.h>

#include <memory>
#include <string_view>

namespace power_supply_visitor_dt {

// Adds a power supply parent spec to nodes referencing a supply via `power-supplies`.
// The supply's `power-supply-type` selects the service (mirroring
// `fuchsia.hardware.power.source/SourceType`) and its `power-supply-output-name`
// identifies it; the consumer's `power-supply-names` names the bind parent, as with
// `clock-names`.
class PowerSupplyVisitor : public fdf_devicetree::Visitor {
 public:
  // Properties on the consumer node.
  static constexpr char kPowerSupplies[] = "power-supplies";
  static constexpr char kPowerSupplyNames[] = "power-supply-names";
  // Properties on the supply (provider) node.
  static constexpr char kPowerSupplyType[] = "power-supply-type";
  static constexpr char kPowerSupplyOutputName[] = "power-supply-output-name";

  PowerSupplyVisitor();
  zx::result<> Visit(fdf_devicetree::Node& node,
                     const devicetree::PropertyDecoder& decoder) override;

 private:
  zx::result<> ParseReferenceChild(fdf_devicetree::Node& child,
                                   fdf_devicetree::ReferenceNode& supply,
                                   std::optional<std::string_view> parent_name);

  std::unique_ptr<fdf_devicetree::PropertyParser> parser_;
};

}  // namespace power_supply_visitor_dt

#endif  // LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_POWER_POWER_SUPPLY_POWER_SUPPLY_VISITOR_H_
