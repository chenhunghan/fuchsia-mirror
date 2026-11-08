// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CAMERA_BIN_DEVICE_WATCHER_DEVICE_INSTANCE_H_
#define SRC_CAMERA_BIN_DEVICE_WATCHER_DEVICE_INSTANCE_H_

#include <fidl/fuchsia.camera2.hal/cpp/fidl.h>
#include <fidl/fuchsia.camera3/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.hardware.camera/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/wait.h>
#include <lib/fit/function.h>
#include <lib/fpromise/result.h>
#include <zircon/status.h>

#include <memory>

#include "src/lib/fxl/memory/weak_ptr.h"

namespace camera {

// Represents a launched device process.
class DeviceInstance {
 public:
  // Called at most once, when the child is gone: it could not be created, or component manager
  // destroyed it. A `single_run` child is destroyed as soon as it stops, including when it
  // crashes. Never called after the instance is destroyed.
  using ExitCallback = fit::callback<void()>;

  // |realm| must outlive the returned DeviceInstance.
  static fpromise::result<std::unique_ptr<DeviceInstance>, zx_status_t> Create(
      fidl::ClientEnd<fuchsia_hardware_camera::Device> camera,
      const fidl::Client<fuchsia_component::Realm>& realm, async_dispatcher_t* dispatcher,
      const std::string& collection_name, const std::string& child_name, const std::string& url,
      ExitCallback on_exit);
  const std::string& name() { return name_; }
  const std::string& collection_name() { return collection_name_; }

 private:
  void WatchForExit(fidl::ClientEnd<fuchsia_io::Directory> exposed_dir);
  void OnExit();

  async_dispatcher_t* dispatcher_;
  std::string name_;
  std::string collection_name_;
  fidl::Client<fuchsia_hardware_camera::Device> camera_;
  ExitCallback on_exit_;
  // Held only to learn when the child is destroyed: component manager closes it then.
  fidl::ClientEnd<fuchsia_io::Directory> exposed_dir_;
  async::WaitOnce exposed_dir_closed_{ZX_HANDLE_INVALID, ZX_CHANNEL_PEER_CLOSED};
  // Lets replies from the realm tell whether this instance still exists. Keep it last.
  fxl::WeakPtrFactory<DeviceInstance> weak_factory_{this};
};

}  // namespace camera

#endif  // SRC_CAMERA_BIN_DEVICE_WATCHER_DEVICE_INSTANCE_H_
