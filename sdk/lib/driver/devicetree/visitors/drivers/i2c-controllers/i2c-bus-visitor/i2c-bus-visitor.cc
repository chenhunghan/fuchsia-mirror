// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "i2c-bus-visitor.h"

#include <fidl/fuchsia.hardware.i2c.businfo/cpp/fidl.h>
#include <lib/ddk/platform-defs.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_properties.h>
#include <lib/driver/devicetree/visitors/common-types.h>
#include <lib/driver/devicetree/visitors/registration.h>
#include <lib/driver/logging/cpp/logger.h>
#include <zircon/assert.h>
#include <zircon/errors.h>

#include <algorithm>
#include <cstdint>
#include <optional>
#include <set>
#include <string>
#include <utility>
#include <vector>

#include <bind/fuchsia/cpp/bind.h>
#include <fbl/string_printf.h>

namespace {

// Phandles of I2C bus child nodes whose channels are used by the node with this property.
constexpr char kI2cChannelsProperty[] = "i2c-channels";
// The `fuchsia.NAME` property of the parent spec for each `i2c-channels` entry.
constexpr char kI2cChannelNamesProperty[] = "i2c-channel-names";

// Returns a parent spec that matches the I2C channel with global ID `global_id` published by the
// I2C core driver.
fuchsia_driver_framework::ParentSpec2 MakeI2cParentSpec(uint32_t global_id, std::string_view name) {
  return fuchsia_driver_framework::ParentSpec2{{
      .bind_rules =
          {
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeAcceptBindRule(bind_fuchsia::ID, global_id),
          },
      .properties =
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeProperty2(bind_fuchsia::NAME, name),
          },
  }};
}

// Returns the `fuchsia.NAME` property of the parent spec for the `index`-th `reg` entry of an I2C
// bus child node with the given `reg-names` (empty if absent).
std::string ChildChannelName(const std::vector<std::string>& reg_names, size_t index) {
  if (index >= reg_names.size()) {
    return "i2c";
  }
  if (reg_names[index].starts_with("i2c-")) {
    return reg_names[index];
  }
  return "i2c-" + reg_names[index];
}

}  // namespace

namespace i2c_bus_dt {

I2cBusVisitor::I2cBusVisitor() {
  fdf_devicetree::Properties properties = {};
  properties.emplace_back(std::make_unique<fdf_devicetree::ReferenceProperty>(
      kI2cChannelsProperty, 0u, /*required=*/false));
  properties.emplace_back(std::make_unique<fdf_devicetree::StringListProperty>(
      kI2cChannelNamesProperty, /*required=*/false));
  channel_reference_parser_ =
      std::make_unique<fdf_devicetree::PropertyParser>(std::move(properties));
}

bool I2cBusVisitor::is_match(fdf_devicetree::Node& node) {
  if (node.name().find("i2c@") == std::string::npos) {
    return false;
  }

  auto address_cells = node.GetProperty<uint32_t>("#address-cells");
  if (address_cells.is_error() || *address_cells != 1) {
    return false;
  }

  auto size_cells = node.GetProperty<uint32_t>("#size-cells");
  if (size_cells.is_error() || *size_cells != 0) {
    return false;
  }

  return true;
}

zx::result<> I2cBusVisitor::CreateController(std::string node_name) {
  if (i2c_controllers_.contains(node_name)) {
    fdf::error("Failed to create I2C Controller. An I2C controller with name '{}' already exists.",
               node_name);

    return zx::error(ZX_ERR_ALREADY_EXISTS);
  }
  i2c_controllers_[node_name] = I2cController();
  i2c_controllers_[node_name].bus_id = bus_id_counter_++;
  return zx::ok();
}

zx::result<> I2cBusVisitor::ParseChild(I2cController& controller, fdf_devicetree::Node& parent,
                                       fdf_devicetree::ChildNode& child) {
  // Parse reg to get the address.
  auto reg = child.GetProperty<std::vector<uint32_t>>("reg");
  if (reg.is_error()) {
    fdf::error("I2C child '{}' has no reg property: {}.", child.name(), reg);

    return reg.take_error();
  }

  if (reg->empty()) {
    fdf::error("I2C child '{}' has an empty reg property.", child.name());

    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  const std::vector<std::string> reg_names =
      child.GetProperty<std::vector<std::string>>("reg-names").value_or(std::vector<std::string>{});

  // The child's entry might already exist if it is referenced by a node that was visited earlier.
  I2cChild& i2c_child = i2c_children_[child.id()];
  for (size_t i = 0; i < reg->size(); ++i) {
    const uint32_t address = (*reg)[i];
    const auto it = std::ranges::find_if(controller.channels, [address](const auto& ch) {
      return ch.address() && *ch.address() == address;
    });

    uint32_t global_id;
    if (it == controller.channels.end()) {
      global_id = channel_id_counter_++;
      fuchsia_hardware_i2c_businfo::I2CChannel channel;
      channel.address() = address;
      channel.global_id() = global_id;

      std::string child_name;
      if (reg->size() > 1) {
        child_name = fbl::StringPrintf("%s-0x%02x", child.name().c_str(), address).c_str();
      } else {
        child_name = child.name();
      }
      channel.name() = std::move(child_name);

      controller.channels.emplace_back(std::move(channel));
      fdf::debug("I2c channel '{}' added at address {:#x} to controller '{}'",
                 *controller.channels.back().name(), address, parent.name());
    } else {
      global_id = *it->global_id();
      fdf::debug(
          "I2c channel at address {:#x} already exists in controller '{}', skipping metadata addition",
          address, parent.name());
    }

    i2c_child.parent_specs.push_back(MakeI2cParentSpec(global_id, ChildChannelName(reg_names, i)));
    if (reg->size() == 1) {
      i2c_child.global_id = global_id;
    }
  }

  return zx::ok();
}

zx::result<> I2cBusVisitor::ParseChannelReferences(fdf_devicetree::Node& node) {
  zx::result properties = channel_reference_parser_->Parse(node);
  if (properties.is_error()) {
    fdf::error("Failed to parse I2C channel references of node '{}'", node.name());

    return properties.take_error();
  }

  auto channels = properties->Get<fdf_devicetree::References>(kI2cChannelsProperty);
  if (!channels) {
    return zx::ok();
  }

  auto channel_names = properties->Get<std::vector<std::string>>(kI2cChannelNamesProperty);
  if (!channel_names || channel_names->size() != channels->size()) {
    fdf::error("Node '{}' has {} entries in '{}' but {} entries in '{}'.", node.name(),
               channels->size(), kI2cChannelsProperty, channel_names ? channel_names->size() : 0,
               kI2cChannelNamesProperty);

    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  // The names of the I2C parents of `node`. The driver's composite bind rules select I2C parents
  // by name, so the names must be unique. If `node` is itself an I2C bus child, this includes the
  // names that ChildChannelName() gives the parents for its own channels.
  std::set<std::string> parent_names;
  if (fdf_devicetree::ParentNode parent = node.parent(); parent && is_match(*parent.GetNode())) {
    auto reg = node.GetProperty<std::vector<uint32_t>>("reg");
    const std::vector<std::string> reg_names =
        node.GetProperty<std::vector<std::string>>("reg-names")
            .value_or(std::vector<std::string>{});
    for (size_t i = 0; reg.is_ok() && i < reg->size(); ++i) {
      parent_names.insert(ChildChannelName(reg_names, i));
    }
  }

  std::vector<ChannelReference>& references = channel_references_[node.id()];
  for (size_t i = 0; i < channels->size(); ++i) {
    fdf_devicetree::ReferenceNode& child = (*channels)[i].reference_node();
    const std::string& name = (*channel_names)[i];

    if (child.id() == node.id()) {
      fdf::error("Node '{}' references itself in '{}'.", node.name(), kI2cChannelsProperty);

      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    fdf_devicetree::ParentNode bus = child.parent();
    if (!bus || !is_match(*bus.GetNode())) {
      fdf::error("Node '{}' references '{}' in '{}', which is not a child of an I2C bus.",
                 node.name(), child.name(), kI2cChannelsProperty);

      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    // The referencing nodes get the child's parent spec instead of the child, so no driver can bind
    // to the child.
    if (child.properties().contains("compatible")) {
      fdf::error(
          "Node '{}' references I2C child '{}' in '{}', but the child has a compatible property.",
          node.name(), child.name(), kI2cChannelsProperty);

      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    // The reference has no specifier cells to select one of multiple addresses.
    auto reg = child.GetProperty<std::vector<uint32_t>>("reg");
    if (reg.is_error() || reg->size() != 1) {
      fdf::error(
          "Node '{}' references I2C child '{}' in '{}', but the child does not have exactly one "
          "address.",
          node.name(), child.name(), kI2cChannelsProperty);

      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    // Other children of the bus at the same address would get parent specs for the same channel.
    const uint32_t address = (*reg)[0];
    for (fdf_devicetree::ChildNode& sibling : bus.GetNode()->children()) {
      auto sibling_reg = sibling.GetProperty<std::vector<uint32_t>>("reg");
      if (sibling.id() != child.id() && sibling_reg.is_ok() &&
          std::ranges::find(*sibling_reg, address) != sibling_reg->end()) {
        fdf::error(
            "Node '{}' references I2C child '{}' in '{}', but the child shares address {:#x} with "
            "'{}'.",
            node.name(), child.name(), kI2cChannelsProperty, address, sibling.name());

        return zx::error(ZX_ERR_INVALID_ARGS);
      }
    }

    if (std::ranges::any_of(references, [&child](const ChannelReference& reference) {
          return reference.child_id == child.id();
        })) {
      fdf::error("Node '{}' references '{}' more than once in '{}'.", node.name(), child.name(),
                 kI2cChannelsProperty);

      return zx::error(ZX_ERR_ALREADY_EXISTS);
    }

    if (!parent_names.insert(name).second) {
      fdf::error("Node '{}' has more than one I2C parent named '{}'. Names in '{}' must be unique.",
                 node.name(), name, kI2cChannelNamesProperty);

      return zx::error(ZX_ERR_ALREADY_EXISTS);
    }

    I2cChild& i2c_child = i2c_children_[child.id()];
    if (i2c_child.has_reference_property) {
      fdf::error("Multiple reference properties for I2C child '{}'", child.name());

      return zx::error(ZX_ERR_ALREADY_EXISTS);
    }
    i2c_child.has_reference_property = true;

    references.push_back({.child_id = child.id(), .name = name});
  }

  return zx::ok();
}

zx::result<> I2cBusVisitor::Visit(fdf_devicetree::Node& node,
                                  const devicetree::PropertyDecoder& decoder) {
  if (is_match(node)) {
    auto result = CreateController(node.name());
    if (result.is_error()) {
      return result.take_error();
    }

    I2cController& controller = i2c_controllers_[node.name()];
    for (auto& child : node.children()) {
      auto result = ParseChild(controller, node, child);
      if (result.is_error()) {
        fdf::error("Failed to parse i2c child '{}' : {}", child.name(), result);

        return result.take_error();
      }
    }
  }
  return ParseChannelReferences(node);
}

zx::result<> I2cBusVisitor::FinalizeNode(fdf_devicetree::Node& node) {
  if (is_match(node)) {
    FinalizeController(node);
  }

  // Only add parents for the I2C child if it does not appear in a reference property.
  if (auto child = i2c_children_.find(node.id());
      child != i2c_children_.end() && !child->second.has_reference_property) {
    for (const fuchsia_driver_framework::ParentSpec2& parent_spec : child->second.parent_specs) {
      node.AddNodeSpec(parent_spec);
    }
  }

  if (auto references = channel_references_.find(node.id());
      references != channel_references_.end()) {
    for (const ChannelReference& reference : references->second) {
      // ParseChannelReferences() checked that the reference is to a child of an I2C bus with a
      // single address, and the bus's children were parsed when it was visited.
      const I2cChild& child = i2c_children_[reference.child_id];
      ZX_ASSERT_MSG(child.global_id.has_value(),
                    "I2C child referenced by node '%s' has no channel.", node.name().c_str());

      node.AddNodeSpec(MakeI2cParentSpec(*child.global_id, reference.name));
      fdf::debug("I2C parent '{}' (global ID {}) added to node '{}'", reference.name,
                 *child.global_id, node.name());
    }
  }

  return zx::ok();
}

void I2cBusVisitor::FinalizeController(fdf_devicetree::Node& node) {
  auto controller = i2c_controllers_.find(node.name());
  ZX_ASSERT_MSG(controller != i2c_controllers_.end(), "i2c controller '%s' entry not found.",
                node.name().c_str());

  fuchsia_hardware_i2c_businfo::I2CBusMetadata bus_metadata = {{
      .channels = controller->second.channels,
      .bus_id = controller->second.bus_id,
  }};
  auto encoded_bus_metadata = fidl::Persist(bus_metadata);
  if (encoded_bus_metadata.is_error()) {
    fdf::info("Failed to persist fidl metadata for i2c controller '{}': {}", node.name(),
              encoded_bus_metadata.error_value().FormatDescription());

    return;
  }
  node.AddMetadata({{
      .id = fuchsia_hardware_i2c_businfo::I2CBusMetadata::kSerializableName,
      .data = std::move(encoded_bus_metadata.value()),
  }});
  fdf::debug("I2C channels metadata added to node '{}'", node.name());
}

}  // namespace i2c_bus_dt

REGISTER_DEVICETREE_VISITOR(i2c_bus_dt::I2cBusVisitor);
