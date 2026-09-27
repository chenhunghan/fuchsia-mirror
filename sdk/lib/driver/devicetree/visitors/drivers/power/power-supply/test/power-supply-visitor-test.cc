// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "../power-supply-visitor.h"

#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/devicetree/testing/visitor-test-helper.h>
#include <lib/driver/devicetree/visitors/default/bind-property/bind-property.h>
#include <lib/driver/devicetree/visitors/registry.h>

#include <bind/fuchsia/cpp/bind.h>
#include <gtest/gtest.h>

namespace power_supply_visitor_dt {

class PowerSupplyVisitorTester
    : public fdf_devicetree::testing::VisitorTestHelper<PowerSupplyVisitor> {
 public:
  explicit PowerSupplyVisitorTester(std::string_view dtb_path)
      : fdf_devicetree::testing::VisitorTestHelper<PowerSupplyVisitor>(
            dtb_path, "PowerSupplyVisitorTester") {}
};

TEST(PowerSupplyVisitorTester, TestReferences) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  auto tester = std::make_unique<PowerSupplyVisitorTester>("/pkg/test-data/power-supply.dtb");
  PowerSupplyVisitorTester* power_supply_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_OK, power_supply_tester->manager()->Walk(visitors).status_value());
  ASSERT_TRUE(power_supply_tester->DoPublish().is_ok());

  auto specs = power_supply_tester->GetCompositeNodeSpecs("test-device");
  ASSERT_EQ(specs.size(), 1u);
  auto mgr_request = specs[0];
  const auto& parents = mgr_request.parents2();
  ASSERT_EQ(parents->size(), 2u);

  // Parent 0 is pdev. Parent 1's rule comes from the provider's `power-supply-output-name`, its
  // property from the consumer's `power-supply-names`.
  EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
      {{
          fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.power.usb.Service"),
          fdf::MakeAcceptBindRule(bind_fuchsia::NAME, "chgin"),
      }},
      (*parents)[1].bind_rules(), false));

  EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
      {{
          fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.power.usb.Service"),
          fdf::MakeProperty2(bind_fuchsia::NAME, "usb-power"),
      }},
      (*parents)[1].properties(), false));
}

TEST(PowerSupplyVisitorTester, TestSingleSupplyWithoutNames) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  auto tester = std::make_unique<PowerSupplyVisitorTester>("/pkg/test-data/power-supply.dtb");
  PowerSupplyVisitorTester* power_supply_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_OK, power_supply_tester->manager()->Walk(visitors).status_value());
  ASSERT_TRUE(power_supply_tester->DoPublish().is_ok());

  auto specs = power_supply_tester->GetCompositeNodeSpecs("unnamed-supply-device");
  ASSERT_EQ(specs.size(), 1u);
  const auto& parents = specs[0].parents2();
  ASSERT_EQ(parents->size(), 2u);

  EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
      {{
          fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.power.usb.Service"),
          fdf::MakeAcceptBindRule(bind_fuchsia::NAME, "chgin"),
      }},
      (*parents)[1].bind_rules(), false));

  EXPECT_EQ((*parents)[1].properties().size(), 1u);
  EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
      {{
          fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.power.usb.Service"),
      }},
      (*parents)[1].properties(), false));
}

TEST(PowerSupplyVisitorTester, TestMultipleSupplies) {
  fdf_devicetree::VisitorRegistry visitors;
  ASSERT_TRUE(
      visitors.RegisterVisitor(std::make_unique<fdf_devicetree::BindPropertyVisitor>()).is_ok());
  auto tester = std::make_unique<PowerSupplyVisitorTester>("/pkg/test-data/power-supply.dtb");
  PowerSupplyVisitorTester* power_supply_tester = tester.get();
  ASSERT_TRUE(visitors.RegisterVisitor(std::move(tester)).is_ok());

  ASSERT_EQ(ZX_OK, power_supply_tester->manager()->Walk(visitors).status_value());
  ASSERT_TRUE(power_supply_tester->DoPublish().is_ok());

  auto specs = power_supply_tester->GetCompositeNodeSpecs("multi-supply-device");
  ASSERT_EQ(specs.size(), 1u);
  const auto& parents = specs[0].parents2();
  ASSERT_EQ(parents->size(), 3u);

  // Same service on both, so the provider's name keeps the parents distinct and the consumer's
  // name lets its bind rules tell them apart.
  const std::pair<const char*, const char*> kExpected[] = {
      {"chgin", "wired"},
      {"wcin", "wireless"},
  };
  for (size_t i = 0; i < std::size(kExpected); i++) {
    const auto& [supply_name, parent_name] = kExpected[i];
    EXPECT_TRUE(fdf_devicetree::testing::CheckHasBindRules(
        {{
            fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.power.usb.Service"),
            fdf::MakeAcceptBindRule(bind_fuchsia::NAME, supply_name),
        }},
        (*parents)[i + 1].bind_rules(), false));

    EXPECT_TRUE(fdf_devicetree::testing::CheckHasProperties(
        {{
            fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.power.usb.Service"),
            fdf::MakeProperty2(bind_fuchsia::NAME, parent_name),
        }},
        (*parents)[i + 1].properties(), false));
  }
}

}  // namespace power_supply_visitor_dt
