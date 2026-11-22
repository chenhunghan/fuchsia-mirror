// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// clang-format off
#pragma GCC diagnostic push
#include <Weave/DeviceLayer/internal/WeaveDeviceLayerInternal.h>
#include <Weave/DeviceLayer/internal/BLEManager.h>
#pragma GCC diagnostic pop
#include "src/connectivity/weave/adaptation/ble_manager_impl.h"
// clang-format on

#include <lib/sys/cpp/testing/component_context_provider.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>

#include <atomic>
#include <vector>

#include <gtest/gtest.h>

#include "connectivity_manager_delegate_impl.h"
#include "fake_ble_peripheral.h"
#include "fake_gatt_server.h"
#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "test_configuration_manager.h"
#include "test_thread_stack_manager.h"
#include "weave_test_fixture.h"

namespace nl::Weave::DeviceLayer::Internal::testing {
namespace {
using nl::Weave::DeviceLayer::ConnectivityManager;
using nl::Weave::DeviceLayer::Internal::BLEManager;
using nl::Weave::DeviceLayer::Internal::BLEManagerImpl;
using weave::adaptation::testing::FakeBLEPeripheral;
using weave::adaptation::testing::FakeGATTService;
using weave::adaptation::testing::TestConfigurationManager;
using weave::adaptation::testing::TestThreadStackManager;
}  // namespace

class BLEManagerTest : public WeaveTestFixture<> {
 public:
  BLEManagerTest() {
    context_provider_.service_directory_provider()->AddService(
        fake_gatt_server_.GetHandler(dispatcher()));
    context_provider_.service_directory_provider()->AddService(
        fake_ble_peripheral_.GetHandler(dispatcher()));
  }

  void SetUp() override {
    WeaveTestFixture<>::SetUp();
    WeaveTestFixture<>::RunFixtureLoop();

    PlatformMgrImpl().SetComponentContextForProcess(context_provider_.TakeContext());
    PlatformMgrImpl().SetDispatcher(event_loop_.dispatcher());
    PlatformMgrImpl().GetSystemLayer().Init(nullptr);

    ThreadStackMgrImpl().SetDelegate(std::make_unique<TestThreadStackManager>());
    ConfigurationMgrImpl().SetDelegate(std::make_unique<TestConfigurationManager>());
    ConnectivityMgrImpl().SetDelegate(std::make_unique<ConnectivityManagerDelegateImpl>());
    EXPECT_EQ(ConfigurationMgrImpl().IsWoBLEEnabled(), true);

    ble_mgr_ = std::make_unique<BLEManagerImpl>();
    InitBleMgr();
  }

  void TearDown() override {
    if (ble_mgr_) {
      ble_mgr_->ReleaseAllConnections();
      ble_mgr_->_GetBleLayer()->Shutdown();
    }
    PlatformMgrImpl().GetSystemLayer().Shutdown();
    event_loop_.Quit();
    WeaveTestFixture<>::StopFixtureLoop();
    WeaveTestFixture<>::TearDown();
    ble_mgr_.reset();

    ThreadStackMgrImpl().SetDelegate(nullptr);
    ConfigurationMgrImpl().SetDelegate(nullptr);
    ConnectivityMgrImpl().SetDelegate(nullptr);
  }

 protected:
  static constexpr size_t kMaxConnections = BLEManagerImpl::kMaxConnections;

  void InitBleMgr() {
    ble_mgr_->_GetBleLayer()->OnWeaveBleConnectReceived = [](::nl::Ble::BLEEndPoint* end_point) {
      end_point->OnMessageReceived = [](::nl::Ble::BLEEndPoint*, PacketBuffer* msg) {
        PacketBuffer::Free(msg);
      };
    };
    EXPECT_EQ(ble_mgr_->_Init(), WEAVE_NO_ERROR);
    event_loop_.RunUntilIdle();
    EXPECT_EQ(GetBLEMgrServiceMode(), ConnectivityManager::kWoBLEServiceMode_Enabled);
    if (ConfigurationMgrImpl().IsWoBLEAdvertisementEnabled()) {
      EXPECT_EQ(IsBLEMgrAdvertising(), true);
    } else {
      EXPECT_EQ(IsBLEMgrAdvertising(), false);
    }
  }

  BLEManager::WoBLEServiceMode GetBLEMgrServiceMode() { return ble_mgr_->_GetWoBLEServiceMode(); }

  uint16_t IsBLEMgrAdvertising() { return ble_mgr_->_IsAdvertising(); }

  WEAVE_ERROR GetBLEMgrDeviceName(char* device_name, size_t device_name_size) {
    return ble_mgr_->_GetDeviceName(device_name, device_name_size);
  }

  WEAVE_ERROR SetBLEMgrDeviceName(const char* device_name) {
    return ble_mgr_->_SetDeviceName(device_name);
  }

  void SetWoBLEAdvertising(bool enabled) {
    EXPECT_EQ(ble_mgr_->_SetAdvertisingEnabled(enabled), WEAVE_NO_ERROR);
    event_loop_.RunUntilIdle();
  }

  void SetWoBLEServiceMode(BLEManager::WoBLEServiceMode mode) {
    EXPECT_EQ(ble_mgr_->_SetWoBLEServiceMode(mode), WEAVE_NO_ERROR);
    event_loop_.RunUntilIdle();
  }

  bool IsBLEMgrAdvertisingEnabled() { return ble_mgr_->_IsAdvertisingEnabled(); }

  bool IsGATTServicePublished() { return GetFlag(ble_mgr_->flags_, kFlag_GATTServicePublished); }

  void OnPlatformEvent(const WeaveDeviceEvent* event) {
    ble_mgr_->_OnPlatformEvent(event);
    event_loop_.RunUntilIdle();
  }

  uint16_t GetNumConnections() { return ble_mgr_->_NumConnections(); }

  static std::vector<uint8_t> BtpConnectReq() {
    return std::vector<uint8_t>(std::begin(FakeGATTService::kBtpConnectReqValue),
                                std::end(FakeGATTService::kBtpConnectReqValue));
  }

  fuchsia::bluetooth::gatt2::LocalService_WriteValue_Result WriteValueForPeer(
      fuchsia::bluetooth::PeerId peer_id, std::vector<uint8_t> value) {
    fuchsia::bluetooth::gatt2::LocalService_WriteValue_Result result;
    std::atomic_bool done{false};
    fake_gatt_server_.WriteRequestForPeer(
        peer_id, std::move(value),
        [&](fuchsia::bluetooth::gatt2::LocalService_WriteValue_Result res) {
          result = std::move(res);
          done.store(true, std::memory_order_release);
        });
    const zx::time deadline = zx::deadline_after(zx::sec(5));
    while (!done.load(std::memory_order_acquire) && zx::clock::get_monotonic() < deadline) {
      event_loop_.Run(zx::deadline_after(zx::msec(10)), /*once=*/true);
    }
    EXPECT_TRUE(done.load(std::memory_order_acquire));
    event_loop_.RunUntilIdle();
    return result;
  }

  void ConfigureCharacteristicForPeer(
      fuchsia::bluetooth::PeerId peer_id, bool indicate,
      fuchsia::bluetooth::gatt2::Handle handle = FakeGATTService::kCharacteristicHandle) {
    std::atomic_bool done{false};
    fake_gatt_server_.OnCharacteristicConfigurationForPeer(
        peer_id, /*notify=*/false, indicate, [&] { done.store(true, std::memory_order_release); },
        handle);
    const zx::time deadline = zx::deadline_after(zx::sec(5));
    while (!done.load(std::memory_order_acquire) && zx::clock::get_monotonic() < deadline) {
      event_loop_.Run(zx::deadline_after(zx::msec(10)), /*once=*/true);
    }
    EXPECT_TRUE(done.load(std::memory_order_acquire));
    event_loop_.RunUntilIdle();
  }

  std::vector<FakeGATTService::ReceivedIndication> GetIndications() const {
    return fake_gatt_server_.GetIndications();
  }

  bool WaitForIndicationCount(size_t expected_count) {
    const zx::time deadline = zx::deadline_after(zx::sec(5));
    while (fake_gatt_server_.GetIndications().size() < expected_count &&
           zx::clock::get_monotonic() < deadline) {
      event_loop_.RunUntilIdle();
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
    return fake_gatt_server_.GetIndications().size() >= expected_count;
  }

  TestConfigurationManager& config_delegate() {
    return *reinterpret_cast<TestConfigurationManager*>(ConfigurationMgrImpl().GetDelegate());
  }

  void WeaveConnect() {
    size_t cb_count = 0;
    auto callback = [&cb_count](fuchsia::bluetooth::gatt2::LocalService_WriteValue_Result res) {
      FX_LOGS(INFO) << "Received gatt2 write request";
      EXPECT_EQ(false, res.is_err());
      cb_count++;
    };
    // Expect the write request to be received by the Weave stack, handled, and positively responded
    // to.
    fake_gatt_server_.WriteRequest(std::move(callback));
    WeaveTestFixture<>::RunFixtureLoop();
    event_loop_.Run(zx::time::infinite(), true /*once*/);
    WeaveTestFixture<>::StopFixtureLoop();
    RunLoopUntil([&]() { return cb_count; });

    WeaveTestFixture<>::RunFixtureLoop();

    // Expect the provided |callback| to receive the positive response.
    EXPECT_EQ(cb_count, 1u);
    FX_LOGS(INFO) << "Verified callback for write";

    EXPECT_EQ(fake_gatt_server_.WeaveConnectionConfirmed(), false);
    fake_gatt_server_.OnCharacteristicConfiguration();
    // Event loop will be idle and waiting for subscribe request(characteristic configuration)
    // on timer. So we need to wait until either subscribe request is received or timeout.
    event_loop_.Run(zx::time::infinite(), true /*once*/);
    WeaveTestFixture<>::RunLoopUntilIdle();
    FX_LOGS(INFO) << "Ran event loop after CCC";

    // Stop fixture loop before waiting for FakeGATTLocalService::NotifyValue
    // on dispatcher().
    WeaveTestFixture<>::StopFixtureLoop();
    // Wait until FakeGATTLocalService::NotifyValue is called.
    RunLoopUntil([&]() {
      bool res = fake_gatt_server_.WeaveConnectionConfirmed();
      return res;
    });
    // The FakeGattServer has received the notification.
    WeaveTestFixture<>::RunFixtureLoop();
    FX_LOGS(INFO) << "Got indication from weavestack";

    bool is_confirmed = fake_gatt_server_.WeaveConnectionConfirmed();
    EXPECT_EQ(is_confirmed, true);
  }

 private:
  sys::testing::ComponentContextProvider context_provider_;
  std::unique_ptr<BLEManagerImpl> ble_mgr_;

  FakeGATTService fake_gatt_server_;
  FakeBLEPeripheral fake_ble_peripheral_;

  async::Loop event_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
};

TEST_F(BLEManagerTest, SetAndGetDeviceName) {
  constexpr char kLargeDeviceName[] = "TOO_LARGE_DEVICE_NAME_FUCHSIA";
  constexpr char kDeviceName[] = "FUCHSIATEST";
  char read_value[kMaxDeviceNameLength + 1];
  EXPECT_EQ(SetBLEMgrDeviceName(kLargeDeviceName), WEAVE_ERROR_INVALID_ARGUMENT);
  EXPECT_EQ(SetBLEMgrDeviceName(kDeviceName), WEAVE_NO_ERROR);
  EXPECT_EQ(GetBLEMgrDeviceName(read_value, 1), WEAVE_ERROR_BUFFER_TOO_SMALL);
  EXPECT_EQ(GetBLEMgrDeviceName(read_value, sizeof(read_value)), WEAVE_NO_ERROR);
  EXPECT_STREQ(kDeviceName, read_value);
}

TEST_F(BLEManagerTest, EnableAndDisableAdvertising) {
  // Disable Weave service advertising
  SetWoBLEAdvertising(false);
  EXPECT_EQ(IsBLEMgrAdvertising(), false);
  // Enable Weave service advertising
  SetWoBLEAdvertising(true);
  EXPECT_EQ(IsBLEMgrAdvertising(), true);
  // Re-enable Weave service advertising
  SetWoBLEAdvertising(true);
  EXPECT_EQ(IsBLEMgrAdvertising(), true);
}

TEST_F(BLEManagerTest, DisableAdvertisingWhenProvisioned) {
  constexpr DeviceEventType::PublicEventTypes kProvisioningEvents[] = {
      DeviceEventType::kFabricMembershipChange,
      DeviceEventType::kServiceProvisioningChange,
      DeviceEventType::kAccountPairingChange,
  };

  config_delegate().set_is_thread_enabled(false);

  for (auto event_type : kProvisioningEvents) {
    // Enable WoBLE service and advertising while not yet fully provisioned.
    config_delegate().set_is_paired_to_account(true).set_is_member_of_fabric(false);
    ASSERT_FALSE(ConfigurationMgr().IsFullyProvisioned());
    SetWoBLEServiceMode(ConnectivityManager::kWoBLEServiceMode_Enabled);
    SetWoBLEAdvertising(true);
    EXPECT_EQ(GetBLEMgrServiceMode(), ConnectivityManager::kWoBLEServiceMode_Enabled);
    EXPECT_TRUE(IsGATTServicePublished());
    EXPECT_TRUE(IsBLEMgrAdvertisingEnabled());
    EXPECT_TRUE(IsBLEMgrAdvertising());

    // A provisioning event while still partially provisioned should not disable WoBLE.
    WeaveDeviceEvent event{.Type = static_cast<uint16_t>(event_type)};
    OnPlatformEvent(&event);
    EXPECT_EQ(GetBLEMgrServiceMode(), ConnectivityManager::kWoBLEServiceMode_Enabled);
    EXPECT_TRUE(IsGATTServicePublished());
    EXPECT_TRUE(IsBLEMgrAdvertisingEnabled());
    EXPECT_TRUE(IsBLEMgrAdvertising());

    // Once the device is fully provisioned, the platform event disables WoBLE advertising
    // and unpublishes the GATT service.
    config_delegate().set_is_member_of_fabric(true);
    ASSERT_TRUE(ConfigurationMgr().IsFullyProvisioned());
    OnPlatformEvent(&event);
    EXPECT_EQ(GetBLEMgrServiceMode(), ConnectivityManager::kWoBLEServiceMode_Disabled);
    EXPECT_FALSE(IsGATTServicePublished());
    EXPECT_FALSE(IsBLEMgrAdvertisingEnabled());
    EXPECT_FALSE(IsBLEMgrAdvertising());
  }
}

// TODO(https://fxbug.dev/42085753): Re-enable this test once the GATT Write / Indicate flakes are
// fixed.
TEST_F(BLEManagerTest, DISABLED_TestWeaveConnect) { WeaveConnect(); }

TEST_F(BLEManagerTest, IsolatesConcurrentPeerConnections) {
  constexpr fuchsia::bluetooth::PeerId kPeer1{1001};
  constexpr fuchsia::bluetooth::PeerId kPeer2{2002};
  constexpr fuchsia::bluetooth::PeerId kUnconnectedPeer{3003};

  EXPECT_EQ(GetNumConnections(), 0u);

  // Peer 1 initiates a BTP connection handshake via C1 write.
  auto write_res_1 = WriteValueForPeer(kPeer1, BtpConnectReq());
  EXPECT_FALSE(write_res_1.is_err());
  EXPECT_EQ(GetNumConnections(), 1u);

  // An unconnected peer attempting to subscribe or unsubscribe must be ignored without
  // allocating a connection slot or tearing down Peer 1's handshake.
  ConfigureCharacteristicForPeer(kUnconnectedPeer, /*indicate=*/true);
  EXPECT_EQ(GetNumConnections(), 1u);
  ConfigureCharacteristicForPeer(kUnconnectedPeer, /*indicate=*/false);
  EXPECT_EQ(GetNumConnections(), 1u);
  EXPECT_TRUE(GetIndications().empty());

  // Peer 2 concurrently initiates its own BTP connection handshake via C1 write.
  auto write_res_2 = WriteValueForPeer(kPeer2, BtpConnectReq());
  EXPECT_FALSE(write_res_2.is_err());
  EXPECT_EQ(GetNumConnections(), 2u);

  // Peer 1 subscribes to C2 indications and receives its BTP CapabilitiesResponse routed
  // specifically to kPeer1.
  ConfigureCharacteristicForPeer(kPeer1, /*indicate=*/true);
  ASSERT_TRUE(WaitForIndicationCount(1));
  {
    auto indications = GetIndications();
    ASSERT_EQ(indications.size(), 1u);
    ASSERT_EQ(indications[0].peer_ids.size(), 1u);
    EXPECT_EQ(indications[0].peer_ids[0].value, kPeer1.value);
  }
  EXPECT_EQ(GetNumConnections(), 2u);

  // Peer 2 subscribes to C2 indications and receives its own BTP CapabilitiesResponse routed
  // specifically to kPeer2.
  ConfigureCharacteristicForPeer(kPeer2, /*indicate=*/true);
  ASSERT_TRUE(WaitForIndicationCount(2));
  {
    auto indications = GetIndications();
    ASSERT_EQ(indications.size(), 2u);
    ASSERT_EQ(indications[1].peer_ids.size(), 1u);
    EXPECT_EQ(indications[1].peer_ids[0].value, kPeer2.value);
  }
  EXPECT_EQ(GetNumConnections(), 2u);

  // Peer 2 sends a malformed BTP frame, causing BleLayer to abort Peer 2's endpoint without
  // affecting Peer 1's active connection.
  auto bad_write_res = WriteValueForPeer(kPeer2, {0xff, 0xff});
  EXPECT_FALSE(bad_write_res.is_err());
  EXPECT_EQ(GetNumConnections(), 1u);

  // Peer 1 unsubscribes cleanly, releasing the remaining connection slot.
  ConfigureCharacteristicForPeer(kPeer1, /*indicate=*/false);
  EXPECT_EQ(GetNumConnections(), 0u);
}

TEST_F(BLEManagerTest, EnforcesMaxConcurrentConnectionsAndReusesSlot) {
  for (size_t i = 0; i < kMaxConnections; ++i) {
    const fuchsia::bluetooth::PeerId peer_id{100 + i};
    auto res = WriteValueForPeer(peer_id, BtpConnectReq());
    EXPECT_FALSE(res.is_err());
    ConfigureCharacteristicForPeer(peer_id, /*indicate=*/true);
    ASSERT_TRUE(WaitForIndicationCount(i + 1));
    auto indications = GetIndications();
    ASSERT_EQ(indications[i].peer_ids.size(), 1u);
    EXPECT_EQ(indications[i].peer_ids[0].value, peer_id.value);
  }
  EXPECT_EQ(GetNumConnections(), kMaxConnections);

  // An additional concurrent peer beyond kMaxConnections is rejected with INSUFFICIENT_RESOURCES.
  constexpr fuchsia::bluetooth::PeerId kOverflowPeer{999};
  auto overflow_res = WriteValueForPeer(kOverflowPeer, BtpConnectReq());
  ASSERT_TRUE(overflow_res.is_err());
  EXPECT_EQ(overflow_res.err(), fuchsia::bluetooth::gatt2::Error::INSUFFICIENT_RESOURCES);
  EXPECT_EQ(GetNumConnections(), kMaxConnections);

  // Unsubscribing one active peer frees its slot for reuse by kOverflowPeer.
  ConfigureCharacteristicForPeer(fuchsia::bluetooth::PeerId{100}, /*indicate=*/false);
  EXPECT_EQ(GetNumConnections(), kMaxConnections - 1);

  auto retry_res = WriteValueForPeer(kOverflowPeer, BtpConnectReq());
  EXPECT_FALSE(retry_res.is_err());
  ConfigureCharacteristicForPeer(kOverflowPeer, /*indicate=*/true);
  ASSERT_TRUE(WaitForIndicationCount(kMaxConnections + 1));
  auto indications = GetIndications();
  ASSERT_EQ(indications.back().peer_ids.size(), 1u);
  EXPECT_EQ(indications.back().peer_ids[0].value, kOverflowPeer.value);
  EXPECT_EQ(GetNumConnections(), kMaxConnections);
}

}  // namespace nl::Weave::DeviceLayer::Internal::testing
