// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BOARD_LIB_ACPI_PCI_INTERNAL_H_
#define SRC_DEVICES_BOARD_LIB_ACPI_PCI_INTERNAL_H_

#include <fidl/fuchsia.hardware.pci.root/cpp/driver/wire.h>
#include <fidl/fuchsia.hardware.pci.root/cpp/wire.h>
#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <lib/async/dispatcher.h>
#include <lib/ddk/device.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/fdf/dispatcher.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/iommu.h>
#include <lib/zx/resource.h>
#include <lib/zx/vmo.h>

#include <cstdint>
#include <memory>
#include <optional>
#include <unordered_map>
#include <vector>

#include <acpica/acpi.h>
#include <acpica/actypes.h>
#include <ddktl/device.h>

#include "src/devices/board/lib/acpi/acpi.h"
#include "src/devices/board/lib/acpi/pci-root-windows.h"

// A structure derived from ACPI _PRTs that represents a zx::interrupt to create and
// provide to the PCI bus driver.
struct acpi_legacy_irq {
  uint32_t vector;   // Hardware vector
  uint32_t options;  // Configuration for zx_interrupt_create
};

namespace acpi {

// Everything a PCI root learns about itself from ACPI. The handles it holds are owned here and
// stay valid for the life of the root.
struct PciRootInfo {
  char name[ACPI_NAMESEG_SIZE + 1] = {};
  zx::iommu iommu;
  uint8_t start_bus_num = 0;
  uint8_t end_bus_num = 0;
  uint16_t segment_group = 0;
  // Present only when the MCFG table has an entry for this root's segment group.
  std::optional<zx::vmo> ecam;
  // The interrupts for each legacy vector found in the _PRTs, and the resources they were
  // created from.
  std::unordered_map<uint32_t, zx::interrupt> legacy_irqs;
  std::vector<zx::resource> irq_resources;
  std::vector<fuchsia_hardware_pci_root::wire::IrqRoutingEntry> routing;
  std::vector<fuchsia_hardware_pci::wire::Bdf> acpi_bdfs;
  // The address space this root produces, and the allocations made from it.
  std::unique_ptr<acpi::PciRootWindows> windows;
  zx::unowned_resource msi_resource;
};

}  // namespace acpi

// Serves fuchsia.hardware.pci.root for a single ACPI PCI root. Config is served only on segment 0
// when the root has no ECAM, since legacy PIO cannot address other segments.
class AcpiPciRoot;
using AcpiPciRootType = ddk::Device<AcpiPciRoot>;
class AcpiPciRoot : public AcpiPciRootType,
                    public fidl::WireServer<fuchsia_hardware_pci_root::Root>,
                    public fdf::WireServer<fuchsia_hardware_pci_root::Config> {
 public:
  // Publishes the root under |parent|, serving its protocols on |dispatcher|.
  static zx_status_t Create(acpi::PciRootInfo info, zx_device_t* parent,
                            async_dispatcher_t* dispatcher);

  // fuchsia.hardware.pci.root/Root
  void GetBti(GetBtiRequestView request, GetBtiCompleter::Sync& completer) override;
  void GetPlatformInfo(GetPlatformInfoCompleter::Sync& completer) override;
  void GetCam(GetCamCompleter::Sync& completer) override;
  void GetLegacyInterrupt(GetLegacyInterruptRequestView request,
                          GetLegacyInterruptCompleter::Sync& completer) override;
  void GetAddressSpace(GetAddressSpaceRequestView request,
                       GetAddressSpaceCompleter::Sync& completer) override;
  void AllocateMsi(AllocateMsiRequestView request, AllocateMsiCompleter::Sync& completer) override;
  void GetMsiHandle(GetMsiHandleRequestView request,
                    GetMsiHandleCompleter::Sync& completer) override;

  // fuchsia.hardware.pci.root/Config
  void Read8(Read8RequestView request, fdf::Arena& arena, Read8Completer::Sync& completer) override;
  void Read16(Read16RequestView request, fdf::Arena& arena,
              Read16Completer::Sync& completer) override;
  void Read32(Read32RequestView request, fdf::Arena& arena,
              Read32Completer::Sync& completer) override;
  void Write8(Write8RequestView request, fdf::Arena& arena,
              Write8Completer::Sync& completer) override;
  void Write16(Write16RequestView request, fdf::Arena& arena,
               Write16Completer::Sync& completer) override;
  void Write32(Write32RequestView request, fdf::Arena& arena,
               Write32Completer::Sync& completer) override;

  void DdkRelease() { delete this; }

 private:
  AcpiPciRoot(acpi::PciRootInfo info, zx_device_t* parent, fdf_dispatcher_t* dispatcher)
      : AcpiPciRootType(parent), info_(std::move(info)), outgoing_(dispatcher) {}

  bool serves_config() const { return !info_.ecam.has_value() && info_.segment_group == 0; }

  acpi::PciRootInfo info_;
  fidl::ServerBindingGroup<fuchsia_hardware_pci_root::Root> root_bindings_;
  fdf::ServerBindingGroup<fuchsia_hardware_pci_root::Config> config_bindings_;
  // Declared last so it is torn down first, as it dispatches connections to the bindings above.
  fdf::OutgoingDirectory outgoing_;
};

namespace acpi {

// Reads the root's _PRTs into |info|'s routing table, and the configuration of each legacy
// interrupt vector they reference into |irqs|.
ACPI_STATUS GetPciRootIrqRouting(acpi::Acpi* acpi, ACPI_HANDLE root_obj, acpi::PciRootInfo* info,
                                 std::unordered_map<uint32_t, acpi_legacy_irq>* irqs);

}  // namespace acpi

#endif  // SRC_DEVICES_BOARD_LIB_ACPI_PCI_INTERNAL_H_
