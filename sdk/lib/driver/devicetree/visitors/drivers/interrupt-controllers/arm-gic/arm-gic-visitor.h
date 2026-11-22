// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_INTERRUPT_CONTROLLERS_ARM_GIC_ARM_GIC_VISITOR_H_
#define LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_INTERRUPT_CONTROLLERS_ARM_GIC_ARM_GIC_VISITOR_H_

#include <lib/driver/devicetree/visitors/driver-visitor.h>
#include <lib/driver/devicetree/visitors/interrupt-parser.h>

#include <map>
#include <optional>
#include <string>

namespace arm_gic_dt {

class ArmGicVisitor : public fdf_devicetree::DriverVisitor {
 public:
  explicit ArmGicVisitor();

  zx::result<> Visit(fdf_devicetree::Node& node,
                     const devicetree::PropertyDecoder& decoder) override;

 private:
  // Decodes interrupt cell values and adds the interrupt to `node`.
  zx::result<fuchsia_hardware_platform_bus::Irq> ParseInterrupt(
      const std::string& node_name, bool is_enabled, fdf_devicetree::ReferenceNode& parent,
      fdf_devicetree::PropertyCells interrupt_cells, std::optional<std::string> interrupt_name);

  fdf_devicetree::InterruptParser interrupt_parser_;
  // Track assigned SGI interrupt vectors to ensure exclusive ownership per SGI vector.
  // The kernel only supports a single client per physical SGI doorbell; assigning the same
  // SGI vector to multiple drivers in devicetree causes runtime race conditions.
  std::map<uint32_t, std::string> assigned_sgis_;
};

}  // namespace arm_gic_dt

#endif  // LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_INTERRUPT_CONTROLLERS_ARM_GIC_ARM_GIC_VISITOR_H_
