// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/camera/bin/device_watcher/device_watcher_impl.h"

#include <fidl/fuchsia.camera2.hal/cpp/fidl.h>
#include <fidl/fuchsia.camera3/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/test_base.h>
#include <fidl/fuchsia.hardware.camera/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>

#include <optional>
#include <string>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"

namespace camera {
namespace {

using Tag = fuchsia_camera3::WatchDevicesEvent::Tag;

constexpr uint16_t kFakeVendorId = 0xFFFF;
constexpr uint16_t kFakeProductId = 0xABCD;
constexpr char kDevicePath[] = "000";
constexpr char kChildName[] = "csi_camera_device_000";

// Answers GetDeviceInfo, which is how the watcher recognizes a MIPI/CSI camera.
class FakeController : public fidl::Server<fuchsia_camera2_hal::Controller> {
 public:
  void GetNextConfig(GetNextConfigCompleter::Sync& completer) override {}
  void CreateStream(CreateStreamRequest& request, CreateStreamCompleter::Sync& completer) override {
  }
  void EnableStreaming(EnableStreamingCompleter::Sync& completer) override {}
  void DisableStreaming(DisableStreamingCompleter::Sync& completer) override {}
  void GetDeviceInfo(GetDeviceInfoCompleter::Sync& completer) override {
    fuchsia_camera2::DeviceInfo info;
    info.vendor_id(kFakeVendorId);
    info.product_id(kFakeProductId);
    completer.Reply({{.info = std::move(info)}});
  }
};

class FakeDevice : public fidl::Server<fuchsia_hardware_camera::Device> {
 public:
  explicit FakeDevice(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  void GetChannel(GetChannelRequest& request, GetChannelCompleter::Sync& completer) override {}
  void GetChannel2(GetChannel2Request& request, GetChannel2Completer::Sync& completer) override {
    fidl::BindServer(dispatcher_, std::move(request.server_end()),
                     std::make_unique<FakeController>());
  }
  void GetDebugChannel(GetDebugChannelRequest& request,
                       GetDebugChannelCompleter::Sync& completer) override {}

 private:
  async_dispatcher_t* dispatcher_;
};

// Records the children the watcher creates and holds their exposed directories. Closing a
// directory is what component manager does when it destroys the child.
class FakeRealm : public fidl::testing::TestBase<fuchsia_component::Realm> {
 public:
  void CreateChild(CreateChildRequest& request, CreateChildCompleter::Sync& completer) override {
    created_children_.push_back(request.decl().name().value_or(""));
    if (create_child_error_.has_value()) {
      completer.Reply(fit::error(*create_child_error_));
      return;
    }
    completer.Reply(fit::ok());
  }

  void OpenExposedDir(OpenExposedDirRequest& request,
                      OpenExposedDirCompleter::Sync& completer) override {
    if (open_exposed_dir_error_.has_value()) {
      completer.Reply(fit::error(*open_exposed_dir_error_));
      return;
    }
    exposed_dirs_.push_back(std::move(request.exposed_dir()));
    completer.Reply(fit::ok());
  }

  // Simulates component manager destroying the children, as it does when a `single_run` child
  // stops or crashes.
  void DestroyAllChildren() { exposed_dirs_.clear(); }

  const std::vector<std::string>& created_children() const { return created_children_; }
  void set_create_child_error(std::optional<fuchsia_component::Error> error) {
    create_child_error_ = error;
  }
  void set_open_exposed_dir_error(fuchsia_component::Error error) {
    open_exposed_dir_error_ = error;
  }

 private:
  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE() << "Unexpected call to Realm." << name;
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  std::vector<std::string> created_children_;
  std::vector<fidl::ServerEnd<fuchsia_io::Directory>> exposed_dirs_;
  std::optional<fuchsia_component::Error> create_child_error_;
  std::optional<fuchsia_component::Error> open_exposed_dir_error_;
};

class DeviceEventHandler : public fidl::AsyncEventHandler<fuchsia_camera3::Device> {
 public:
  void on_fidl_error(fidl::UnbindInfo info) override { error = info; }

  std::optional<fidl::UnbindInfo> error;
};

class DeviceWatcherImplTest : public gtest::TestLoopFixture {
 protected:
  // Whether the camera is there when the watcher's first scan completes. If not, tests add it.
  virtual bool CameraPresentAtFirstScan() const { return true; }

  void SetUp() override {
    ASSERT_EQ(camera_loop_.StartThread("fake-camera"), ZX_OK);

    auto realm = fidl::Endpoints<fuchsia_component::Realm>::Create();
    realm_binding_.emplace(dispatcher(), std::move(realm.server), &realm_,
                           fidl::kIgnoreBindingClosure);

    // The watcher talks to the camera synchronously, so the fake camera runs on its own thread.
    auto watcher = DeviceWatcherImpl::Create(
        std::move(realm.client), dispatcher(),
        [this](const std::string& full_path)
            -> fpromise::result<fidl::ClientEnd<fuchsia_hardware_camera::Device>, zx_status_t> {
          auto endpoints = fidl::Endpoints<fuchsia_hardware_camera::Device>::Create();
          fidl::BindServer(camera_loop_.dispatcher(), std::move(endpoints.server),
                           std::make_unique<FakeDevice>(camera_loop_.dispatcher()));
          return fpromise::ok(std::move(endpoints.client));
        });
    ASSERT_TRUE(watcher.is_ok());
    watcher_ = watcher.take_value();
    if (CameraPresentAtFirstScan()) {
      watcher_->AddDeviceByPath(kDevicePath);
    }
    watcher_->UpdateClients();

    auto client = fidl::Endpoints<fuchsia_camera3::DeviceWatcher>::Create();
    watcher_->OnNewRequest(std::move(client.server));
    client_.Bind(std::move(client.client), dispatcher());
    RunLoopUntilIdle();
  }

  void TearDown() override {
    client_ = {};
    watcher_.reset();
    realm_binding_.reset();
    RunLoopUntilIdle();
    camera_loop_.Shutdown();
  }

  // Starts a WatchDevices call. The reply is stored in |events_|.
  void Watch() {
    ASSERT_FALSE(watch_pending_);
    watch_pending_ = true;
    client_->WatchDevices().Then(
        [this](fidl::Result<fuchsia_camera3::DeviceWatcher::WatchDevices>& result) {
          watch_pending_ = false;
          ASSERT_TRUE(result.is_ok()) << result.error_value();
          events_ = std::move(result.value().events());
        });
  }

  // Returns the ID of the only device, from the first WatchDevices reply.
  uint64_t WatchFirstDevice() {
    Watch();
    RunLoopUntilIdle();
    EXPECT_FALSE(watch_pending_);
    EXPECT_EQ(events_.size(), 1u);
    if (events_.size() != 1u || events_[0].Which() != Tag::kAdded) {
      ADD_FAILURE() << "Expected one added device";
      return 0;
    }
    return events_[0].added().value();
  }

  async::Loop camera_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  FakeRealm realm_;
  std::optional<fidl::ServerBinding<fuchsia_component::Realm>> realm_binding_;
  std::unique_ptr<DeviceWatcherImpl> watcher_;
  fidl::Client<fuchsia_camera3::DeviceWatcher> client_;
  bool watch_pending_ = false;
  std::vector<fuchsia_camera3::WatchDevicesEvent> events_;
};

TEST_F(DeviceWatcherImplTest, RelaunchesDeviceAfterChildIsDestroyed) {
  uint64_t first_id = WatchFirstDevice();
  EXPECT_EQ(realm_.created_children(), std::vector<std::string>{kChildName});

  // The child crashes, so component manager destroys it. Clients see the device go away.
  Watch();
  realm_.DestroyAllChildren();
  RunLoopUntilIdle();
  ASSERT_FALSE(watch_pending_);
  ASSERT_EQ(events_.size(), 1u);
  ASSERT_EQ(events_[0].Which(), Tag::kRemoved);
  EXPECT_EQ(events_[0].removed().value(), first_id);

  // After one to two seconds (the first backoff delay with jitter), the watcher launches the child
  // again and reports it under a new ID.
  Watch();
  RunLoopFor(zx::sec(2));
  ASSERT_FALSE(watch_pending_);
  ASSERT_EQ(events_.size(), 1u);
  ASSERT_EQ(events_[0].Which(), Tag::kAdded);
  EXPECT_NE(events_[0].added().value(), first_id);
  EXPECT_EQ(realm_.created_children(), (std::vector<std::string>{kChildName, kChildName}));
}

TEST_F(DeviceWatcherImplTest, ConnectToDestroyedChildClosesRequest) {
  uint64_t id = WatchFirstDevice();

  // The child is already gone when a client connects, so OpenExposedDir fails. This used to abort
  // the watcher (b/244178394).
  realm_.set_open_exposed_dir_error(fuchsia_component::Error::kInstanceNotFound);
  auto device = fidl::Endpoints<fuchsia_camera3::Device>::Create();
  ASSERT_TRUE(client_->ConnectToDevice({{.id = id, .request = std::move(device.server)}}).is_ok());
  DeviceEventHandler handler;
  fidl::Client<fuchsia_camera3::Device> device_client(std::move(device.client), dispatcher(),
                                                      &handler);
  RunLoopUntilIdle();

  ASSERT_TRUE(handler.error.has_value());
  EXPECT_EQ(handler.error->status(), ZX_ERR_UNAVAILABLE);
}

TEST_F(DeviceWatcherImplTest, StopsRelaunchingAfterRepeatedFailures) {
  WatchFirstDevice();

  realm_.set_create_child_error(fuchsia_component::Error::kInstanceAlreadyExists);
  realm_.DestroyAllChildren();
  RunLoopFor(zx::min(10));

  // One launch at startup, then three relaunch attempts.
  EXPECT_EQ(realm_.created_children().size(), 4u);
}

TEST_F(DeviceWatcherImplTest, ResetsRelaunchAttemptsAfterStableRun) {
  WatchFirstDevice();

  // Spend two of the three relaunch attempts in quick succession.
  realm_.DestroyAllChildren();
  RunLoopFor(zx::sec(2));
  RunLoopUntilIdle();
  ASSERT_EQ(realm_.created_children().size(), 2u);

  realm_.DestroyAllChildren();
  RunLoopFor(zx::sec(4));
  RunLoopUntilIdle();
  ASSERT_EQ(realm_.created_children().size(), 3u);

  // Once the child has run for at least a minute, exiting resets both the backoff delay (back to
  // one to two seconds instead of four to eight) and the three-attempt budget.
  RunLoopFor(zx::min(1));
  realm_.set_create_child_error(fuchsia_component::Error::kInstanceAlreadyExists);
  realm_.DestroyAllChildren();

  RunLoopFor(zx::sec(2));
  RunLoopUntilIdle();
  EXPECT_EQ(realm_.created_children().size(), 4u);

  RunLoopFor(zx::min(10));
  EXPECT_EQ(realm_.created_children().size(), 6u);
}

TEST_F(DeviceWatcherImplTest, ResetsRelaunchAttemptsWhenDeviceIsReAdded) {
  WatchFirstDevice();

  realm_.set_create_child_error(fuchsia_component::Error::kInstanceAlreadyExists);
  realm_.DestroyAllChildren();
  RunLoopFor(zx::min(10));
  ASSERT_EQ(realm_.created_children().size(), 4u);

  // Re-adding the device starts it with a fresh backoff delay and three-attempt budget even if it
  // exits again before running for a minute.
  realm_.set_create_child_error(std::nullopt);
  watcher_->AddDeviceByPath(kDevicePath);
  watcher_->UpdateClients();
  RunLoopUntilIdle();
  ASSERT_EQ(realm_.created_children().size(), 5u);

  realm_.set_create_child_error(fuchsia_component::Error::kInstanceAlreadyExists);
  realm_.DestroyAllChildren();

  RunLoopFor(zx::sec(2));
  RunLoopUntilIdle();
  EXPECT_EQ(realm_.created_children().size(), 6u);

  RunLoopFor(zx::min(10));
  EXPECT_EQ(realm_.created_children().size(), 8u);
}

class DeviceWatcherImplLateCameraTest : public DeviceWatcherImplTest {
 protected:
  bool CameraPresentAtFirstScan() const override { return false; }
};

TEST_F(DeviceWatcherImplLateCameraTest, ReportsCameraAddedAfterFirstScan) {
  // The first scan found no camera.
  Watch();
  RunLoopUntilIdle();
  ASSERT_FALSE(watch_pending_);
  EXPECT_TRUE(events_.empty());

  // The camera driver comes up after the first scan, as it can at boot. The pending WatchDevices
  // call returns with the camera, instead of waiting for some later device change.
  Watch();
  RunLoopUntilIdle();
  ASSERT_TRUE(watch_pending_);
  watcher_->AddDeviceByPath(kDevicePath);
  RunLoopUntilIdle();
  ASSERT_FALSE(watch_pending_);
  ASSERT_EQ(events_.size(), 1u);
  EXPECT_EQ(events_[0].Which(), Tag::kAdded);
  EXPECT_EQ(realm_.created_children(), std::vector<std::string>{kChildName});
}

}  // namespace
}  // namespace camera
