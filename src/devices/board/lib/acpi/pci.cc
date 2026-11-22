// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
#include "pci.h"

#include <inttypes.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/driver.h>
#include <lib/pci/constants.h>
#include <lib/zx/iommu.h>
#include <lib/zx/resource.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <zircon/status.h>
#include <zircon/syscalls/iommu.h>
#include <zircon/syscalls/resource.h>
#include <zircon/types.h>

#include <memory>
#include <optional>
#include <vector>

#include <acpica/acpi.h>
#include <acpica/actypes.h>
#include <acpica/acuuid.h>
#include <bits/limits.h>
#include <fbl/string_buffer.h>

#include "src/devices/board/lib/acpi/acpi.h"
#include "src/devices/board/lib/acpi/manager.h"
#include "src/devices/board/lib/acpi/pci-internal.h"
#include "src/devices/board/lib/acpi/resources.h"

struct ResourceContext {
  acpi::PciRootWindows* windows;
  bool device_is_root_bridge;
  bool add_pass;
};

// ACPICA will call this function for each resource found while walking a device object's resource
// list.
static acpi::status<> resource_report_callback(ACPI_RESOURCE* res, ResourceContext* ctx) {
  zx_status_t status;

  bool is_mmio = false;
  uint64_t base = 0;
  uint64_t len = 0;
  bool add_range = false;

  if (resource_is_memory(res)) {
    resource_memory_t mem;
    status = resource_parse_memory(res, &mem);
    if (status != ZX_OK || mem.minimum != mem.maximum) {
      return acpi::error(AE_ERROR);
    }

    is_mmio = true;
    base = mem.minimum;
    len = mem.address_length;
  } else if (resource_is_address(res)) {
    resource_address_t addr;
    status = resource_parse_address(res, &addr);
    if (status != ZX_OK) {
      return acpi::error(AE_ERROR);
    }

    if (addr.resource_type == RESOURCE_ADDRESS_MEMORY) {
      is_mmio = true;
    } else if (addr.resource_type == RESOURCE_ADDRESS_IO) {
      is_mmio = false;
    } else {
      return acpi::ok();
    }

    if (!addr.min_address_fixed || !addr.max_address_fixed || addr.maximum < addr.minimum) {
      zxlogf(WARNING, "ACPI found bad _CRS address entry\n");
      return acpi::ok();
    }

    // We compute len from maximum rather than address_length, since some
    // implementations don't set address_length...
    base = addr.minimum;
    len = addr.maximum - base + 1;

    // PCI root bridges report downstream resources via _CRS.  Since we're
    // gathering data on acceptable ranges for PCI to use for MMIO, consider
    // non-consume-only address resources to be valid for PCI MMIO.
    if (ctx->device_is_root_bridge && !addr.consumed_only) {
      add_range = true;
    }
  } else if (resource_is_io(res)) {
    resource_io_t io;
    status = resource_parse_io(res, &io);
    if (status != ZX_OK) {
      return acpi::error(AE_ERROR);
    }

    if (io.minimum != io.maximum) {
      zxlogf(WARNING, "ACPI found bad _CRS IO entry\n");
      return acpi::ok();
    }

    is_mmio = false;
    base = io.minimum;
    len = io.address_length;
  } else {
    return acpi::ok();
  }

  // Ignore empty regions that are reported, and skip any resources that
  // aren't for the pass we're doing.
  if (len == 0 || add_range != ctx->add_pass) {
    return acpi::ok();
  }

  constexpr uint64_t k1MiB = 1024ULL * 1024ULL;
  if (add_range && is_mmio && base < k1MiB) {
    // The PC platform defines many legacy regions below 1MB that we do not
    // want PCIe to try to map onto.
    zxlogf(DEBUG, "Skipping adding MMIO range due to being below 1MB");
    return acpi::ok();
  }

  // Add/Subtract the [base, len] region we found through ACPI to the windows
  // that PCI can use to allocate BARs.
  const acpi::PciRootWindows::Space space =
      is_mmio ? acpi::PciRootWindows::Space::kMemory : acpi::PciRootWindows::Space::kIo;

  zxlogf(DEBUG, "ACPI range modification: %sing %s %016lx %016lx", add_range ? "add" : "subtract",
         is_mmio ? "MMIO" : "PIO", base, len);
  // Not all resources ACPI informs us are in use are provided to us as
  // resources in the first search, so we allow Incomplete ranges in both add
  // and subtract passes.
  if (add_range) {
    status = ctx->windows->AddWindow(space, base, len);
  } else {
    status = ctx->windows->SubtractWindow(space, base, len);
  }

  if (status != ZX_OK) {
    if (add_range) {
      zxlogf(INFO, "Failed to add range: [%#lx - %#lx] (%#lx): %d", base, base + len, len, status);
    } else {
      // If we are subtracting a range and fail, abort.  This is bad.
      zxlogf(INFO, "Failed to subtract range [%#lx - %#lx] (%#lx): %d", base, base + len, len,
             status);
      return acpi::error(AE_ERROR);
    }
  }
  return acpi::ok();
}

// ACPICA will call this function once per device object found while walking the device
// tree off of the PCI root.
static acpi::status<> walk_devices_callback(ACPI_HANDLE object, ResourceContext* ctx,
                                            acpi::Acpi* acpi) {
  acpi::UniquePtr<ACPI_DEVICE_INFO> info;
  auto res = acpi->GetObjectInfo(object);
  if (res.is_error()) {
    zxlogf(DEBUG, "acpi::GetObjectInfo failed %d", res.error_value());
    return res.take_error();
  }
  info = std::move(res.value());

  ctx->device_is_root_bridge = (info->Flags & ACPI_PCI_ROOT_BRIDGE) != 0;

  acpi::status<> status = acpi->WalkResources(
      object, const_cast<char*>("_CRS"),
      [ctx](ACPI_RESOURCE* rsrc) { return resource_report_callback(rsrc, ctx); });
  if (status.is_ok() || status.error_value() == AE_NOT_FOUND) {
    return acpi::ok();
  }
  return status;
}

// Fills |windows| with the address space the PCI root |root| produces, minus anything consumed
// by other devices in the platform.
static zx_status_t scan_acpi_tree_for_resources(acpi::Acpi* acpi, ACPI_HANDLE root,
                                                acpi::PciRootWindows* windows) {
  // First we search for resources to add, then we subtract out things that
  // are being consumed elsewhere.  This forces an ordering on the
  // operations so that it should be consistent, and should protect against
  // inconistencies in the _CRS methods.

  // Add the ranges this root "produces" in its own _CRS.
  ResourceContext ctx = {
      .windows = windows,
      .device_is_root_bridge = true,
      .add_pass = true,
  };
  acpi::status<> status = acpi->WalkResources(root, "_CRS", [ctx = &ctx](ACPI_RESOURCE* rsrc) {
    return resource_report_callback(rsrc, ctx);
  });
  if (status.is_error() && status.error_value() != AE_NOT_FOUND) {
    return ZX_ERR_INTERNAL;
  }

  // Removes resources we believe are in use by other parts of the platform
  ctx.add_pass = false;
  status = acpi->GetDevices(nullptr, [acpi, ctx = &ctx](ACPI_HANDLE device, uint32_t) {
    return walk_devices_callback(device, ctx, acpi);
  });
  if (status.is_error()) {
    return ZX_ERR_INTERNAL;
  }

  return ZX_OK;
}

// Looks up the MCFG allocation entry for |segment_group|, if the platform has an MCFG table.
static std::optional<ACPI_MCFG_ALLOCATION> read_mcfg_allocation(uint16_t segment_group) {
  ACPI_TABLE_HEADER* raw_table = nullptr;
  ACPI_STATUS status = AcpiGetTable(const_cast<char*>(ACPI_SIG_MCFG), 1, &raw_table);
  if (status != AE_OK) {
    zxlogf(DEBUG, "no MCFG table found: %d", status);
    return std::nullopt;
  }

  auto* mcfg = reinterpret_cast<ACPI_TABLE_MCFG*>(raw_table);
  uintptr_t table_start = reinterpret_cast<uintptr_t>(mcfg) + sizeof(ACPI_TABLE_MCFG);
  uintptr_t table_end = reinterpret_cast<uintptr_t>(mcfg) + mcfg->Header.Length;
  if (table_end < table_start || (table_end - table_start) % sizeof(ACPI_MCFG_ALLOCATION) != 0) {
    zxlogf(ERROR, "MCFG table has invalid size %u", mcfg->Header.Length);
    return std::nullopt;
  }

  const size_t count = (table_end - table_start) / sizeof(ACPI_MCFG_ALLOCATION);
  const auto* entries = reinterpret_cast<const ACPI_MCFG_ALLOCATION*>(table_start);
  for (size_t i = 0; i < count; i++) {
    if (entries[i].PciSegment == segment_group) {
      return entries[i];
    }
  }
  return std::nullopt;
}

static void pci_init_interrupts(zx_device_t* parent, acpi::Acpi* acpi, ACPI_HANDLE object,
                                acpi::PciRootInfo* info) {
  std::unordered_map<uint32_t, acpi_legacy_irq> irqs;
  if (acpi::GetPciRootIrqRouting(acpi, object, info, &irqs) != AE_OK) {
    zxlogf(ERROR, "Failed to obtain PCI IRQ routing information, legacy IRQs will not function");
  }

  fbl::StringBuffer<ZX_MAX_NAME_LEN> name = {};
  name.Append(info->name);
  name.Append(" legacy");

  for (const auto& [vector, irq_cfg] : irqs) {
    zx::resource resource;
    zx_status_t status = zx::resource::create(*zx::unowned_resource(get_irq_resource(parent)),
                                              ZX_RSRC_KIND_IRQ | ZX_RSRC_FLAG_EXCLUSIVE, vector, 1,
                                              name.data(), name.size(), &resource);

    if (status != ZX_OK) {
      zxlogf(ERROR, "Couldn't create resource for legacy vector %#x: %s, skipping it", vector,
             zx_status_get_string(status));
      continue;
    }

    zx::interrupt interrupt;
    status = zx::interrupt::create(resource, vector, irq_cfg.options, &interrupt);
    if (status != ZX_OK) {
      zxlogf(ERROR, "Couldn't create irq for legacy vector %#x: %s, skipping it", vector,
             zx_status_get_string(status));
      continue;
    }

    info->irq_resources.push_back(std::move(resource));
    info->legacy_irqs.emplace(vector, std::move(interrupt));
  }
}

static zx_status_t pci_init_segment_and_ecam(zx_device_t* parent, acpi::Acpi* acpi,
                                             ACPI_HANDLE object, acpi::PciRootInfo* info) {
  auto bbn = acpi->CallBbn(object);
  if (bbn.is_error() && acpi_to_zx_status(bbn.error_value()) != ZX_ERR_NOT_FOUND) {
    zxlogf(DEBUG, "Unable to read _BBN for '%s' (%d), assuming base bus of 0", info->name,
           bbn.error_value());

    // Until we find an ecam we assume this potential legacy pci bus spans
    // bus 0 to bus 255 in its segment group.
    info->end_bus_num = pci::kMaxBuses - 1;
  }
  bool found_bbn = false;
  if (bbn.is_ok()) {
    info->start_bus_num = bbn.value();
    found_bbn = true;
  }

  auto seg = acpi->CallSeg(object);
  if (seg.is_error()) {
    info->segment_group = 0;
    zxlogf(DEBUG, "Unable to read _SEG for '%s' (%d), assuming segment group 0.", info->name,
           seg.error_value());
  } else {
    info->segment_group = seg.value();
  }

  // If an MCFG is found for the given segment group this root has then we'll
  // use its information to populate any fields missing via _BBN / _SEG.
  // Per the PCI Firmware spec v3 table 4-3 note 1, a given segment group will
  // contain only a single MCFG allocation entry.
  std::optional<ACPI_MCFG_ALLOCATION> mcfg = read_mcfg_allocation(info->segment_group);
  if (mcfg.has_value()) {
    // Print a warning if _BBN and MCFG bus numbers don't match. We'll use the
    // MCFG first if we have one, but a mismatch likely represents an error in
    // an ACPI table.
    if (found_bbn && mcfg->StartBusNumber != info->start_bus_num) {
      zxlogf(WARNING, "conflicting base bus num for '%s', _BBN reports %u and MCFG reports %u",
             info->name, info->start_bus_num, mcfg->StartBusNumber);
    }

    // Since we have an ecam its metadata will replace anything defined in the ACPI tables.
    info->start_bus_num = mcfg->StartBusNumber;
    info->end_bus_num = mcfg->EndBusNumber;

    // The bus driver needs a VMO representing the entire ecam region so it can map it in.
    // The range from start_bus_num to end_bus_num is inclusive.
    size_t ecam_size = (info->end_bus_num - info->start_bus_num + 1) * pci::kEcamBytesPerBus;
    zx_paddr_t vmo_base = mcfg->Address + (info->start_bus_num * pci::kEcamBytesPerBus);
    const size_t vmo_size = fbl::round_up<size_t>(ecam_size, zx_system_get_page_size());
    zx::vmo ecam;
    zx_status_t status = zx::vmo::create_physical(*zx::unowned_resource(get_mmio_resource(parent)),
                                                  vmo_base, vmo_size, &ecam);
    if (status != ZX_OK) {
      zxlogf(ERROR, "couldn't create VMO for ecam, mmio cfg will not work: %s!",
             zx_status_get_string(status));
      return status;
    }
    info->ecam = std::move(ecam);
  }

  if (zxlog_level_enabled(DEBUG)) {
    fbl::StringBuffer<128> log;
    log.AppendPrintf("%s { acpi_obj(%p), bus range: %u:%u, segment: %u", info->name, object,
                     info->start_bus_num, info->end_bus_num, info->segment_group);
    if (info->ecam) {
      log.AppendPrintf(", ecam base: %#llx", mcfg->Address);
    }
    log.AppendPrintf(" }");
    zxlogf(DEBUG, "%s", log.c_str());
  }

  return ZX_OK;
}

// Gathers everything a PCI root needs from ACPI: its bus range, segment group, ECAM, and legacy
// interrupt routing.
static zx::result<acpi::PciRootInfo> discover_pci_root(
    zx_device_t* parent, acpi::Manager* manager, ACPI_HANDLE object,
    acpi::UniquePtr<ACPI_DEVICE_INFO> device_info, std::vector<ACPI_PCI_ID> acpi_bdfs) {
  // If we find _BBN / _SEG we will use those, but if we don't we can fall
  // back on having an ecam from mcfg allocations.
  acpi::PciRootInfo info{};
  // ACPI names are stored as 4 bytes in a u32
  memcpy(info.name, &device_info->Name, ACPI_NAMESEG_SIZE);
  zx_iommu_desc_stub_t stub{};
  if (zx_status_t status = zx::iommu::create(*zx::unowned_resource(get_iommu_resource(parent)),
                                             ZX_IOMMU_TYPE_STUB, &stub, sizeof(stub), &info.iommu);
      status != ZX_OK) {
    zxlogf(WARNING, "Couldn't create stub IOMMU for %s: %s", info.name,
           zx_status_get_string(status));
  }

  info.acpi_bdfs.reserve(acpi_bdfs.size());
  for (const ACPI_PCI_ID& bdf : acpi_bdfs) {
    info.acpi_bdfs.push_back({
        .bus_id = static_cast<uint8_t>(bdf.Bus),
        .device_id = static_cast<uint8_t>(bdf.Device),
        .function_id = static_cast<uint8_t>(bdf.Function),
    });
  }

  info.windows =
      std::make_unique<acpi::PciRootWindows>(zx::unowned_resource(get_mmio_resource(parent)),
                                             zx::unowned_resource(get_ioport_resource(parent)));
  info.msi_resource = zx::unowned_resource(get_msi_resource(parent));
  zx_status_t status = scan_acpi_tree_for_resources(manager->acpi(), object, info.windows.get());
  if (status != ZX_OK) {
    zxlogf(ERROR, "Scanning acpi resources for %s failed: %s", info.name,
           zx_status_get_string(status));
    return zx::error(status);
  }

  status = pci_init_segment_and_ecam(parent, manager->acpi(), object, &info);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Initializing %s ecam and bus information failed: %s", info.name,
           zx_status_get_string(status));
    return zx::error(status);
  }

  pci_init_interrupts(parent, manager->acpi(), object, &info);
  return zx::ok(std::move(info));
}

zx_status_t pci_init(zx_device_t* parent, async_dispatcher_t* dispatcher, ACPI_HANDLE object,
                     acpi::UniquePtr<ACPI_DEVICE_INFO> info, acpi::Manager* manager,
                     std::vector<ACPI_PCI_ID> acpi_bdfs) {
  zx::result root_info =
      discover_pci_root(parent, manager, object, std::move(info), std::move(acpi_bdfs));
  if (root_info.is_error()) {
    return root_info.status_value();
  }

  // The name is copied out because |root_info| is moved into the device below.
  char name[ZX_DEVICE_NAME_MAX] = {0};
  memcpy(name, root_info->name, sizeof(root_info->name));

  zx_status_t status = AcpiPciRoot::Create(std::move(*root_info), parent, dispatcher);
  if (status != ZX_OK) {
    zxlogf(ERROR, "failed to add pciroot device for '%s': %d", name, status);
  } else {
    zxlogf(INFO, "published pciroot '%s'", name);
  }

  return status;
}
