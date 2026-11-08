// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_TEST_FAKES_FAKE_CONFIG_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_TEST_FAKES_FAKE_CONFIG_H_

#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/stdcompat/bit.h>
#include <lib/zx/vmo.h>

#include <cstdint>

#include "src/devices/pci/drivers/pci/test/fakes/test_device.h"
#include "src/devices/pci/lib/cpp/config.h"

namespace pci {

// For most operations a real MmioConfig is fine for working with
// a fake ecam. However, for BAR probing we need to mock the side
// effects of the writes that are used to determine the size of
// the BAR. Fortunately, these writes are always UINT32_MAX and can
// be caught because they are not otherwise valid. If necessary, this
// class can be extended to handle other side-effects as well.

class FakeMmioConfig final : public MmioConfig {
 public:
  FakeMmioConfig(fuchsia_hardware_pci::wire::Bdf bdf, fdf::MmioView&& view)
      : MmioConfig(bdf, view) {}
  void MockBarProbeSideEffects(uint32_t bar_id) const {
    auto reg = config::BaseAddress(bar_id).ReadFrom(this);

    // When probing a BAR, the hardware writes a 0 in every valid bit used
    // for an address. The least significant address bit set represents the
    // size of the BAR. For example, if our size was 1M we would have an
    // unmanipulated 0x00100000
    //
    // 0x00100000 - 1 = 0x000FFFFF
    //  ~(0x000FFFFF) = 0xFFF00000

    uint32_t size = kTestDeviceBars[bar_id].size;
    if (config::VendorId.ReadFrom(this).reg_value() == 0x1af4) {
      // Virtio-input config has BAR 1 and BAR 4 implemented (each 4KB), other BARs unimplemented.
      size = (bar_id == 1 || bar_id == 4) ? 4096 : 0;
    }
    if (size == 0) {
      MmioConfig::Write32(0, static_cast<uint16_t>(config::BaseAddress(bar_id).addr()));
      return;
    }
    ZX_ASSERT(cpp20::has_single_bit(size));
    size = ~(size - 1);

    if (reg.is_io_space()) {
      reg.set_reg_value(reg.io().set_base_address(size).reg_value());
    } else {
      reg.set_reg_value(reg.mmio().set_base_address(size).reg_value());
    }
    MmioConfig::Write32(reg.reg_value(), static_cast<uint16_t>(config::BaseAddress(bar_id).addr()));
  }

  const char* type() const final { return "mmio"; }

 protected:
  void Write32(uint32_t val, uint16_t offset) const final {
    switch (offset) {
      case pci::kConfigBaseAddresses + (0 * sizeof(uint32_t)):
      case pci::kConfigBaseAddresses + (1 * sizeof(uint32_t)):
      case pci::kConfigBaseAddresses + (2 * sizeof(uint32_t)):
      case pci::kConfigBaseAddresses + (3 * sizeof(uint32_t)):
      case pci::kConfigBaseAddresses + (4 * sizeof(uint32_t)):
      case pci::kConfigBaseAddresses + (5 * sizeof(uint32_t)): {
        // A 32-bit write of all 1s in these addresses is reserved for
        // querying the BAR size as long as it's not the upper half of
        // a 64 bit register.
        uint32_t bar_id = (offset - pci::kConfigBaseAddresses) / 4;
        bool is_upper_half = (config::VendorId.ReadFrom(this).reg_value() == 0x1af4)
                                 ? (bar_id == 5)
                                 : kTestDeviceBars[bar_id].is_upper_half;
        if (val == UINT32_MAX && !is_upper_half) {
          MockBarProbeSideEffects(bar_id);
          break;
        }
        __FALLTHROUGH;
      }
      default:
        MmioConfig::Write32(val, offset);
    }
  }
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_TEST_FAKES_FAKE_CONFIG_H_
