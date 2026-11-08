// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "adb.h"

#include <fuchsia/hardware/adb/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/fit/defer.h>
#include <lib/sync/cpp/completion.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>
#include <sched.h>
#include <unistd.h>

#include <memory>
#include <mutex>
#include <queue>
#include <thread>
#include <vector>

#include <fbl/auto_lock.h>
#include <fbl/mutex.h>
#include <gtest/gtest.h>
#include <src/lib/testing/loop_fixture/real_loop_fixture.h>

#include "src/developer/adb/bin/adb/state-controller.h"
#include "src/developer/adb/third_party/adb/socket.h"

namespace adb {

using namespace component_testing;

class FakeAdbDriver {
 public:
  explicit FakeAdbDriver() : device_(this) {}

  bool expectations_empty() {
    fbl::AutoLock _(&impl_.lock_);
    return device_.expect_start_ == 0 && impl_.expect_queue_tx_.empty() &&
           impl_.expect_receive_.empty();
  }

  // Helper functions to test ADB Protocol
  void ExpectStart() { device_.expect_start_++; }

  void ExpectQueueTx(uint32_t options, std::vector<uint8_t> buffer) {
    fbl::AutoLock _(&impl_.lock_);
    impl_.expect_queue_tx_.push(std::move(buffer));
  }
  void ExpectQueueTx(uint32_t options, apacket packet) {
    ExpectQueueTx(options, std::vector<uint8_t>(reinterpret_cast<uint8_t*>(&packet),
                                                reinterpret_cast<uint8_t*>(&packet) +
                                                    sizeof(packet.msg) + packet.payload.size()));
  }

  void SendConnect() {
    apacket packet{
        .msg =
            {
                .command = A_CNXN,
                .arg0 = A_VERSION,
                .arg1 = MAX_PAYLOAD,
                .data_length = 234,
                .data_check = 0x5b44,
                .magic = A_CNXN ^ 0xffffffff,
            },
        .payload = {},
    };
    impl_.ExpectReceive(std::move(packet));
    std::string connection_string = "host::features=shell_v2,cmd,stat_v2,ls_v2,fixed_pu";
    std::vector<uint8_t> data = {connection_string.begin(), connection_string.end()};
    data.resize(234);
    impl_.ExpectReceive(std::move(data));
  }

  void SendOpen(std::vector<uint8_t> data) {
    impl_.ExpectReceive({.msg =
                             {
                                 .command = A_OPEN,
                                 .arg0 = 1,  // local-id:1
                                 .arg1 = 0,
                                 .data_length = static_cast<uint32_t>(data.size()),
                                 .data_check = 0,
                                 .magic = A_OPEN ^ 0xffffffff,
                             },
                         .payload = {data.begin(), data.end()}});
  }

  void SendWrite(std::vector<uint8_t> data) {
    impl_.ExpectReceive({.msg =
                             {
                                 .command = A_WRTE,
                                 .arg0 = 1,  // local-id:1
                                 .arg1 = 1,  // remote-id: 1
                                 .data_length = static_cast<uint32_t>(data.size()),
                                 .data_check = 0,
                                 .magic = A_WRTE ^ 0xffffffff,
                             },
                         .payload = {data.begin(), data.end()}});
  }

  void TearDown() {
    impl_.Close();
    impl_.loop_.Shutdown();

    EXPECT_EQ(device_.expect_start_, 0UL);
    fbl::AutoLock _(&impl_.lock_);
    EXPECT_TRUE(impl_.expect_queue_tx_.empty());
    EXPECT_TRUE(impl_.expect_receive_.empty());
  }

  //  private:
  friend class DeviceServer;
  friend class UsbAdbImplServer;
  friend class TestDeviceConnector;

  class UsbAdbImplServer : public fidl::Server<fuchsia_hardware_adb::UsbAdbImpl> {
   public:
    UsbAdbImplServer() : loop_(&kAsyncLoopConfigNeverAttachToThread) {
      EXPECT_EQ(loop_.StartThread("adb-test-impl-thread"), ZX_OK);
    }

    ~UsbAdbImplServer() override { Close(); }

    void Bind(fidl::ServerEnd<fuchsia_hardware_adb::UsbAdbImpl> server) {
      Close();
      auto unbound = std::make_shared<libsync::Completion>();
      fbl::AutoLock _(&lock_);
      unbound_ = unbound;
      binding_.emplace(
          fidl::BindServer(loop_.dispatcher(), std::move(server), this,
                           [unbound](UsbAdbImplServer* self, fidl::UnbindInfo,
                                     fidl::ServerEnd<fuchsia_hardware_adb::UsbAdbImpl>) {
                             {
                               fbl::AutoLock _(&self->lock_);
                               self->receive_completer_.reset();
                               self->receive_armed_.Reset();
                             }
                             unbound->Signal();
                           }));
    }

    void Close() {
      std::shared_ptr<libsync::Completion> unbound;
      {
        fbl::AutoLock _(&lock_);
        receive_completer_.reset();
        receive_armed_.Reset();
        if (binding_.has_value()) {
          binding_->Unbind();
          binding_.reset();
        }
        unbound = std::move(unbound_);
      }
      if (unbound) {
        EXPECT_EQ(unbound->Wait(zx::sec(5)), ZX_OK);
      }
    }

    // fuchsia_hardware_adb::UsbAdbImpl functions
    void QueueTx(QueueTxRequest& request, QueueTxCompleter::Sync& completer) override {
      std::vector<uint8_t> expect;
      {
        fbl::AutoLock _(&lock_);
        ASSERT_FALSE(expect_queue_tx_.empty());
        expect = std::move(expect_queue_tx_.front());
        expect_queue_tx_.pop();
      }
      EXPECT_EQ(request.data().size(), expect.size());
      EXPECT_TRUE(
          std::equal(request.data().begin(), request.data().end(), expect.begin(), expect.end()));

      completer.Reply(fit::success());
    }

    // Neither Receive nor ExpectReceive should block the thread. If either blocks the thread,
    // execution will not be able to proceed.
    void Receive(ReceiveCompleter::Sync& completer) override {
      fbl::AutoLock _(&lock_);
      if (expect_receive_.empty()) {
        receive_completer_.emplace(completer.ToAsync());
        receive_armed_.Signal();
        return;
      }
      completer.Reply(fit::ok(std::move(expect_receive_.front())));
      expect_receive_.pop();
    }
    void ExpectReceive(std::vector<uint8_t> vec) {
      fbl::AutoLock _(&lock_);
      if (receive_completer_.has_value()) {
        receive_completer_->Reply(fit::ok(std::move(vec)));
        receive_completer_.reset();
        receive_armed_.Reset();
        return;
      }
      expect_receive_.push(std::move(vec));
    }
    void ExpectReceive(apacket packet) {
      std::vector<uint8_t> vec;
      std::copy(reinterpret_cast<uint8_t*>(&packet.msg),
                reinterpret_cast<uint8_t*>(&packet.msg) + sizeof(amessage),
                std::back_inserter(vec));
      std::copy(packet.payload.data(), packet.payload.data() + packet.payload.size(),
                std::back_inserter(vec));
      ExpectReceive(std::move(vec));
    }

    //  private:
    friend class FakeAdbDriver;

    async::Loop loop_;
    fbl::Mutex lock_;
    libsync::Completion receive_armed_;
    std::shared_ptr<libsync::Completion> unbound_ __TA_GUARDED(lock_);
    std::optional<fidl::ServerBindingRef<fuchsia_hardware_adb::UsbAdbImpl>> binding_
        __TA_GUARDED(lock_);
    std::queue<std::vector<uint8_t>> expect_queue_tx_ __TA_GUARDED(lock_);
    std::queue<std::vector<uint8_t>> expect_receive_ __TA_GUARDED(lock_);
    std::optional<ReceiveCompleter::Async> receive_completer_ __TA_GUARDED(lock_);
  } impl_;

  class DeviceServer : public fidl::Server<fuchsia_hardware_adb::Device> {
   public:
    explicit DeviceServer(FakeAdbDriver* adb)
        : impl_(&adb->impl_), loop_(&kAsyncLoopConfigNeverAttachToThread) {
      EXPECT_EQ(loop_.StartThread("adb-test-device-thread"), ZX_OK);
    }

    void Bind(fidl::ServerEnd<fuchsia_hardware_adb::Device> server) {
      binding_.emplace(fidl::BindServer(loop_.dispatcher(), std::move(server), this));
    }

    void StartAdb(StartAdbRequest& request, StartAdbCompleter::Sync& completer) override {
      ASSERT_TRUE(expect_start_ > 0);
      expect_start_--;
      impl_->Bind(std::move(request.interface()));

      completer.Reply(fit::success());
    }

    void StopAdb(StopAdbCompleter::Sync& completer) override {
      stop_called_++;
      if (on_stop_) {
        on_stop_();
      }
      completer.Reply(fit::success());
    }

    std::atomic_uint32_t stop_called_ = 0;
    fit::closure on_stop_;

   private:
    friend class FakeAdbDriver;
    UsbAdbImplServer* impl_;

    std::atomic_uint32_t expect_start_ = 0;
    async::Loop loop_;
    std::optional<fidl::ServerBindingRef<fuchsia_hardware_adb::Device>> binding_;
  } device_;
};

class FakeAdb : public Adb, public component_testing::LocalComponentImpl {
 public:
  explicit FakeAdb(async_dispatcher_t* dispatcher) : Adb(dispatcher) {}

  // component_testing::LocalComponentImpl methods
  void OnStart() override { started_ = true; }

  bool IsStarted() const { return started_; }

  void OnStop() override { started_ = false; }

  zx::result<zx::socket> GetServiceSocket(std::string_view service_name,
                                          std::string_view args) override {
    if (!client_.is_valid()) {
      return zx::error(ZX_ERR_INTERNAL);
    }
    return zx::ok(std::move(client_));
  }

  void SetServiceSocket(zx::socket client) { client_ = std::move(client); }

 private:
  friend class AdbTest;

  bool started_ = false;
  fidl::BindingSet<fuchsia::hardware::adb::Provider> bindings_;
  zx::socket client_;
};

class AdbTest : public testing::Test {
 public:
  AdbTest()
      : fidl_loop_(&kAsyncLoopConfigNeverAttachToThread),
        dev_(std::make_unique<FakeAdb>(fidl_loop_.dispatcher())) {
    EXPECT_EQ(fidl_loop_.StartThread("adb-test-fidl-thread"), ZX_OK);
  }

  void SetUp() override {
    // The test device connector
    class TestConnector : public DeviceConnector {
     public:
      explicit TestConnector(FakeAdbDriver* fake_driver) : fake_driver_(fake_driver) {}

      zx::result<fidl::ClientEnd<fuchsia_hardware_adb::Device>> ConnectToFirstDevice() override {
        zx::channel server, client;
        EXPECT_EQ(zx::channel::create(0, &server, &client), ZX_OK);
        fake_driver_->device_.Bind(
            fidl::ServerEnd<fuchsia_hardware_adb::Device>(std::move(server)));
        return zx::ok(fidl::ClientEnd<fuchsia_hardware_adb::Device>(std::move(client)));
      }

     private:
      FakeAdbDriver* fake_driver_;
    } test_connector(&fake_driver_);
    fake_driver_.ExpectStart();
    EXPECT_EQ(dev_->Init(&test_connector), ZX_OK);
  }
  void TearDown() override {
    while (!fake_driver_.expectations_empty()) {
      usleep(1'000);
    }
    fake_driver_.TearDown();
    fidl_loop_.RunUntilIdle();
    dev_.reset();
    fidl_loop_.Shutdown();
    set_system_type(kCsDevice);
  }

 private:
 protected:
  void WaitForImplTeardown() {
    if (auto teardown = dev_->impl_teardown_) {
      EXPECT_EQ(teardown->Wait(zx::sec(5)), ZX_OK);
    }
  }

  void SendConnect() {
    std::string connection_string =
        "device::ro.product.name=zircon;ro.product.model=zircon;ro.product.device=zircon;";
    fake_driver_.ExpectQueueTx(
        0, {
               .msg =
                   {
                       .command = A_CNXN,
                       .arg0 = A_VERSION,
                       .arg1 = MAX_PAYLOAD,
                       .data_length = static_cast<uint32_t>(connection_string.size()),
                       .data_check = 0,
                       .magic = A_CNXN ^ 0xffffffff,
                   },
               .payload = {},
           });
    fake_driver_.ExpectQueueTx(
        0, std::vector<uint8_t>(connection_string.begin(), connection_string.end()));
    fake_driver_.SendConnect();
    while (!fake_driver_.expectations_empty()) {
      usleep(1'000);
    }
  }

  async::Loop fidl_loop_;
  FakeAdbDriver fake_driver_;
  std::unique_ptr<FakeAdb> dev_;
};

TEST_F(AdbTest, SendUsbPacketTest) {
  uint8_t test[] = {0x00, 0x01, 0x02, 0x03, 0x04, 0x05};
  fake_driver_.ExpectQueueTx(0, std::vector(test, test + sizeof(test)));
  dev_->SendUsbPacket(test, sizeof(test));
}

TEST_F(AdbTest, ConnectTest) {
  // Send A_CNXN
  std::string connection_string =
      "device::ro.product.name=zircon;ro.product.model=zircon;ro.product.device=zircon;";
  fake_driver_.ExpectQueueTx(
      0, {
             .msg =
                 {
                     .command = A_CNXN,
                     .arg0 = A_VERSION,
                     .arg1 = MAX_PAYLOAD,
                     .data_length = static_cast<uint32_t>(connection_string.size()),
                     .data_check = 0,
                     .magic = A_CNXN ^ 0xffffffff,
                 },
             .payload = {},
         });
  fake_driver_.ExpectQueueTx(
      0, std::vector<uint8_t>(connection_string.begin(), connection_string.end()));
  fake_driver_.SendConnect();
}

TEST_F(AdbTest, SplitHeaderConnectTest) {
  std::string connection_string =
      "device::ro.product.name=zircon;ro.product.model=zircon;ro.product.device=zircon;";
  fake_driver_.ExpectQueueTx(
      0, {
             .msg =
                 {
                     .command = A_CNXN,
                     .arg0 = A_VERSION,
                     .arg1 = MAX_PAYLOAD,
                     .data_length = static_cast<uint32_t>(connection_string.size()),
                     .data_check = 0,
                     .magic = A_CNXN ^ 0xffffffff,
                 },
             .payload = {},
         });
  fake_driver_.ExpectQueueTx(
      0, std::vector<uint8_t>(connection_string.begin(), connection_string.end()));

  amessage msg{
      .command = A_CNXN,
      .arg0 = A_VERSION,
      .arg1 = MAX_PAYLOAD,
      .data_length = 234,
      .data_check = 0x5b44,
      .magic = A_CNXN ^ 0xffffffff,
  };
  const auto* msg_bytes = reinterpret_cast<const uint8_t*>(&msg);
  constexpr size_t kFirstFragmentSize = 10;
  fake_driver_.impl_.ExpectReceive(std::vector<uint8_t>(msg_bytes, msg_bytes + kFirstFragmentSize));
  fake_driver_.impl_.ExpectReceive(
      std::vector<uint8_t>(msg_bytes + kFirstFragmentSize, msg_bytes + sizeof(amessage)));

  std::string host_banner = "host::features=shell_v2,cmd,stat_v2,ls_v2,fixed_pu";
  std::vector<uint8_t> payload(host_banner.begin(), host_banner.end());
  payload.resize(234);
  fake_driver_.impl_.ExpectReceive(std::move(payload));
}

TEST_F(AdbTest, OversizedPayloadHeaderRejectedTest) {
  fuchsia_logging::LogSettingsBuilder()
      .WithMinLogSeverity(fuchsia_logging::LogSeverity::Fatal)
      .BuildAndInitialize();
  auto restore_logs = fit::defer([this]() {
    while (!fake_driver_.expectations_empty()) {
      usleep(1'000);
    }
    fuchsia_logging::LogSettingsBuilder()
        .WithMinLogSeverity(fuchsia_logging::kDefaultLogLevel)
        .BuildAndInitialize();
  });

  // Header with untrusted data_length > MAX_PAYLOAD (0xffffffff).
  fake_driver_.impl_.ExpectReceive({
      .msg =
          {
              .command = A_CNXN,
              .arg0 = A_VERSION,
              .arg1 = MAX_PAYLOAD,
              .data_length = 0xffffffff,
              .data_check = 0,
              .magic = A_CNXN ^ 0xffffffff,
          },
      .payload = {},
  });

  // Subsequent valid A_CNXN should still succeed.
  std::string connection_string =
      "device::ro.product.name=zircon;ro.product.model=zircon;ro.product.device=zircon;";
  fake_driver_.ExpectQueueTx(
      0, {
             .msg =
                 {
                     .command = A_CNXN,
                     .arg0 = A_VERSION,
                     .arg1 = MAX_PAYLOAD,
                     .data_length = static_cast<uint32_t>(connection_string.size()),
                     .data_check = 0,
                     .magic = A_CNXN ^ 0xffffffff,
                 },
             .payload = {},
         });
  fake_driver_.ExpectQueueTx(
      0, std::vector<uint8_t>(connection_string.begin(), connection_string.end()));
  fake_driver_.SendConnect();
}

TEST_F(AdbTest, InvalidMagicHeaderRejectedTest) {
  fuchsia_logging::LogSettingsBuilder()
      .WithMinLogSeverity(fuchsia_logging::LogSeverity::Fatal)
      .BuildAndInitialize();
  auto restore_logs = fit::defer([this]() {
    while (!fake_driver_.expectations_empty()) {
      usleep(1'000);
    }
    fuchsia_logging::LogSettingsBuilder()
        .WithMinLogSeverity(fuchsia_logging::kDefaultLogLevel)
        .BuildAndInitialize();
  });

  // Split header with invalid magic, also exercising pending_packet_ and copied_len_ reset.
  amessage bad_msg{
      .command = A_CNXN,
      .arg0 = A_VERSION,
      .arg1 = MAX_PAYLOAD,
      .data_length = 234,
      .data_check = 0,
      .magic = 0,
  };
  const auto* bad_msg_bytes = reinterpret_cast<const uint8_t*>(&bad_msg);
  constexpr size_t kFirstFragmentSize = 10;
  fake_driver_.impl_.ExpectReceive(
      std::vector<uint8_t>(bad_msg_bytes, bad_msg_bytes + kFirstFragmentSize));
  fake_driver_.impl_.ExpectReceive(
      std::vector<uint8_t>(bad_msg_bytes + kFirstFragmentSize, bad_msg_bytes + sizeof(amessage)));

  // Subsequent valid A_CNXN should still succeed.
  std::string connection_string =
      "device::ro.product.name=zircon;ro.product.model=zircon;ro.product.device=zircon;";
  fake_driver_.ExpectQueueTx(
      0, {
             .msg =
                 {
                     .command = A_CNXN,
                     .arg0 = A_VERSION,
                     .arg1 = MAX_PAYLOAD,
                     .data_length = static_cast<uint32_t>(connection_string.size()),
                     .data_check = 0,
                     .magic = A_CNXN ^ 0xffffffff,
                 },
             .payload = {},
         });
  fake_driver_.ExpectQueueTx(
      0, std::vector<uint8_t>(connection_string.begin(), connection_string.end()));
  fake_driver_.SendConnect();
}

TEST_F(AdbTest, RecoveryConnectTest) {
  set_system_type(kCsRecovery);
  std::string connection_string =
      "recovery::ro.product.name=zircon;ro.product.model=zircon;ro.product.device=zircon;";
  fake_driver_.ExpectQueueTx(
      0, {
             .msg =
                 {
                     .command = A_CNXN,
                     .arg0 = A_VERSION,
                     .arg1 = MAX_PAYLOAD,
                     .data_length = static_cast<uint32_t>(connection_string.size()),
                     .data_check = 0,
                     .magic = A_CNXN ^ 0xffffffff,
                 },
             .payload = {},
         });
  fake_driver_.ExpectQueueTx(
      0, std::vector<uint8_t>(connection_string.begin(), connection_string.end()));
  fake_driver_.SendConnect();
}

TEST_F(AdbTest, ResetTest) {
  fake_driver_.ExpectStart();
  EXPECT_EQ(dev_->Reset(), ZX_OK);
  EXPECT_EQ(fake_driver_.device_.stop_called_.load(), 1UL);

  while (!fake_driver_.expectations_empty()) {
    usleep(1'000);
  }
}

TEST_F(AdbTest, ResetClearsIncompletePacketTest) {
  // Deliver a partial ADB header so |pending_packet_| and |copied_len_| hold incomplete state.
  ASSERT_EQ(fake_driver_.impl_.receive_armed_.Wait(zx::sec(5)), ZX_OK);
  fake_driver_.impl_.ExpectReceive(std::vector<uint8_t>(10, 0xff));
  ASSERT_EQ(fake_driver_.impl_.receive_armed_.Wait(zx::sec(5)), ZX_OK);
  libsync::Completion callback_done;
  ASSERT_EQ(async::PostTask(fidl_loop_.dispatcher(), [&]() { callback_done.Signal(); }), ZX_OK);
  ASSERT_EQ(callback_done.Wait(zx::sec(5)), ZX_OK);

  fake_driver_.ExpectStart();
  EXPECT_EQ(dev_->Reset(), ZX_OK);

  SendConnect();
}

TEST_F(AdbTest, ResetWhenDriverClosesImplDuringStopTest) {
  fake_driver_.device_.on_stop_ = [this]() {
    fake_driver_.impl_.Close();
    WaitForImplTeardown();
  };
  fake_driver_.ExpectStart();
  EXPECT_EQ(dev_->Reset(), ZX_OK);
  EXPECT_EQ(fake_driver_.device_.stop_called_.load(), 1UL);
  EXPECT_TRUE(fake_driver_.expectations_empty());

  SendConnect();
}

TEST(StateControllerTest, SetSystemTypeHandlesResetError) {
  auto restore_type = fit::defer([prev = get_system_type()] { set_system_type(prev); });
  set_system_type(kCsRecovery);
  std::atomic<zx_status_t> reset_status = ZX_ERR_PEER_CLOSED;
  std::atomic<ConnectionState> expected_callback_state = kCsSideload;
  std::atomic_uint32_t reset_count = 0;

  StateControllerServer server;
  server.set_reset_callback([&]() {
    EXPECT_EQ(get_system_type(), expected_callback_state.load());
    reset_count++;
    return reset_status.load();
  });

  async::Loop loop{&kAsyncLoopConfigNeverAttachToThread};
  ASSERT_EQ(loop.StartThread("state-controller-test-thread"), ZX_OK);
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_hardware_adb::StateController>::Create();
  fidl::BindServer(loop.dispatcher(), std::move(server_end), &server);
  fidl::SyncClient<fuchsia_hardware_adb::StateController> client(std::move(client_end));

  auto failed = client->SetSystemType({fuchsia_hardware_adb::SystemType::kSideload});
  ASSERT_TRUE(failed.is_error());
  ASSERT_TRUE(failed.error_value().is_domain_error());
  EXPECT_EQ(failed.error_value().domain_error(), ZX_ERR_PEER_CLOSED);
  EXPECT_EQ(get_system_type(), kCsRecovery);
  EXPECT_EQ(reset_count.load(), 1u);

  // Requesting the restored system type after a failed reset must still reset adb because
  // UsbAdbImpl was stopped before StartUsbAdbImpl() failed.
  reset_status = ZX_OK;
  expected_callback_state = kCsRecovery;
  EXPECT_TRUE(client->SetSystemType({fuchsia_hardware_adb::SystemType::kRecovery}).is_ok());
  EXPECT_EQ(get_system_type(), kCsRecovery);
  EXPECT_EQ(reset_count.load(), 2u);

  // Repeating the active system type once healthy is a no-op.
  EXPECT_TRUE(client->SetSystemType({fuchsia_hardware_adb::SystemType::kRecovery}).is_ok());
  EXPECT_EQ(reset_count.load(), 2u);

  loop.Shutdown();
}

// Integration tests
class FakeAdbServiceProvider : public fuchsia::hardware::adb::Provider,
                               public component_testing::LocalComponentImpl {
 public:
  explicit FakeAdbServiceProvider(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  // fuchsia_hardware_adb::Provider methods
  void ConnectToService(zx::socket socket, fidl::StringPtr args,
                        ConnectToServiceCallback callback) override {
    socket_ = std::move(socket);
    callback(fuchsia::hardware::adb::Provider_ConnectToService_Result::WithResponse(
        fuchsia::hardware::adb::Provider_ConnectToService_Response()));
  }

  // component_testing::LocalComponentImpl methods
  void OnStart() override {
    started_ = true;
    ASSERT_EQ(outgoing()->AddPublicService(bindings_.GetHandler(this, dispatcher_)), ZX_OK);
  }

  bool IsStarted() const { return started_; }

  void OnStop() override { started_ = false; }

  zx::socket& socket() { return socket_; }

 private:
  bool started_ = false;
  async_dispatcher_t* dispatcher_;
  fidl::BindingSet<fuchsia::hardware::adb::Provider> bindings_;
  zx::socket socket_;
};

class AdbRealmTest : public AdbTest, public loop_fixture::RealLoop {
 public:
  void SetUp() override {
    AdbTest::SetUp();
    SendConnect();
    BuildRealm();
  }

  void TearDown() override {
    bool complete = false;
    realm_->Teardown([&](fit::result<fuchsia::component::Error> result) { complete = true; });
    RunLoopUntil([&]() { return complete; });
    AdbTest::TearDown();
  }

  void SendConnect() {
    // Send A_CNXN
    std::string connection_string =
        "device::ro.product.name=zircon;ro.product.model=zircon;ro.product.device=zircon;";
    fake_driver_.ExpectQueueTx(
        0, {
               .msg =
                   {
                       .command = A_CNXN,
                       .arg0 = A_VERSION,
                       .arg1 = MAX_PAYLOAD,
                       .data_length = static_cast<uint32_t>(connection_string.size()),
                       .data_check = 0,
                       .magic = A_CNXN ^ 0xffffffff,
                   },
               .payload = {},
           });
    fake_driver_.ExpectQueueTx(
        0, std::vector<uint8_t>(connection_string.begin(), connection_string.end()));
    fake_driver_.SendConnect();
    while (!fake_driver_.expectations_empty()) {
      usleep(1'000);
    }
  }

  void BuildRealm() {
    auto builder = RealmBuilder::Create();
    dev_ptr_ = dev_.get();
    builder.AddLocalChild(
        "adb", [&]() { return std::move(dev_); }, ChildOptions{.startup_mode = StartupMode::EAGER});

    auto service_provider = std::make_unique<FakeAdbServiceProvider>(dispatcher());
    service_provider_ptr_ = service_provider.get();

    // Add component to the realm, providing a mock implementation
    builder.AddLocalChild(
        std::string(kShellService),
        [provider = std::move(service_provider)]() mutable { return std::move(provider); },
        ChildOptions{});

    builder.AddRoute(Route{.capabilities = {Protocol{fuchsia::hardware::adb::Provider::Name_}},
                           .source = ChildRef{std::string(kShellService)},
                           .targets = {ParentRef()}});
    realm_ = builder.Build(dispatcher());
    RunLoopUntil([&]() { return dev_ptr_->IsStarted(); });
  }

 protected:
  std::optional<RealmRoot> realm_;
  FakeAdbServiceProvider* service_provider_ptr_ = nullptr;
  FakeAdb* dev_ptr_ = nullptr;
};

TEST_F(AdbRealmTest, ServiceConnectTest) {
  auto provider = realm_->component().Connect<fuchsia::hardware::adb::Provider>();

  zx::socket server, client;
  EXPECT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);
  provider->ConnectToService(std::move(server), "",
                             [&](fuchsia::hardware::adb::Provider_ConnectToService_Result result) {
                               EXPECT_FALSE(result.is_err());
                               QuitLoop();
                             });

  RunLoop();
}

TEST_F(AdbRealmTest, ServiceOpenCloseTest) {
  auto provider = realm_->component().Connect<fuchsia::hardware::adb::Provider>();
  zx::socket server, client;
  EXPECT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  provider->ConnectToService(std::move(server), "",
                             [&](fuchsia::hardware::adb::Provider_ConnectToService_Result result) {
                               EXPECT_FALSE(result.is_err());
                               QuitLoop();
                             });

  RunLoop();
  dev_ptr_->SetServiceSocket(std::move(client));

  // Send A_OPEN
  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_OKAY,
                                            .arg0 = 1,  // local-id
                                            .arg1 = 1,  // remote-id
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_OKAY ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  fake_driver_.SendOpen({'s', 'h', 'e', 'l', 'l', ':', '\0'});

  while (!fake_driver_.expectations_empty()) {
    usleep(1'000);
  }

  // Expect A_CLSE
  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_CLSE,
                                            .arg0 = 1,  // local-id
                                            .arg1 = 1,  // remote-id
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_CLSE ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  service_provider_ptr_->socket().reset();
  while (!fake_driver_.expectations_empty()) {
    usleep(1'000);
  }
}

TEST_F(AdbRealmTest, ServiceReadWriteTest) {
  auto provider = realm_->component().Connect<fuchsia::hardware::adb::Provider>();
  zx::socket server, client;
  EXPECT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  provider->ConnectToService(std::move(server), "",
                             [&](fuchsia::hardware::adb::Provider_ConnectToService_Result result) {
                               EXPECT_FALSE(result.is_err());
                               QuitLoop();
                             });

  RunLoop();
  dev_ptr_->SetServiceSocket(std::move(client));

  uint8_t helloworld[] = {'H', 'e', 'l', 'l', 'o', 'W', 'o', 'r', 'l', 'd', '\0'};
  size_t actual;

  // Open service
  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_OKAY,
                                            .arg0 = 1,  // local-id
                                            .arg1 = 1,  // remote-id
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_OKAY ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  fake_driver_.SendOpen({'s', 'h', 'e', 'l', 'l', ':', '\0'});

  // Test sending data to the service.
  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_OKAY,
                                            .arg0 = 1,  // local-id
                                            .arg1 = 1,  // remote-id
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_OKAY ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  fake_driver_.SendWrite(std::vector<uint8_t>(helloworld, helloworld + sizeof(helloworld)));
  while (!fake_driver_.expectations_empty()) {
    usleep(1'000);
  }

  zx_signals_t signal;
  ASSERT_EQ(service_provider_ptr_->socket().wait_one(ZX_SOCKET_READABLE | ZX_SOCKET_PEER_CLOSED,
                                                     zx::deadline_after(zx::duration::infinite()),
                                                     &signal),
            ZX_OK);
  ASSERT_FALSE((signal & ZX_SOCKET_READABLE) == 0);
  char out_helloworld[11];
  EXPECT_EQ(
      service_provider_ptr_->socket().read(0, out_helloworld, sizeof(out_helloworld), &actual),
      ZX_OK);
  EXPECT_EQ(actual, sizeof(out_helloworld));

  EXPECT_EQ(sizeof(helloworld), sizeof(out_helloworld));
  for (size_t i = 0; i < sizeof(helloworld); i++) {
    EXPECT_EQ(helloworld[i], out_helloworld[i]);
  }

  // Test service sending data to the client.
  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_WRTE,
                                            .arg0 = 1,  // local-id
                                            .arg1 = 1,  // remote-id
                                            .data_length = sizeof(helloworld),
                                            .data_check = 0,
                                            .magic = A_WRTE ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  // Payload is sent in the next packet.
  std::vector<uint8_t> expect_data(sizeof(helloworld));
  std::copy(helloworld, helloworld + sizeof(helloworld), expect_data.begin());
  fake_driver_.ExpectQueueTx(0, expect_data);
  ASSERT_EQ(service_provider_ptr_->socket().write(0, helloworld, sizeof(helloworld), &actual),
            ZX_OK);
  ASSERT_EQ(actual, sizeof(helloworld));

  while (!fake_driver_.expectations_empty()) {
    usleep(1'000);
  }

  // Expect A_CLSE
  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_CLSE,
                                            .arg0 = 1,  // local-id
                                            .arg1 = 1,  // remote-id
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_CLSE ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  service_provider_ptr_->socket().reset();
  while (!fake_driver_.expectations_empty()) {
    usleep(1'000);
  }
}

namespace {

struct RecordingConnection : public Connection {
  bool Write(std::unique_ptr<apacket> packet) override {
    std::lock_guard<std::mutex> lock(mutex);
    if (packet->msg.command == A_OKAY) {
      okay_packets.push_back(std::move(packet));
    }
    return true;
  }
  void Start() override {}
  void Stop() override {}
  bool DoTlsHandshake(char* key, std::string* auth_key) override { return true; }
  void* adb() override { return nullptr; }

  size_t okay_count() {
    std::lock_guard<std::mutex> lock(mutex);
    return okay_packets.size();
  }

  std::mutex mutex;
  std::vector<std::unique_ptr<apacket>> okay_packets;
};

size_t FillSocketToCapacity(const zx::socket& sock) {
  size_t total = 0;
  std::vector<uint8_t> chunk(4096, 0x5a);
  while (true) {
    size_t actual = 0;
    zx_status_t status = sock.write(0, chunk.data(), chunk.size(), &actual);
    if (status == ZX_OK) {
      total += actual;
    } else {
      EXPECT_EQ(status, ZX_ERR_SHOULD_WAIT);
      break;
    }
  }
  uint8_t byte = 0x5a;
  while (true) {
    size_t actual = 0;
    zx_status_t status = sock.write(0, &byte, 1, &actual);
    if (status == ZX_OK) {
      total += actual;
    } else {
      EXPECT_EQ(status, ZX_ERR_SHOULD_WAIT);
      break;
    }
  }
  return total;
}

void CloseAndWaitLocalSocket(asocket* local) {
  local->zx_socket.reset();
  const unsigned local_id = local->id;
  local->close(local);
  while (find_local_socket(local_id, 0) != nullptr) {
    sched_yield();
  }
}

void DrainSocketBytes(const zx::socket& server, size_t remaining) {
  std::vector<uint8_t> read_buf(4096);
  while (remaining > 0) {
    zx_signals_t pending = 0;
    zx_status_t status =
        server.wait_one(ZX_SOCKET_READABLE | ZX_SOCKET_PEER_CLOSED, zx::time::infinite(), &pending);
    EXPECT_EQ(status, ZX_OK);
    if (status != ZX_OK) {
      return;
    }
    if (pending & ZX_SOCKET_READABLE) {
      size_t read_bytes = 0;
      status = server.read(0, read_buf.data(), std::min(read_buf.size(), remaining), &read_bytes);
      EXPECT_EQ(status, ZX_OK);
      if (status != ZX_OK) {
        return;
      }
      remaining -= read_bytes;
      continue;
    }
    ADD_FAILURE() << "Socket peer closed before draining remaining " << remaining << " bytes";
    return;
  }
}

}  // namespace

TEST(AdbSocketFlowControlTest, LegacyModeSendsSingleOkayAfterPartialWritesComplete) {
  atransport t(kCsDevice);
  t.online = true;
  t.SetDelayedAck(false);
  auto conn = std::make_shared<RecordingConnection>();
  t.SetConnection(conn);

  asocket* local = create_local_socket();
  local->transport = &t;
  local->available_send_bytes = std::nullopt;
  asocket* remote = create_remote_socket(10, &t);
  local->peer = remote;
  remote->peer = local;

  zx::socket server, client;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  constexpr size_t kInitialFreeSpace = 100;
  constexpr size_t kPayloadSize = 250;
  const size_t capacity = FillSocketToCapacity(client);
  ASSERT_GT(capacity, kInitialFreeSpace);

  std::vector<uint8_t> drain_buf(kInitialFreeSpace);
  size_t actual = 0;
  ASSERT_EQ(server.read(0, drain_buf.data(), drain_buf.size(), &actual), ZX_OK);
  ASSERT_EQ(actual, kInitialFreeSpace);

  local->zx_socket = std::move(client);

  std::thread reader(
      [&](zx::socket server_end) {
        // Own `server_end` inside the lambda body so any early return closes the read end
        // immediately, unblocking `local->enqueue()` via ZX_ERR_PEER_CLOSED instead of deadlocking.
        zx::socket reader_server = std::move(server_end);

        // Wait until local_socket_flush_incoming writes the first 100 bytes and fills the socket.
        while (true) {
          zx_info_socket_t info{};
          zx_status_t status =
              reader_server.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr);
          EXPECT_EQ(status, ZX_OK);
          if (status != ZX_OK) {
            return;
          }
          if (info.rx_buf_available == capacity) {
            break;
          }
          zx_signals_t observed = 0;
          if (reader_server.wait_one(ZX_SOCKET_PEER_CLOSED, zx::time::infinite_past(), &observed) ==
              ZX_OK) {
            status = reader_server.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr);
            EXPECT_EQ(status, ZX_OK);
            EXPECT_EQ(info.rx_buf_available, capacity);
            if (status != ZX_OK || info.rx_buf_available != capacity) {
              return;
            }
            break;
          }
          sched_yield();
        }
        // No A_OKAY should have been sent after only a partial write of the packet.
        EXPECT_EQ(conn->okay_count(), 0u);

        // Drain the socket so the remaining 150 bytes can be flushed.
        DrainSocketBytes(reader_server, (capacity - kInitialFreeSpace) + kPayloadSize);
      },
      std::move(server));

  apacket::payload_type payload(kPayloadSize);
  memset(payload.data(), 'A', payload.size());
  EXPECT_EQ(local->enqueue(local, std::move(payload)), 0);
  // Reset `zx_socket` before joining `reader` so `ZX_SOCKET_PEER_CLOSED` wakes `reader`
  // immediately if `enqueue()` returns without writing all expected bytes.
  local->zx_socket.reset();
  reader.join();

  ASSERT_EQ(conn->okay_packets.size(), 1u);
  EXPECT_EQ(conn->okay_packets[0]->msg.command, static_cast<uint32_t>(A_OKAY));
  EXPECT_EQ(conn->okay_packets[0]->msg.arg0, local->id);
  EXPECT_EQ(conn->okay_packets[0]->msg.arg1, remote->id);
  EXPECT_EQ(conn->okay_packets[0]->msg.data_length, 0u);

  CloseAndWaitLocalSocket(local);
}

TEST(AdbSocketFlowControlTest, DelayedAckModeSkipsZeroByteOkayAndReportsExactFlushedBytes) {
  atransport t(kCsDevice);
  t.online = true;
  t.SetDelayedAck(true);
  auto conn = std::make_shared<RecordingConnection>();
  t.SetConnection(conn);

  asocket* local = create_local_socket();
  local->transport = &t;
  local->available_send_bytes = INITIAL_DELAYED_ACK_BYTES;
  asocket* remote = create_remote_socket(20, &t);
  local->peer = remote;
  remote->peer = local;

  zx::socket server, client;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  constexpr size_t kInitialFreeSpace = 100;
  constexpr size_t kPayloadSize = 250;
  const size_t capacity = FillSocketToCapacity(client);
  ASSERT_GT(capacity, kInitialFreeSpace);

  std::vector<uint8_t> drain_buf(kInitialFreeSpace);
  size_t actual = 0;
  ASSERT_EQ(server.read(0, drain_buf.data(), drain_buf.size(), &actual), ZX_OK);
  ASSERT_EQ(actual, kInitialFreeSpace);

  local->zx_socket = std::move(client);

  std::thread reader(
      [&](zx::socket server_end) {
        zx::socket reader_server = std::move(server_end);

        // Wait until the first 100-byte flush emits its A_OKAY and fills the socket.
        while (conn->okay_count() == 0) {
          zx_signals_t observed = 0;
          if (reader_server.wait_one(ZX_SOCKET_PEER_CLOSED, zx::time::infinite_past(), &observed) ==
                  ZX_OK &&
              conn->okay_count() == 0) {
            ADD_FAILURE() << "Socket peer closed before initial A_OKAY was sent";
            return;
          }
          sched_yield();
        }
        // Give the enqueue loop a chance to hit ZX_ERR_SHOULD_WAIT while the socket is full.
        for (int i = 0; i < 10; ++i) {
          sched_yield();
        }
        // Verify no 0-byte A_OKAY packets were spammed while the socket was full.
        EXPECT_EQ(conn->okay_count(), 1u);

        DrainSocketBytes(reader_server, (capacity - kInitialFreeSpace) + kPayloadSize);
      },
      std::move(server));

  apacket::payload_type payload(kPayloadSize);
  memset(payload.data(), 'B', payload.size());
  EXPECT_EQ(local->enqueue(local, std::move(payload)), 0);
  local->zx_socket.reset();
  reader.join();

  ASSERT_EQ(conn->okay_packets.size(), 2u);
  uint32_t total_acked = 0;
  for (const auto& pkt : conn->okay_packets) {
    ASSERT_EQ(pkt->msg.data_length, sizeof(uint32_t));
    ASSERT_EQ(pkt->payload.size(), sizeof(uint32_t));
    uint32_t acked = 0;
    memcpy(&acked, pkt->payload.data(), sizeof(acked));
    EXPECT_GT(acked, 0u);
    total_acked += acked;
  }
  EXPECT_EQ(total_acked, kPayloadSize);

  CloseAndWaitLocalSocket(local);
}

TEST(AdbSocketFlowControlTest, DelayedAckModeDoesNotSendZeroByteOkayOnClosedPeer) {
  atransport t(kCsDevice);
  t.online = true;
  t.SetDelayedAck(true);
  auto conn = std::make_shared<RecordingConnection>();
  t.SetConnection(conn);

  asocket* local = create_local_socket();
  local->transport = &t;
  local->available_send_bytes = INITIAL_DELAYED_ACK_BYTES;
  asocket* remote = create_remote_socket(30, &t);
  local->peer = remote;
  remote->peer = local;

  zx::socket server, client;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);
  server.reset();
  local->zx_socket = std::move(client);

  apacket::payload_type payload(64);
  memset(payload.data(), 'C', payload.size());
  EXPECT_EQ(local->enqueue(local, std::move(payload)), 0);
  EXPECT_EQ(conn->okay_count(), 0u);

  CloseAndWaitLocalSocket(local);
}

}  // namespace adb
