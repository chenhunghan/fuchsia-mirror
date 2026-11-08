// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "fake_bus_driver.h"

#include <fidl/fuchsia.kernel/cpp/wire.h>
#include <lib/driver/component/cpp/driver_export2.h>

#include "src/devices/pci/drivers/pci/device.h"
#include "src/devices/pci/drivers/pci/test/driver/driver_tests.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_config.h"
#include "src/devices/pci/drivers/pci/test/fakes/test_device.h"
#include "src/devices/pci/lib/cpp/config.h"

namespace pci {

zx::result<> FakeBusDriver::Start(fdf::DriverContext context) {
  bus_.SetDriverContext(outgoing().get(), dispatcher());

  auto ioport_client_end = context.incoming().Connect<fuchsia_kernel::IoportResource>();
  if (ioport_client_end.is_ok()) {
    auto ioport_res = fidl::WireCall(ioport_client_end.value())->Get();
    if (ioport_res.ok() && ioport_res->resource.is_valid()) {
      upstream_.fake_pio_regions().SetResource(std::move(ioport_res->resource));
    }
  }

  zx_status_t st =
      CreateDevice(test_bdf(), kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  if (st != ZX_OK) {
    return zx::error(st);
  }

  upstream_.ConfigureDownstreamDevices();
  return zx::ok();
}

void FakeBusDriver::Stop(fdf::StopCompleter completer) {
  Teardown();
  completer(zx::ok());
}

void FakeBusDriver::Teardown() {
  upstream_.DisableDownstream();
  upstream_.UnplugDownstream();
}

// Creates a device, seeding the configuration space with a given buffer if provided.
zx_status_t FakeBusDriver::CreateDevice(fuchsia_hardware_pci::wire::Bdf bdf, uint8_t* base_cfg,
                                        size_t base_cfg_size, uint16_t vid, uint16_t did) {
  fdf::MmioView view = bus_.ecam().mmio().View(
      GetConfigOffsetInCam(bdf, 0, /*is_extended=*/bus_.is_extended()), zx_system_get_page_size());
  for (uint32_t off = 0; off < base_cfg_size; off++) {
    view.Write(base_cfg[off], off);
  }

  std::unique_ptr<Config> cfg = std::make_unique<FakeMmioConfig>(bdf, std::move(view));
  config::VendorId.FromValue(vid).WriteTo(cfg.get());
  config::DeviceId.FromValue(did).WriteTo(cfg.get());
  return pci::Device::Create(std::move(cfg), &upstream(), bus().bdi(),
                             /*has_acpi=*/false, /*has_devicetree=*/false);
}

}  // namespace pci

FUCHSIA_DRIVER_EXPORT2(pci::FakeBusDriver);
