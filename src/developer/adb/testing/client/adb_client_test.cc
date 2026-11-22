// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/adb/testing/client/adb_client.h"

#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.request/cpp/fidl.h>
#include <fidl/fuchsia.testing.adb/cpp/fidl.h>
#include <lib/fidl/cpp/channel.h>
#include <lib/zx/vmo.h>

#include <cstdint>
#include <cstring>
#include <deque>
#include <optional>
#include <string>
#include <vector>

#include <gtest/gtest.h>

#include "src/developer/adb/third_party/adb/adb-protocol.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

namespace {

namespace fendpoint = fuchsia_hardware_usb_endpoint;
namespace frequest = fuchsia_hardware_usb_request;

std::vector<uint8_t> MakeHeader(uint32_t command, uint32_t arg0, uint32_t arg1,
                                uint32_t data_length) {
  amessage msg{};
  msg.command = command;
  msg.arg0 = arg0;
  msg.arg1 = arg1;
  msg.data_length = data_length;
  msg.data_check = 0;
  msg.magic = command ^ 0xffffffffu;
  std::vector<uint8_t> out(sizeof(msg));
  memcpy(out.data(), &msg, sizeof(msg));
  return out;
}

class FakeEndpoint : public fidl::Server<fendpoint::Endpoint> {
 public:
  void Bind(async_dispatcher_t* dispatcher, fidl::ServerEnd<fendpoint::Endpoint> server_end) {
    binding_.emplace(dispatcher, std::move(server_end), this, fidl::kIgnoreBindingClosure);
  }

  void Close() { binding_.reset(); }

  void GetInfo(GetInfoCompleter::Sync& completer) override {
    completer.Reply(fit::as_error(ZX_ERR_NOT_SUPPORTED));
  }

  void GetAlignmentInfo(GetAlignmentInfoCompleter::Sync& completer) override {
    completer.Reply(fit::as_error(ZX_ERR_NOT_SUPPORTED));
  }

  void RegisterVmos(RegisterVmosRequest& request, RegisterVmosCompleter::Sync& completer) override {
    completer.Reply({{}});
  }

  void UnregisterVmos(UnregisterVmosRequest& request,
                      UnregisterVmosCompleter::Sync& completer) override {
    completer.Reply({{}, {}});
  }

  void QueueRequests(QueueRequestsRequest& request,
                     QueueRequestsCompleter::Sync& completer) override {
    for (auto& req : request.req()) {
      queued_.push_back(std::move(req));
    }
  }

  void CancelAll(CancelAllCompleter::Sync& completer) override { completer.Reply(fit::ok()); }

  void CompleteBatch(std::vector<fendpoint::Completion> completions) {
    ASSERT_TRUE(binding_.has_value());
    ASSERT_TRUE(fidl::SendEvent(*binding_)->OnCompletion(std::move(completions)).is_ok());
  }

  frequest::Request PopRequest() {
    EXPECT_FALSE(queued_.empty());
    frequest::Request req = std::move(queued_.front());
    queued_.pop_front();
    return req;
  }

  bool empty() const { return queued_.empty(); }
  size_t queued_count() const { return queued_.size(); }

 private:
  std::optional<fidl::ServerBinding<fendpoint::Endpoint>> binding_;
  std::deque<frequest::Request> queued_;
};

class AdbClientTest : public gtest::RealLoopFixture {
 protected:
  static constexpr size_t kVmoSize = 16384;

  void SetUp() override {
    auto client_endpoints = fidl::Endpoints<fuchsia_testing_adb::Client>::Create();
    auto in_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();
    auto out_endpoints = fidl::Endpoints<fendpoint::Endpoint>::Create();

    ASSERT_EQ(zx::vmo::create(kVmoSize, 0, &bulk_in_vmo_), ZX_OK);
    zx::vmo client_vmo;
    ASSERT_EQ(bulk_in_vmo_.duplicate(ZX_RIGHT_SAME_RIGHTS, &client_vmo), ZX_OK);

    bulk_in_server_.Bind(dispatcher(), std::move(in_endpoints.server));
    bulk_out_server_.Bind(dispatcher(), std::move(out_endpoints.server));

    impl_ = std::make_unique<AdbClientImpl>(dispatcher());
    impl_->SetEndpointsForTesting(std::move(in_endpoints.client), std::move(out_endpoints.client),
                                  std::move(client_vmo));
    impl_binding_.emplace(dispatcher(), std::move(client_endpoints.server), impl_.get(),
                          fidl::kIgnoreBindingClosure);
    client_.Bind(std::move(client_endpoints.client), dispatcher());
  }

  void CompleteNextReadWithVmo(const std::vector<uint8_t>& bytes, size_t expected_read_size) {
    RunLoopUntil([&]() { return !bulk_in_server_.empty(); });
    frequest::Request req = bulk_in_server_.PopRequest();
    ASSERT_TRUE(req.data().has_value());
    ASSERT_EQ(req.data()->size(), 1u);
    const auto& region = (*req.data())[0];
    ASSERT_TRUE(region.buffer().has_value());
    ASSERT_TRUE(region.buffer()->vmo_id().has_value());
    EXPECT_EQ(region.size(), expected_read_size);

    ASSERT_LE(bytes.size(), kVmoSize);
    if (!bytes.empty()) {
      ASSERT_EQ(bulk_in_vmo_.write(bytes.data(), 0, bytes.size()), ZX_OK);
    }

    std::vector<fendpoint::Completion> completions;
    completions.emplace_back(std::move(
        fendpoint::Completion().request(std::move(req)).status(ZX_OK).transfer_size(bytes.size())));
    bulk_in_server_.CompleteBatch(std::move(completions));
  }

  amessage PopOutPacketHeader() {
    RunLoopUntil([&]() { return !bulk_out_server_.empty(); });
    frequest::Request req = bulk_out_server_.PopRequest();
    EXPECT_TRUE(req.data().has_value());
    EXPECT_EQ(req.data()->size(), 1u);
    const auto& data = (*req.data())[0].buffer()->data().value();
    EXPECT_GE(data.size(), sizeof(amessage));
    amessage msg{};
    memcpy(&msg, data.data(), sizeof(msg));
    return msg;
  }

  void PerformHandshake(const std::string& banner = "device::") {
    bool connected = false;
    client_->Connect().ThenExactlyOnce([&](auto& result) {
      EXPECT_TRUE(result.is_ok());
      connected = true;
    });

    amessage cnxn = PopOutPacketHeader();
    EXPECT_EQ(cnxn.command, static_cast<uint32_t>(A_CNXN));

    CompleteNextReadWithVmo(
        MakeHeader(A_CNXN, A_VERSION, MAX_PAYLOAD, static_cast<uint32_t>(banner.size())), kVmoSize);
    if (!banner.empty()) {
      std::vector<uint8_t> payload(banner.begin(), banner.end());
      CompleteNextReadWithVmo(payload, banner.size());
    }

    RunLoopUntil([&]() { return connected; });
  }

  FakeEndpoint bulk_in_server_;
  FakeEndpoint bulk_out_server_;
  zx::vmo bulk_in_vmo_;
  std::unique_ptr<AdbClientImpl> impl_;
  std::optional<fidl::ServerBinding<fuchsia_testing_adb::Client>> impl_binding_;
  fidl::Client<fuchsia_testing_adb::Client> client_;
};

TEST_F(AdbClientTest, MultiPacketConnectAndPacketAlignedWrite) {
  PerformHandshake("device::ro.product.name=test;");

  bool done = false;
  std::string output;
  client_->ExecuteCommand({"echo test"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    if (result.is_ok()) {
      output = result->output();
    }
    done = true;
  });

  amessage open_msg = PopOutPacketHeader();
  EXPECT_EQ(open_msg.command, static_cast<uint32_t>(A_OPEN));
  const uint32_t local_id = open_msg.arg0;
  constexpr uint32_t kRemoteId = 7;

  CompleteNextReadWithVmo(MakeHeader(A_OKAY, kRemoteId, local_id, 0), kVmoSize);

  // Send a 4096-byte (512-byte aligned) WRTE payload across separate header and payload reads.
  // QueueReadRequest must request exactly 4096 bytes so the transfer completes without a ZLP.
  constexpr size_t kAlignedPayloadSize = 4096;
  std::vector<uint8_t> payload(kAlignedPayloadSize, 'A');
  CompleteNextReadWithVmo(MakeHeader(A_WRTE, kRemoteId, local_id, kAlignedPayloadSize), kVmoSize);
  CompleteNextReadWithVmo(payload, kAlignedPayloadSize);

  amessage ack = PopOutPacketHeader();
  EXPECT_EQ(ack.command, static_cast<uint32_t>(A_OKAY));
  EXPECT_EQ(ack.arg0, local_id);
  EXPECT_EQ(ack.arg1, kRemoteId);
  EXPECT_EQ(ack.data_length, 0u);

  CompleteNextReadWithVmo(MakeHeader(A_CLSE, kRemoteId, local_id, 0), kVmoSize);
  RunLoopUntil([&]() { return done; });
  EXPECT_EQ(output, std::string(kAlignedPayloadSize, 'A'));
}

TEST_F(AdbClientTest, MultiChunkWriteExceeding16KiB) {
  PerformHandshake();

  bool done = false;
  std::string output;
  client_->ExecuteCommand({"big"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    if (result.is_ok()) {
      output = result->output();
    }
    done = true;
  });

  amessage open_msg = PopOutPacketHeader();
  const uint32_t local_id = open_msg.arg0;
  constexpr uint32_t kRemoteId = 9;
  constexpr size_t kTotalSize = 20480;

  CompleteNextReadWithVmo(MakeHeader(A_WRTE, kRemoteId, local_id, kTotalSize), kVmoSize);
  CompleteNextReadWithVmo(std::vector<uint8_t>(16384, 'B'), 16384);
  CompleteNextReadWithVmo(std::vector<uint8_t>(4096, 'C'), 4096);

  amessage ack = PopOutPacketHeader();
  EXPECT_EQ(ack.command, static_cast<uint32_t>(A_OKAY));

  CompleteNextReadWithVmo(MakeHeader(A_CLSE, kRemoteId, local_id, 0), kVmoSize);
  RunLoopUntil([&]() { return done; });
  EXPECT_EQ(output, std::string(16384, 'B') + std::string(4096, 'C'));
}

TEST_F(AdbClientTest, FailedOkayResetsSessionAndFailsCommand) {
  PerformHandshake();

  bool done = false;
  zx_status_t err_status = ZX_OK;
  client_->ExecuteCommand({"fail-ack"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_error());
    if (result.is_error() && result.error_value().is_domain_error()) {
      err_status = result.error_value().domain_error();
    }
    done = true;
  });

  amessage open_msg = PopOutPacketHeader();
  const uint32_t local_id = open_msg.arg0;
  constexpr uint32_t kRemoteId = 3;

  // Unbind bulk OUT before the WRTE arrives so SendOkay() fails synchronously in OnCompletion.
  impl_->UnbindBulkOutForTesting();

  std::vector<uint8_t> pkt = MakeHeader(A_WRTE, kRemoteId, local_id, 4);
  pkt.insert(pkt.end(), {'t', 'e', 's', 't'});
  CompleteNextReadWithVmo(pkt, kVmoSize);

  RunLoopUntil([&]() { return done; });
  EXPECT_EQ(err_status, ZX_ERR_BAD_STATE);
  RunLoopUntilIdle();
  EXPECT_TRUE(bulk_in_server_.empty());

  // Handshake state was reset, so a subsequent ExecuteCommand is rejected with ZX_ERR_BAD_STATE.
  bool second_done = false;
  client_->ExecuteCommand({"after-error"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_error());
    if (result.is_error() && result.error_value().is_domain_error()) {
      EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_BAD_STATE);
    }
    second_done = true;
  });
  RunLoopUntil([&]() { return second_done; });
}

TEST_F(AdbClientTest, BulkOutPeerClosedResetsSessionAndFailsCommand) {
  PerformHandshake();

  bool done = false;
  zx_status_t err_status = ZX_OK;
  client_->ExecuteCommand({"peer-closed"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_error());
    if (result.is_error() && result.error_value().is_domain_error()) {
      err_status = result.error_value().domain_error();
    }
    done = true;
  });
  (void)PopOutPacketHeader();

  bulk_out_server_.Close();
  RunLoopUntil([&]() { return done; });
  EXPECT_EQ(err_status, ZX_ERR_PEER_CLOSED);

  bool second_done = false;
  client_->ExecuteCommand({"after-close"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_error());
    if (result.is_error() && result.error_value().is_domain_error()) {
      EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_BAD_STATE);
    }
    second_done = true;
  });
  RunLoopUntil([&]() { return second_done; });
}

TEST_F(AdbClientTest, BulkOutCompletionErrorResetsSessionAndFailsCommand) {
  PerformHandshake();

  bool done = false;
  zx_status_t err_status = ZX_OK;
  client_->ExecuteCommand({"out-io-error"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_error());
    if (result.is_error() && result.error_value().is_domain_error()) {
      err_status = result.error_value().domain_error();
    }
    done = true;
  });

  amessage open_msg = PopOutPacketHeader();
  const uint32_t local_id = open_msg.arg0;
  constexpr uint32_t kRemoteId = 4;

  std::vector<uint8_t> pkt = MakeHeader(A_WRTE, kRemoteId, local_id, 4);
  pkt.insert(pkt.end(), {'t', 'e', 's', 't'});
  CompleteNextReadWithVmo(pkt, kVmoSize);

  RunLoopUntil([&]() { return !bulk_out_server_.empty(); });
  frequest::Request ack_req = bulk_out_server_.PopRequest();
  std::vector<fendpoint::Completion> completions;
  completions.emplace_back(std::move(
      fendpoint::Completion().request(std::move(ack_req)).status(ZX_ERR_IO).transfer_size(0)));
  bulk_out_server_.CompleteBatch(std::move(completions));

  RunLoopUntil([&]() { return done; });
  EXPECT_EQ(err_status, ZX_ERR_IO);

  bool second_done = false;
  client_->ExecuteCommand({"after-out-error"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_error());
    if (result.is_error() && result.error_value().is_domain_error()) {
      EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_BAD_STATE);
    }
    second_done = true;
  });
  RunLoopUntil([&]() { return second_done; });
}

TEST_F(AdbClientTest, UnsolicitedWriteAfterCloseIsDiscarded) {
  PerformHandshake();

  bool done1 = false;
  std::string out1;
  client_->ExecuteCommand({"first"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    if (result.is_ok()) {
      out1 = result->output();
    }
    done1 = true;
  });

  amessage open1 = PopOutPacketHeader();
  const uint32_t local1 = open1.arg0;
  constexpr uint32_t kRemote1 = 5;

  std::vector<uint8_t> wrte1 = MakeHeader(A_WRTE, kRemote1, local1, 2);
  wrte1.insert(wrte1.end(), {'o', 'k'});
  CompleteNextReadWithVmo(wrte1, kVmoSize);
  EXPECT_EQ(PopOutPacketHeader().command, static_cast<uint32_t>(A_OKAY));

  CompleteNextReadWithVmo(MakeHeader(A_CLSE, kRemote1, local1, 0), kVmoSize);
  RunLoopUntil([&]() { return done1; });
  EXPECT_EQ(out1, "ok");

  // Deliver a late WRTE for the closed stream across two packets; it must be discarded.
  CompleteNextReadWithVmo(MakeHeader(A_WRTE, kRemote1, local1, 6), kVmoSize);
  CompleteNextReadWithVmo({'s', 't', 'a', 'l', 'e', '!'}, 6);
  RunLoopUntilIdle();
  EXPECT_TRUE(bulk_out_server_.empty());

  // Next command should receive only its own payload.
  bool done2 = false;
  std::string out2;
  client_->ExecuteCommand({"second"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    if (result.is_ok()) {
      out2 = result->output();
    }
    done2 = true;
  });

  amessage open2 = PopOutPacketHeader();
  std::vector<uint8_t> wrte2 = MakeHeader(A_WRTE, 6, open2.arg0, 3);
  wrte2.insert(wrte2.end(), {'n', 'e', 'w'});
  CompleteNextReadWithVmo(wrte2, kVmoSize);
  EXPECT_EQ(PopOutPacketHeader().command, static_cast<uint32_t>(A_OKAY));
  CompleteNextReadWithVmo(MakeHeader(A_CLSE, 6, open2.arg0, 0), kVmoSize);

  RunLoopUntil([&]() { return done2; });
  EXPECT_EQ(out2, "new");
}

TEST_F(AdbClientTest, ConnectWhileCommandActiveReturnsShouldWait) {
  PerformHandshake();

  bool cmd_done = false;
  std::string output;
  client_->ExecuteCommand({"busy"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_ok());
    if (result.is_ok()) {
      output = result->output();
    }
    cmd_done = true;
  });

  amessage open_msg = PopOutPacketHeader();

  bool connect_done = false;
  client_->Connect().ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_error());
    if (result.is_error() && result.error_value().is_domain_error()) {
      EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_SHOULD_WAIT);
    }
    connect_done = true;
  });
  RunLoopUntil([&]() { return connect_done; });
  EXPECT_FALSE(cmd_done);

  // The active command must still complete cleanly.
  CompleteNextReadWithVmo(MakeHeader(A_CLSE, 11, open_msg.arg0, 0), kVmoSize);
  RunLoopUntil([&]() { return cmd_done; });
  EXPECT_TRUE(output.empty());
}

TEST_F(AdbClientTest, FatalCompletionErrorResetsSessionAndStopsBatch) {
  PerformHandshake();

  bool cmd_done = false;
  client_->ExecuteCommand({"malformed"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_error());
    if (result.is_error() && result.error_value().is_domain_error()) {
      EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_IO);
    }
    cmd_done = true;
  });
  (void)PopOutPacketHeader();

  RunLoopUntil([&]() { return !bulk_in_server_.empty(); });
  frequest::Request req1 = bulk_in_server_.PopRequest();

  // Send a batch whose first completion has an out-of-bounds VMO transfer_size followed by a
  // second completion. OnCompletion must reset session state and return early without re-queuing.
  std::vector<fendpoint::Completion> completions;
  completions.emplace_back(std::move(
      fendpoint::Completion().request(std::move(req1)).status(ZX_OK).transfer_size(kVmoSize + 1)));
  completions.emplace_back(std::move(fendpoint::Completion().status(ZX_OK).transfer_size(0)));
  bulk_in_server_.CompleteBatch(std::move(completions));

  RunLoopUntil([&]() { return cmd_done; });
  RunLoopUntilIdle();
  EXPECT_TRUE(bulk_in_server_.empty());

  // Handshake must be cleared so subsequent ExecuteCommand fails with ZX_ERR_BAD_STATE.
  bool next_done = false;
  client_->ExecuteCommand({"after-oob"}).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.is_error());
    if (result.is_error() && result.error_value().is_domain_error()) {
      EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_BAD_STATE);
    }
    next_done = true;
  });
  RunLoopUntil([&]() { return next_done; });
}

}  // namespace
