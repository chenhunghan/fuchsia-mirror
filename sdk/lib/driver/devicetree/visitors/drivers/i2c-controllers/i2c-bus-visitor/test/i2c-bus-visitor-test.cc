// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "../i2c-bus-visitor.h"

#include <fidl/fuchsia.hardware.i2c.businfo/cpp/fidl.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_properties.h>
#include <lib/driver/devicetree/testing/visitor-test-helper.h>
#include <lib/driver/devicetree/visitors/default/bind-property/bind-property.h>
#include <lib/driver/devicetree/visitors/default/mmio/mmio.h>
#include <lib/driver/devicetree/visitors/registry.h>

#include <algorithm>
#include <cstdint>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/platform/cpp/bind.h>
#include <gtest/gtest.h>

#include "dts/i2c.h"

namespace i2c_bus_dt {

class I2cBusVisitorTester : public fdf_devicetree::testing::VisitorTestHelper<I2cBusVisitor> {
 public:
  explicit I2cBusVisitorTester(std::string_view dtb_path)
      : fdf_devicetree::testing::VisitorTestHelper<I2cBusVisitor>(dtb_path, "I2cBusVisitorTest") {}
};

namespace {

// Checks that `parent` is an I2C parent named `name` for the channel with global ID `global_id`.
void ExpectI2cParent(const fuchsia_driver_framework::ParentSpec2& parent, std::string_view name,
                     uint32_t global_id) {
  SCOPED_TRACE(name);
  EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
      {
          fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
          fdf::MakeProperty2(bind_fuchsia::NAME, name),
      },
      parent.properties(), false));
  EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
      {{
          fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
          fdf::MakeAcceptBindRule(bind_fuchsia::ID, global_id),
      }},
      parent.bind_rules(), false));
}

// Walks the devicetree at `dtb_path` and returns whether the walk succeeded.
bool WalkSucceeds(std::string_view dtb_path) {
  fdf_devicetree::VisitorRegistry visitors;
  EXPECT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  EXPECT_TRUE(visitors.RegisterVisitor(std::make_unique<fdf_devicetree::MmioVisitor>()).is_ok());

  auto tester = std::make_unique<I2cBusVisitorTester>(dtb_path);
  I2cBusVisitorTester* i2c_tester = tester.get();
  EXPECT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  return i2c_tester->manager()->Walk(visitors).is_ok();
}

}  // namespace

TEST(I2cBusVisitorTest, TestI2CChannels) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  ASSERT_TRUE(visitors.RegisterVisitor(std::make_unique<fdf_devicetree::MmioVisitor>()).is_ok());

  auto tester = std::make_unique<I2cBusVisitorTester>("/pkg/test-data/i2c.dtb");
  I2cBusVisitorTester* i2c_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_OK, i2c_tester->manager()->Walk(visitors).status_value());
  ASSERT_TRUE(i2c_tester->DoPublish().is_ok());

  ASSERT_EQ(7lu, i2c_tester->GetCompositeNodeSpecs().size());

  uint32_t node_tested_count = 0;
  std::vector<fuchsia_hardware_platform_bus::Node> nodes = i2c_tester->GetPbusNodes("i2c-");
  for (const auto& node : nodes) {
    auto metadata = node.metadata();

    // Test metadata properties.
    ASSERT_TRUE(metadata);
    ASSERT_EQ(1lu, metadata->size());

    // I2C Channels metadata
    std::vector<uint8_t> metadata_blob = std::move(*(*metadata)[0].data());
    fit::result decoded =
        fidl::Unpersist<fuchsia_hardware_i2c_businfo::I2CBusMetadata>(cpp20::span(metadata_blob));
    ASSERT_TRUE(decoded.is_ok());
    ASSERT_EQ(decoded->bus_id(), 0u);
    auto& channels = *decoded->channels();
    ASSERT_EQ(channels.size(), 6lu);
    EXPECT_EQ(channels[0].address(), static_cast<uint32_t>(I2C_ADDRESS1));
    EXPECT_EQ(channels[0].global_id(), 0u);
    EXPECT_EQ(channels[1].address(), static_cast<uint32_t>(I2C_ADDRESS2));
    EXPECT_EQ(channels[1].global_id(), 1u);
    EXPECT_EQ(channels[2].address(), static_cast<uint32_t>(I2C_ADDRESS3));
    EXPECT_EQ(channels[2].global_id(), 2u);
    EXPECT_EQ(channels[3].address(), static_cast<uint32_t>(I2C_ADDRESS4));
    EXPECT_EQ(channels[3].global_id(), 3u);
    EXPECT_EQ(channels[4].address(), static_cast<uint32_t>(I2C_ADDRESS5));
    EXPECT_EQ(channels[4].global_id(), 4u);
    EXPECT_EQ(channels[5].address(), static_cast<uint32_t>(I2C_ADDRESS6));
    EXPECT_EQ(channels[5].global_id(), 5u);

    node_tested_count++;
  }

  for (auto& node : i2c_tester->GetBoardChildNodes("child-")) {
    std::string node_name = node.name;

    auto composite_node_specs = i2c_tester->GetCompositeNodeSpecs(node_name);
    ASSERT_EQ(1lu, composite_node_specs.size());
    fuchsia_driver_framework::CompositeNodeSpec composite_node_spec = composite_node_specs[0];
    ASSERT_TRUE(composite_node_spec.parents2().has_value());
    const std::vector<fuchsia_driver_framework::ParentSpec2>& parent_specs =
        *composite_node_spec.parents2();

    // The first parent is the pdev node and the rest parents are I2c nodes.
    ASSERT_GT(parent_specs.size(), 1lu);
    cpp20::span<const fuchsia_driver_framework::ParentSpec2> i2c_nodes(++parent_specs.begin(),
                                                                       parent_specs.end());

    if (node_name == "child-c") {
      ASSERT_EQ(i2c_nodes.size(), 1lu);
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeProperty2(bind_fuchsia::NAME, "i2c"),
          },
          i2c_nodes[0].properties(), false));
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
          {{
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeAcceptBindRule(bind_fuchsia::ID, 0u),
          }},
          i2c_nodes[0].bind_rules(), false));
    }

    if (node_name == "child-dup-c") {
      ASSERT_EQ(i2c_nodes.size(), 1lu);
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeProperty2(bind_fuchsia::NAME, "i2c"),
          },
          i2c_nodes[0].properties(), false));
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
          {{
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeAcceptBindRule(bind_fuchsia::ID, 0u),
          }},
          i2c_nodes[0].bind_rules(), false));
    }

    if (node_name == "child-1e") {
      ASSERT_EQ(i2c_nodes.size(), 1lu);
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeProperty2(bind_fuchsia::NAME, "i2c"),
          },
          i2c_nodes[0].properties(), false));
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
          {{
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeAcceptBindRule(bind_fuchsia::ID, 1u),
          }},
          i2c_nodes[0].bind_rules(), false));
    }

    if (node_name == "child-2b") {
      ASSERT_EQ(i2c_nodes.size(), 2lu);

      EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeProperty2(bind_fuchsia::NAME, "i2c"),
          },
          i2c_nodes[0].properties(), false));
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
          {{
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeAcceptBindRule(bind_fuchsia::ID, 2u),
          }},
          i2c_nodes[0].bind_rules(), false));

      EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeProperty2(bind_fuchsia::NAME, "i2c"),
          },
          i2c_nodes[1].properties(), false));
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
          {{
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeAcceptBindRule(bind_fuchsia::ID, 3u),
          }},
          i2c_nodes[1].bind_rules(), false));
    }

    if (node_name == "child-names-30") {
      ASSERT_EQ(i2c_nodes.size(), 2lu);

      EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeProperty2(bind_fuchsia::NAME, "i2c-control"),
          },
          i2c_nodes[0].properties(), false));
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
          {{
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeAcceptBindRule(bind_fuchsia::ID, 4u),
          }},
          i2c_nodes[0].bind_rules(), false));

      EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeProperty2(bind_fuchsia::NAME, "i2c-data"),
          },
          i2c_nodes[1].properties(), false));
      EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
          {{
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.i2c.Service"),
              fdf::MakeAcceptBindRule(bind_fuchsia::ID, 5u),
          }},
          i2c_nodes[1].bind_rules(), false));
    }

    node_tested_count++;
  }

  ASSERT_EQ(node_tested_count, 6u);
}

TEST(I2cBusVisitorTest, TestI2CChannelReferences) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  ASSERT_TRUE(visitors.RegisterVisitor(std::make_unique<fdf_devicetree::MmioVisitor>()).is_ok());

  auto tester = std::make_unique<I2cBusVisitorTester>("/pkg/test-data/i2c-channels.dtb");
  I2cBusVisitorTester* i2c_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_OK, i2c_tester->manager()->Walk(visitors).status_value());
  ASSERT_TRUE(i2c_tester->DoPublish().is_ok());

  // Referenced children get no parent spec, so they have no composite node spec.
  std::vector<std::string> composite_node_spec_names;
  for (const auto& spec : i2c_tester->GetCompositeNodeSpecs()) {
    composite_node_spec_names.push_back(*spec.name());
  }
  std::ranges::sort(composite_node_spec_names);
  EXPECT_EQ(composite_node_spec_names,
            (std::vector<std::string>{"child-2b", "child-c", "consumer-after-ffffd000",
                                      "consumer-before-ffff9000", "dt-root", "i2c-ffffa000",
                                      "i2c-ffffc000"}));

  // Referenced children keep their channels in the bus metadata.
  struct ExpectedChannel {
    uint32_t address;
    uint32_t global_id;
    std::string name;
  };
  struct ExpectedBus {
    std::string node_name;
    uint32_t bus_id;
    std::vector<ExpectedChannel> channels;
  };
  for (const ExpectedBus& expected : std::vector<ExpectedBus>{
           {"i2c-ffffa000",
            0,
            {{I2C_ADDRESS1, 0, "child@c"},
             {I2C_ADDRESS2, 1, "pmic@1e"},
             {I2C_ADDRESS3, 2, "child@2b"},
             {I2C_ADDRESS6, 3, "pmic@38"}}},
           {"i2c-ffffc000",
            1,
            {{I2C_ADDRESS5, 4, "pmic@30"},
             {I2C_ADDRESS4, 5, "pmic@3a"},
             {I2C_ADDRESS1, 6, "pmic@c"}}},
       }) {
    SCOPED_TRACE(expected.node_name);
    std::vector<fuchsia_hardware_platform_bus::Node> buses =
        i2c_tester->GetPbusNodes(expected.node_name);
    ASSERT_EQ(1lu, buses.size());
    ASSERT_TRUE(buses[0].metadata());
    ASSERT_EQ(1lu, buses[0].metadata()->size());
    std::vector<uint8_t> metadata_blob = std::move(*(*buses[0].metadata())[0].data());
    fit::result decoded =
        fidl::Unpersist<fuchsia_hardware_i2c_businfo::I2CBusMetadata>(cpp20::span(metadata_blob));
    ASSERT_TRUE(decoded.is_ok());
    EXPECT_EQ(decoded->bus_id(), expected.bus_id);
    const auto& channels = *decoded->channels();
    ASSERT_EQ(channels.size(), expected.channels.size());
    for (size_t i = 0; i < channels.size(); ++i) {
      EXPECT_EQ(channels[i].address(), expected.channels[i].address);
      EXPECT_EQ(channels[i].global_id(), expected.channels[i].global_id);
      EXPECT_EQ(channels[i].name(), expected.channels[i].name);
    }
  }

  // Referenced children without other properties are not published.
  EXPECT_TRUE(i2c_tester->GetBoardChildNodes("pmic-").empty());

  // The first parent is the platform device or board child node. The node's own I2C parents come
  // next, followed by the referenced channels in property order.
  struct ExpectedParent {
    std::string name;
    uint32_t global_id;
  };
  for (const auto& [node_name, expected_parents] :
       std::vector<std::pair<std::string, std::vector<ExpectedParent>>>{
           {"consumer-before-ffff9000", {{"bus1-pmic", 5}, {"bus0-pmic", 3}}},
           {"child-c", {{"i2c", 0}}},
           {"child-2b", {{"i2c", 2}, {"bus0-pmic", 1}, {"bus1-pmic", 4}}},
           {"consumer-after-ffffd000", {{"bus1-pmic", 6}}},
       }) {
    SCOPED_TRACE(node_name);
    std::vector<fuchsia_driver_framework::CompositeNodeSpec> composite_node_specs =
        i2c_tester->GetCompositeNodeSpecs(node_name);
    ASSERT_EQ(1lu, composite_node_specs.size());
    ASSERT_TRUE(composite_node_specs[0].parents2().has_value());
    const std::vector<fuchsia_driver_framework::ParentSpec2>& parents =
        *composite_node_specs[0].parents2();
    ASSERT_EQ(parents.size(), expected_parents.size() + 1);
    for (size_t i = 0; i < expected_parents.size(); ++i) {
      ExpectI2cParent(parents[i + 1], expected_parents[i].name, expected_parents[i].global_id);
    }
  }
}

TEST(I2cBusVisitorTest, TestI2CChannelReferenceErrors) {
  for (const char* dtb_path : {
           "/pkg/test-data/i2c-channels-duplicate-names.dtb",
           "/pkg/test-data/i2c-channels-duplicate-phandles.dtb",
           "/pkg/test-data/i2c-channels-grandchild.dtb",
           "/pkg/test-data/i2c-channels-multi-address.dtb",
           "/pkg/test-data/i2c-channels-multiple-referrers.dtb",
           "/pkg/test-data/i2c-channels-names-mismatch.dtb",
           "/pkg/test-data/i2c-channels-no-names.dtb",
           "/pkg/test-data/i2c-channels-not-i2c-child.dtb",
           "/pkg/test-data/i2c-channels-own-name-collision.dtb",
           "/pkg/test-data/i2c-channels-referenced-compatible.dtb",
           "/pkg/test-data/i2c-channels-self-reference.dtb",
           "/pkg/test-data/i2c-channels-shared-address.dtb",
       }) {
    SCOPED_TRACE(dtb_path);
    EXPECT_FALSE(WalkSucceeds(dtb_path));
  }
}

}  // namespace i2c_bus_dt
