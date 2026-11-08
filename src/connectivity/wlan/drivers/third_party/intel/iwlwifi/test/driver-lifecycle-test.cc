// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <lib/async/default.h>
#include <lib/driver/testing/cpp/driver_test.h>

#include <gtest/gtest.h>

#include "src/devices/pci/testing/pci_protocol_fake.h"
#include "third_party/iwlwifi/platform/pcie-iwlwifi-driver.h"

namespace {

constexpr uint16_t kTestDeviceId = 0x095a;
constexpr uint16_t kTestSubsysDeviceId = 0x9e10;

class IwlwifiTestEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    fake_pci_.SetDeviceInfo({.device_id = kTestDeviceId});
    const uint16_t subsys_id = kTestSubsysDeviceId;
    zx_status_t status = fake_pci_.GetConfigVmo()->write(
        &subsys_id, fidl::ToUnderlying(fuchsia_hardware_pci::Config::kSubsystemId),
        sizeof(subsys_id));
    if (status != ZX_OK) {
      return zx::error(status);
    }

    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    return to_driver_vfs.AddService<fuchsia_hardware_pci::Service>(
        fuchsia_hardware_pci::Service::InstanceHandler({
            .device = fake_pci_.bind_handler(dispatcher),
        }));
  }

 private:
  pci::FakePciProtocol fake_pci_;
};

class TestConfig final {
 public:
  using DriverType = wlan::iwlwifi::PcieIwlwifiDriver;
  using EnvironmentType = IwlwifiTestEnvironment;
};

class DriverLifeCycleTest : public ::testing::Test {
 public:
  void SetUp() override {
    zx::result<> result = driver_test().StartDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
    driver_stopped_ = false;
  }

  void TearDown() override {
    if (!driver_stopped_) {
      zx::result<> result = driver_test().StopDriver();
      ASSERT_EQ(ZX_OK, result.status_value());
      driver_test().ShutdownAndDestroyDriver();
    }
  }

  size_t GetNodeNumber() {
    size_t count = 0;
    driver_test().RunInNodeContext([&count](fdf_testing::TestNode& node) {
      count = node.children().size();
    });
    return count;
  }

  fdf_testing::ForegroundDriverTest<TestConfig>& driver_test() { return driver_test_; }

 protected:
  bool driver_stopped_ = false;

 private:
  fdf_testing::ForegroundDriverTest<TestConfig> driver_test_;
};

TEST_F(DriverLifeCycleTest, DeviceLifeCycle) {
  // Starting PcieIwlwifiDriver will trigger AddNode for wlanphy virtual device. Start() hook is
  // called in Setup().
  EXPECT_EQ(GetNodeNumber(), (size_t)1);

  // TODO(b/290283534): Add more operations here like AddWlansoftmacDevice() and
  // RemoveWlansoftmacDevice().

  zx::result<> stop_result = driver_test().StopDriver();
  EXPECT_EQ(ZX_OK, stop_result.status_value());
  // We don't shutdown any of the children devices during PrepareStop(), this will be handled by
  // driver framework.
  EXPECT_EQ(GetNodeNumber(), (size_t)1);

  driver_test().ShutdownAndDestroyDriver();
  driver_stopped_ = true;

  while (GetNodeNumber() > 0) {
    driver_test().runtime().RunUntilIdle();
  }

  EXPECT_EQ(GetNodeNumber(), 0UL);
}

}  // namespace
