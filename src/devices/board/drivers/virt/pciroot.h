// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_BOARD_DRIVERS_VIRT_PCIROOT_H_
#define SRC_DEVICES_BOARD_DRIVERS_VIRT_PCIROOT_H_

#include <fuchsia/hardware/pciroot/c/banjo.h>
#include <fuchsia/hardware/pciroot/cpp/banjo.h>
#include <lib/pci/constants.h>
#include <lib/pci/pciroot.h>
#include <lib/pci/root_host.h>
#include <lib/zx/bti.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <zircon/assert.h>
#include <zircon/status.h>

#include <cstdint>
#include <vector>

namespace board_virt {

inline constexpr char kPcirootName[] = "PCI0";

// Per-architecture PCIe memory map and IRQ constants from QEMU hw/arm/virt.c and hw/riscv/virt.c.
#ifdef __aarch64__
inline constexpr ralloc_region_t kVirtPcieEcam = {.base = 0x3f00'0000, .size = 0x0100'0000};
inline constexpr ralloc_region_t kVirtPcieMmio32 = {.base = 0x1000'0000, .size = 0x2eff'0000};
inline constexpr ralloc_region_t kVirtPcieMmio64 = {.base = 0x80'0000'0000, .size = 0x80'0000'0000};
inline constexpr ralloc_region_t kVirtPciePio = {.base = 0x3eff'0000, .size = 0x0001'0000};
inline constexpr uint8_t kVirtPcieIrqBase = 32 + 3;
inline constexpr uint8_t kVirtPcieIrqCount = 4;
inline constexpr uint32_t kVirtPcieIrqOptions = ZX_INTERRUPT_MODE_LEVEL_HIGH;
#elifdef __riscv
inline constexpr ralloc_region_t kVirtPcieEcam = {.base = 0x3000'0000, .size = 0x1000'0000};
inline constexpr ralloc_region_t kVirtPcieMmio32 = {.base = 0x4000'0000, .size = 0x4000'0000};
inline constexpr ralloc_region_t kVirtPcieMmio64 = {.base = 0, .size = 0};
inline constexpr ralloc_region_t kVirtPciePio = {.base = 0x300'0000, .size = 0x0001'0000};
inline constexpr uint8_t kVirtPcieIrqBase = 0x20;
inline constexpr uint8_t kVirtPcieIrqCount = 4;
inline constexpr uint32_t kVirtPcieIrqOptions = ZX_INTERRUPT_MODE_DEFAULT;
#else
#error "Unsupported architecture"
#endif

inline constexpr uint8_t kEndBusNumber =
    static_cast<uint8_t>((kVirtPcieEcam.size / pci::kEcamBytesPerBus) - 1);

inline constexpr McfgAllocation kVirtPcieMcfg = {
    .address = kVirtPcieEcam.base,
    .pci_segment = 0,
    .start_bus_number = 0,
    .end_bus_number = kEndBusNumber,
};

class VirtPciroot : public PcirootBase, public ddk::PcirootProtocol<VirtPciroot> {
 public:
  VirtPciroot() = delete;
  VirtPciroot(PciRootHost* root_host, zx::vmo ecam, zx::resource irq_resource,
              zx::resource iommu_resource)
      : PcirootBase(root_host),
        ecam_(std::move(ecam)),
        irq_resource_(std::move(irq_resource)),
        iommu_resource_(std::move(iommu_resource)) {
    ZX_DEBUG_ASSERT(irq_resource_.is_valid());
    ZX_DEBUG_ASSERT(iommu_resource_.is_valid());
  }
  ~VirtPciroot() override = default;

  zx::result<> CreateInterrupts();

  // fuchsia.hardware.pciroot implementation fill-ins
  using PcirootBase::PcirootAllocateMsi;
  using PcirootBase::PcirootDriverShouldProxyConfig;
  using PcirootBase::PcirootGetAddressSpace;
  using PcirootBase::PcirootGetMsiHandle;
  using PcirootBase::PcirootReadConfig16;
  using PcirootBase::PcirootReadConfig32;
  using PcirootBase::PcirootReadConfig8;
  using PcirootBase::PcirootWriteConfig16;
  using PcirootBase::PcirootWriteConfig32;
  using PcirootBase::PcirootWriteConfig8;
  zx_status_t PcirootGetBti(uint32_t bdf, uint32_t index, zx::bti* bti);
  zx_status_t PcirootGetPciPlatformInfo(pci_platform_info_t* info);

  pciroot_protocol_ops_t* pciroot_protocol_ops() { return &pciroot_protocol_ops_; }

 private:
  zx::vmo ecam_;
  const zx::resource irq_resource_;
  const zx::resource iommu_resource_;
  std::vector<zx::interrupt> owned_interrupts_;
  std::vector<pci_legacy_irq_t> interrupts_;
  std::vector<pci_irq_routing_entry_t> irq_routing_entries_;
};

}  // namespace board_virt

#endif  // SRC_DEVICES_BOARD_DRIVERS_VIRT_PCIROOT_H_
