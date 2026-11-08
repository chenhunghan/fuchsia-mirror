// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CAMERA_BIN_DEVICE_WATCHER_DEVICE_WATCHER_IMPL_H_
#define SRC_CAMERA_BIN_DEVICE_WATCHER_DEVICE_WATCHER_IMPL_H_

#include <fidl/fuchsia.camera2.hal/cpp/fidl.h>
#include <fidl/fuchsia.camera3/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.hardware.camera/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/fidl/cpp/binding.h>
#include <lib/fit/function.h>
#include <lib/fpromise/result.h>
#include <lib/zx/time.h>
#include <zircon/status.h>

#include <memory>
#include <queue>
#include <set>
#include <string>
#include <unordered_map>

#include "lib/async/dispatcher.h"
#include "src/camera/bin/device_watcher/device_instance.h"
#include "src/lib/backoff/backoff.h"

namespace camera {

constexpr auto kCameraPath = "/svc/fuchsia.hardware.camera.Service";

constexpr std::string_view kMipiCsiDeviceInstanceCollectionName{"csi_camera_devices"};
constexpr std::string_view kMipiCsiDeviceInstanceNamePrefix{"csi_camera_device_"};
constexpr std::string_view kMipiCsiDeviceInstanceUrl{
    "fuchsia-pkg://fuchsia.com/camera_device#meta/camera_device.cm"};

constexpr std::string_view kUvcDeviceInstanceCollectionName{"usb_camera_devices"};
constexpr std::string_view kUvcDeviceInstanceNamePrefix{"usb_camera_device_"};
constexpr std::string_view kUvcDeviceInstanceUrl{
    "fuchsia-pkg://fuchsia.com/usb_camera_device#meta/usb_camera_device.cm"};

using ClientId = uint64_t;
using TransientDeviceId = uint64_t;
using PersistentDeviceId = uint64_t;

enum CameraType {
  kCameraTypeMipiCsi = 1,
  kCameraTypeUvc = 2,
};

struct UniqueDevice {
  TransientDeviceId id = 0;
  // Null while the device waits to be relaunched. Clients do not see the device then.
  std::unique_ptr<DeviceInstance> instance;
  // What is needed to launch the device again after its instance exits.
  CameraType type = kCameraTypeMipiCsi;
  std::string path;
  uint32_t launch_count = 0;
  uint32_t relaunch_attempts = 0;
  // Delays between relaunch attempts.
  std::unique_ptr<backoff::Backoff> relaunch_backoff;
  zx::time launch_time;
  std::unique_ptr<async::TaskClosure> relaunch_task;
};

using DevicesMap = std::unordered_map<PersistentDeviceId, UniqueDevice>;

class DeviceWatcherImpl {
 public:
  // Connects to the camera device at |full_path|. Tests replace the default, which connects
  // through the component's namespace.
  using CameraConnector =
      fit::function<fpromise::result<fidl::ClientEnd<fuchsia_hardware_camera::Device>, zx_status_t>(
          const std::string& full_path)>;

  static fpromise::result<std::unique_ptr<DeviceWatcherImpl>, zx_status_t> Create(
      fidl::ClientEnd<fuchsia_component::Realm> realm, async_dispatcher_t* dispatcher,
      CameraConnector camera_connector = nullptr);

  fpromise::result<CameraType, zx_status_t> GetDeviceInfoAndIdentifyCameraType(
      fidl::SyncClient<fuchsia_hardware_camera::Device>& dev,
      fuchsia_camera2::DeviceInfo& device_info, const std::string& full_path);
  void AddDeviceByPath(const std::string& path);
  void UpdateClients();
  fidl::ProtocolHandler<fuchsia_camera3::DeviceWatcher> GetHandler();

  void OnNewRequest(fidl::ServerEnd<fuchsia_camera3::DeviceWatcher> request);

 private:
  void ConnectDynamicChild(fidl::ServerEnd<fuchsia_camera3::Device> request,
                           const UniqueDevice& unique_device);

  fpromise::result<PersistentDeviceId, zx_status_t> AddMipiCsiDevice(
      fidl::ClientEnd<fuchsia_hardware_camera::Device> camera,
      fuchsia_camera2::DeviceInfo& device_info, const std::string& path);

  fpromise::result<PersistentDeviceId, zx_status_t> AddUvcDevice(
      fidl::ClientEnd<fuchsia_hardware_camera::Device> camera,
      fuchsia_camera2::DeviceInfo& device_info, const std::string& path);

  // Launches a child for the device in |devices_| and gives the device a new transient ID.
  fpromise::result<PersistentDeviceId, zx_status_t> LaunchDevice(
      PersistentDeviceId persistent_id, fidl::ClientEnd<fuchsia_hardware_camera::Device> camera);
  // Hides the device from clients and schedules a relaunch. Ignored if the device has been
  // launched again since |launch_count|.
  void OnDeviceExited(PersistentDeviceId persistent_id, uint32_t launch_count);
  void ScheduleRelaunch(PersistentDeviceId persistent_id);
  void RelaunchDevice(PersistentDeviceId persistent_id);
  void NotifyClients();

  // Implements the server endpoint for a single client, and maintains per-client state.
  class Client : public fidl::Server<fuchsia_camera3::DeviceWatcher> {
   public:
    explicit Client(DeviceWatcherImpl& watcher);
    static fpromise::result<std::unique_ptr<Client>, zx_status_t> Create(
        DeviceWatcherImpl& watcher, ClientId id,
        fidl::ServerEnd<fuchsia_camera3::DeviceWatcher> request, async_dispatcher_t* dispatcher);
    void UpdateDevices(const DevicesMap& devices);
    explicit operator bool();

   private:
    void CheckDevicesChanged();
    // |fuchsia_camera3::DeviceWatcher|
    void WatchDevices(WatchDevicesCompleter::Sync& completer) override;
    void ConnectToDevice(ConnectToDeviceRequest& request,
                         ConnectToDeviceCompleter::Sync& completer) override;

    DeviceWatcherImpl& watcher_;
    ClientId id_;
    std::optional<fidl::ServerBinding<fuchsia_camera3::DeviceWatcher>> binding_;
    std::optional<WatchDevicesCompleter::Async> completer_;
    std::set<TransientDeviceId> last_known_ids_;
    std::optional<std::set<TransientDeviceId>> last_sent_ids_;
  };

  async_dispatcher_t* dispatcher_;

  fidl::Client<fuchsia_component::Realm> realm_;
  CameraConnector camera_connector_;
  TransientDeviceId device_id_next_ = 1;
  DevicesMap devices_;
  ClientId client_id_next_ = 1;
  std::unordered_map<ClientId, std::unique_ptr<Client>> clients_;
  bool initial_update_received_ = false;
  std::queue<fidl::ServerEnd<fuchsia_camera3::DeviceWatcher>> requests_;
};

}  // namespace camera

#endif  // SRC_CAMERA_BIN_DEVICE_WATCHER_DEVICE_WATCHER_IMPL_H_
