// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "../optee-controller.h"

#include <fidl/fuchsia.hardware.rpmb/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/fake-bti/cpp/fake-bti.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/fake-resource/cpp/fake-resource.h>
#include <lib/fdf/env.h>
#include <lib/fidl/cpp/wire/client.h>
#include <lib/fidl/cpp/wire/connect_service.h>
#include <lib/sync/completion.h>
#include <lib/zx/bti.h>
#include <stdlib.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <atomic>
#include <functional>
#include <thread>

#include <ddktl/suspend-txn.h>
#include <tee-client-api/tee-client-types.h>
#include <zxtest/zxtest.h>

#include "../optee-message.h"
#include "../optee-smc.h"
#include "../tee-smc.h"
#include "src/devices/testing/mock-ddk/mock-device.h"

struct SharedMemoryInfo {
  zx_paddr_t secure_world_paddr = 0;
  zx_paddr_t address = 0;
  size_t size = 0;
  zx::vmo vmo;
};

// This will be populated once the FakePdev creates the fake contiguous vmo so we can use the
// physical addresses within it.
static SharedMemoryInfo gSharedMemory = {};

constexpr fuchsia_tee::wire::Uuid kOpteeOsUuid = {
    0x486178E0, 0xE7F8, 0x11E3, {0xBC, 0x5E, 0x00, 0x02, 0xA5, 0xD5, 0xC5, 0x1B}};

constexpr fuchsia_tee::wire::Uuid kCustomTaWireUuid = {
    0x11223344, 0x5566, 0x7788, {0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00}};

using SmcCb = std::function<void(const zx_smc_parameters_t*, zx_smc_result_t*)>;
static SmcCb call_with_arg_handler;
static std::atomic<uint32_t> call_with_args_count = 0;
static std::mutex handler_mut;

void SetSmcCallWithArgHandler(SmcCb handler) {
  std::lock_guard<std::mutex> lock(handler_mut);
  call_with_arg_handler = std::move(handler);
}

zx_status_t zx_smc_call(zx_handle_t handle, const zx_smc_parameters_t* parameters,
                        zx_smc_result_t* out_smc_result) {
  EXPECT_TRUE(parameters);
  EXPECT_TRUE(out_smc_result);
  switch (parameters->func_id) {
    case tee_smc::kTrustedOsCallUidFuncId:
      out_smc_result->arg0 = optee::kOpteeApiUid_0;
      out_smc_result->arg1 = optee::kOpteeApiUid_1;
      out_smc_result->arg2 = optee::kOpteeApiUid_2;
      out_smc_result->arg3 = optee::kOpteeApiUid_3;
      break;
    case tee_smc::kTrustedOsCallRevisionFuncId:
      out_smc_result->arg0 = optee::kOpteeApiRevisionMajor;
      out_smc_result->arg1 = optee::kOpteeApiRevisionMinor;
      break;
    case optee::kGetOsRevisionFuncId:
      out_smc_result->arg0 = 1;
      out_smc_result->arg1 = 0;
      break;
    case optee::kExchangeCapabilitiesFuncId:
      out_smc_result->arg0 = optee::kReturnOk;
      out_smc_result->arg1 =
          optee::kSecureCapHasReservedSharedMem | optee::kSecureCapCanUsePrevUnregisteredSharedMem;
      break;
    case optee::kGetSharedMemConfigFuncId:
      out_smc_result->arg0 = optee::kReturnOk;
      out_smc_result->arg1 = gSharedMemory.address;
      out_smc_result->arg2 = gSharedMemory.size;
      break;
    case optee::kCallWithArgFuncId:
    case optee::kReturnFromRpcFuncId: {
      call_with_args_count++;
      SmcCb handler;
      {
        std::lock_guard<std::mutex> lock(handler_mut);
        std::swap(handler, call_with_arg_handler);
      }
      if (handler != nullptr) {
        handler(parameters, out_smc_result);
      } else {
        out_smc_result->arg0 = optee::kReturnOk;
      }
    } break;
    default:
      return ZX_ERR_NOT_SUPPORTED;
  }
  return ZX_OK;
}

namespace optee {
namespace {

class IncomingNamespace {
 public:
  explicit IncomingNamespace() : outgoing_(async_get_default_dispatcher()) {}

  fidl::ClientEnd<fuchsia_io::Directory> ConnectRpmb() {
    auto device_handler = [](fidl::ServerEnd<fuchsia_hardware_rpmb::Rpmb> request) {};
    fuchsia_hardware_rpmb::Service::InstanceHandler handler({.device = std::move(device_handler)});

    auto service_result = outgoing_.AddService<fuchsia_hardware_rpmb::Service>(std::move(handler));
    ZX_ASSERT(service_result.is_ok());

    auto endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
    ZX_ASSERT(outgoing_.Serve(std::move(endpoints.server)).is_ok());

    return std::move(endpoints.client);
  }

  fidl::ClientEnd<fuchsia_io::Directory> ConnectPdev() {
    auto service_result = outgoing_.AddService<fuchsia_hardware_platform_device::Service>(
        std::move(pdev_.GetInstanceHandler()));
    ZX_ASSERT(service_result.is_ok());

    auto endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
    ZX_ASSERT(outgoing_.Serve(std::move(endpoints.server)).is_ok());

    return std::move(endpoints.client);
  }

  fdf_fake::FakePDev& pdev() { return pdev_; }

 private:
  fdf_fake::FakePDev pdev_;
  component::OutgoingDirectory outgoing_;
};

class FakeTeeService : public fidl::WireServer<fuchsia_hardware_tee::DeviceConnector> {
 public:
  explicit FakeTeeService(OpteeController* optee)
      : dispatcher_(fdf::Dispatcher::GetCurrent()),
        outgoing_(dispatcher_->async_dispatcher()),
        optee_(optee) {}

  fidl::ClientEnd<fuchsia_io::Directory> Connect() {
    auto device_handler = [this](fidl::ServerEnd<fuchsia_hardware_tee::DeviceConnector> request) {
      fidl::BindServer(dispatcher_->async_dispatcher(), std::move(request), this);
      client_connected_.Signal();
    };
    fuchsia_hardware_tee::Service::InstanceHandler handler(
        {.device_connector = std::move(device_handler)});

    auto service_result = outgoing_.AddService<fuchsia_hardware_tee::Service>(std::move(handler));
    ZX_ASSERT(service_result.is_ok());

    auto endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
    ZX_ASSERT(outgoing_.Serve(std::move(endpoints.server)).is_ok());

    return std::move(endpoints.client);
  }

  void ConnectToApplication(ConnectToApplicationRequestView request,
                            ConnectToApplicationCompleter::Sync& completer) override {
    optee_->ConnectToApplication(std::move(request), completer);
  }

  void ConnectToDeviceInfo(ConnectToDeviceInfoRequestView request,
                           ConnectToDeviceInfoCompleter::Sync& completer) override {
    optee_->ConnectToDeviceInfo(std::move(request), completer);
  }

  void WaitForClientConnected() { ZX_ASSERT(fdf::WaitFor(client_connected_).is_ok()); }

 private:
  fdf::UnownedDispatcher dispatcher_;
  component::OutgoingDirectory outgoing_;
  OpteeController* optee_;

  libsync::Completion client_connected_;
};

class FakeDdkOptee : public zxtest::Test {
 public:
  FakeDdkOptee() : clients_loop_(&kAsyncLoopConfigNoAttachToCurrentThread) {
    incoming_loop_.StartThread("Incoming loop");
  }

  void SetUp() override {
    fdf_fake::FakePDev::Config config;
    config.smcs[0] = {};
    ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_SMC, config.smcs[0].reset_and_get_address()));
    config.btis[0] = {};
    zx::result bti = fake_bti::CreateFakeBti();
    ASSERT_OK(bti);
    config.btis[0] = std::move(bti.value());
    config.mmios[0] = CreateMmio(config.btis[0].borrow());
    std::optional<fuchsia_hardware_tee::TeeMetadata> tee_metadata = GetTeeMetadata();
    incoming_.SyncCall([config = std::move(config), tee_metadata = std::move(tee_metadata)](
                           IncomingNamespace* incoming) mutable {
      incoming->pdev().SetConfig(std::move(config));
      if (tee_metadata.has_value()) {
        ZX_ASSERT(incoming->pdev().AddFidlMetadata(
                      fuchsia_hardware_tee::TeeMetadata::kSerializableName, *tee_metadata) ==
                  ZX_OK);
      }
    });

    ASSERT_OK(clients_loop_.StartThread());
    ASSERT_OK(clients_loop_.StartThread());
    ASSERT_OK(clients_loop_.StartThread());
    parent_->AddFidlService(
        fuchsia_hardware_platform_device::Service::Name,
        incoming_.SyncCall([](auto* incoming) { return incoming->ConnectPdev(); }), "pdev");
    parent_->AddFidlService(
        fuchsia_hardware_rpmb::Service::Name,
        incoming_.SyncCall([](auto* incoming) { return incoming->ConnectRpmb(); }), "rpmb");

    ASSERT_OK(OpteeController::Create(nullptr, parent_.get()));
    optee_ = parent_->GetLatestChild()->GetDeviceContext<OpteeController>();

    tee_service_ = FakeTeeService(optee_);
    parent_->AddFidlService(fuchsia_hardware_tee::Service::Name, tee_service_->Connect(), "tee");

    zx::result client_end =
        optee_->DdkConnectFragmentFidlProtocol<fuchsia_hardware_tee::Service::DeviceConnector>(
            "tee");
    ASSERT_OK(client_end.status_value());
    tee_proto_client_.Bind(std::move(client_end.value()));

    tee_service_->WaitForClientConnected();

    call_with_args_count = 0;
  }

  void TearDown() override {
    if (!released_in_test_) {
      device_async_remove(parent_->GetLatestChild());
      EXPECT_OK(mock_ddk::ReleaseFlaggedDevices(parent_.get()));
    }
    SetSmcCallWithArgHandler(nullptr);
  }

  static fdf::PDev::MmioInfo CreateMmio(const zx::unowned_bti& fake_bti) {
    constexpr size_t kSecureWorldMemorySize = 0x20000;

    zx::vmo fake_vmo;
    EXPECT_OK(zx::vmo::create_contiguous(*fake_bti, 0x20000, 0, &fake_vmo));

    // Briefly pin the vmo to get the paddr for populating the gSharedMemory object
    zx_paddr_t secure_world_paddr;
    zx::pmt pmt;
    EXPECT_OK(fake_bti->pin(ZX_BTI_PERM_READ | ZX_BTI_CONTIGUOUS, fake_vmo, 0,
                            kSecureWorldMemorySize, &secure_world_paddr, 1, &pmt));
    // Use the second half of the secure world range to use as shared memory
    gSharedMemory.secure_world_paddr = secure_world_paddr;
    gSharedMemory.address = secure_world_paddr + (kSecureWorldMemorySize / 2);
    gSharedMemory.size = kSecureWorldMemorySize / 2;
    EXPECT_OK(fake_vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &gSharedMemory.vmo));
    EXPECT_OK(pmt.unpin());

    return fdf::PDev::MmioInfo{
        .offset = 0,
        .size = kSecureWorldMemorySize,
        .vmo = std::move(fake_vmo),
    };
  }

 protected:
  virtual std::optional<fuchsia_hardware_tee::TeeMetadata> GetTeeMetadata() { return std::nullopt; }

  async::Loop incoming_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  async_patterns::TestDispatcherBound<IncomingNamespace> incoming_{incoming_loop_.dispatcher(),
                                                                   std::in_place};

  std::shared_ptr<MockDevice> parent_{MockDevice::FakeRootParent()};
  OpteeController* optee_ = nullptr;
  bool released_in_test_ = false;

  async::Loop clients_loop_;
  std::optional<FakeTeeService> tee_service_;
  fidl::WireSyncClient<fuchsia_hardware_tee::DeviceConnector> tee_proto_client_;
};

TEST_F(FakeDdkOptee, PmtUnpinned) {
  EXPECT_TRUE(optee_->pinned_mmio().has_value());

  optee_->zxdev()->SuspendNewOp(DEV_POWER_STATE_D3COLD, false, DEVICE_SUSPEND_REASON_REBOOT);
  EXPECT_FALSE(optee_->pinned_mmio().has_value());
}

TEST_F(FakeDdkOptee, RpmbTest) { EXPECT_EQ(optee_->RpmbConnectServer().status_value(), ZX_OK); }

TEST_F(FakeDdkOptee, MultiThreadTest) {
  fidl::ClientEnd<fuchsia_tee::Application> tee_app_client[2];
  libsync::Completion completion1;
  libsync::Completion completion2;
  libsync::Completion smc_completion;
  libsync::Completion smc_completion1;
  zx_status_t status;

  for (auto& i : tee_app_client) {
    auto tee_endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();

    i = std::move(tee_endpoints.client);

    auto result = tee_proto_client_->ConnectToApplication(
        kOpteeOsUuid, fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
        std::move(tee_endpoints.server));
    ASSERT_OK(result.status());
  }

  fidl::WireSharedClient fidl_client1(std::move(tee_app_client[0]), clients_loop_.dispatcher());
  fidl::WireSharedClient fidl_client2(std::move(tee_app_client[1]), clients_loop_.dispatcher());

  {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
      sync_completion_signal(smc_completion1.get());
      sync_completion_wait(smc_completion.get(), ZX_TIME_INFINITE);
      out->arg0 = optee::kReturnOk;
    });
  }
  {
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    fidl_client1->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](::fidl::WireUnownedResult<::fuchsia_tee::Application::OpenSession2>& result) {
              if (!result.ok()) {
                FAIL("OpenSession2 failed: %s", result.error().FormatDescription().c_str());
                return;
              }
              sync_completion_signal(completion1.get());
            });
  }
  status = sync_completion_wait(completion1.get(), ZX_SEC(1));
  EXPECT_EQ(status, ZX_ERR_TIMED_OUT);
  ASSERT_OK(fdf::WaitFor(smc_completion1));

  {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
      out->arg0 = optee::kReturnOk;
    });
  }
  {
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    fidl_client2->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](::fidl::WireUnownedResult<::fuchsia_tee::Application::OpenSession2>& result) {
              if (!result.ok()) {
                FAIL("OpenSession2 failed: %s", result.error().FormatDescription().c_str());
                return;
              }
              sync_completion_signal(completion2.get());
            });
  }
  ASSERT_OK(fdf::WaitFor(completion2));
  sync_completion_signal(smc_completion.get());
  ASSERT_OK(fdf::WaitFor(completion1));
  EXPECT_EQ(call_with_args_count, 2u);
}

TEST_F(FakeDdkOptee, TheadLimitCorrectOrder) {
  fidl::ClientEnd<fuchsia_tee::Application> tee_app_client[2];
  libsync::Completion completion1;
  libsync::Completion completion2;
  libsync::Completion smc_completion;
  zx_status_t status;

  for (auto& i : tee_app_client) {
    auto tee_endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();

    i = std::move(tee_endpoints.client);

    auto result = tee_proto_client_->ConnectToApplication(
        kOpteeOsUuid, fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
        std::move(tee_endpoints.server));
    ASSERT_OK(result.status());
  }

  fidl::WireSharedClient fidl_client1(std::move(tee_app_client[0]), clients_loop_.dispatcher());
  fidl::WireSharedClient fidl_client2(std::move(tee_app_client[1]), clients_loop_.dispatcher());

  {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
      sync_completion_signal(smc_completion.get());
      out->arg0 = optee::kReturnEThreadLimit;
    });
  }
  {
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    fidl_client1->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](fidl::WireUnownedResult<::fuchsia_tee::Application::OpenSession2>& result) {
              if (!result.ok()) {
                FAIL("OpenSession2 failed: %s", result.error().FormatDescription().c_str());
                return;
              }
              sync_completion_signal(completion1.get());
            });
  }

  ASSERT_OK(fdf::WaitFor(smc_completion));
  status = sync_completion_wait(completion1.get(), ZX_SEC(1));
  EXPECT_EQ(status, ZX_ERR_TIMED_OUT);
  EXPECT_EQ(call_with_args_count, 1u);
  EXPECT_EQ(optee_->CommandQueueSize(), 1);

  {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
      out->arg0 = optee::kReturnOk;
    });
  }
  {
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    fidl_client2->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](::fidl::WireUnownedResult<::fuchsia_tee::Application::OpenSession2>& result) {
              if (!result.ok()) {
                FAIL("OpenSession2 failed: %s", result.error().FormatDescription().c_str());
                return;
              }
              sync_completion_signal(completion2.get());
            });
  }

  ASSERT_OK(fdf::WaitFor(completion2));
  ASSERT_OK(fdf::WaitFor(completion1));
  EXPECT_EQ(call_with_args_count, 3u);
  EXPECT_EQ(optee_->CommandQueueSize(), 0);
  EXPECT_EQ(optee_->CommandQueueWaitSize(), 0);
}

TEST_F(FakeDdkOptee, TheadLimitWrongOrder) {
  fidl::ClientEnd<fuchsia_tee::Application> tee_app_client[3];
  libsync::Completion completion1;
  libsync::Completion completion2;
  libsync::Completion completion3;
  libsync::Completion smc_completion;
  libsync::Completion smc_sleep_completion;

  for (auto& i : tee_app_client) {
    auto tee_endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();

    i = std::move(tee_endpoints.client);

    auto result = tee_proto_client_->ConnectToApplication(
        kOpteeOsUuid, fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
        std::move(tee_endpoints.server));
    ASSERT_OK(result.status());
  }

  fidl::WireSharedClient fidl_client1(std::move(tee_app_client[0]), clients_loop_.dispatcher());
  fidl::WireSharedClient fidl_client2(std::move(tee_app_client[1]), clients_loop_.dispatcher());
  fidl::WireSharedClient fidl_client3(std::move(tee_app_client[2]), clients_loop_.dispatcher());

  {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
      sync_completion_signal(smc_completion.get());
      sync_completion_wait(smc_sleep_completion.get(), ZX_TIME_INFINITE);
      out->arg0 = optee::kReturnOk;
    });
  }
  {  // first client is just sleeping for a long time (without ThreadLimit)
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    fidl_client1->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](::fidl::WireUnownedResult<::fuchsia_tee::Application::OpenSession2>& result) {
              if (!result.ok()) {
                FAIL("OpenSession2 failed: %s", result.error().FormatDescription().c_str());
                return;
              }
              sync_completion_signal(completion1.get());
            });
  }

  ASSERT_OK(fdf::WaitFor(smc_completion));
  EXPECT_FALSE(sync_completion_signaled(completion1.get()));
  EXPECT_EQ(call_with_args_count, 1u);
  sync_completion_reset(smc_completion.get());

  {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
      sync_completion_signal(smc_completion.get());
      out->arg0 = optee::kReturnEThreadLimit;
    });
  }
  {  // 2nd client got ThreadLimit
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    fidl_client2->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](::fidl::WireUnownedResult<::fuchsia_tee::Application::OpenSession2>& result) {
              if (!result.ok()) {
                FAIL("OpenSession2 failed: %s", result.error().FormatDescription().c_str());
                return;
              }
              sync_completion_signal(completion2.get());
            });
  }

  ASSERT_OK(fdf::WaitFor(smc_completion));
  EXPECT_FALSE(sync_completion_signaled(completion2.get()));
  EXPECT_EQ(call_with_args_count, 2u);
  EXPECT_EQ(optee_->CommandQueueSize(), 2);

  {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
      out->arg0 = optee::kReturnOk;
    });
  }
  {
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    fidl_client3->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](::fidl::WireUnownedResult<::fuchsia_tee::Application::OpenSession2>& result) {
              if (!result.ok()) {
                FAIL("OpenSession2 failed: %s", result.error().FormatDescription().c_str());
                return;
              }
              sync_completion_signal(completion3.get());
            });
  }

  ASSERT_OK(fdf::WaitFor(completion3));
  ASSERT_OK(fdf::WaitFor(completion2));
  EXPECT_EQ(call_with_args_count, 4u);
  sync_completion_signal(smc_sleep_completion.get());
  ASSERT_OK(fdf::WaitFor(completion1));
  EXPECT_EQ(optee_->CommandQueueSize(), 0);
  EXPECT_EQ(optee_->CommandQueueWaitSize(), 0);
}

TEST_F(FakeDdkOptee, TheadLimitWrongOrderCascade) {
  fidl::ClientEnd<fuchsia_tee::Application> tee_app_client[3];
  libsync::Completion completion1;
  libsync::Completion completion2;
  libsync::Completion completion3;
  libsync::Completion smc_completion;
  libsync::Completion smc_sleep_completion1;
  libsync::Completion smc_sleep_completion2;

  for (auto& i : tee_app_client) {
    auto tee_endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();

    i = std::move(tee_endpoints.client);

    auto result = tee_proto_client_->ConnectToApplication(
        kOpteeOsUuid, fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
        std::move(tee_endpoints.server));
    ASSERT_OK(result.status());
  }

  fidl::WireSharedClient fidl_client1(std::move(tee_app_client[0]), clients_loop_.dispatcher());
  fidl::WireSharedClient fidl_client2(std::move(tee_app_client[1]), clients_loop_.dispatcher());
  fidl::WireSharedClient fidl_client3(std::move(tee_app_client[2]), clients_loop_.dispatcher());

  {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
      sync_completion_signal(smc_completion.get());
      sync_completion_wait(smc_sleep_completion1.get(), ZX_TIME_INFINITE);
      out->arg0 = optee::kReturnEThreadLimit;
    });
  }
  {  // first client is just sleeping for a long time (without ThreadLimit)
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    fidl_client1->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](::fidl::WireUnownedResult<::fuchsia_tee::Application::OpenSession2>& result) {
              if (!result.ok()) {
                FAIL("OpenSession2 failed: %s", result.error().FormatDescription().c_str());
                return;
              }
              sync_completion_signal(completion1.get());
            });
  }

  ASSERT_OK(fdf::WaitFor(smc_completion));
  EXPECT_FALSE(sync_completion_signaled(completion1.get()));
  EXPECT_EQ(call_with_args_count, 1u);
  sync_completion_reset(smc_completion.get());

  {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
      sync_completion_signal(smc_completion.get());
      sync_completion_wait(smc_sleep_completion2.get(), ZX_TIME_INFINITE);
      out->arg0 = optee::kReturnOk;
    });
  }
  {  // 2nd client got ThreadLimit
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    fidl_client2->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](::fidl::WireUnownedResult<::fuchsia_tee::Application::OpenSession2>& result) {
              if (!result.ok()) {
                FAIL("OpenSession2 failed: %s", result.error().FormatDescription().c_str());
                return;
              }
              sync_completion_signal(completion2.get());
            });
  }

  ASSERT_OK(fdf::WaitFor(smc_completion));
  EXPECT_FALSE(sync_completion_signaled(completion2.get()));
  EXPECT_EQ(call_with_args_count, 2u);
  EXPECT_EQ(optee_->CommandQueueSize(), 2);

  {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
      out->arg0 = optee::kReturnOk;
    });
  }
  {
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    fidl_client3->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](::fidl::WireUnownedResult<::fuchsia_tee::Application::OpenSession2>& result) {
              if (!result.ok()) {
                FAIL("OpenSession2 failed: %s", result.error().FormatDescription().c_str());
                return;
              }
              sync_completion_signal(completion3.get());
            });
  }
  ASSERT_OK(fdf::WaitFor(completion3));
  EXPECT_EQ(call_with_args_count, 3u);

  sync_completion_signal(smc_sleep_completion2.get());
  ASSERT_OK(fdf::WaitFor(completion2));
  EXPECT_EQ(call_with_args_count, 3u);
  sync_completion_signal(smc_sleep_completion1.get());
  ASSERT_OK(fdf::WaitFor(completion1));
  EXPECT_EQ(call_with_args_count, 4u);

  EXPECT_EQ(optee_->CommandQueueSize(), 0);
  EXPECT_EQ(optee_->CommandQueueWaitSize(), 0);
}

constexpr uint32_t kTestSessionId = 7;
constexpr uint32_t kTestRpcAllocSize = 128;
constexpr uint32_t kDefaultPoolThreadCount = 2;
constexpr uint32_t kCustomPoolThreadCount = 2;
constexpr char kCustomPoolRoleName[] = "fuchsia.tee.custom";

void ConfigureSmcOpenSessionWithRpcAlloc() {
  SetSmcCallWithArgHandler([](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
    const zx_paddr_t msg_paddr =
        (static_cast<zx_paddr_t>(params->arg1) << 32) | static_cast<zx_paddr_t>(params->arg2);
    SetSmcCallWithArgHandler(
        [msg_paddr](const zx_smc_parameters_t* rpc_params, zx_smc_result_t* rpc_out) {
          const uint64_t alloc_paddr =
              (static_cast<uint64_t>(rpc_params->arg1) << 32) | rpc_params->arg2;
          const uint64_t alloc_mem_id =
              (static_cast<uint64_t>(rpc_params->arg4) << 32) | rpc_params->arg5;
          EXPECT_NE(alloc_paddr, 0u);
          EXPECT_NE(alloc_mem_id, 0u);

          ASSERT_GE(msg_paddr, gSharedMemory.secure_world_paddr);
          const uint64_t vmo_offset =
              gSharedMemory.size + (msg_paddr - gSharedMemory.secure_world_paddr);
          MessageHeader hdr = {};
          ASSERT_OK(gSharedMemory.vmo.read(&hdr, vmo_offset, sizeof(hdr)));
          hdr.session_id = kTestSessionId;
          hdr.return_code = TEEC_SUCCESS;
          hdr.return_origin = TEEC_ORIGIN_TRUSTED_APP;
          ASSERT_OK(gSharedMemory.vmo.write(&hdr, vmo_offset, sizeof(hdr)));

          rpc_out->arg0 = optee::kReturnOk;
        });

    out->arg0 = optee::kReturnRpcPrefix | optee::kRpcFunctionIdAllocateMemory;
    out->arg1 = kTestRpcAllocSize;
  });
}

class FakeDdkOpteeWithCustomThreads : public FakeDdkOptee {
 protected:
  std::optional<fuchsia_hardware_tee::TeeMetadata> GetTeeMetadata() override {
    fuchsia_hardware_tee::CustomThreadConfig custom_thread;
    custom_thread.role(kCustomPoolRoleName);
    custom_thread.count(kCustomPoolThreadCount);
    custom_thread.trusted_apps(std::vector<fuchsia_tee::Uuid>{fidl::ToNatural(kCustomTaWireUuid)});

    fuchsia_hardware_tee::TeeMetadata metadata;
    metadata.default_thread_count(kDefaultPoolThreadCount);
    metadata.custom_threads(
        std::vector<fuchsia_hardware_tee::CustomThreadConfig>{std::move(custom_thread)});
    return metadata;
  }
};

TEST_F(FakeDdkOptee, SuspendWithActiveClientAndSharedMemory) {
  auto tee_endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();
  ASSERT_OK(tee_proto_client_
                ->ConnectToApplication(kOpteeOsUuid,
                                       fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
                                       std::move(tee_endpoints.server))
                .status());

  fidl::WireSharedClient app_client(std::move(tee_endpoints.client), clients_loop_.dispatcher());
  ConfigureSmcOpenSessionWithRpcAlloc();

  libsync::Completion open_done;
  uint32_t opened_session_id = 0;
  fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
  app_client->OpenSession2(parameter_set)
      .ThenExactlyOnce(
          [&](fidl::WireUnownedResult<fuchsia_tee::Application::OpenSession2>& result) {
            ASSERT_OK(result.status());
            opened_session_id = result->session_id;
            open_done.Signal();
          });
  ASSERT_OK(fdf::WaitFor(open_done));
  EXPECT_EQ(opened_session_id, kTestSessionId);
  EXPECT_EQ(call_with_args_count, 2u);

  // Suspend while `app_client` is still open and holding both an active session and an
  // RPC-allocated SharedMemory region. Suspend must shut down client loops so ~OpteeClient closes
  // the session and returns all Regions before SharedMemoryManager is destroyed.
  optee_->zxdev()->SuspendNewOp(DEV_POWER_STATE_D3COLD, false, DEVICE_SUSPEND_REASON_REBOOT);
  EXPECT_FALSE(optee_->pinned_mmio().has_value());
  EXPECT_EQ(call_with_args_count, 3u);
}

TEST_F(FakeDdkOpteeWithCustomThreads, UnbindWithCustomThreadPoolActiveClient) {
  auto tee_endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();
  ASSERT_OK(tee_proto_client_
                ->ConnectToApplication(kCustomTaWireUuid,
                                       fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
                                       std::move(tee_endpoints.server))
                .status());

  fidl::WireSharedClient app_client(std::move(tee_endpoints.client), clients_loop_.dispatcher());
  ConfigureSmcOpenSessionWithRpcAlloc();

  libsync::Completion open_done;
  uint32_t opened_session_id = 0;
  fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
  app_client->OpenSession2(parameter_set)
      .ThenExactlyOnce(
          [&](fidl::WireUnownedResult<fuchsia_tee::Application::OpenSession2>& result) {
            ASSERT_OK(result.status());
            opened_session_id = result->session_id;
            open_done.Signal();
          });
  ASSERT_OK(fdf::WaitFor(open_done));
  EXPECT_EQ(opened_session_id, kTestSessionId);
  EXPECT_EQ(call_with_args_count, 2u);

  // Trigger unbind + release while `app_client` on `custom_loops_` is still open and holding an
  // active session and RPC-allocated SharedMemory region.
  device_async_remove(parent_->GetLatestChild());
  ASSERT_OK(mock_ddk::ReleaseFlaggedDevices(parent_.get()));
  released_in_test_ = true;
  EXPECT_EQ(call_with_args_count, 3u);
}

TEST_F(FakeDdkOptee, ConnectAfterSuspendClosesWithUnavailable) {
  optee_->zxdev()->SuspendNewOp(DEV_POWER_STATE_D3COLD, false, DEVICE_SUSPEND_REASON_REBOOT);

  auto app_endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();
  ASSERT_OK(tee_proto_client_
                ->ConnectToApplication(kOpteeOsUuid,
                                       fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
                                       std::move(app_endpoints.server))
                .status());
  fidl::WireSharedClient app_client(std::move(app_endpoints.client), clients_loop_.dispatcher());
  libsync::Completion app_closed;
  zx_status_t app_status = ZX_OK;
  fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
  app_client->OpenSession2(parameter_set)
      .ThenExactlyOnce(
          [&](fidl::WireUnownedResult<fuchsia_tee::Application::OpenSession2>& result) {
            app_status = result.status();
            app_closed.Signal();
          });
  ASSERT_OK(fdf::WaitFor(app_closed));
  EXPECT_NE(app_status, ZX_OK);

  auto info_endpoints = fidl::Endpoints<fuchsia_tee::DeviceInfo>::Create();
  ASSERT_OK(tee_proto_client_->ConnectToDeviceInfo(std::move(info_endpoints.server)).status());
  fidl::WireSharedClient info_client(std::move(info_endpoints.client), clients_loop_.dispatcher());
  libsync::Completion info_closed;
  zx_status_t info_status = ZX_OK;
  info_client->GetOsInfo().ThenExactlyOnce(
      [&](fidl::WireUnownedResult<fuchsia_tee::DeviceInfo::GetOsInfo>& result) {
        info_status = result.status();
        info_closed.Signal();
      });
  ASSERT_OK(fdf::WaitFor(info_closed));
  EXPECT_NE(info_status, ZX_OK);
}

TEST_F(FakeDdkOptee, SuspendWhileHandlerBlockedInTeeCall) {
  auto tee_endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();
  ASSERT_OK(tee_proto_client_
                ->ConnectToApplication(kOpteeOsUuid,
                                       fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
                                       std::move(tee_endpoints.server))
                .status());

  fidl::WireSharedClient app_client(std::move(tee_endpoints.client), clients_loop_.dispatcher());
  libsync::Completion entered_tee;
  libsync::Completion release_tee;
  libsync::Completion open_done;

  SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
    const zx_paddr_t msg_paddr =
        (static_cast<zx_paddr_t>(params->arg1) << 32) | static_cast<zx_paddr_t>(params->arg2);
    entered_tee.Signal();
    release_tee.Wait();

    ASSERT_GE(msg_paddr, gSharedMemory.secure_world_paddr);
    const uint64_t vmo_offset = gSharedMemory.size + (msg_paddr - gSharedMemory.secure_world_paddr);
    MessageHeader hdr = {};
    ASSERT_OK(gSharedMemory.vmo.read(&hdr, vmo_offset, sizeof(hdr)));
    hdr.session_id = kTestSessionId;
    hdr.return_code = TEEC_SUCCESS;
    hdr.return_origin = TEEC_ORIGIN_TRUSTED_APP;
    ASSERT_OK(gSharedMemory.vmo.write(&hdr, vmo_offset, sizeof(hdr)));
    out->arg0 = optee::kReturnOk;
  });

  fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
  app_client->OpenSession2(parameter_set)
      .ThenExactlyOnce(
          [&](fidl::WireUnownedResult<fuchsia_tee::Application::OpenSession2>& result) {
            ASSERT_OK(result.status());
            EXPECT_EQ(result->session_id, kTestSessionId);
            open_done.Signal();
          });

  ASSERT_OK(fdf::WaitFor(entered_tee));

  std::thread unblocker([&]() {
    while (!optee_->IsShuttingDown()) {
      std::this_thread::yield();
    }
    // Shared memory must remain pinned while an SMC call is still in flight.
    EXPECT_TRUE(optee_->pinned_mmio().has_value());
    release_tee.Signal();
  });

  optee_->zxdev()->SuspendNewOp(DEV_POWER_STATE_D3COLD, false, DEVICE_SUSPEND_REASON_REBOOT);
  unblocker.join();
  ASSERT_OK(fdf::WaitFor(open_done));
  EXPECT_FALSE(optee_->pinned_mmio().has_value());
  EXPECT_EQ(call_with_args_count, 2u);
}

TEST_F(FakeDdkOptee, SuspendWhileHandlerInWaitQueueSleep) {
  constexpr uint64_t kTestWaitQueueKey = 0xCAFE;
  struct WaitQueueRpcWire {
    MessageHeader hdr;
    MessageParam param;
  };

  auto populate_open_session_header = [](zx_paddr_t msg_paddr, uint32_t session_id) {
    ASSERT_GE(msg_paddr, gSharedMemory.secure_world_paddr);
    const uint64_t offset = gSharedMemory.size + (msg_paddr - gSharedMemory.secure_world_paddr);
    MessageHeader hdr = {};
    ASSERT_OK(gSharedMemory.vmo.read(&hdr, offset, sizeof(hdr)));
    hdr.session_id = session_id;
    hdr.return_code = TEEC_SUCCESS;
    hdr.return_origin = TEEC_ORIGIN_TRUSTED_APP;
    ASSERT_OK(gSharedMemory.vmo.write(&hdr, offset, sizeof(hdr)));
  };

  fidl::ClientEnd<fuchsia_tee::Application> client_ends[2];
  for (auto& end : client_ends) {
    auto endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();
    end = std::move(endpoints.client);
    ASSERT_OK(tee_proto_client_
                  ->ConnectToApplication(kOpteeOsUuid,
                                         fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
                                         std::move(endpoints.server))
                  .status());
  }

  fidl::WireSharedClient sleeper_client(std::move(client_ends[0]), clients_loop_.dispatcher());
  fidl::WireSharedClient waker_client(std::move(client_ends[1]), clients_loop_.dispatcher());
  libsync::Completion dispatched_wq_sleep;
  libsync::Completion waker_in_flight;
  libsync::Completion allow_waker_proceed;
  libsync::Completion sleeper_done;
  libsync::Completion waker_done;

  SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
    const zx_paddr_t sleeper_msg_paddr =
        (static_cast<zx_paddr_t>(params->arg1) << 32) | static_cast<zx_paddr_t>(params->arg2);
    populate_open_session_header(sleeper_msg_paddr, kTestSessionId);

    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* rpc_params, zx_smc_result_t* rpc_out) {
      const uint64_t alloc_paddr =
          (static_cast<uint64_t>(rpc_params->arg1) << 32) | rpc_params->arg2;
      const uint64_t alloc_mem_id =
          (static_cast<uint64_t>(rpc_params->arg4) << 32) | rpc_params->arg5;
      ASSERT_GE(alloc_paddr, gSharedMemory.secure_world_paddr);

      WaitQueueRpcWire wq_msg = {};
      wq_msg.hdr.command = RpcMessage::Command::kWaitQueue;
      wq_msg.hdr.num_params = 1;
      wq_msg.param.attribute = MessageParam::kAttributeTypeValueInput;
      wq_msg.param.payload.value.wait_queue.command = WaitQueueRpcMessage::Command::kSleep;
      wq_msg.param.payload.value.wait_queue.key = kTestWaitQueueKey;

      const uint64_t wq_offset =
          gSharedMemory.size + (alloc_paddr - gSharedMemory.secure_world_paddr);
      ASSERT_OK(gSharedMemory.vmo.write(&wq_msg, wq_offset, sizeof(wq_msg)));

      rpc_out->arg0 = optee::kReturnRpcPrefix | optee::kRpcFunctionIdExecuteCommand;
      rpc_out->arg1 = static_cast<uint32_t>(alloc_mem_id >> 32);
      rpc_out->arg2 = static_cast<uint32_t>(alloc_mem_id);
      dispatched_wq_sleep.Signal();
    });

    out->arg0 = optee::kReturnRpcPrefix | optee::kRpcFunctionIdAllocateMemory;
    out->arg1 = sizeof(WaitQueueRpcWire);
  });

  fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
  sleeper_client->OpenSession2(parameter_set)
      .ThenExactlyOnce(
          [&](fidl::WireUnownedResult<fuchsia_tee::Application::OpenSession2>& result) {
            ASSERT_OK(result.status());
            EXPECT_EQ(result->session_id, kTestSessionId);
            sleeper_done.Signal();
          });

  ASSERT_OK(fdf::WaitFor(dispatched_wq_sleep));
  while (optee_->WaitQueueSize() == 0) {
    std::this_thread::yield();
  }

  // Start a second client call before suspend begins; once suspend enters JoinThreads(), let this
  // already-in-flight TEE call issue a real kWakeUp RPC to unblock the sleeping handler.
  SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* params, zx_smc_result_t* out) {
    const zx_paddr_t waker_msg_paddr =
        (static_cast<zx_paddr_t>(params->arg1) << 32) | static_cast<zx_paddr_t>(params->arg2);
    populate_open_session_header(waker_msg_paddr, kTestSessionId + 1);
    waker_in_flight.Signal();
    allow_waker_proceed.Wait();

    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* rpc_params, zx_smc_result_t* rpc_out) {
      const uint64_t alloc_paddr =
          (static_cast<uint64_t>(rpc_params->arg1) << 32) | rpc_params->arg2;
      const uint64_t alloc_mem_id =
          (static_cast<uint64_t>(rpc_params->arg4) << 32) | rpc_params->arg5;
      ASSERT_GE(alloc_paddr, gSharedMemory.secure_world_paddr);

      WaitQueueRpcWire wq_msg = {};
      wq_msg.hdr.command = RpcMessage::Command::kWaitQueue;
      wq_msg.hdr.num_params = 1;
      wq_msg.param.attribute = MessageParam::kAttributeTypeValueInput;
      wq_msg.param.payload.value.wait_queue.command = WaitQueueRpcMessage::Command::kWakeUp;
      wq_msg.param.payload.value.wait_queue.key = kTestWaitQueueKey;

      const uint64_t wq_offset =
          gSharedMemory.size + (alloc_paddr - gSharedMemory.secure_world_paddr);
      ASSERT_OK(gSharedMemory.vmo.write(&wq_msg, wq_offset, sizeof(wq_msg)));

      rpc_out->arg0 = optee::kReturnRpcPrefix | optee::kRpcFunctionIdExecuteCommand;
      rpc_out->arg1 = static_cast<uint32_t>(alloc_mem_id >> 32);
      rpc_out->arg2 = static_cast<uint32_t>(alloc_mem_id);
    });

    out->arg0 = optee::kReturnRpcPrefix | optee::kRpcFunctionIdAllocateMemory;
    out->arg1 = sizeof(WaitQueueRpcWire);
  });

  waker_client->OpenSession2(parameter_set)
      .ThenExactlyOnce(
          [&](fidl::WireUnownedResult<fuchsia_tee::Application::OpenSession2>& result) {
            ASSERT_OK(result.status());
            EXPECT_EQ(result->session_id, kTestSessionId + 1);
            waker_done.Signal();
          });

  ASSERT_OK(fdf::WaitFor(waker_in_flight));

  std::thread unblocker([&]() {
    while (!optee_->IsShuttingDown()) {
      std::this_thread::yield();
    }
    EXPECT_TRUE(optee_->pinned_mmio().has_value());
    EXPECT_EQ(optee_->WaitQueueSize(), 1u);
    allow_waker_proceed.Signal();
  });

  optee_->zxdev()->SuspendNewOp(DEV_POWER_STATE_D3COLD, false, DEVICE_SUSPEND_REASON_REBOOT);
  unblocker.join();
  ASSERT_OK(fdf::WaitFor(sleeper_done));
  ASSERT_OK(fdf::WaitFor(waker_done));
  EXPECT_EQ(optee_->WaitQueueSize(), 0u);
  EXPECT_FALSE(optee_->pinned_mmio().has_value());
  EXPECT_EQ(call_with_args_count, 8u);
}

TEST_F(FakeDdkOptee, CloseSessionFailureDuringSuspend) {
  fidl::ClientEnd<fuchsia_tee::Application> client_ends[2];
  for (auto& end : client_ends) {
    auto endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();
    end = std::move(endpoints.client);
    ASSERT_OK(tee_proto_client_
                  ->ConnectToApplication(kOpteeOsUuid,
                                         fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
                                         std::move(endpoints.server))
                  .status());
  }

  fidl::WireSharedClient app_client1(std::move(client_ends[0]), clients_loop_.dispatcher());
  fidl::WireSharedClient app_client2(std::move(client_ends[1]), clients_loop_.dispatcher());

  for (auto* client : {&app_client1, &app_client2}) {
    ConfigureSmcOpenSessionWithRpcAlloc();
    libsync::Completion open_done;
    fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
    (*client)
        ->OpenSession2(parameter_set)
        .ThenExactlyOnce(
            [&](fidl::WireUnownedResult<fuchsia_tee::Application::OpenSession2>& result) {
              ASSERT_OK(result.status());
              EXPECT_EQ(result->session_id, kTestSessionId);
              open_done.Signal();
            });
    ASSERT_OK(fdf::WaitFor(open_done));
  }
  EXPECT_EQ(call_with_args_count, 4u);

  // Fail the first CloseSession with an explicit TEE error and the second with EThreadLimit (which
  // must abort immediately during single-threaded shutdown rather than hanging in
  // CommandQueueWait).
  SetSmcCallWithArgHandler([](const zx_smc_parameters_t* /*params*/, zx_smc_result_t* out) {
    SetSmcCallWithArgHandler([](const zx_smc_parameters_t* /*params2*/, zx_smc_result_t* out2) {
      out2->arg0 = optee::kReturnEThreadLimit;
    });
    out->arg0 = optee::kReturnEBadCommand;
  });

  optee_->zxdev()->SuspendNewOp(DEV_POWER_STATE_D3COLD, false, DEVICE_SUSPEND_REASON_REBOOT);
  EXPECT_FALSE(optee_->pinned_mmio().has_value());
  EXPECT_EQ(call_with_args_count, 6u);
}

TEST_F(FakeDdkOptee, CloseSessionWaitQueueSleepDuringSuspendAborts) {
  constexpr uint64_t kTestWaitQueueKey = 0xBEEF;
  struct WaitQueueRpcWire {
    MessageHeader hdr;
    MessageParam param;
  };

  auto endpoints = fidl::Endpoints<fuchsia_tee::Application>::Create();
  ASSERT_OK(tee_proto_client_
                ->ConnectToApplication(kOpteeOsUuid,
                                       fidl::ClientEnd<::fuchsia_tee_manager::Provider>(),
                                       std::move(endpoints.server))
                .status());

  fidl::WireSharedClient app_client(std::move(endpoints.client), clients_loop_.dispatcher());
  ConfigureSmcOpenSessionWithRpcAlloc();
  libsync::Completion open_done;
  fidl::VectorView<fuchsia_tee::wire::Parameter> parameter_set;
  app_client->OpenSession2(parameter_set)
      .ThenExactlyOnce(
          [&](fidl::WireUnownedResult<fuchsia_tee::Application::OpenSession2>& result) {
            ASSERT_OK(result.status());
            EXPECT_EQ(result->session_id, kTestSessionId);
            open_done.Signal();
          });
  ASSERT_OK(fdf::WaitFor(open_done));
  EXPECT_EQ(call_with_args_count, 2u);

  // During Phase 3 of DdkSuspend, ~OpteeClient() invokes CloseSession() single-threaded. If the
  // TEE requests a wait-queue sleep, WaitQueueWait must reject it and CallWithMessage must abort
  // immediately rather than re-entering SMC in a tight loop.
  bool reentered_smc_after_rejected_sleep = false;
  SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* /*params*/, zx_smc_result_t* out) {
    SetSmcCallWithArgHandler([&](const zx_smc_parameters_t* rpc_params, zx_smc_result_t* rpc_out) {
      const uint64_t alloc_paddr =
          (static_cast<uint64_t>(rpc_params->arg1) << 32) | rpc_params->arg2;
      const uint64_t alloc_mem_id =
          (static_cast<uint64_t>(rpc_params->arg4) << 32) | rpc_params->arg5;
      ASSERT_GE(alloc_paddr, gSharedMemory.secure_world_paddr);

      WaitQueueRpcWire wq_msg = {};
      wq_msg.hdr.command = RpcMessage::Command::kWaitQueue;
      wq_msg.hdr.num_params = 1;
      wq_msg.param.attribute = MessageParam::kAttributeTypeValueInput;
      wq_msg.param.payload.value.wait_queue.command = WaitQueueRpcMessage::Command::kSleep;
      wq_msg.param.payload.value.wait_queue.key = kTestWaitQueueKey;

      const uint64_t wq_offset =
          gSharedMemory.size + (alloc_paddr - gSharedMemory.secure_world_paddr);
      ASSERT_OK(gSharedMemory.vmo.write(&wq_msg, wq_offset, sizeof(wq_msg)));

      SetSmcCallWithArgHandler(
          [&](const zx_smc_parameters_t* /*retry*/, zx_smc_result_t* retry_out) {
            reentered_smc_after_rejected_sleep = true;
            retry_out->arg0 = optee::kReturnOk;
          });

      rpc_out->arg0 = optee::kReturnRpcPrefix | optee::kRpcFunctionIdExecuteCommand;
      rpc_out->arg1 = static_cast<uint32_t>(alloc_mem_id >> 32);
      rpc_out->arg2 = static_cast<uint32_t>(alloc_mem_id);
    });

    out->arg0 = optee::kReturnRpcPrefix | optee::kRpcFunctionIdAllocateMemory;
    out->arg1 = sizeof(WaitQueueRpcWire);
  });

  optee_->zxdev()->SuspendNewOp(DEV_POWER_STATE_D3COLD, false, DEVICE_SUSPEND_REASON_REBOOT);
  EXPECT_FALSE(reentered_smc_after_rejected_sleep);
  EXPECT_EQ(optee_->WaitQueueSize(), 0u);
  EXPECT_FALSE(optee_->pinned_mmio().has_value());
  EXPECT_EQ(call_with_args_count, 4u);
}

}  // namespace
}  // namespace optee
