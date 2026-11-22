// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "network_device.h"

#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fdf/cpp/env.h>

#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>

namespace network {
namespace {

constexpr char kDriverName[] = "network-device";

}  // namespace

NetworkDevice::NetworkDevice() : fdf::DriverBase2(kDriverName) {}

NetworkDevice::~NetworkDevice() {
  if (dispatchers_) {
    dispatchers_->ShutdownSync();
  }
}

void NetworkDevice::Start(fdf::DriverContext context, fdf::StartCompleter completer) {
  auto incoming = std::shared_ptr<fdf::Namespace>(context.take_incoming());
  node_token_ = context.take_node_token();

  zx::result<> result = [&]() -> zx::result<> {
    zx::result dispatchers = OwnedDeviceInterfaceDispatchers::Create();
    if (dispatchers.is_error()) {
      fdf::error("failed to create owned dispatchers: {}", dispatchers);
      return dispatchers.take_error();
    }
    dispatchers_ = std::move(dispatchers.value());

    zx::result<std::unique_ptr<NetworkDeviceImplBinder>> binder = CreateImplBinder(incoming);
    if (binder.is_error()) {
      fdf::error("failed to create network device binder: {}", binder);
      return binder.take_error();
    }

    zx::result device =
        NetworkDeviceInterface::Create(dispatchers_->Unowned(), std::move(binder.value()));
    if (device.is_error()) {
      fdf::error("failed to create inner device {}", device);
      return device.take_error();
    }
    device_ = std::move(device.value());

    if (zx::result result = outgoing()->AddService<fuchsia_hardware_network::Service>(
            fuchsia_hardware_network::Service::InstanceHandler({
                .device = fit::bind_member<&NetworkDevice::Connect>(this),
                .token =
                    token_bindings_.CreateHandler(this, dispatcher(), fidl::kIgnoreBindingClosure),
            }));
        result.is_error()) {
      fdf::error("failed to add network service to outgoing directory: {}", result);
      return result.take_error();
    }

    return zx::ok();
  }();

  if (result.is_error() && device_) {
    // Start failed but got to the point where the device was created. We must tear it down.
    // PrepareStop will not be called if Start fails.
    device_->Teardown([result, completer = std::move(completer)]() mutable { completer(result); });
  } else {
    completer(result);
  }
}

void NetworkDevice::Stop(fdf::StopCompleter completer) {
  fdf::info("{:p} Stop", static_cast<const void*>(this));
  token_bindings_.RemoveAll();
  device_->Teardown([completer = std::move(completer), this]() mutable {
    fdf::info("{:p} Stop Done", static_cast<const void*>(this));
    completer(zx::ok());
  });
}

void NetworkDevice::Get(GetCompleter::Sync& completer) {
  if (!node_token_.is_valid()) {
    completer.ReplyError(ZX_ERR_NOT_FOUND);
    return;
  }
  zx::event copy;
  if (zx_status_t status = node_token_.duplicate(ZX_RIGHT_SAME_RIGHTS, &copy); status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  completer.ReplySuccess(std::move(copy));
}

void NetworkDevice::Connect(fidl::ServerEnd<fuchsia_hardware_network::Device> request) {
  ZX_ASSERT_MSG(device_, "can't serve device if not bound to parent implementation");
  device_->Bind(std::move(request));
}

zx::result<std::unique_ptr<NetworkDeviceImplBinder>> NetworkDevice::CreateImplBinder(
    const std::shared_ptr<fdf::Namespace>& incoming) {
  fbl::AllocChecker ac;

  std::unique_ptr fidl = fbl::make_unique_checked<FidlNetworkDeviceImplBinder>(&ac, incoming);
  if (!ac.check()) {
    fdf::error("no memory");
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  return zx::ok(std::move(fidl));
}

zx::result<fdf::ClientEnd<fuchsia_hardware_network_driver::NetworkDeviceImpl>>
FidlNetworkDeviceImplBinder::Bind() {
  if (!incoming_) {
    return zx::error(ZX_ERR_UNAVAILABLE);
  }
  zx::result client_end =
      incoming_->Connect<fuchsia_hardware_network_driver::Service::NetworkDeviceImpl>();
  if (client_end.is_error()) {
    fdf::error("failed to connect to parent device: {}", client_end);
    return client_end;
  }
  return client_end;
}

}  // namespace network

FUCHSIA_DRIVER_EXPORT2(::network::NetworkDevice);
