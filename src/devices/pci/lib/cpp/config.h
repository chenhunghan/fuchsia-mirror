// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_LIB_CPP_CONFIG_H_
#define SRC_DEVICES_PCI_LIB_CPP_CONFIG_H_

#include <fidl/fuchsia.hardware.pci/cpp/wire_types.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/pci/constants.h>
#include <lib/pci/hw.h>
#include <lib/zx/result.h>
#include <stdio.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <memory>
#include <type_traits>

#include <hwreg/bitfields.h>

namespace pci {

class Config;

namespace config {

// Fields correspond to the name in the PCI Local Bus Spec section 6.2.
struct VendorIdReg : public hwreg::RegisterBase<VendorIdReg, uint16_t> {};
inline const hwreg::RegisterAddr<VendorIdReg> VendorId(pci::kConfigVendorId);

struct DeviceIdReg : public hwreg::RegisterBase<DeviceIdReg, uint16_t> {};
inline const hwreg::RegisterAddr<DeviceIdReg> DeviceId(pci::kConfigDeviceId);

struct CommandReg : public hwreg::RegisterBase<CommandReg, uint16_t> {
  // 15-11 are reserved preserve.
  DEF_BIT(10, interrupt_disable);
  DEF_BIT(9, fast_back_to_back_enable);
  DEF_BIT(8, serr_enable);
  // 7 is reserved preserve.
  DEF_BIT(6, parity_error_response);
  DEF_BIT(5, vga_palette_snoop);
  DEF_BIT(4, memory_write_and_invalidate_enable);
  DEF_BIT(3, special_cycles);
  DEF_BIT(2, bus_master);
  DEF_BIT(1, memory_space);
  DEF_BIT(0, io_space);
};
inline const hwreg::RegisterAddr<CommandReg> Command(pci::kConfigCommand);

// Status register (PCI Local Bus Spec section 6.2.3, PCIe Base Spec section 7.5.1.1.4).
// Bits 8 and 11-15 are RW1C. Writes must set only the bits to be cleared (no read-modify-write).
struct StatusReg : public hwreg::RegisterBase<StatusReg, uint16_t> {
  DEF_BIT(15, detected_parity_error);
  DEF_BIT(14, signaled_system_error);
  DEF_BIT(13, received_master_abort);
  DEF_BIT(12, received_target_abort);
  DEF_BIT(11, signaled_target_abort);
  DEF_FIELD(10, 9, devsel_timing);
  DEF_BIT(8, master_data_parity_error);
  DEF_BIT(7, fast_b2b_capable);
  // 6 is reserved preserve.
  DEF_BIT(5, capable_66mhz);
  DEF_BIT(4, has_capabilities_list);
  DEF_BIT(3, interrupt_status);
  // 2-1 are reserved preserve.
  DEF_BIT(0, immediate_readiness);
};
inline const hwreg::RegisterAddr<StatusReg> Status(pci::kConfigStatus);

struct RevisionIdReg : public hwreg::RegisterBase<RevisionIdReg, uint8_t> {};
inline const hwreg::RegisterAddr<RevisionIdReg> RevisionId(pci::kConfigRevisionId);

struct ProgramInterfaceReg : public hwreg::RegisterBase<ProgramInterfaceReg, uint8_t> {};
inline const hwreg::RegisterAddr<ProgramInterfaceReg> ProgramInterface(pci::kConfigClassCodeIntr);

struct SubClassReg : public hwreg::RegisterBase<SubClassReg, uint8_t> {};
inline const hwreg::RegisterAddr<SubClassReg> SubClass(pci::kConfigClassCodeSub);

struct BaseClassReg : public hwreg::RegisterBase<BaseClassReg, uint8_t> {};
inline const hwreg::RegisterAddr<BaseClassReg> BaseClass(pci::kConfigClassCodeBase);

struct CacheLineSizeReg : public hwreg::RegisterBase<CacheLineSizeReg, uint8_t> {};
inline const hwreg::RegisterAddr<CacheLineSizeReg> CacheLineSize(pci::kConfigCacheLineSize);

struct LatencyTimerReg : public hwreg::RegisterBase<LatencyTimerReg, uint8_t> {};
inline const hwreg::RegisterAddr<LatencyTimerReg> LatencyTimer(pci::kConfigLatencyTimer);

struct HeaderTypeReg : public hwreg::RegisterBase<HeaderTypeReg, uint8_t> {
  DEF_BIT(7, multi_function);
  DEF_FIELD(6, 0, type);
};
inline const hwreg::RegisterAddr<HeaderTypeReg> HeaderType(pci::kConfigHeaderType);

struct BistReg : public hwreg::RegisterBase<BistReg, uint8_t> {
  DEF_BIT(7, capable);
  DEF_BIT(6, start);
  // 5-4 are reserved.
  DEF_FIELD(3, 0, completion_code);
};
inline const hwreg::RegisterAddr<BistReg> Bist(pci::kConfigBist);

// The layout of a Base Address Register changes based on its type.
// PCI Local Bus Spec section 6.2.5.1.
struct IoBaseAddressReg : public hwreg::RegisterBase<IoBaseAddressReg, uint32_t> {
  DEF_UNSHIFTED_FIELD(31, 2, base_address);
  DEF_BIT(0, is_io_space);
};

struct MmioBaseAddressReg : public hwreg::RegisterBase<MmioBaseAddressReg, uint32_t> {
  static constexpr uint32_t kType32Bit = 0b00;
  static constexpr uint32_t kType64Bit = 0b10;

  DEF_UNSHIFTED_FIELD(31, 4, base_address);
  DEF_BIT(3, is_prefetchable);
  DEF_FIELD(2, 1, type);
  DEF_BIT(0, is_io_space);
};

struct BaseAddressReg : public hwreg::RegisterBase<BaseAddressReg, uint32_t> {
  // Bit 1 is reserved in I/O BARs and part of the 2-bit type field in MMIO BARs.
  DEF_BIT(0, is_io_space);

  IoBaseAddressReg io() const {
    return hwreg::RegisterAddr<IoBaseAddressReg>(reg_addr()).FromValue(reg_value());
  }
  MmioBaseAddressReg mmio() const {
    return hwreg::RegisterAddr<MmioBaseAddressReg>(reg_addr()).FromValue(reg_value());
  }
};

inline hwreg::RegisterAddr<BaseAddressReg> BaseAddress(uint32_t bar_id) {
  ZX_ASSERT(bar_id < pci::kMaxBarCount);
  return hwreg::RegisterAddr<BaseAddressReg>(
      static_cast<uint32_t>(pci::kConfigBaseAddresses + (bar_id * sizeof(uint32_t))));
}

struct CardbusCisPtrReg : public hwreg::RegisterBase<CardbusCisPtrReg, uint32_t> {};
inline const hwreg::RegisterAddr<CardbusCisPtrReg> CardbusCisPtr(pci::kConfigCardbusCisPtr);

struct SubsystemVendorIdReg : public hwreg::RegisterBase<SubsystemVendorIdReg, uint16_t> {};
inline const hwreg::RegisterAddr<SubsystemVendorIdReg> SubsystemVendorId(
    pci::kConfigSubsysVendorId);

struct SubsystemIdReg : public hwreg::RegisterBase<SubsystemIdReg, uint16_t> {};
inline const hwreg::RegisterAddr<SubsystemIdReg> SubsystemId(pci::kConfigSubsysId);

struct ExpansionRomAddressReg : public hwreg::RegisterBase<ExpansionRomAddressReg, uint32_t> {
  DEF_UNSHIFTED_FIELD(31, 11, address);
  // Bits 10:8 are reserved.
  DEF_FIELD(7, 4, validation_details);
  DEF_FIELD(3, 1, validation_status);
  DEF_BIT(0, enable);
};
inline const hwreg::RegisterAddr<ExpansionRomAddressReg> ExpansionRomAddress(
    pci::kConfigExpRomAddress);

struct CapabilitiesPtrReg : public hwreg::RegisterBase<CapabilitiesPtrReg, uint8_t> {};
inline const hwreg::RegisterAddr<CapabilitiesPtrReg> CapabilitiesPtr(pci::kConfigCapabilities);

// 0x35-0x3B is reserved.

struct InterruptLineReg : public hwreg::RegisterBase<InterruptLineReg, uint8_t> {};
inline const hwreg::RegisterAddr<InterruptLineReg> InterruptLine(pci::kConfigInterruptLine);

struct InterruptPinReg : public hwreg::RegisterBase<InterruptPinReg, uint8_t> {};
inline const hwreg::RegisterAddr<InterruptPinReg> InterruptPin(pci::kConfigInterruptPin);

struct MinGrantReg : public hwreg::RegisterBase<MinGrantReg, uint8_t> {};
inline const hwreg::RegisterAddr<MinGrantReg> MinGrant(pci::kConfigMinGrant);

struct MaxLatencyReg : public hwreg::RegisterBase<MaxLatencyReg, uint8_t> {};
inline const hwreg::RegisterAddr<MaxLatencyReg> MaxLatency(pci::kConfigMaxLatency);

// PCI-to-PCI bridge configuration registers (Type 1 header).
// Unlike a normal PCI header, a bridge only has two BARs, but the BAR offset in config space
// is the same.
struct PrimaryBusIdReg : public hwreg::RegisterBase<PrimaryBusIdReg, uint8_t> {};
inline const hwreg::RegisterAddr<PrimaryBusIdReg> PrimaryBusId(0x18);

struct SecondaryBusIdReg : public hwreg::RegisterBase<SecondaryBusIdReg, uint8_t> {};
inline const hwreg::RegisterAddr<SecondaryBusIdReg> SecondaryBusId(0x19);

struct SubordinateBusIdReg : public hwreg::RegisterBase<SubordinateBusIdReg, uint8_t> {};
inline const hwreg::RegisterAddr<SubordinateBusIdReg> SubordinateBusId(0x1A);

struct SecondaryLatencyTimerReg : public hwreg::RegisterBase<SecondaryLatencyTimerReg, uint8_t> {};
inline const hwreg::RegisterAddr<SecondaryLatencyTimerReg> SecondaryLatencyTimer(0x1B);

struct IoBaseReg : public hwreg::RegisterBase<IoBaseReg, uint8_t> {
  DEF_FIELD(7, 4, address);
  DEF_FIELD(3, 0, address_type);
};
inline const hwreg::RegisterAddr<IoBaseReg> IoBase(0x1C);

struct IoLimitReg : public hwreg::RegisterBase<IoLimitReg, uint8_t> {
  DEF_FIELD(7, 4, address);
  DEF_FIELD(3, 0, address_type);
};
inline const hwreg::RegisterAddr<IoLimitReg> IoLimit(0x1D);

// Secondary Status register (PCI to PCI Bridge Architecture Spec section 3.2.5.7, PCIe Base Spec
// section 7.5.1.3.7). Bits 8 and 11-15 are RW1C. Writes must set only the bits to be cleared (no
// read-modify-write).
struct SecondaryStatusReg : public hwreg::RegisterBase<SecondaryStatusReg, uint16_t> {};
inline const hwreg::RegisterAddr<SecondaryStatusReg> SecondaryStatus(0x1E);

struct MemoryBaseReg : public hwreg::RegisterBase<MemoryBaseReg, uint16_t> {
  DEF_FIELD(15, 4, address);
};
inline const hwreg::RegisterAddr<MemoryBaseReg> MemoryBase(0x20);

struct MemoryLimitReg : public hwreg::RegisterBase<MemoryLimitReg, uint16_t> {
  DEF_FIELD(15, 4, address);
};
inline const hwreg::RegisterAddr<MemoryLimitReg> MemoryLimit(0x22);

struct PrefetchableMemoryBaseReg : public hwreg::RegisterBase<PrefetchableMemoryBaseReg, uint16_t> {
  DEF_FIELD(15, 4, address);
  DEF_FIELD(3, 0, address_type);
};
inline const hwreg::RegisterAddr<PrefetchableMemoryBaseReg> PrefetchableMemoryBase(0x24);

struct PrefetchableMemoryLimitReg
    : public hwreg::RegisterBase<PrefetchableMemoryLimitReg, uint16_t> {
  DEF_FIELD(15, 4, address);
  DEF_FIELD(3, 0, address_type);
};
inline const hwreg::RegisterAddr<PrefetchableMemoryLimitReg> PrefetchableMemoryLimit(0x26);

struct PrefetchableMemoryBaseUpperReg
    : public hwreg::RegisterBase<PrefetchableMemoryBaseUpperReg, uint32_t> {};
inline const hwreg::RegisterAddr<PrefetchableMemoryBaseUpperReg> PrefetchableMemoryBaseUpper(0x28);

struct PrefetchableMemoryLimitUpperReg
    : public hwreg::RegisterBase<PrefetchableMemoryLimitUpperReg, uint32_t> {};
inline const hwreg::RegisterAddr<PrefetchableMemoryLimitUpperReg> PrefetchableMemoryLimitUpper(
    0x2C);

struct IoBaseUpperReg : public hwreg::RegisterBase<IoBaseUpperReg, uint16_t> {};
inline const hwreg::RegisterAddr<IoBaseUpperReg> IoBaseUpper(0x30);

struct IoLimitUpperReg : public hwreg::RegisterBase<IoLimitUpperReg, uint16_t> {};
inline const hwreg::RegisterAddr<IoLimitUpperReg> IoLimitUpper(0x32);

// Capabilities Pointer (0x34) is the same as standard Type 0 config.
// 0x35-0x37 is reserved.
struct BridgeExpansionRomAddressReg
    : public hwreg::RegisterBase<BridgeExpansionRomAddressReg, uint32_t> {
  DEF_UNSHIFTED_FIELD(31, 11, address);
  // Bits 10:8 are reserved.
  DEF_FIELD(7, 4, validation_details);
  DEF_FIELD(3, 1, validation_status);
  DEF_BIT(0, enable);
};
inline const hwreg::RegisterAddr<BridgeExpansionRomAddressReg> BridgeExpansionRomAddress(0x38);

// Interrupt Line (0x3C) and Interrupt Pin (0x3D) are the same as Type 0 config.
struct BridgeControlReg : public hwreg::RegisterBase<BridgeControlReg, uint16_t> {};
inline const hwreg::RegisterAddr<BridgeControlReg> BridgeControl(0x3E);

}  // namespace config

// Per spec (quoting Linux DT host-generic bindings):
// For CAM, this 24-bit offset is:
//         cfg_offset(bus, device, function, register) =
//                    bus << 16 | device << 11 | function << 8 | register
// While ECAM extends this by 4 bits to accommodate 4k of function space:
//         cfg_offset(bus, device, function, register) =
//                    bus << 20 | device << 15 | function << 12 | register
struct CamOffset {
  zx_vaddr_t offset = 0;
  DEF_SUBFIELD(offset, 23, 16, bus);
  DEF_SUBFIELD(offset, 15, 11, device);
  DEF_SUBFIELD(offset, 10, 8, function);
};

struct EcamOffset {
  zx_vaddr_t offset = 0;
  DEF_SUBFIELD(offset, 27, 20, bus);
  DEF_SUBFIELD(offset, 19, 15, device);
  DEF_SUBFIELD(offset, 14, 12, function);
};

template <class CamType>
constexpr zx_vaddr_t GetConfigOffsetInCam(fuchsia_hardware_pci::wire::Bdf bdf, uint8_t start_bus) {
  zx_vaddr_t bus_offset = CamType().set_bus(start_bus).offset;
  zx_vaddr_t bdf_offset =
      CamType().set_bus(bdf.bus_id).set_device(bdf.device_id).set_function(bdf.function_id).offset;
  return bdf_offset - bus_offset;
}

// Find the offset into the cam region for the given bdf address. Every bus
// has 32 devices, every device has 8 functions, and each function has a
// configuration access region of 256 bytes base, 4096 if extended. The base
// address of the vmo provided to the bus driver corresponds to the
// start_bus_num, so offset the bdf address based on the bottom of our ecam.
constexpr zx_vaddr_t GetConfigOffsetInCam(fuchsia_hardware_pci::wire::Bdf bdf, uint8_t start_bus,
                                          bool is_extended) {
  if (is_extended) {
    return GetConfigOffsetInCam<EcamOffset>(bdf, start_bus);
  }
  return GetConfigOffsetInCam<CamOffset>(bdf, start_bus);
}

// Config supplies the factory for creating the appropriate pci config
// object based on the address space of the pci device.
class Config {
 public:
  const fuchsia_hardware_pci::wire::Bdf& bdf() const { return bdf_; }
  const char* addr() const { return addr_; }
  virtual const char* type() const = 0;
  // Return a copy of the MmioView backing the Config's MMIO space, if supported.
  virtual zx::result<fdf::MmioView> get_view() const { return zx::error(ZX_ERR_NOT_SUPPORTED); }

  // Sized configuration space accessors used by hwreg for register I/O and direct raw-offset
  // access.
  template <typename IntType, typename = std::enable_if_t<std::is_unsigned_v<IntType> &&
                                                          !std::is_same_v<IntType, bool>>>
  IntType Read(uint32_t offset) const {
    static_assert(sizeof(IntType) == 1 || sizeof(IntType) == 2 || sizeof(IntType) == 4,
                  "Invalid PCI config access size");
    ZX_DEBUG_ASSERT(offset <= UINT16_MAX);
    if constexpr (sizeof(IntType) == 1) {
      return Read8(static_cast<uint16_t>(offset));
    } else if constexpr (sizeof(IntType) == 2) {
      return Read16(static_cast<uint16_t>(offset));
    } else {
      return Read32(static_cast<uint16_t>(offset));
    }
  }

  template <typename IntType, typename = std::enable_if_t<std::is_unsigned_v<IntType> &&
                                                          !std::is_same_v<IntType, bool>>>
  void Write(IntType val, uint32_t offset) const {
    static_assert(sizeof(IntType) == 1 || sizeof(IntType) == 2 || sizeof(IntType) == 4,
                  "Invalid PCI config access size");
    ZX_DEBUG_ASSERT(offset <= UINT16_MAX);
    if constexpr (sizeof(IntType) == 1) {
      Write8(val, static_cast<uint16_t>(offset));
    } else if constexpr (sizeof(IntType) == 2) {
      Write16(val, static_cast<uint16_t>(offset));
    } else {
      Write32(val, static_cast<uint16_t>(offset));
    }
  }
  virtual ~Config() = default;

 protected:
  explicit Config(fuchsia_hardware_pci::wire::Bdf bdf) : bdf_(bdf) {
    snprintf(addr_, sizeof(addr_), "%02x:%02x.%01x", bdf_.bus_id, bdf_.device_id, bdf_.function_id);
  }

  virtual uint8_t Read8(uint16_t offset) const = 0;
  virtual uint16_t Read16(uint16_t offset) const = 0;
  virtual uint32_t Read32(uint16_t offset) const = 0;
  virtual void Write8(uint8_t val, uint16_t offset) const = 0;
  virtual void Write16(uint16_t val, uint16_t offset) const = 0;
  virtual void Write32(uint32_t val, uint16_t offset) const = 0;

 private:
  const fuchsia_hardware_pci::wire::Bdf bdf_;
  char addr_[8];
};

// MMIO config is the standard method for accessing modern pci configuration space.
// A device's configuration space is mapped to a specific place in a given pci root's
// ecam and can be directly accessed with standard IO operations.
class MmioConfig : public Config {
 public:
  static zx::result<std::unique_ptr<Config>> Create(fuchsia_hardware_pci::wire::Bdf bdf,
                                                    const fdf::MmioBuffer& ecam, uint8_t start_bus,
                                                    uint8_t end_bus, bool is_extended) {
    if (bdf.bus_id < start_bus || bdf.bus_id > end_bus || bdf.device_id >= pci::kMaxDevicesPerBus ||
        bdf.function_id >= pci::kMaxFunctionsPerDevice) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    zx_vaddr_t config_offset = GetConfigOffsetInCam(bdf, start_bus, /*is_extended=*/is_extended);
    zx_vaddr_t config_size = is_extended ? pci::kExtendedConfigSize : pci::kBaseConfigSize;

    return zx::ok(std::make_unique<MmioConfig>(bdf, ecam.View(config_offset, config_size)));
  }

  MmioConfig(fuchsia_hardware_pci::wire::Bdf bdf, const fdf::MmioView& view)
      : Config(bdf), view_(view) {}
  explicit MmioConfig(const fdf::MmioView& view, fuchsia_hardware_pci::wire::Bdf bdf = {})
      : Config(bdf), view_(view) {}

  const char* type() const override { return "mmio"; }
  zx::result<fdf::MmioView> get_view() const final { return zx::ok(fdf::MmioView(view_)); }

 protected:
  uint8_t Read8(uint16_t offset) const final { return view_.Read<uint8_t>(offset); }
  uint16_t Read16(uint16_t offset) const final { return view_.Read<uint16_t>(offset); }
  uint32_t Read32(uint16_t offset) const final { return view_.Read<uint32_t>(offset); }
  void Write8(uint8_t val, uint16_t offset) const final { view_.Write(val, offset); }
  void Write16(uint16_t val, uint16_t offset) const final { view_.Write(val, offset); }
  void Write32(uint32_t val, uint16_t offset) const override { view_.Write(val, offset); }

 private:
  const fdf::MmioView view_;
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_LIB_CPP_CONFIG_H_
