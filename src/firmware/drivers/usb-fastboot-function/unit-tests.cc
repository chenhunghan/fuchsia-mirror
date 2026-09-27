// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.fastboot/cpp/wire_test_base.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <lib/sync/cpp/completion.h>

#include <gtest/gtest.h>
#include <usb-inspect/usb-inspect-test-helper.h>

#include "src/devices/usb/lib/usb-endpoint/testing/fake-usb-endpoint-server.h"
#include "src/firmware/drivers/usb-fastboot-function/usb_fastboot_function.h"

namespace usb_fastboot_function {
namespace {

constexpr uint32_t kBulkOutEp = 1;
constexpr uint32_t kBulkInEp = 2;

class TestEndpoint : public fake_usb_endpoint::FakeEndpoint {
 public:
  void QueueRequests(QueueRequestsRequest& request,
                     QueueRequestsCompleter::Sync& completer) override {
    fake_usb_endpoint::FakeEndpoint::QueueRequests(request, completer);
    if (on_queue_requests_) {
      on_queue_requests_();
    }
  }

  void CancelAll(CancelAllCompleter::Sync& completer) override {
    cancel_all_called_ = true;
    cancel_all_count_++;
    if (on_cancel_all_) {
      on_cancel_all_();
    }
    fake_usb_endpoint::FakeEndpoint::CancelAll(completer);
  }

  void SetOnQueueRequests(fit::closure callback) { on_queue_requests_ = std::move(callback); }
  void SetOnCancelAll(fit::closure callback) { on_cancel_all_ = std::move(callback); }
  bool cancel_all_called() const { return cancel_all_called_; }
  size_t cancel_all_count() const { return cancel_all_count_; }
  void reset_cancel_all_called() { cancel_all_called_ = false; }

 private:
  fit::closure on_queue_requests_;
  fit::closure on_cancel_all_;
  bool cancel_all_called_ = false;
  size_t cancel_all_count_ = 0;
};

class FakeUsbFunction
    : public fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                                    TestEndpoint> {
 public:
  using Base = fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                                      TestEndpoint>;
  using Base::Base;

  void Configure(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::Configure>& request,
      fidl::internal::NaturalCompleter<fuchsia_hardware_usb_function::UsbFunction::Configure>::Sync&
          completer) override {
    interface_ = std::move(request.iface());
    completer.Reply(fit::ok());
  }

  void AllocResources(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::AllocResources>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::AllocResources>::Sync& completer) override {
    fuchsia_hardware_usb_function::UsbFunctionAllocResourcesResponse response;
    ASSERT_EQ(request.endpoints().size(), 2u);
    ASSERT_EQ(request.interface_count(), 2u);
    response.interface_nums() = {0, 1};
    response.endpoint_addrs() = {kBulkOutEp, kBulkInEp};
    response.string_indices() = {};
    for (size_t i = 0; i < 2; i++) {
      fidl::ServerEnd ep = std::move(request.endpoints()[i].endpoint());
      fake_endpoint(response.endpoint_addrs()[i]).Connect(dispatcher(), std::move(ep));
    }
    completer.Reply(fit::ok(std::move(response)));
  }

  void ConfigureEndpoint(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::ConfigureEndpoint>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::ConfigureEndpoint>::Sync& completer)
      override {
    configured_endpoints_.push_back(request.endpoint_address());
    if (fail_configure_endpoint_addr_.has_value() &&
        *fail_configure_endpoint_addr_ == request.endpoint_address()) {
      completer.Reply(fit::error(fail_configure_status_));
      return;
    }
    Base::ConfigureEndpoint(request, completer);
  }

  void DisableEndpoint(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::DisableEndpoint>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::DisableEndpoint>::Sync& completer) override {
    disabled_endpoints_.push_back(request.endpoint_address());
    if (verify_lifecycle_order_) {
      auto& ep = fake_endpoint(request.endpoint_address());
      EXPECT_TRUE(ep.cancel_all_called()) << "DisableEndpoint called before CancelAll on endpoint "
                                          << static_cast<int>(request.endpoint_address());
      EXPECT_EQ(ep.pending_request_count(), 0u)
          << "DisableEndpoint called while requests still pending on endpoint "
          << static_cast<int>(request.endpoint_address());
    }
    if (fail_disable_endpoint_status_.has_value()) {
      completer.Reply(fit::error(*fail_disable_endpoint_status_));
      return;
    }
    Base::DisableEndpoint(request, completer);
  }

  void set_fail_configure_endpoint(uint8_t ep_addr, zx_status_t status) {
    fail_configure_endpoint_addr_ = ep_addr;
    fail_configure_status_ = status;
  }
  void set_fail_disable_endpoint(zx_status_t status) { fail_disable_endpoint_status_ = status; }
  const std::vector<uint8_t>& configured_endpoints() const { return configured_endpoints_; }
  const std::vector<uint8_t>& disabled_endpoints() const { return disabled_endpoints_; }
  void set_verify_lifecycle_order(bool verify) { verify_lifecycle_order_ = verify; }

  fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> TakeInterface() {
    return std::move(interface_);
  }

 private:
  std::optional<uint8_t> fail_configure_endpoint_addr_;
  zx_status_t fail_configure_status_ = ZX_OK;
  std::optional<zx_status_t> fail_disable_endpoint_status_;
  std::vector<uint8_t> configured_endpoints_;
  bool verify_lifecycle_order_ = false;
  std::vector<uint8_t> disabled_endpoints_;
  fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> interface_;
};

class UsbFastbootEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    device_server_.Initialize("default", std::nullopt);
    EXPECT_EQ(ZX_OK, device_server_.Serve(dispatcher, &to_driver_vfs));
    fuchsia_hardware_usb_function::UsbFunctionService::InstanceHandler handler({
        .device = usb_function_bindings_.CreateHandler(&fake_dev_, dispatcher,
                                                       fidl::kIgnoreBindingClosure),
    });
    EXPECT_TRUE(
        to_driver_vfs
            .AddService<fuchsia_hardware_usb_function::UsbFunctionService>(std::move(handler))
            .is_ok());

    return zx::ok();
  }

  compat::DeviceServer device_server_;
  FakeUsbFunction fake_dev_ = FakeUsbFunction(fdf::Dispatcher::GetCurrent()->async_dispatcher());
  fidl::ServerBindingGroup<fuchsia_hardware_usb_function::UsbFunction> usb_function_bindings_;
};

class UsbFastbootTestConfig final {
 public:
  using DriverType = UsbFastbootFunction;
  using EnvironmentType = UsbFastbootEnvironment;
};

class UsbFastbootFunctionTest : public ::testing::Test {
 protected:
  void SetUp() override {
    ASSERT_EQ(ZX_OK, driver_test_.StartDriver().status_value());
    auto device = driver_test_.Connect<fuchsia_hardware_fastboot::Service::Fastboot>();
    EXPECT_EQ(ZX_OK, device.status_value());
    client_.Bind(std::move(device.value()));

    driver_test_.RunInEnvironmentTypeContext([this](UsbFastbootEnvironment& env) {
      function_client_.Bind(env.fake_dev_.TakeInterface());
    });

    driver_test_.RunInDriverContext([](UsbFastbootFunction& driver) {
      EXPECT_EQ(driver.bulk_in_addr(), kBulkInEp);
      EXPECT_EQ(driver.bulk_out_addr(), kBulkOutEp);
    });
  }

  void TearDown() override {}

  void EnableUsb() {
    ASSERT_TRUE(function_client_.is_valid());
    {
      fidl::Result result = function_client_->SetConfigured({{
          .configured = true,
          .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
      }});
      ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();
    }
    {
      fidl::Result result = function_client_->SetInterface({{
          .interface = 0,
          .alt_setting = 0,
      }});
      ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();
    }
  }

  fidl::WireSyncClient<fuchsia_hardware_fastboot::FastbootImpl>& client() { return client_; }

 protected:
  fidl::SyncClient<fuchsia_hardware_usb_function::UsbFunctionInterface> function_client_;
  fidl::WireSyncClient<fuchsia_hardware_fastboot::FastbootImpl> client_;
  fdf_testing::BackgroundDriverTest<UsbFastbootTestConfig> driver_test_;
};

TEST_F(UsbFastbootFunctionTest, LifetimeTest) {
  // Lifetime tested in test Setup() and TearDown()
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

void ValidateVmo(const zx::vmo& vmo, std::string_view payload) {
  fzl::VmoMapper mapper;
  ASSERT_EQ(ZX_OK, mapper.Map(vmo));
  size_t content_size = 0;
  ASSERT_EQ(ZX_OK, vmo.get_prop_content_size(&content_size));
  ASSERT_EQ(content_size, payload.size());
}

TEST_F(UsbFastbootFunctionTest, ReceiveTestSinglePacket) {
  const std::string_view test_data = "getvar:all";
  EnableUsb();

  std::thread t([&] {
    auto res = client()->Receive(0);
    ValidateVmo(res.value()->data, test_data);
  });

  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, test_data.size());
  });

  t.join();
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, ReceiveStateReset) {
  EnableUsb();

  {
    const std::string_view test_data = "getvar:all";
    std::thread t1([&] {
      auto res = client()->Receive(0);
      ValidateVmo(res.value()->data, test_data);
    });
    driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
      env.fake_dev_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, test_data.size());
    });
    t1.join();
  }

  {
    const std::string_view test_data = "getvar:max-download-size";
    std::thread t1([&] {
      auto res = client()->Receive(0);
      ValidateVmo(res.value()->data, test_data);
    });
    driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
      env.fake_dev_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, test_data.size());
    });
    t1.join();
  }
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, ReceiveFailsOnError) {
  const std::string_view test_data = "getvar:all";
  EnableUsb();

  std::thread t([&]() { ASSERT_FALSE(client()->Receive(0)->is_ok()); });

  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.fake_endpoint(kBulkOutEp)
        .RequestComplete(ZX_ERR_IO_NOT_PRESENT, test_data.size());
  });

  t.join();
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

void InitializeSendVmo(fzl::OwnedVmoMapper& vmo, std::string_view data) {
  ASSERT_EQ(ZX_OK, vmo.CreateAndMap(data.size(), "test"));
  ASSERT_EQ(ZX_OK, vmo.vmo().set_prop_content_size(data.size()));
  memcpy(vmo.start(), data.data(), data.size());
}

TEST_F(UsbFastbootFunctionTest, ReceiveFailsOnNonConfiguredInterface) {
  ASSERT_FALSE(client()->Receive(0)->is_ok());
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, Send) {
  const std::string_view send_data = "OKAY0.4";
  fzl::OwnedVmoMapper send_vmo;
  InitializeSendVmo(send_vmo, send_data);

  EnableUsb();

  std::thread t([&]() {
    auto result = client()->Send(send_vmo.Release());
    ASSERT_TRUE(result.ok());
    ASSERT_TRUE(result->is_ok());
  });

  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, send_data.size());
  });

  t.join();
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, SendStatesReset) {
  EnableUsb();

  {
    const std::string_view send_data = "OKAY0.4";
    fzl::OwnedVmoMapper send_vmo;
    InitializeSendVmo(send_vmo, send_data);
    std::thread t([&]() {
      auto result = client()->Send(send_vmo.Release());
      ASSERT_TRUE(result.ok());
      ASSERT_TRUE(result->is_ok());
    });
    driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
      env.fake_dev_.fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, send_data.size());
    });
    t.join();
  }

  {
    const std::string_view send_data = "OKAY0.6";
    fzl::OwnedVmoMapper send_vmo;
    InitializeSendVmo(send_vmo, send_data);
    std::thread t([&]() {
      auto result = client()->Send(send_vmo.Release());
      ASSERT_TRUE(result.ok());
      ASSERT_TRUE(result->is_ok());
    });
    driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
      env.fake_dev_.fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, send_data.size());
    });
    t.join();
  }
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, SendFailsOnNonConfiguredInterface) {
  const std::string_view send_data = "OKAY0.4";
  fzl::OwnedVmoMapper send_vmo;
  InitializeSendVmo(send_vmo, send_data);
  ASSERT_FALSE(client()->Send(send_vmo.Release())->is_ok());
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, SendFailOnError) {
  EnableUsb();

  const std::string_view send_data = "OKAY0.6";
  fzl::OwnedVmoMapper send_vmo;
  InitializeSendVmo(send_vmo, send_data);

  std::thread t([&]() { ASSERT_FALSE(client()->Send(send_vmo.Release())->is_ok()); });

  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.fake_endpoint(kBulkInEp).RequestComplete(ZX_ERR_IO_NOT_PRESENT, send_data.size());
  });

  t.join();
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, SendFailsOnZeroContentSize) {
  EnableUsb();
  const std::string_view send_data = "OKAY0.4";
  fzl::OwnedVmoMapper send_vmo;
  InitializeSendVmo(send_vmo, send_data);
  ASSERT_EQ(ZX_OK, send_vmo.vmo().set_prop_content_size(0));
  ASSERT_FALSE(client()->Send(send_vmo.Release())->is_ok());
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, Inspect) {
  EnableUsb();

  const std::string_view send_data = "OKAY0.4";
  const std::string_view recv_data = "getvar:all";

  // 1. Send (TX)
  fzl::OwnedVmoMapper send_vmo;
  InitializeSendVmo(send_vmo, send_data);
  std::thread t_send([&]() {
    auto result = client()->Send(send_vmo.Release());
    ASSERT_TRUE(result.ok());
    ASSERT_TRUE(result->is_ok());
  });
  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, send_data.size());
  });
  t_send.join();

  // 2. Receive (RX)
  std::thread t_recv([&] {
    auto res = client()->Receive(0);
    ValidateVmo(res.value()->data, recv_data);
  });
  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, recv_data.size());
  });
  t_recv.join();

  // 3. Trigger throughput and verify
  driver_test_.RunInDriverContext(
      [tx_size = send_data.size(), rx_size = recv_data.size()](UsbFastbootFunction& driver) {
        driver.GetThroughputTrackerForTesting().MeasureForTesting(zx::sec(1));

        auto hierarchy = usb_inspect::ReadHierarchyFromInspector(driver.inspector().inspector());

        auto* fastboot_node = hierarchy.GetByPath({"usb-fastboot"});
        ASSERT_TRUE(fastboot_node != nullptr);

        auto* bulk_in = hierarchy.GetByPath({"usb-fastboot", "bulk_in"});
        ASSERT_TRUE(bulk_in != nullptr);
        auto err_in = usb_inspect::VerifyEndpointInspect(bulk_in, tx_size, std::nullopt, 0,
                                                         std::nullopt, tx_size);
        EXPECT_TRUE(err_in.is_ok()) << err_in.error_value();

        auto* bulk_out = hierarchy.GetByPath({"usb-fastboot", "bulk_out"});
        ASSERT_TRUE(bulk_out != nullptr);
        auto err_out = usb_inspect::VerifyEndpointInspect(bulk_out, std::nullopt, rx_size,
                                                          std::nullopt, 0, rx_size);
        EXPECT_TRUE(err_out.is_ok()) << err_out.error_value();
      });

  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, TeardownWithPendingReceiveDrainsCleanly) {
  EnableUsb();

  libsync::Completion requests_queued;
  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.fake_endpoint(kBulkOutEp).SetOnQueueRequests([&]() { requests_queued.Signal(); });
  });

  std::thread client_thread([&]() {
    auto res = client()->Receive(1024);
    ASSERT_TRUE(res.ok());
    EXPECT_TRUE(res->is_error());
    if (res->is_error()) {
      EXPECT_EQ(res->error_value(), ZX_ERR_CANCELED);
    }
  });

  // Await requests queued in fake endpoint deterministically.
  requests_queued.Wait();

  // Verify requests were queued in fake endpoint.
  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    EXPECT_GT(env.fake_dev_.fake_endpoint(kBulkOutEp).pending_request_count(), 0u);
  });

  // StopDriver should drain requests and complete pending client call.
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());

  client_thread.join();

  // Verify fake endpoint has no pending requests remaining and was disabled.
  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    EXPECT_EQ(env.fake_dev_.fake_endpoint(kBulkOutEp).pending_request_count(), 0u);
    EXPECT_EQ(env.fake_dev_.disabled_endpoints().size(), 2u);
  });
}

TEST_F(UsbFastbootFunctionTest, TeardownWithPendingSendDrainsCleanly) {
  EnableUsb();

  const std::string_view send_data = "PENDING_SEND_DATA";
  fzl::OwnedVmoMapper send_vmo;
  InitializeSendVmo(send_vmo, send_data);

  libsync::Completion requests_queued;
  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.fake_endpoint(kBulkInEp).SetOnQueueRequests([&]() { requests_queued.Signal(); });
  });

  std::thread client_thread([&]() {
    auto res = client()->Send(send_vmo.Release());
    ASSERT_TRUE(res.ok());
    EXPECT_TRUE(res->is_error());
    if (res->is_error()) {
      EXPECT_EQ(res->error_value(), ZX_ERR_CANCELED);
    }
  });

  // Await requests queued in fake endpoint deterministically.
  requests_queued.Wait();

  // Verify requests were queued in fake endpoint.
  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    EXPECT_GT(env.fake_dev_.fake_endpoint(kBulkInEp).pending_request_count(), 0u);
  });

  // StopDriver should drain requests and complete pending client call.
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());

  client_thread.join();

  // Verify fake endpoint has no pending requests remaining and was disabled.
  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    EXPECT_EQ(env.fake_dev_.fake_endpoint(kBulkInEp).pending_request_count(), 0u);
    EXPECT_EQ(env.fake_dev_.disabled_endpoints().size(), 2u);
  });
}

TEST_F(UsbFastbootFunctionTest, TeardownWhileIdleDisablesEndpoints) {
  EnableUsb();

  ASSERT_TRUE(driver_test_.StopDriver().is_ok());

  // Verify fake endpoints were disabled during teardown.
  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    EXPECT_EQ(env.fake_dev_.disabled_endpoints().size(), 2u);
  });
}

TEST_F(UsbFastbootFunctionTest, SendAndReceiveFailWithCanceledWhileStopping) {
  EnableUsb();

  libsync::Completion cancel_started;
  libsync::Completion client_calls_done;

  std::thread client_thread([&]() {
    // Wait until Stop() has set stopping_ = true and invoked CancelAll().
    cancel_started.Wait();

    // Calls to Receive() and Send() while stopping must return ZX_ERR_CANCELED.
    {
      auto res = client()->Receive(1024);
      ASSERT_TRUE(res.ok());
      EXPECT_TRUE(res->is_error());
      if (res->is_error()) {
        EXPECT_EQ(res->error_value(), ZX_ERR_CANCELED);
      }
    }
    {
      const std::string_view send_data = "CANCEL_TEST";
      fzl::OwnedVmoMapper send_vmo;
      InitializeSendVmo(send_vmo, send_data);
      auto res = client()->Send(send_vmo.Release());
      ASSERT_TRUE(res.ok());
      EXPECT_TRUE(res->is_error());
      if (res->is_error()) {
        EXPECT_EQ(res->error_value(), ZX_ERR_CANCELED);
      }
    }

    client_calls_done.Signal();
  });

  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.fake_endpoint(kBulkOutEp).SetOnCancelAll([&]() {
      cancel_started.Signal();
      client_calls_done.Wait();
    });
  });

  // StopDriver runs on the main test thread.
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());

  client_thread.join();
}

TEST_F(UsbFastbootFunctionTest, SetConfiguredFalseWithPendingReceiveDrainsAndCancels) {
  EnableUsb();

  libsync::Completion requests_queued;
  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.set_verify_lifecycle_order(true);
    env.fake_dev_.fake_endpoint(kBulkOutEp).SetOnQueueRequests([&]() { requests_queued.Signal(); });
  });

  std::thread client_thread([&]() {
    auto res = client()->Receive(1024);
    ASSERT_TRUE(res.ok());
    EXPECT_TRUE(res->is_error());
    if (res->is_error()) {
      EXPECT_EQ(res->error_value(), ZX_ERR_CANCELED);
    }
  });

  requests_queued.Wait();

  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    EXPECT_GT(env.fake_dev_.fake_endpoint(kBulkOutEp).pending_request_count(), 0u);
  });

  // Deconfigure USB interface while Receive is pending.
  fidl::Result result = function_client_->SetConfigured({{
      .configured = false,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kUndefined,
  }});
  ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();

  client_thread.join();

  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    EXPECT_TRUE(env.fake_dev_.fake_endpoint(kBulkOutEp).cancel_all_called());
    EXPECT_EQ(env.fake_dev_.fake_endpoint(kBulkOutEp).pending_request_count(), 0u);
    EXPECT_EQ(env.fake_dev_.disabled_endpoints().size(), 2u);
  });

  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, SetConfiguredFalseWithPendingSendDrainsAndCancels) {
  EnableUsb();

  const std::string_view send_data = "PENDING_SEND_DATA";
  fzl::OwnedVmoMapper send_vmo;
  InitializeSendVmo(send_vmo, send_data);

  libsync::Completion requests_queued;
  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.set_verify_lifecycle_order(true);
    env.fake_dev_.fake_endpoint(kBulkInEp).SetOnQueueRequests([&]() { requests_queued.Signal(); });
  });

  std::thread client_thread([&]() {
    auto res = client()->Send(send_vmo.Release());
    ASSERT_TRUE(res.ok());
    EXPECT_TRUE(res->is_error());
    if (res->is_error()) {
      EXPECT_EQ(res->error_value(), ZX_ERR_CANCELED);
    }
  });

  requests_queued.Wait();

  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    EXPECT_GT(env.fake_dev_.fake_endpoint(kBulkInEp).pending_request_count(), 0u);
  });

  // Deconfigure USB interface while Send is pending.
  fidl::Result result = function_client_->SetConfigured({{
      .configured = false,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kUndefined,
  }});
  ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();

  client_thread.join();

  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    EXPECT_TRUE(env.fake_dev_.fake_endpoint(kBulkInEp).cancel_all_called());
    EXPECT_EQ(env.fake_dev_.fake_endpoint(kBulkInEp).pending_request_count(), 0u);
    EXPECT_EQ(env.fake_dev_.disabled_endpoints().size(), 2u);
  });

  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, SetConfiguredFalseIdempotentWhenAlreadyUnconfigured) {
  // Device starts unconfigured. Calling SetConfigured(false) should succeed immediately.
  fidl::Result result = function_client_->SetConfigured({{
      .configured = false,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kUndefined,
  }});
  ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();

  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, ConfigureEndpointsRollsBackOnPartialFailure) {
  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    // Cause ConfigureEndpoint to fail on the second endpoint (bulk IN).
    env.fake_dev_.set_fail_configure_endpoint(kBulkInEp, ZX_ERR_IO_NOT_PRESENT);
  });

  // Calling SetConfigured(true) should fail.
  fidl::Result result = function_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
  }});
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_IO_NOT_PRESENT);

  // Verify rollback: bulk OUT was configured, and then disabled during rollback.
  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    // Both endpoints were attempted to be configured.
    EXPECT_EQ(env.fake_dev_.configured_endpoints().size(), 2u);
    // Rollback must have disabled the successfully configured bulk OUT endpoint.
    EXPECT_EQ(env.fake_dev_.disabled_endpoints().size(), 1u);
    if (!env.fake_dev_.disabled_endpoints().empty()) {
      EXPECT_EQ(env.fake_dev_.disabled_endpoints()[0], kBulkOutEp);
    }
  });

  // Client requests must fail as driver is not configured.
  ASSERT_FALSE(client()->Receive(0)->is_ok());

  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, StopWhileSetConfiguredFalseDrainingDoesNotDuplicateCancel) {
  EnableUsb();

  libsync::Completion cancel_started;
  libsync::Completion stop_called;

  driver_test_.RunInEnvironmentTypeContext([&](UsbFastbootEnvironment& env) {
    env.fake_dev_.fake_endpoint(kBulkOutEp).SetOnCancelAll([&]() {
      cancel_started.Signal();
      stop_called.Wait();
    });
  });

  std::thread set_configured_thread([&]() {
    fidl::Result result = function_client_->SetConfigured({{
        .configured = false,
        .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kUndefined,
    }});
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_CANCELED);
    // SetConfigured received ZX_ERR_CANCELED, meaning Stop() has preempted it and ran.
    // Now allow CancelAll to finish.
    stop_called.Signal();
  });

  // Await SetConfigured(false) issuing CancelAll.
  cancel_started.Wait();

  // Call StopDriver on the main test thread while SetConfigured(false) is actively draining.
  ASSERT_TRUE(driver_test_.StopDriver().is_ok());

  set_configured_thread.join();

  // Verify CancelAll was called only once per endpoint (no duplicate cancel).
  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    EXPECT_EQ(env.fake_dev_.fake_endpoint(kBulkOutEp).cancel_all_count(), 1u);
    EXPECT_EQ(env.fake_dev_.fake_endpoint(kBulkInEp).cancel_all_count(), 1u);
    EXPECT_EQ(env.fake_dev_.disabled_endpoints().size(), 2u);
  });
}

TEST_F(UsbFastbootFunctionTest, DisableEndpointExpectedDisconnectToleratedDuringTeardown) {
  EnableUsb();

  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    // Simulate peripheral driver disconnect / unplug error during teardown.
    env.fake_dev_.set_fail_disable_endpoint(ZX_ERR_PEER_CLOSED);
  });

  // SetConfigured(false) during unbind or bus drop should tolerate expected disconnects.
  fidl::Result result = function_client_->SetConfigured({{
      .configured = false,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kUndefined,
  }});
  ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();

  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}

TEST_F(UsbFastbootFunctionTest, RollbackToleratesExpectedDisconnectDuringTeardown) {
  driver_test_.RunInEnvironmentTypeContext([](UsbFastbootEnvironment& env) {
    // Cause ConfigureEndpoint to fail on the second endpoint (bulk IN).
    env.fake_dev_.set_fail_configure_endpoint(kBulkInEp, ZX_ERR_IO_NOT_PRESENT);
    // Simulate peripheral driver disconnect during rollback DisableEndpoint call.
    env.fake_dev_.set_fail_disable_endpoint(ZX_ERR_PEER_CLOSED);
  });

  fidl::Result result = function_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
  }});
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_IO_NOT_PRESENT);

  ASSERT_TRUE(driver_test_.StopDriver().is_ok());
}
}  // namespace
}  // namespace usb_fastboot_function
