// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/node_properties.h>
#include <lib/driver/logging/cpp/logger.h>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/usb/cpp/bind.h>

#include "src/devices/usb/testing/usb-function-fidl-test/fake-usb-peripheral/fake_usb_peripheral_config.h"

namespace fake_usb_peripheral {

class FakeUsbPeripheral : public fdf::DriverBase2 {
 public:
  FakeUsbPeripheral() : fdf::DriverBase2("fake_usb_peripheral") {}

  zx::result<> Start(fdf::DriverContext context) override {
    auto config = context.take_config<fake_usb_peripheral_config::Config>();

    // Create properties for the child node dynamically from config
    std::vector<fuchsia_driver_framework::NodeProperty2> properties;
    properties.push_back(
        fdf::MakeProperty2(bind_fuchsia::PROTOCOL, bind_fuchsia_usb::BIND_PROTOCOL_FUNCTION));
    properties.push_back(
        fdf::MakeProperty2(bind_fuchsia::USB_CLASS, static_cast<uint32_t>(config.usb_class())));

    if (config.usb_subclass() >= 0) {
      properties.push_back(fdf::MakeProperty2(bind_fuchsia::USB_SUBCLASS,
                                              static_cast<uint32_t>(config.usb_subclass())));
    }
    if (config.usb_protocol() >= 0) {
      properties.push_back(fdf::MakeProperty2(bind_fuchsia::USB_PROTOCOL,
                                              static_cast<uint32_t>(config.usb_protocol())));
    }
    if (config.usb_vid() > 0) {
      properties.push_back(
          fdf::MakeProperty2(bind_fuchsia::USB_VID, static_cast<uint32_t>(config.usb_vid())));
    }
    if (config.usb_pid() > 0) {
      properties.push_back(
          fdf::MakeProperty2(bind_fuchsia::USB_PID, static_cast<uint32_t>(config.usb_pid())));
    }

    zx::result child = AddChild("usb-function-device", properties, {});
    if (child.is_error()) {
      fdf::error("Failed to add child: {}", child.status_string());
      return child.take_error();
    }
    child_controller_ = std::move(*child);
    fdf::info("Started successfully and added child 'usb-function-device'");
    return zx::ok();
  }

 private:
  fidl::ClientEnd<fuchsia_driver_framework::NodeController> child_controller_;
};

}  // namespace fake_usb_peripheral

FUCHSIA_DRIVER_EXPORT2(fake_usb_peripheral::FakeUsbPeripheral);
