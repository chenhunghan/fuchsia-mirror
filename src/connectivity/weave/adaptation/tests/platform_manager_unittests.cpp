// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// clang-format off
#pragma GCC diagnostic push
#include <Weave/DeviceLayer/PlatformManager.h>
#include <Weave/DeviceLayer/internal/WeaveDeviceLayerInternal.h>
#pragma GCC diagnostic pop
// clang-format on

#include <fidl/fuchsia.hwinfo/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/component/testing/cpp/service_directory_provider.h>
#include <lib/vfs/cpp/service.h>

#include "src/connectivity/weave/adaptation/platform_manager_impl.h"
#include "src/lib/testing/loop_fixture/test_loop_fixture.h"

namespace weave::adaptation::testing {
namespace {

using nl::Weave::DeviceLayer::PlatformMgr;
using nl::Weave::DeviceLayer::PlatformMgrImpl;

class FakeHwinfoDevice final : public fidl::Server<fuchsia_hwinfo::Device> {
 public:
  explicit FakeHwinfoDevice(std::string serial) : serial_(std::move(serial)) {}

  void GetInfo(GetInfoCompleter::Sync& completer) override {
    fuchsia_hwinfo::DeviceInfo info;
    info.serial_number(serial_);
    completer.Reply({std::move(info)});
  }

 private:
  std::string serial_;
};

}  // namespace

class PlatformManagerTest : public ::gtest::TestLoopFixture {
 public:
  void SetUp() override { PlatformMgrImpl().SetDispatcher(dispatcher()); }
  void TearDown() override {
    PlatformMgrImpl().SetComponentContextForProcess(nullptr);
    TestLoopFixture::TearDown();
  }
};

TEST_F(PlatformManagerTest, ScheduleWork) {
  size_t counter = 0;
  auto increment_counter = [](intptr_t context) { (*reinterpret_cast<size_t*>(context))++; };

  PlatformMgr().ScheduleWork(increment_counter, reinterpret_cast<intptr_t>(&counter));
  RunLoopUntilIdle();
  EXPECT_EQ(counter, 1U);

  PlatformMgr().ScheduleWork(increment_counter, reinterpret_cast<intptr_t>(&counter));
  PlatformMgr().ScheduleWork(increment_counter, reinterpret_cast<intptr_t>(&counter));
  RunLoopUntilIdle();
  EXPECT_EQ(counter, 3U);
}

TEST_F(PlatformManagerTest, SetComponentContextFromNaturalIncomingDirectory) {
  FakeHwinfoDevice fake_device("NATURAL-SVC-SERIAL");
  component::testing::ServiceDirectoryProvider svc_provider(dispatcher());
  ASSERT_TRUE(svc_provider.AddProtocol<fuchsia_hwinfo::Device>(&fake_device).is_ok());

  auto incoming_svc = svc_provider.Clone();
  ASSERT_TRUE(incoming_svc.is_ok());
  PlatformMgrImpl().SetComponentContextForProcess(std::move(*incoming_svc));

  auto [client_end, server_end] = fidl::Endpoints<fuchsia_hwinfo::Device>::Create();
  ASSERT_NE(PlatformMgrImpl().GetComponentContextForProcess(), nullptr);
  EXPECT_EQ(PlatformMgrImpl().GetComponentContextForProcess()->svc()->Connect(
                std::string(fidl::DiscoverableProtocolName<fuchsia_hwinfo::Device>),
                server_end.TakeChannel()),
            ZX_OK);

  fidl::Client<fuchsia_hwinfo::Device> client(std::move(client_end), dispatcher());
  std::optional<std::string> received_serial;
  client->GetInfo().Then([&received_serial](auto& result) {
    EXPECT_TRUE(result.is_ok());
    if (result.is_ok()) {
      received_serial = result->info().serial_number();
    }
  });
  RunLoopUntilIdle();
  EXPECT_EQ(received_serial, "NATURAL-SVC-SERIAL");
}

TEST_F(PlatformManagerTest, SetComponentContextFromNaturalIncomingAndOutgoingDirectories) {
  FakeHwinfoDevice incoming_device("INCOMING-SERIAL");
  FakeHwinfoDevice outgoing_device("OUTGOING-SERIAL");
  fidl::ServerBindingGroup<fuchsia_hwinfo::Device> outgoing_bindings;

  component::testing::ServiceDirectoryProvider svc_provider(dispatcher());
  ASSERT_TRUE(svc_provider.AddProtocol<fuchsia_hwinfo::Device>(&incoming_device).is_ok());

  auto incoming_svc = svc_provider.Clone();
  ASSERT_TRUE(incoming_svc.is_ok());
  auto [outgoing_client, outgoing_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();

  PlatformMgrImpl().SetComponentContextForProcess(std::move(*incoming_svc),
                                                  std::move(outgoing_server));

  auto* ctx = PlatformMgrImpl().GetComponentContextForProcess();
  ASSERT_NE(ctx, nullptr);
  EXPECT_EQ(ctx->outgoing()->AddPublicService(
                std::make_unique<vfs::Service>(outgoing_bindings.CreateHandler(
                    &outgoing_device, dispatcher(), fidl::kIgnoreBindingClosure)),
                fidl::DiscoverableProtocolName<fuchsia_hwinfo::Device>),
            ZX_OK);

  auto [incoming_client_end, incoming_server_end] =
      fidl::Endpoints<fuchsia_hwinfo::Device>::Create();
  EXPECT_EQ(ctx->svc()->Connect(std::string(fidl::DiscoverableProtocolName<fuchsia_hwinfo::Device>),
                                incoming_server_end.TakeChannel()),
            ZX_OK);
  fidl::Client<fuchsia_hwinfo::Device> incoming_client(std::move(incoming_client_end),
                                                       dispatcher());
  std::optional<std::string> incoming_serial;
  incoming_client->GetInfo().Then([&incoming_serial](auto& result) {
    EXPECT_TRUE(result.is_ok());
    if (result.is_ok()) {
      incoming_serial = result->info().serial_number();
    }
  });

  std::string protocol_path =
      std::string("svc/") + fidl::DiscoverableProtocolName<fuchsia_hwinfo::Device>;
  auto outgoing_client_end =
      component::ConnectAt<fuchsia_hwinfo::Device>(outgoing_client.borrow(), protocol_path);
  ASSERT_TRUE(outgoing_client_end.is_ok());

  fidl::Client<fuchsia_hwinfo::Device> outgoing_fidl_client(std::move(*outgoing_client_end),
                                                            dispatcher());
  std::optional<std::string> outgoing_serial;
  outgoing_fidl_client->GetInfo().Then([&outgoing_serial](auto& result) {
    EXPECT_TRUE(result.is_ok());
    if (result.is_ok()) {
      outgoing_serial = result->info().serial_number();
    }
  });

  RunLoopUntilIdle();
  EXPECT_EQ(incoming_serial, "INCOMING-SERIAL");
  EXPECT_EQ(outgoing_serial, "OUTGOING-SERIAL");

  PlatformMgrImpl().SetComponentContextForProcess(nullptr);
}

TEST_F(PlatformManagerTest, SetComponentContextWithExplicitDispatcher) {
  async::Loop custom_loop(&kAsyncLoopConfigNeverAttachToThread);
  FakeHwinfoDevice outgoing_device("EXPLICIT-DISPATCHER-SERIAL");
  fidl::ServerBindingGroup<fuchsia_hwinfo::Device> outgoing_bindings;

  auto [outgoing_client, outgoing_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  PlatformMgrImpl().SetComponentContextForProcess({}, std::move(outgoing_server),
                                                  custom_loop.dispatcher());

  auto* ctx = PlatformMgrImpl().GetComponentContextForProcess();
  ASSERT_NE(ctx, nullptr);
  EXPECT_EQ(ctx->outgoing()->AddPublicService(
                std::make_unique<vfs::Service>(outgoing_bindings.CreateHandler(
                    &outgoing_device, custom_loop.dispatcher(), fidl::kIgnoreBindingClosure)),
                fidl::DiscoverableProtocolName<fuchsia_hwinfo::Device>),
            ZX_OK);

  std::string protocol_path =
      std::string("svc/") + fidl::DiscoverableProtocolName<fuchsia_hwinfo::Device>;
  auto client_end =
      component::ConnectAt<fuchsia_hwinfo::Device>(outgoing_client.borrow(), protocol_path);
  ASSERT_TRUE(client_end.is_ok());

  fidl::Client<fuchsia_hwinfo::Device> client(std::move(*client_end), custom_loop.dispatcher());
  std::optional<std::string> outgoing_serial;
  client->GetInfo().Then([&outgoing_serial](auto& result) {
    EXPECT_TRUE(result.is_ok());
    if (result.is_ok()) {
      outgoing_serial = result->info().serial_number();
    }
  });

  // Only advance |custom_loop| (not the fixture dispatcher configured via SetDispatcher) to verify
  // |outgoing_dir| is bound to the explicit dispatcher argument.
  custom_loop.RunUntilIdle();
  EXPECT_EQ(outgoing_serial, "EXPLICIT-DISPATCHER-SERIAL");

  PlatformMgrImpl().SetComponentContextForProcess(nullptr);
}

}  // namespace weave::adaptation::testing
