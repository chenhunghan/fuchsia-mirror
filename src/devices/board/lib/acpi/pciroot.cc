// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/ddk/debug.h>
#include <lib/pci/constants.h>
#include <lib/pci/pio.h>
#include <lib/stdcompat/span.h>
#include <lib/zx/bti.h>
#include <lib/zx/msi.h>
#include <zircon/status.h>
#include <zircon/syscalls/object.h>

#include <cstdio>

#include <bind/fuchsia/cpp/bind.h>

#include "src/devices/board/lib/acpi/pci-internal.h"

namespace {

bool IsValidBdf(const fuchsia_hardware_pci::wire::Bdf& bdf) {
  return bdf.device_id < pci::kMaxDevicesPerBus && bdf.function_id < pci::kMaxFunctionsPerDevice;
}

// The legacy PIO config mechanism only reaches the standard 256 byte configuration header.
bool PioOffsetInRange(uint16_t offset, size_t width) {
  return offset + width <= pci::kBaseConfigSize;
}

pci_bdf_t ToBanjoBdf(const fuchsia_hardware_pci::wire::Bdf& bdf) {
  return {.bus_id = bdf.bus_id, .device_id = bdf.device_id, .function_id = bdf.function_id};
}

template <typename T, typename ReadFn, typename Completer>
void PioRead(const fuchsia_hardware_pci::wire::Bdf& address, uint16_t offset, ReadFn read_fn,
             fdf::Arena& arena, Completer& completer) {
  if (!IsValidBdf(address)) {
    completer.buffer(arena).ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }
  if (!PioOffsetInRange(offset, sizeof(T))) {
    completer.buffer(arena).ReplyError(ZX_ERR_OUT_OF_RANGE);
    return;
  }
  T value = 0;
  zx_status_t status = read_fn(ToBanjoBdf(address), static_cast<uint8_t>(offset), &value);
  if (status != ZX_OK) {
    completer.buffer(arena).ReplyError(status);
    return;
  }
  completer.buffer(arena).ReplySuccess(value);
}

template <typename T, typename WriteFn, typename Completer>
void PioWrite(const fuchsia_hardware_pci::wire::Bdf& address, uint16_t offset, T value,
              WriteFn write_fn, fdf::Arena& arena, Completer& completer) {
  if (!IsValidBdf(address)) {
    completer.buffer(arena).ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }
  if (!PioOffsetInRange(offset, sizeof(T))) {
    completer.buffer(arena).ReplyError(ZX_ERR_OUT_OF_RANGE);
    return;
  }
  zx_status_t status = write_fn(ToBanjoBdf(address), static_cast<uint8_t>(offset), value);
  if (status != ZX_OK) {
    completer.buffer(arena).ReplyError(status);
    return;
  }
  completer.buffer(arena).ReplySuccess();
}

}  // namespace

zx_status_t AcpiPciRoot::Create(acpi::PciRootInfo info, zx_device_t* parent,
                                async_dispatcher_t* dispatcher) {
  fdf_dispatcher_t* fdf_dispatcher = fdf_dispatcher_downcast_async_dispatcher(dispatcher);
  auto root =
      std::unique_ptr<AcpiPciRoot>(new AcpiPciRoot(std::move(info), parent, fdf_dispatcher));

  zx::result result = root->outgoing_.AddService<fuchsia_hardware_pci_root::RootService>(
      fuchsia_hardware_pci_root::RootService::InstanceHandler({
          .root = root->root_bindings_.CreateHandler(root.get(), dispatcher,
                                                     fidl::kIgnoreBindingClosure),
      }));
  if (result.is_error()) {
    return result.status_value();
  }

  if (root->serves_config()) {
    result = root->outgoing_.AddService<fuchsia_hardware_pci_root::ConfigService>(
        fuchsia_hardware_pci_root::ConfigService::InstanceHandler({
            .config = root->config_bindings_.CreateHandler(root.get(), fdf_dispatcher,
                                                           fidl::kIgnoreBindingClosure),
        }));
    if (result.is_error()) {
      return result.status_value();
    }
  }

  auto [client, server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  if (zx::result serve_result = root->outgoing_.Serve(std::move(server)); serve_result.is_error()) {
    return serve_result.status_value();
  }

  // The bus driver binds on the service property. The compat layer does not derive it from the
  // offers, so it is set explicitly.
  const zx_device_str_prop_t props[] = {
      ddk::MakeStrProperty(bind_fuchsia::SERVICE, fuchsia_hardware_pci_root::RootService::Name),
  };
  const char* fidl_offers[] = {
      fuchsia_hardware_pci_root::RootService::Name,
  };
  const char* runtime_offers[] = {
      fuchsia_hardware_pci_root::ConfigService::Name,
  };
  const size_t runtime_offer_count = root->serves_config() ? 1 : 0;

  zx_status_t status =
      root->DdkAdd(ddk::DeviceAddArgs(root->info_.name)
                       .set_str_props(props)
                       .set_fidl_service_offers(fidl_offers)
                       .set_runtime_service_offers(cpp20::span(runtime_offers, runtime_offer_count))
                       .set_outgoing_dir(client.TakeChannel()));
  if (status != ZX_OK) {
    return status;
  }

  // The DDK now owns the device.
  [[maybe_unused]] auto* unused = root.release();
  return ZX_OK;
}

void AcpiPciRoot::GetBti(GetBtiRequestView request, GetBtiCompleter::Sync& completer) {
  const fuchsia_hardware_pci::wire::Bdf& bdf = request->bdf;
  if (!IsValidBdf(bdf)) {
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }

  // x86 uses PCI BDFs as hardware identifiers, and ARM uses PCI root complexes. There will be at
  // most one BTI per device.
  if (request->index != 0) {
    completer.ReplyError(ZX_ERR_OUT_OF_RANGE);
    return;
  }

  const uint32_t packed_bdf = (static_cast<uint32_t>(bdf.bus_id) << 8u) |
                              (static_cast<uint32_t>(bdf.device_id) << 3u) |
                              static_cast<uint32_t>(bdf.function_id);
  zx::bti bti;
  zx_status_t status = zx::bti::create(info_.iommu, 0, packed_bdf, &bti);
  if (status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }

  char name[ZX_MAX_NAME_LEN]{};
  snprintf(name, std::size(name) - 1, "acpi bti %02x:%02x.%1x", bdf.bus_id, bdf.device_id,
           bdf.function_id);
  if (zx_status_t name_status = bti.set_property(ZX_PROP_NAME, name, std::size(name));
      name_status != ZX_OK) {
    zxlogf(WARNING, "Couldn't set name for BTI '%s': %s", name, zx_status_get_string(name_status));
  }

  completer.ReplySuccess(std::move(bti));
}

void AcpiPciRoot::GetPlatformInfo(GetPlatformInfoCompleter::Sync& completer) {
  fidl::Arena arena;
  fidl::WireOptional<fuchsia_hardware_pci_root::wire::ParentBdfs> parent_bdfs;
  if (!info_.acpi_bdfs.empty()) {
    parent_bdfs = fuchsia_hardware_pci_root::wire::ParentBdfs::WithAcpi(
        arena, fidl::VectorView<fuchsia_hardware_pci::wire::Bdf>::FromExternal(info_.acpi_bdfs));
  }

  completer.ReplySuccess({
      .name = fidl::StringView::FromExternal(info_.name),
      .start_bus_num = info_.start_bus_num,
      .end_bus_num = info_.end_bus_num,
      .segment_group = info_.segment_group,
      .irq_routing =
          fidl::VectorView<fuchsia_hardware_pci_root::wire::IrqRoutingEntry>::FromExternal(
              info_.routing),
      .parent_bdfs = parent_bdfs,
  });
}

void AcpiPciRoot::GetCam(GetCamCompleter::Sync& completer) {
  if (!info_.ecam) {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
    return;
  }

  zx::vmo vmo;
  if (zx_status_t status = info_.ecam->duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo); status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  completer.ReplySuccess(std::move(vmo), /*is_extended=*/true);
}

void AcpiPciRoot::GetLegacyInterrupt(GetLegacyInterruptRequestView request,
                                     GetLegacyInterruptCompleter::Sync& completer) {
  auto entry = info_.legacy_irqs.find(request->vector);
  if (entry == info_.legacy_irqs.end()) {
    completer.ReplyError(ZX_ERR_NOT_FOUND);
    return;
  }

  zx::interrupt interrupt;
  if (zx_status_t status = entry->second.duplicate(ZX_RIGHT_SAME_RIGHTS, &interrupt);
      status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  completer.ReplySuccess(std::move(interrupt));
}

void AcpiPciRoot::GetAddressSpace(GetAddressSpaceRequestView request,
                                  GetAddressSpaceCompleter::Sync& completer) {
  const acpi::PciRootWindows::Space space =
      (request->type == fuchsia_hardware_pci_root::wire::AddressSpace::kIo)
          ? acpi::PciRootWindows::Space::kIo
          : acpi::PciRootWindows::Space::kMemory;
  zx::result allocation =
      info_.windows->Allocate(space, request->in_base, request->size, request->low);
  if (allocation.is_error()) {
    completer.ReplyError(allocation.status_value());
    return;
  }
  completer.ReplySuccess(allocation->base, std::move(allocation->resource),
                         std::move(allocation->token));
}

void AcpiPciRoot::AllocateMsi(AllocateMsiRequestView request,
                              AllocateMsiCompleter::Sync& completer) {
  zx::msi msi;
  zx_status_t status = zx::msi::allocate(*info_.msi_resource, request->msi_count, &msi);
  if (status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  zx_info_msi_t msi_info;
  status = msi.get_info(ZX_INFO_MSI, &msi_info, sizeof(msi_info), nullptr, nullptr);
  if (status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  completer.ReplySuccess(zx::handle(msi.release()),
                         fuchsia_hardware_pci_root::wire::MsiAllocationInfo{
                             .target_addr = msi_info.target_addr,
                             .target_data = msi_info.target_data,
                             .irq_count = msi_info.num_irq,
                         });
}

void AcpiPciRoot::GetMsiHandle(GetMsiHandleRequestView request,
                               GetMsiHandleCompleter::Sync& completer) {
  zx::interrupt interrupt;
  zx_status_t status =
      zx::msi::create(zx::msi(request->allocation.release()), request->options, request->msi_id,
                      request->cfg_vmo, request->cfg_offset, &interrupt);
  if (status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  completer.ReplySuccess(std::move(interrupt));
}

void AcpiPciRoot::Read8(Read8RequestView request, fdf::Arena& arena,
                        Read8Completer::Sync& completer) {
  PioRead<uint8_t>(request->address, request->offset, pci_pio_read8, arena, completer);
}

void AcpiPciRoot::Read16(Read16RequestView request, fdf::Arena& arena,
                         Read16Completer::Sync& completer) {
  PioRead<uint16_t>(request->address, request->offset, pci_pio_read16, arena, completer);
}

void AcpiPciRoot::Read32(Read32RequestView request, fdf::Arena& arena,
                         Read32Completer::Sync& completer) {
  PioRead<uint32_t>(request->address, request->offset, pci_pio_read32, arena, completer);
}

void AcpiPciRoot::Write8(Write8RequestView request, fdf::Arena& arena,
                         Write8Completer::Sync& completer) {
  PioWrite<uint8_t>(request->address, request->offset, request->value, pci_pio_write8, arena,
                    completer);
}

void AcpiPciRoot::Write16(Write16RequestView request, fdf::Arena& arena,
                          Write16Completer::Sync& completer) {
  PioWrite<uint16_t>(request->address, request->offset, request->value, pci_pio_write16, arena,
                     completer);
}

void AcpiPciRoot::Write32(Write32RequestView request, fdf::Arena& arena,
                          Write32Completer::Sync& completer) {
  PioWrite<uint32_t>(request->address, request->offset, request->value, pci_pio_write32, arena,
                     completer);
}
