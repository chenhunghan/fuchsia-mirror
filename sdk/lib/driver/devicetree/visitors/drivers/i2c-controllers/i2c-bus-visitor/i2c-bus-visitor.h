// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_I2C_CONTROLLERS_I2C_BUS_VISITOR_I2C_BUS_VISITOR_H_
#define LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_I2C_CONTROLLERS_I2C_BUS_VISITOR_I2C_BUS_VISITOR_H_

#include <fidl/fuchsia.hardware.i2c.businfo/cpp/fidl.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/devicetree/visitors/driver-visitor.h>
#include <lib/driver/devicetree/visitors/property-parser.h>

#include <cstdint>
#include <map>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

namespace i2c_bus_dt {

// Devicetree visitor for I2C buses, their child devices, and consumer nodes.
//
// Publishes the channels of each I2C bus node's children to platform bus as I2C bus metadata, and
// generates I2C parent specs for the children. A child node that is referenced by the
// `i2c-channels` property of another node gets no parent spec of its own. Instead, the referencing
// node gets a parent spec for the child's channel. A child can be referenced by only one node.
class I2cBusVisitor : public fdf_devicetree::Visitor {
 public:
  I2cBusVisitor();

  zx::result<> FinalizeNode(fdf_devicetree::Node& node) override;

  zx::result<> Visit(fdf_devicetree::Node& node,
                     const devicetree::PropertyDecoder& decoder) override;

 private:
  struct I2cController {
    std::vector<fuchsia_hardware_i2c_businfo::I2CChannel> channels;
    uint32_t bus_id;
  };

  // Tracks the parent specs and reference state of an I2C bus child node.
  struct I2cChild {
    // Parent specs for the child's channels, in `reg` order.
    std::vector<fuchsia_driver_framework::ParentSpec2> parent_specs;
    // Global ID of the child's channel, if it has exactly one.
    std::optional<uint32_t> global_id;
    bool has_reference_property = false;
  };

  // An entry of an `i2c-channels` property.
  struct ChannelReference {
    // The referenced I2C bus child node.
    fdf_devicetree::NodeID child_id;
    // The `fuchsia.NAME` property of the parent spec, from `i2c-channel-names`.
    std::string name;
  };

  // Create new instance of I2cController, returns error if one already exists for the node_name.
  zx::result<> CreateController(std::string node_name);

  // Assigns global IDs to the channels of `child` and adds them to `controller`.
  zx::result<> ParseChild(I2cController& controller, fdf_devicetree::Node& parent,
                          fdf_devicetree::ChildNode& child);

  // Parses the `i2c-channels` property of `node`.
  zx::result<> ParseChannelReferences(fdf_devicetree::Node& node);

  // Adds the I2C bus metadata to the I2C bus `node`.
  void FinalizeController(fdf_devicetree::Node& node);

  bool is_match(fdf_devicetree::Node& node);

  // Mapping of devicetree node name to i2c controller struct.
  std::map<std::string, I2cController> i2c_controllers_;
  uint32_t bus_id_counter_ = 0;
  uint32_t channel_id_counter_ = 0;

  // Maps I2C bus child node ID to I2cChild struct.
  std::map<fdf_devicetree::NodeID, I2cChild> i2c_children_;

  std::unique_ptr<fdf_devicetree::PropertyParser> channel_reference_parser_;
  // Maps the ID of a node with an `i2c-channels` property to its entries, in property order.
  std::map<fdf_devicetree::NodeID, std::vector<ChannelReference>> channel_references_;
};

}  // namespace i2c_bus_dt

#endif  // LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_I2C_CONTROLLERS_I2C_BUS_VISITOR_I2C_BUS_VISITOR_H_
