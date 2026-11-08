// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_CAPABILITIES_MSI_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_CAPABILITIES_MSI_H_

#include <lib/driver/logging/cpp/logger.h>
#include <zircon/assert.h>

#include <utility>

#include <hwreg/bitfields.h>

#include "src/devices/pci/lib/cpp/capabilities.h"
#include "src/devices/pci/lib/cpp/config.h"

namespace pci {

struct MsiControlReg : public hwreg::RegisterBase<MsiControlReg, uint16_t> {
  DEF_BIT(0, enable);
  DEF_FIELD(3, 1, mm_capable);
  DEF_FIELD(6, 4, mm_enable);
  DEF_BIT(7, is_64bit_capable);
  DEF_BIT(8, is_pvm_capable);
  DEF_BIT(9, ext_msg_data_capable);
  DEF_BIT(10, ext_msg_data_enable);
};

struct MsiTargetAddrReg : public hwreg::RegisterBase<MsiTargetAddrReg, uint32_t> {};

struct MsiTargetAddrUpperReg : public hwreg::RegisterBase<MsiTargetAddrUpperReg, uint32_t> {};

struct MsiTargetDataReg : public hwreg::RegisterBase<MsiTargetDataReg, uint16_t> {};

struct MsiMaskBitsReg : public hwreg::RegisterBase<MsiMaskBitsReg, uint32_t> {};

struct MsiPendingBitsReg : public hwreg::RegisterBase<MsiPendingBitsReg, uint32_t> {};

// PCI Local Bus Spec 6.8.1: MSI Capability Structure.
class MsiCapability : public Capability {
 public:
  static constexpr size_t kMaxMsiVectors = 32u;
  // These methods convert from the mm_capable register values to irq count and
  // back. The register stores the nth power of two rather than the count itself
  // to save bits, but it's easier to lean on the compiler here than use pow()
  // methods. PCI Local Bus Specification v3.0 section 6.8.1.3.
  static constexpr uint8_t MmcToCount(uint16_t reg_value) {
    switch (reg_value) {
      case 0x000:
        return 1;
      case 0b001:
        return 2;
      case 0b010:
        return 4;
      case 0b011:
        return 8;
      case 0b100:
        return 16;
      case 0b101:
        return 32;
      default:
        break;
    }
    fdf::error("Invalid mm_capable value read: {:#x}", reg_value);
    return 1;
  }

  static constexpr uint8_t CountToMmc(uint16_t count) {
    switch (count) {
      case 1:
        return 0b000;
      case 2:
        return 0b001;
      case 4:
        return 0b010;
      case 8:
        return 0b011;
      case 16:
        return 0b100;
      case 32:
        return 0b101;
      default:
        ZX_PANIC("Invalid mm_capable value = %#x", count);
    }
  }

  MsiCapability(const Config& cfg, uint8_t base)
      : Capability(std::to_underlying(Capability::Id::kMsi), base, cfg.addr()),
        ctrl_(static_cast<uint16_t>(base + 0x2)),
        tgt_addr_(static_cast<uint16_t>(base + 0x4)),
        // In all 64 bit layouts the upper address bits are at base + 0x8
        tgt_addr_upper_(static_cast<uint16_t>(base + 0x8)) {
    // MSI has a structure layout that varies based on whether it supports
    // 64 bit address writes and per vector masking. Since there are four
    // possible layouts we need to determine the register offsets via probing.
    auto ctrl = ctrl_.ReadFrom(&cfg);
    vectors_avail_ = MmcToCount(ctrl.mm_capable());
    supports_pvm_ = ctrl.is_pvm_capable();
    is_64bit_ = ctrl.is_64bit_capable();

    if (is_64bit_) {
      tgt_data_addr_ = static_cast<uint16_t>(base + 0xC);
      if (supports_pvm_) {
        mask_bits_addr_ = static_cast<uint16_t>(base + 0x10);
        pending_bits_addr_ = static_cast<uint16_t>(base + 0x14);
      }
    } else {
      tgt_data_addr_ = static_cast<uint16_t>(base + 0x8);
      if (supports_pvm_) {
        mask_bits_addr_ = static_cast<uint16_t>(base + 0xC);
        pending_bits_addr_ = static_cast<uint16_t>(base + 0x10);
      }
    }
  }

  hwreg::RegisterAddr<MsiControlReg> ctrl() const { return ctrl_; }
  hwreg::RegisterAddr<MsiTargetAddrReg> tgt_addr() const { return tgt_addr_; }
  hwreg::RegisterAddr<MsiTargetAddrUpperReg> tgt_addr_upper() const {
    ZX_DEBUG_ASSERT(is_64bit_);
    return tgt_addr_upper_;
  }
  hwreg::RegisterAddr<MsiTargetDataReg> tgt_data() const {
    return hwreg::RegisterAddr<MsiTargetDataReg>(tgt_data_addr_);
  }

  hwreg::RegisterAddr<MsiMaskBitsReg> mask_bits() const {
    ZX_DEBUG_ASSERT(supports_pvm_);
    return hwreg::RegisterAddr<MsiMaskBitsReg>(mask_bits_addr_);
  }

  hwreg::RegisterAddr<MsiPendingBitsReg> pending_bits() const {
    ZX_DEBUG_ASSERT(supports_pvm_);
    return hwreg::RegisterAddr<MsiPendingBitsReg>(pending_bits_addr_);
  }

  uint8_t vectors_avail() const { return vectors_avail_; }
  bool supports_pvm() const { return supports_pvm_; }
  bool is_64bit() const { return is_64bit_; }

 private:
  const hwreg::RegisterAddr<MsiControlReg> ctrl_;
  const hwreg::RegisterAddr<MsiTargetAddrReg> tgt_addr_;
  const hwreg::RegisterAddr<MsiTargetAddrUpperReg> tgt_addr_upper_;
  // These values can only be determined at runtime based on the capability layout.
  uint16_t tgt_data_addr_ = 0;
  uint16_t mask_bits_addr_ = 0;
  uint16_t pending_bits_addr_ = 0;
  uint8_t vectors_avail_ = 0;
  bool supports_pvm_ = false;
  bool is_64bit_ = false;
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_CAPABILITIES_MSI_H_
