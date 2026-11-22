// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/netsvc/netifc-discover.h"

#include <fidl/fuchsia.driver.framework/cpp/wire.h>

#include <array>

#include <zxtest/zxtest.h>

namespace {

namespace fdf = fuchsia_driver_framework;

TEST(NetifcDiscoverTest, BusInfoToPathEmptyAndMissingAddresses) {
  EXPECT_EQ(BusInfoToPath({}), "");

  fidl::Arena arena;
  std::array<fdf::wire::BusInfo, 2> list = {
      fdf::wire::BusInfo::Builder(arena).bus(fdf::wire::BusType::kVirtio).Build(),
      fdf::wire::BusInfo::Builder(arena)
          .bus(fdf::wire::BusType::kDeviceTree)
          .address(fdf::wire::DeviceAddress::WithStringValue(arena, "///"))
          .Build(),
  };
  EXPECT_EQ(BusInfoToPath(fidl::VectorView<fdf::wire::BusInfo>::FromExternal(list)), "");
}

TEST(NetifcDiscoverTest, BusInfoToPathPciAndNonPciArrayInt) {
  fidl::Arena arena;
  std::array<uint8_t, 3> pci_bdf = {0x00, 0x14, 0x02};
  std::array<uint8_t, 3> non_pci_ints = {0x05, 0x00, 0x06};
  std::array<uint8_t, 2> short_pci_ints = {0x01, 0x0a};

  std::array<fdf::wire::BusInfo, 3> list = {
      fdf::wire::BusInfo::Builder(arena)
          .bus(fdf::wire::BusType::kPci)
          .address(fdf::wire::DeviceAddress::WithArrayIntValue(
              arena, fidl::VectorView<uint8_t>::FromExternal(pci_bdf)))
          .Build(),
      fdf::wire::BusInfo::Builder(arena)
          .bus(fdf::wire::BusType::kSdio)
          .address(fdf::wire::DeviceAddress::WithArrayIntValue(
              arena, fidl::VectorView<uint8_t>::FromExternal(non_pci_ints)))
          .Build(),
      fdf::wire::BusInfo::Builder(arena)
          .bus(fdf::wire::BusType::kPci)
          .address(fdf::wire::DeviceAddress::WithArrayIntValue(
              arena, fidl::VectorView<uint8_t>::FromExternal(short_pci_ints)))
          .Build(),
  };

  EXPECT_EQ(BusInfoToPath(fidl::VectorView<fdf::wire::BusInfo>::FromExternal(list)),
            "/00_14_2/05:00:06/01:0a");
}

TEST(NetifcDiscoverTest, BusInfoToPathAllRemainingAddressVariantsAndLeadingSlashes) {
  fidl::Arena arena;
  std::array<fidl::StringView, 3> acpi_segments = {
      fidl::StringView("05"),
      fidl::StringView("00"),
      fidl::StringView("2"),
  };

  std::array<fdf::wire::BusInfo, 5> list = {
      fdf::wire::BusInfo::Builder(arena)
          .bus(fdf::wire::BusType::kPlatform)
          .address(fdf::wire::DeviceAddress::WithArrayCharIntValue(
              arena, fidl::VectorView<fidl::StringView>::FromExternal(acpi_segments)))
          .Build(),
      fdf::wire::BusInfo::Builder(arena)
          .bus(fdf::wire::BusType::kAcpi)
          .address(fdf::wire::DeviceAddress::WithCharIntValue(arena, "I2C0"))
          .Build(),
      fdf::wire::BusInfo::Builder(arena)
          .bus(fdf::wire::BusType::kUsb)
          .address(fdf::wire::DeviceAddress::WithIntValue(4))
          .Build(),
      fdf::wire::BusInfo::Builder(arena)
          .bus(fdf::wire::BusType::kDeviceTree)
          .address(fdf::wire::DeviceAddress::WithStringValue(arena, "//netemul"))
          .Build(),
      fdf::wire::BusInfo::Builder(arena)
          .address(fdf::wire::DeviceAddress::WithStringValue(arena, "ethernet-phy-ff634000"))
          .Build(),
  };

  EXPECT_EQ(BusInfoToPath(fidl::VectorView<fdf::wire::BusInfo>::FromExternal(list)),
            "/05:00:2/I2C0/4/netemul/ethernet-phy-ff634000");
}

}  // namespace
