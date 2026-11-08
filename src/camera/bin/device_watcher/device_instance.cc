// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/camera/bin/device_watcher/device_instance.h"

#include <fidl/fuchsia.camera2.hal/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fidl/fuchsia.process/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/errors.h>
#include <zircon/processargs.h>

#include "lib/fit/function.h"

namespace camera {

fpromise::result<std::unique_ptr<DeviceInstance>, zx_status_t> DeviceInstance::Create(
    fidl::ClientEnd<fuchsia_hardware_camera::Device> camera,
    const fidl::Client<fuchsia_component::Realm>& realm, async_dispatcher_t* dispatcher,
    const std::string& collection_name, const std::string& child_name, const std::string& url,
    ExitCallback on_exit) {
  auto instance = std::make_unique<DeviceInstance>();
  instance->dispatcher_ = dispatcher;
  instance->name_ = child_name;
  instance->collection_name_ = collection_name;
  instance->on_exit_ = std::move(on_exit);

  // Launch the child device.
  fuchsia_component_decl::CollectionRef collection{{.name = collection_name}};
  fuchsia_component_decl::Child child;
  child.name(child_name);
  child.url(url);
  child.startup(fuchsia_component_decl::StartupMode::kLazy);

  fuchsia_component::CreateChildArgs args;

  // Pass the camera DeviceHandle to the child so the child can communicate with the correct
  // instance.
  fuchsia_process::HandleInfo handle_info{{
      .handle = zx::handle(camera.TakeChannel().release()),
      .id = PA_HND(PA_USER0, 0),
  }};
  std::vector<fuchsia_process::HandleInfo> numbered_handles;
  numbered_handles.push_back(std::move(handle_info));
  args.numbered_handles(std::move(numbered_handles));

  realm->CreateChild({{.collection = collection, .decl = child, .args = std::move(args)}})
      .Then([&realm, weak = instance->weak_factory_.GetWeakPtr()](
                fidl::Result<fuchsia_component::Realm::CreateChild>& result) {
        if (!weak) {
          return;
        }
        if (result.is_error()) {
          FX_LOGS(ERROR) << "Failed to create camera device child " << weak->name_ << ": "
                         << result.error_value();
          weak->OnExit();
          return;
        }
        FX_LOGS(INFO) << "Created camera device child: " << weak->name_;

        // Component manager closes the child's exposed directory when it destroys the child, so
        // holding the directory tells us when the child exits or crashes.
        auto endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
        fuchsia_component_decl::ChildRef child_ref{{
            .name = weak->name_,
            .collection = weak->collection_name_,
        }};
        realm->OpenExposedDir({child_ref, std::move(endpoints.server)})
            .Then([weak, exposed_dir = std::move(endpoints.client)](
                      fidl::Result<fuchsia_component::Realm::OpenExposedDir>& result) mutable {
              if (!weak) {
                return;
              }
              if (result.is_error()) {
                FX_LOGS(WARNING) << "Failed to open exposed directory for " << weak->name_ << ": "
                                 << result.error_value();
                weak->OnExit();
                return;
              }
              weak->WatchForExit(std::move(exposed_dir));
            });
      });

  return fpromise::ok(std::move(instance));
}

void DeviceInstance::WatchForExit(fidl::ClientEnd<fuchsia_io::Directory> exposed_dir) {
  exposed_dir_ = std::move(exposed_dir);
  exposed_dir_closed_.set_object(exposed_dir_.channel().get());
  zx_status_t status = exposed_dir_closed_.Begin(
      dispatcher_, [this](async_dispatcher_t* dispatcher, async::WaitOnce* wait, zx_status_t status,
                          const zx_packet_signal_t* signal) {
        if (status == ZX_ERR_CANCELED) {
          return;
        }
        FX_LOGS(WARNING) << "Camera device child " << name_ << " exited.";
        OnExit();
      });
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Failed to watch camera device child " << name_;
    OnExit();
  }
}

void DeviceInstance::OnExit() {
  // The callback may destroy this instance, so take it out first.
  ExitCallback on_exit = std::move(on_exit_);
  if (on_exit) {
    on_exit();
  }
}

}  // namespace camera
