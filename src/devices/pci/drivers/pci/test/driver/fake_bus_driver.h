// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_TEST_DRIVER_FAKE_BUS_DRIVER_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_TEST_DRIVER_FAKE_BUS_DRIVER_H_

#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <lib/driver/component/cpp/driver_base2.h>

#include "src/devices/pci/drivers/pci/test/driver/driver_tests.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_bus.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_upstream_node.h"

namespace pci {

// Fake bus driver loaded in DriverTestRealm by pci-driver-test.
// Configures a fake Quadro device and serves fuchsia.hardware.pci.Service
// for driver_tests.rs.
class FakeBusDriver : public fdf::DriverBase2 {
 public:
  FakeBusDriver()
      : fdf::DriverBase2("fake-bus-pci"),
        bus_(/*bus_start=*/0, /*bus_end=*/1, /*is_extended=*/false),
        upstream_(UpstreamNode::Type::ROOT, 0) {}

  ~FakeBusDriver() override { Teardown(); }

  zx::result<> Start(fdf::DriverContext context) override;
  void Stop(fdf::StopCompleter completer) override;

  zx_status_t CreateDevice(fuchsia_hardware_pci::wire::Bdf bdf, uint8_t* base_cfg,
                           size_t base_cfg_size, uint16_t vid = PCI_TEST_DRIVER_VID,
                           uint16_t did = PCI_TEST_DRIVER_DID);

  FakePciType0Config* GetDevice(fuchsia_hardware_pci::wire::Bdf bdf) {
    return bus().ecam().get_device(bdf);
  }
  FakePciType1Config* GetBridge(fuchsia_hardware_pci::wire::Bdf bdf) {
    return bus().ecam().get_bridge(bdf);
  }

  FakeUpstreamNode& upstream() { return upstream_; }
  FakeBus& bus() { return bus_; }
  fuchsia_hardware_pci::wire::Bdf test_bdf() { return test_bdf_; }

 private:
  // Disables and unplugs every downstream device. Safe to call more than once.
  void Teardown();

  // Devices downstream of upstream_ are linked into bus_, so bus_ must outlive upstream_.
  FakeBus bus_;
  FakeUpstreamNode upstream_;
  const fuchsia_hardware_pci::wire::Bdf test_bdf_ = {
      .bus_id = PCI_TEST_BUS_ID,
      .device_id = PCI_TEST_DEV_ID,
      .function_id = PCI_TEST_FUNC_ID,
  };
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_TEST_DRIVER_FAKE_BUS_DRIVER_H_
