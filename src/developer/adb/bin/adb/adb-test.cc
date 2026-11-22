// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "adb.h"

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/fit/defer.h>
#include <lib/sync/cpp/completion.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/clock.h>

#include <algorithm>
#include <cstring>
#include <memory>
#include <mutex>
#include <optional>
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
  explicit FakeAdbDriver() : impl_(this), device_(this) {}

  bool expectations_empty() {
    fbl::AutoLock _(&impl_.lock_);
    return device_.expect_start_ == 0 && impl_.expect_queue_tx_.empty() &&
           impl_.expect_receive_.empty();
  }

  void WaitForExpectations(zx::duration timeout = zx::sec(5)) {
    const zx::time deadline = zx::deadline_after(timeout);
    while (true) {
      expectations_changed_.Reset();
      if (expectations_empty() || testing::Test::HasFailure()) {
        return;
      }
      const zx::duration remaining = deadline - zx::clock::get_monotonic();
      if (remaining <= zx::duration(0)) {
        ADD_FAILURE() << "Timed out waiting for FakeAdbDriver expectations";
        return;
      }
      const zx_status_t status = expectations_changed_.Wait(std::min(remaining, zx::msec(10)));
      if (status != ZX_OK && status != ZX_ERR_TIMED_OUT) {
        ADD_FAILURE() << "Unexpected Wait status: " << status;
        return;
      }
    }
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
  void ExpectDelayedAckOkay(uint32_t local_id, uint32_t remote_id, uint32_t ack_bytes) {
    ExpectQueueTx(0, {
                         .msg =
                             {
                                 .command = A_OKAY,
                                 .arg0 = local_id,
                                 .arg1 = remote_id,
                                 .data_length = sizeof(ack_bytes),
                                 .data_check = 0,
                                 .magic = static_cast<uint32_t>(A_OKAY) ^ 0xffffffffu,
                             },
                         .payload = {},
                     });
    std::vector<uint8_t> payload(sizeof(ack_bytes));
    memcpy(payload.data(), &ack_bytes, sizeof(ack_bytes));
    ExpectQueueTx(0, std::move(payload));
  }

  void SendConnect(bool delayed_ack = false) {
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
    std::string connection_string =
        delayed_ack ? "host::features=shell_v2,cmd,stat_v2,ls_v2,fixed_pu,delayed_ack;"
                    : "host::features=shell_v2,cmd,stat_v2,ls_v2,fixed_pu";
    std::vector<uint8_t> data = {connection_string.begin(), connection_string.end()};
    data.resize(234);
    impl_.ExpectReceive(std::move(data));
  }

  void SendOpen(std::vector<uint8_t> data, uint32_t arg1 = 0) {
    impl_.ExpectReceive({.msg =
                             {
                                 .command = A_OPEN,
                                 .arg0 = 1,  // local-id:1
                                 .arg1 = arg1,
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

  void SendClose() {
    impl_.ExpectReceive({.msg =
                             {
                                 .command = A_CLSE,
                                 .arg0 = 1,  // local-id:1
                                 .arg1 = 1,  // remote-id: 1
                                 .data_length = 0,
                                 .data_check = 0,
                                 .magic = A_CLSE ^ 0xffffffff,
                             },
                         .payload = {}});
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
    explicit UsbAdbImplServer(FakeAdbDriver* driver)
        : driver_(driver), loop_(&kAsyncLoopConfigNeverAttachToThread) {
      EXPECT_EQ(loop_.StartThread("adb-test-impl-thread"), ZX_OK);
    }

    ~UsbAdbImplServer() override {
      Close();
      // Shut down before member destructors run so safety does not depend on |loop_| being last.
      loop_.Shutdown();
    }

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
      if (zx_status_t err = queue_tx_error_.exchange(ZX_OK); err != ZX_OK) {
        completer.Reply(fit::error(err));
        queue_tx_failed_.Signal();
        return;
      }
      if (ignore_okay_queue_tx_ && request.data().size() == sizeof(amessage)) {
        amessage msg;
        memcpy(&msg, request.data().data(), sizeof(amessage));
        if (msg.command == A_OKAY) {
          completer.Reply(fit::success());
          return;
        }
      }
      std::vector<uint8_t> expect;
      {
        fbl::AutoLock _(&lock_);
        if (expect_queue_tx_.empty()) {
          ADD_FAILURE() << "Unexpected QueueTx of " << request.data().size() << " bytes";
          completer.Reply(fit::error(ZX_ERR_BAD_STATE));
          driver_->expectations_changed_.Signal();
          return;
        }
        expect = expect_queue_tx_.front();
      }
      EXPECT_EQ(request.data().size(), expect.size());
      EXPECT_TRUE(
          std::equal(request.data().begin(), request.data().end(), expect.begin(), expect.end()));
      completer.Reply(fit::success());
      {
        fbl::AutoLock _(&lock_);
        if (!expect_queue_tx_.empty()) {
          expect_queue_tx_.pop();
        }
      }
      driver_->expectations_changed_.Signal();
    }

    // Neither Receive nor ExpectReceive should block the thread. If either blocks the thread,
    // execution will not be able to proceed.
    void Receive(ReceiveCompleter::Sync& completer) override {
      {
        fbl::AutoLock _(&lock_);
        if (expect_receive_.empty()) {
          receive_completer_.emplace(completer.ToAsync());
          receive_armed_.Signal();
          return;
        }
        completer.Reply(fit::ok(std::move(expect_receive_.front())));
        expect_receive_.pop();
      }
      driver_->expectations_changed_.Signal();
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

    FakeAdbDriver* driver_;
    fbl::Mutex lock_;
    // When set, QueueTx() accepts A_OKAY headers without checking |expect_queue_tx_|.
    std::atomic_bool ignore_okay_queue_tx_ = false;
    // When not ZX_OK, the next QueueTx() replies with this error and signals |queue_tx_failed_|.
    std::atomic<zx_status_t> queue_tx_error_ = ZX_OK;
    libsync::Completion queue_tx_failed_;
    libsync::Completion receive_armed_;
    std::shared_ptr<libsync::Completion> unbound_ __TA_GUARDED(lock_);
    std::optional<fidl::ServerBindingRef<fuchsia_hardware_adb::UsbAdbImpl>> binding_
        __TA_GUARDED(lock_);
    std::queue<std::vector<uint8_t>> expect_queue_tx_ __TA_GUARDED(lock_);
    std::queue<std::vector<uint8_t>> expect_receive_ __TA_GUARDED(lock_);
    std::optional<ReceiveCompleter::Async> receive_completer_ __TA_GUARDED(lock_);
    async::Loop loop_;
  } impl_;

  class DeviceServer : public fidl::Server<fuchsia_hardware_adb::Device> {
   public:
    explicit DeviceServer(FakeAdbDriver* adb)
        : driver_(adb), impl_(&adb->impl_), loop_(&kAsyncLoopConfigNeverAttachToThread) {
      EXPECT_EQ(loop_.StartThread("adb-test-device-thread"), ZX_OK);
    }

    ~DeviceServer() override {
      // Shut down before member destructors run so safety does not depend on |loop_| being last.
      loop_.Shutdown();
    }

    void Bind(fidl::ServerEnd<fuchsia_hardware_adb::Device> server) {
      binding_.emplace(fidl::BindServer(loop_.dispatcher(), std::move(server), this));
    }

    void StartAdb(StartAdbRequest& request, StartAdbCompleter::Sync& completer) override {
      if (expect_start_ == 0) {
        ADD_FAILURE() << "Unexpected StartAdb call";
        completer.Reply(fit::error(ZX_ERR_BAD_STATE));
        driver_->expectations_changed_.Signal();
        return;
      }
      expect_start_--;
      driver_->expectations_changed_.Signal();
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
    FakeAdbDriver* driver_;
    UsbAdbImplServer* impl_;

    std::atomic_uint32_t expect_start_ = 0;
    std::optional<fidl::ServerBindingRef<fuchsia_hardware_adb::Device>> binding_;
    async::Loop loop_;
  } device_;

  libsync::Completion expectations_changed_;
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
    fake_driver_.WaitForExpectations();
    fake_driver_.TearDown();
    dev_.reset();
    fidl_loop_.Shutdown();
    set_system_type(kCsDevice);
  }

 private:
 protected:
  void WaitForImplTeardown() {
    std::shared_ptr<libsync::Completion> teardown;
    {
      std::lock_guard lock(dev_->impl_lock_);
      teardown = dev_->impl_teardown_;
    }
    if (teardown) {
      EXPECT_EQ(teardown->Wait(zx::sec(5)), ZX_OK);
    }
  }
  void QueueTransportPacket(apacket* packet) { send_packet(packet, &dev_->transport_); }

  void SendConnect(std::string_view system_type = "device", bool delayed_ack = false) {
    std::string connection_string = std::string(system_type) +
                                    "::ro.product.name=zircon;ro.product.model=zircon;"
                                    "ro.product.device=zircon;";
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
    fake_driver_.SendConnect(delayed_ack);
    fake_driver_.WaitForExpectations();
  }

  async::Loop fidl_loop_;
  FakeAdbDriver fake_driver_;
  std::unique_ptr<FakeAdb> dev_;
};

TEST_F(AdbTest, SendUsbPacketTest) {
  uint8_t test[] = {0x00, 0x01, 0x02, 0x03, 0x04, 0x05};
  fake_driver_.ExpectQueueTx(0, std::vector(test, test + sizeof(test)));
  EXPECT_TRUE(dev_->SendUsbPacket(test, sizeof(test)));
  fake_driver_.WaitForExpectations();
}

TEST_F(AdbTest, ConnectTest) { SendConnect(); }

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
  auto restore_logs = fit::defer([this] {
    fake_driver_.WaitForExpectations();
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
  SendConnect();
}

TEST_F(AdbTest, InvalidMagicHeaderRejectedTest) {
  fuchsia_logging::LogSettingsBuilder()
      .WithMinLogSeverity(fuchsia_logging::LogSeverity::Fatal)
      .BuildAndInitialize();
  auto restore_logs = fit::defer([this] {
    fake_driver_.WaitForExpectations();
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
  SendConnect();
}

TEST_F(AdbTest, RecoveryConnectTest) {
  set_system_type(kCsRecovery);
  SendConnect("recovery");
}

TEST_F(AdbTest, ResetTest) {
  fake_driver_.ExpectStart();
  EXPECT_EQ(dev_->Reset(), ZX_OK);
  EXPECT_EQ(fake_driver_.device_.stop_called_.load(), 1UL);
  EXPECT_TRUE(fake_driver_.expectations_empty());
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

TEST_F(AdbTest, WriteFailureDoesNotStopTransportTest) {
  fake_driver_.impl_.queue_tx_error_ = ZX_ERR_CANCELED;
  apacket* p = get_apacket();
  p->msg.command = A_OKAY;
  QueueTransportPacket(p);
  ASSERT_EQ(fake_driver_.impl_.queue_tx_failed_.Wait(zx::sec(5)), ZX_OK);

  SendConnect();
}

TEST_F(AdbTest, ResetWhileSendingTest) {
  // Outgoing packets sent via send_packet() on |transport_| race with StopAdb() closing UsbAdbImpl
  // and Reset() replacing the client and connection. Sends during the switch may fail or land on a
  // stopped connection, and the replacement connection must still transmit A_CNXN afterwards.
  fake_driver_.device_.on_stop_ = [this]() { fake_driver_.impl_.Close(); };
  fake_driver_.impl_.ignore_okay_queue_tx_ = true;
  std::atomic_bool stop = false;
  libsync::Completion sender_started;
  std::thread sender([this, &stop, &sender_started]() {
    while (!stop) {
      apacket* p = get_apacket();
      p->msg.command = A_OKAY;
      QueueTransportPacket(p);
      sender_started.Signal();
      // Sends to a torn down client fail and log a warning, so avoid spinning.
      zx::nanosleep(zx::deadline_after(zx::usec(10)));
    }
  });
  auto join_sender = fit::defer([&stop, &sender]() {
    stop = true;
    if (sender.joinable()) {
      sender.join();
    }
  });
  sender_started.Wait();

  constexpr uint32_t kResets = 20;
  for (uint32_t i = 0; i < kResets; i++) {
    fake_driver_.ExpectStart();
    EXPECT_EQ(dev_->Reset(), ZX_OK) << "at reset " << i;
  }
  join_sender.call();

  SendConnect();

  EXPECT_EQ(fake_driver_.device_.stop_called_.load(), kResets);
}

// Integration tests
class FakeAdbServiceProvider : public fidl::Server<fuchsia_hardware_adb::Provider>,
                               public component_testing::LocalComponentImpl {
 public:
  explicit FakeAdbServiceProvider(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  // fuchsia_hardware_adb::Provider methods
  void ConnectToService(ConnectToServiceRequest& request,
                        ConnectToServiceCompleter::Sync& completer) override {
    socket_ = std::move(request.socket());
    completer.Reply(fit::ok());
  }

  // component_testing::LocalComponentImpl methods
  void OnStart() override {
    started_ = true;
    ASSERT_EQ(outgoing()->AddProtocol<fuchsia_hardware_adb::Provider>(
                  bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure)),
              ZX_OK);
  }

  bool IsStarted() const { return started_; }

  void OnStop() override { started_ = false; }

  zx::socket& socket() { return socket_; }

 private:
  bool started_ = false;
  async_dispatcher_t* dispatcher_;
  fidl::ServerBindingGroup<fuchsia_hardware_adb::Provider> bindings_;
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

    builder.AddRoute(Route{
        .capabilities = {Protocol{fidl::DiscoverableProtocolName<fuchsia_hardware_adb::Provider>}},
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
  auto client_end = realm_->component().Connect<fuchsia_hardware_adb::Provider>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::Client<fuchsia_hardware_adb::Provider> provider(std::move(*client_end), dispatcher());

  zx::socket server, client;
  EXPECT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);
  provider->ConnectToService({std::move(server), ""}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    QuitLoop();
  });

  RunLoop();
}

TEST_F(AdbRealmTest, ServiceOpenCloseTest) {
  auto client_end = realm_->component().Connect<fuchsia_hardware_adb::Provider>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::Client<fuchsia_hardware_adb::Provider> provider(std::move(*client_end), dispatcher());
  zx::socket server, client;
  EXPECT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  provider->ConnectToService({std::move(server), ""}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
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

  fake_driver_.WaitForExpectations();

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
  fake_driver_.WaitForExpectations();
  SendConnect();
}

TEST_F(AdbRealmTest, ServiceReadWriteTest) {
  auto client_end = realm_->component().Connect<fuchsia_hardware_adb::Provider>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::Client<fuchsia_hardware_adb::Provider> provider(std::move(*client_end), dispatcher());
  zx::socket server, client;
  EXPECT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  provider->ConnectToService({std::move(server), ""}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
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
  fake_driver_.WaitForExpectations();

  zx_signals_t signal;
  ASSERT_EQ(service_provider_ptr_->socket().wait_one(ZX_SOCKET_READABLE | ZX_SOCKET_PEER_CLOSED,
                                                     zx::deadline_after(zx::sec(5)), &signal),
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

  fake_driver_.WaitForExpectations();

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
  fake_driver_.WaitForExpectations();
}

TEST_F(AdbRealmTest, ServiceRemoteCloseTest) {
  auto client_end = realm_->component().Connect<fuchsia_hardware_adb::Provider>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::Client<fuchsia_hardware_adb::Provider> provider(std::move(*client_end), dispatcher());
  zx::socket server, client;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  provider->ConnectToService({std::move(server), ""}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    QuitLoop();
  });
  RunLoop();
  dev_ptr_->SetServiceSocket(std::move(client));

  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_OKAY,
                                            .arg0 = 1,
                                            .arg1 = 1,
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_OKAY ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  fake_driver_.SendOpen({'s', 'h', 'e', 'l', 'l', ':', '\0'});
  fake_driver_.WaitForExpectations();

  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_CLSE,
                                            .arg0 = 1,
                                            .arg1 = 1,
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_CLSE ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  fake_driver_.SendClose();
  fake_driver_.WaitForExpectations();
  zx_signals_t observed = 0;
  EXPECT_EQ(service_provider_ptr_->socket().wait_one(ZX_SOCKET_PEER_CLOSED, zx::time::infinite(),
                                                     &observed),
            ZX_OK);
}

TEST_F(AdbRealmTest, BidirectionalLargeWriteDoesNotDeadlockTest) {
  auto client_end = realm_->component().Connect<fuchsia_hardware_adb::Provider>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::Client<fuchsia_hardware_adb::Provider> provider(std::move(*client_end), dispatcher());
  zx::socket server, client;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  zx_info_socket_t info{};
  ASSERT_EQ(client.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  ASSERT_GT(info.tx_buf_max, 4u);
  ASSERT_EQ(
      server.set_property(ZX_PROP_SOCKET_RX_THRESHOLD, &info.tx_buf_max, sizeof(info.tx_buf_max)),
      ZX_OK);

  // Leave 4 bytes of free space in |client| so a 16-byte A_WRTE fills the socket and arms an async
  // wait on |dispatcher_| until the service drains it.
  const std::vector<uint8_t> prefill(info.tx_buf_max - 4, 0xaa);
  size_t actual = 0;
  ASSERT_EQ(client.write(0, prefill.data(), prefill.size(), &actual), ZX_OK);
  ASSERT_EQ(actual, prefill.size());

  provider->ConnectToService({std::move(server), ""}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    QuitLoop();
  });
  RunLoop();
  dev_ptr_->SetServiceSocket(std::move(client));

  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_OKAY,
                                            .arg0 = 1,
                                            .arg1 = 1,
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_OKAY ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  fake_driver_.SendOpen({'s', 'h', 'e', 'l', 'l', ':', 'c', 'a', 't', '\0'});
  fake_driver_.WaitForExpectations();

  const std::vector<uint8_t> write_data = {'0', '1', '2', '3', '4', '5', '6', '7',
                                           '8', '9', 'a', 'b', 'c', 'd', 'e', 'f'};
  const std::vector<uint8_t> reply_data = {'p', 'o', 'n', 'g'};

  // First 4 bytes fill the socket without emitting A_OKAY while 12 bytes remain queued.
  fake_driver_.SendWrite(write_data);
  zx_signals_t signals = 0;
  ASSERT_EQ(service_provider_ptr_->socket().wait_one(ZX_SOCKET_READ_THRESHOLD,
                                                     zx::deadline_after(zx::sec(5)), &signals),
            ZX_OK);

  // Write "pong" before draining any input, proving local_socket_enqueue() returned without
  // blocking |dispatcher_| or emitting an early A_OKAY.
  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_WRTE,
                                            .arg0 = 1,
                                            .arg1 = 1,
                                            .data_length = static_cast<uint32_t>(reply_data.size()),
                                            .data_check = 0,
                                            .magic = A_WRTE ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  fake_driver_.ExpectQueueTx(0, reply_data);
  size_t written = 0;
  ASSERT_EQ(
      service_provider_ptr_->socket().write(0, reply_data.data(), reply_data.size(), &written),
      ZX_OK);
  ASSERT_EQ(written, reply_data.size());
  fake_driver_.WaitForExpectations();

  // Drain the full socket buffer so the async wait handler flushes the remaining 12 bytes and
  // emits the single A_OKAY for the completed A_WRTE.
  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_OKAY,
                                            .arg0 = 1,
                                            .arg1 = 1,
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_OKAY ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  std::vector<uint8_t> received(prefill.size() + write_data.size());
  size_t first_read = 0;
  ASSERT_EQ(service_provider_ptr_->socket().read(0, received.data(), info.tx_buf_max, &first_read),
            ZX_OK);
  ASSERT_EQ(first_read, info.tx_buf_max);
  fake_driver_.WaitForExpectations();

  size_t tail_read = 0;
  ASSERT_EQ(service_provider_ptr_->socket().read(0, received.data() + first_read,
                                                 received.size() - first_read, &tail_read),
            ZX_OK);
  ASSERT_EQ(first_read + tail_read, received.size());
  EXPECT_TRUE(std::equal(write_data.begin(), write_data.end(), received.begin() + prefill.size()));

  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_CLSE,
                                            .arg0 = 1,
                                            .arg1 = 1,
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_CLSE ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  service_provider_ptr_->socket().reset();
  fake_driver_.WaitForExpectations();
}

TEST_F(AdbRealmTest, ResetClosesOpenServiceSocketsTest) {
  auto client_end = realm_->component().Connect<fuchsia_hardware_adb::Provider>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::Client<fuchsia_hardware_adb::Provider> provider(std::move(*client_end), dispatcher());
  zx::socket server, client;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  zx_info_socket_t info{};
  ASSERT_EQ(client.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  ASSERT_GT(info.tx_buf_max, 4u);
  ASSERT_EQ(
      server.set_property(ZX_PROP_SOCKET_RX_THRESHOLD, &info.tx_buf_max, sizeof(info.tx_buf_max)),
      ZX_OK);
  const std::vector<uint8_t> prefill(info.tx_buf_max - 4, 0xaa);
  size_t prefilled = 0;
  ASSERT_EQ(client.write(0, prefill.data(), prefill.size(), &prefilled), ZX_OK);
  ASSERT_EQ(prefilled, prefill.size());

  provider->ConnectToService({std::move(server), ""}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    QuitLoop();
  });
  RunLoop();
  dev_ptr_->SetServiceSocket(std::move(client));

  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_OKAY,
                                            .arg0 = 1,
                                            .arg1 = 1,
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = A_OKAY ^ 0xffffffff,
                                        },
                                    .payload = {},
                                });
  fake_driver_.SendOpen({'s', 'h', 'e', 'l', 'l', ':', '\0'});
  fake_driver_.WaitForExpectations();

  // Flush 4 bytes to fill the socket and leave 4 bytes queued so |incoming_wait| is pending on
  // |dispatcher_| when Reset() runs.
  fake_driver_.SendWrite({'0', '1', '2', '3', '4', '5', '6', '7'});
  zx_signals_t signals = 0;
  ASSERT_EQ(service_provider_ptr_->socket().wait_one(ZX_SOCKET_READ_THRESHOLD,
                                                     zx::deadline_after(zx::sec(5)), &signals),
            ZX_OK);

  // Resetting adb while a service socket has pending waits must cancel the waits on |dispatcher_|
  // and close adb's end of the socket so no stale A_WRTE/A_CLSE packets leak into the next session.
  fake_driver_.ExpectStart();
  EXPECT_EQ(dev_ptr_->Reset(), ZX_OK);

  zx_signals_t observed = 0;
  EXPECT_EQ(service_provider_ptr_->socket().wait_one(ZX_SOCKET_PEER_CLOSED,
                                                     zx::time::infinite_past(), &observed),
            ZX_OK);
  SendConnect();
}

TEST_F(AdbRealmTest, DelayedAckPartialWriteAndRemoteCloseTest) {
  SendConnect("device", /*delayed_ack=*/true);

  ASSERT_TRUE(realm_.has_value());
  auto client_end = realm_->component().Connect<fuchsia_hardware_adb::Provider>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::Client<fuchsia_hardware_adb::Provider> provider(std::move(*client_end), dispatcher());
  zx::socket server, client;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  zx_info_socket_t info{};
  ASSERT_EQ(client.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  ASSERT_GT(info.tx_buf_max, 20u);

  // Leave 4 bytes of free space in |client| so a 16-byte A_WRTE flushes 4 bytes immediately and
  // leaves 12 bytes queued behind |incoming_wait|.
  const std::vector<uint8_t> prefill(info.tx_buf_max - 4, 0xaa);
  size_t prefilled = 0;
  ASSERT_EQ(client.write(0, prefill.data(), prefill.size(), &prefilled), ZX_OK);
  ASSERT_EQ(prefilled, prefill.size());

  provider->ConnectToService({std::move(server), ""}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    QuitLoop();
  });
  RunLoop();
  dev_ptr_->SetServiceSocket(std::move(client));

  constexpr uint32_t kInitialAck = static_cast<uint32_t>(INITIAL_DELAYED_ACK_BYTES);
  fake_driver_.ExpectDelayedAckOkay(1, 1, kInitialAck);
  fake_driver_.SendOpen({'s', 'h', 'e', 'l', 'l', ':', 'c', 'a', 't', '\0'}, kInitialAck);
  fake_driver_.WaitForExpectations();

  const std::vector<uint8_t> first_write = {
      '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
  };
  const std::vector<uint8_t> second_write = {
      'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v',
  };
  std::vector<uint8_t> received(prefill.size() + first_write.size() + second_write.size());

  // With delayed_ack enabled, flushing the first 4 bytes immediately emits A_OKAY(4) while the
  // remaining 12 bytes stay queued.
  fake_driver_.ExpectDelayedAckOkay(1, 1, 4);
  fake_driver_.SendWrite(first_write);
  fake_driver_.WaitForExpectations();

  // Draining 16 prefill bytes frees room for the 12 queued bytes, emitting A_OKAY(12) and leaving
  // 4 bytes of free space in the socket buffer again.
  fake_driver_.ExpectDelayedAckOkay(1, 1, 12);
  size_t total_read = 0;
  size_t chunk_read = 0;
  ASSERT_EQ(service_provider_ptr_->socket().read(0, received.data(), 16, &chunk_read), ZX_OK);
  ASSERT_EQ(chunk_read, 16u);
  total_read += chunk_read;
  fake_driver_.WaitForExpectations();

  // Fill the 4 remaining bytes with a second 16-byte A_WRTE (leaving 12 bytes queued), then send
  // A_CLSE before the service drains the socket.
  fake_driver_.ExpectDelayedAckOkay(1, 1, 4);
  fake_driver_.SendWrite(second_write);
  fake_driver_.WaitForExpectations();

  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_CLSE,
                                            .arg0 = 1,
                                            .arg1 = 1,
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = static_cast<uint32_t>(A_CLSE) ^ 0xffffffffu,
                                        },
                                    .payload = {},
                                });
  fake_driver_.SendClose();
  fake_driver_.WaitForExpectations();

  libsync::Completion close_processed;
  ASSERT_EQ(async::PostTask(fidl_loop_.dispatcher(), [&] { close_processed.Signal(); }), ZX_OK);
  ASSERT_EQ(close_processed.Wait(zx::sec(5)), ZX_OK);

  // local_socket_close() must keep adb's socket open on |local_socket_closing_list| until the
  // queued tail finishes flushing.
  zx_signals_t observed = 0;
  EXPECT_EQ(service_provider_ptr_->socket().wait_one(ZX_SOCKET_PEER_CLOSED,
                                                     zx::time::infinite_past(), &observed),
            ZX_ERR_TIMED_OUT);

  while (total_read < received.size()) {
    zx_signals_t signals = 0;
    ASSERT_EQ(service_provider_ptr_->socket().wait_one(ZX_SOCKET_READABLE | ZX_SOCKET_PEER_CLOSED,
                                                       zx::deadline_after(zx::sec(5)), &signals),
              ZX_OK);
    ASSERT_NE(signals & ZX_SOCKET_READABLE, 0u);
    chunk_read = 0;
    ASSERT_EQ(service_provider_ptr_->socket().read(0, received.data() + total_read,
                                                   received.size() - total_read, &chunk_read),
              ZX_OK);
    total_read += chunk_read;
  }

  EXPECT_TRUE(
      std::equal(first_write.begin(), first_write.end(), received.begin() + prefill.size()));
  EXPECT_TRUE(std::equal(second_write.begin(), second_write.end(),
                         received.begin() + prefill.size() + first_write.size()));
  EXPECT_EQ(service_provider_ptr_->socket().wait_one(ZX_SOCKET_PEER_CLOSED,
                                                     zx::deadline_after(zx::sec(5)), &observed),
            ZX_OK);
}

TEST_F(AdbRealmTest, ServiceOutputBeforeCloseTest) {
  ASSERT_TRUE(realm_.has_value());
  auto client_end = realm_->component().Connect<fuchsia_hardware_adb::Provider>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::Client<fuchsia_hardware_adb::Provider> provider(std::move(*client_end), dispatcher());
  zx::socket server, client;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  provider->ConnectToService({std::move(server), ""}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    QuitLoop();
  });

  RunLoop();
  dev_ptr_->SetServiceSocket(std::move(client));

  // The service writes its output and closes its end before adb reads anything, like a shell
  // command that exits right away. adb must forward the output before closing the stream.
  uint8_t output[] = {'b', 'y', 'e', '\n'};
  size_t actual = 0;
  ASSERT_EQ(service_provider_ptr_->socket().write(0, output, sizeof(output), &actual), ZX_OK);
  ASSERT_EQ(actual, sizeof(output));
  service_provider_ptr_->socket().reset();

  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_OKAY,
                                            .arg0 = 1,  // local-id
                                            .arg1 = 1,  // remote-id
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = static_cast<uint32_t>(A_OKAY) ^ 0xffffffffu,
                                        },
                                    .payload = {},
                                });
  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_WRTE,
                                            .arg0 = 1,  // local-id
                                            .arg1 = 1,  // remote-id
                                            .data_length = sizeof(output),
                                            .data_check = 0,
                                            .magic = static_cast<uint32_t>(A_WRTE) ^ 0xffffffffu,
                                        },
                                    .payload = {},
                                });
  fake_driver_.ExpectQueueTx(0, std::vector<uint8_t>(output, output + sizeof(output)));
  fake_driver_.ExpectQueueTx(0, {
                                    .msg =
                                        {
                                            .command = A_CLSE,
                                            .arg0 = 1,  // local-id
                                            .arg1 = 1,  // remote-id
                                            .data_length = 0,
                                            .data_check = 0,
                                            .magic = static_cast<uint32_t>(A_CLSE) ^ 0xffffffffu,
                                        },
                                    .payload = {},
                                });
  fake_driver_.SendOpen({'s', 'h', 'e', 'l', 'l', ':', '\0'});
  fake_driver_.WaitForExpectations();
}

namespace {

struct FakeSocketAdb : public AdbBase {
  explicit FakeSocketAdb(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}
  async_dispatcher_t* dispatcher() const override { return dispatcher_; }
  bool SendUsbPacket(uint8_t* buf, size_t len) override { return true; }
  zx::result<zx::socket> GetServiceSocket(std::string_view service,
                                          std::string_view args) override {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  async_dispatcher_t* dispatcher_;
};

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
  EXPECT_EQ(find_local_socket(local_id, 0), nullptr);
}

void DrainSocketBytes(const zx::socket& server, size_t remaining) {
  std::vector<uint8_t> read_buf(4096);
  while (remaining > 0) {
    zx_signals_t pending = 0;
    zx_status_t status = server.wait_one(ZX_SOCKET_READABLE | ZX_SOCKET_PEER_CLOSED,
                                         zx::deadline_after(zx::sec(5)), &pending);
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
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  FakeSocketAdb fake_adb(loop.dispatcher());

  atransport t(kCsDevice);
  t.online = true;
  t.SetDelayedAck(false);
  auto conn = std::make_shared<RecordingConnection>();
  t.SetConnection(conn);

  asocket* local = create_local_socket();
  local->adb = &fake_adb;
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

  apacket::payload_type payload(kPayloadSize);
  memset(payload.data(), 'A', payload.size());
  EXPECT_EQ(local->enqueue(local, std::move(payload)), 1);

  zx_info_socket_t info{};
  ASSERT_EQ(server.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  EXPECT_EQ(info.rx_buf_available, capacity);
  loop.RunUntilIdle();
  // No A_OKAY should have been sent after only a partial write of the packet.
  EXPECT_EQ(conn->okay_count(), 0u);

  // Drain the full buffer so the async wait flushes the remaining 150 bytes and emits A_OKAY.
  DrainSocketBytes(server, capacity);
  loop.RunUntilIdle();
  DrainSocketBytes(server, kPayloadSize - kInitialFreeSpace);

  ASSERT_EQ(conn->okay_packets.size(), 1u);
  EXPECT_EQ(conn->okay_packets[0]->msg.command, static_cast<uint32_t>(A_OKAY));
  EXPECT_EQ(conn->okay_packets[0]->msg.arg0, local->id);
  EXPECT_EQ(conn->okay_packets[0]->msg.arg1, remote->id);
  EXPECT_EQ(conn->okay_packets[0]->msg.data_length, 0u);

  CloseAndWaitLocalSocket(local);
}

TEST(AdbSocketFlowControlTest, DelayedAckModeSkipsZeroByteOkayAndReportsExactFlushedBytes) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  FakeSocketAdb fake_adb(loop.dispatcher());

  atransport t(kCsDevice);
  t.online = true;
  t.SetDelayedAck(true);
  auto conn = std::make_shared<RecordingConnection>();
  t.SetConnection(conn);

  asocket* local = create_local_socket();
  local->adb = &fake_adb;
  local->transport = &t;
  local->available_send_bytes = INITIAL_DELAYED_ACK_BYTES;
  asocket* remote = create_remote_socket(20, &t);
  local->peer = remote;
  remote->peer = local;

  zx::socket server, client;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &server, &client), ZX_OK);

  constexpr size_t kInitialFreeSpace = 100;
  constexpr size_t kFirstPayloadSize = 200;
  constexpr size_t kSecondPayloadSize = 50;
  constexpr size_t kTotalPayloadSize = kFirstPayloadSize + kSecondPayloadSize;
  const size_t capacity = FillSocketToCapacity(client);
  ASSERT_GT(capacity, kInitialFreeSpace);

  std::vector<uint8_t> drain_buf(kInitialFreeSpace);
  size_t actual = 0;
  ASSERT_EQ(server.read(0, drain_buf.data(), drain_buf.size(), &actual), ZX_OK);
  ASSERT_EQ(actual, kInitialFreeSpace);

  local->zx_socket = std::move(client);

  apacket::payload_type first_payload(kFirstPayloadSize);
  memset(first_payload.data(), 'B', first_payload.size());
  EXPECT_EQ(local->enqueue(local, std::move(first_payload)), 1);
  EXPECT_EQ(conn->okay_count(), 1u);

  // Enqueue another packet while the socket buffer is completely full (bytes_flushed == 0) and
  // verify no 0-byte A_OKAY packet is emitted.
  apacket::payload_type second_payload(kSecondPayloadSize);
  memset(second_payload.data(), 'B', second_payload.size());
  EXPECT_EQ(local->enqueue(local, std::move(second_payload)), 1);
  loop.RunUntilIdle();
  EXPECT_EQ(conn->okay_count(), 1u);

  DrainSocketBytes(server, capacity);
  loop.RunUntilIdle();
  DrainSocketBytes(server, kTotalPayloadSize - kInitialFreeSpace);

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
  EXPECT_EQ(total_acked, kTotalPayloadSize);

  CloseAndWaitLocalSocket(local);
}

TEST(AdbSocketFlowControlTest, DelayedAckModeDoesNotSendZeroByteOkayOnClosedPeer) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  FakeSocketAdb fake_adb(loop.dispatcher());

  atransport t(kCsDevice);
  t.online = true;
  t.SetDelayedAck(true);
  auto conn = std::make_shared<RecordingConnection>();
  t.SetConnection(conn);

  asocket* local = create_local_socket();
  local->adb = &fake_adb;
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
