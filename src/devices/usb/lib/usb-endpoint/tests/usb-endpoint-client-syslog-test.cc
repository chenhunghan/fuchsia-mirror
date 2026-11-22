// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/sync/cpp/completion.h>
#include <lib/syslog/cpp/log_settings.h>

#include <usb-endpoint/usb-endpoint-client.h>
#include <zxtest/zxtest.h>

#include "src/devices/usb/lib/usb-endpoint/testing/fake-usb-endpoint-server.h"

namespace {

class SyslogEndpointClientTest : public zxtest::Test {
 public:
  void Complete(std::vector<fuchsia_hardware_usb_endpoint::Completion> completions) {
    if (on_completion_) {
      on_completion_(std::move(completions));
    }
  }

  std::function<void(std::vector<fuchsia_hardware_usb_endpoint::Completion>)> on_completion_;
};

TEST_F(SyslogEndpointClientTest, RegisterCopyAndPeerCloseLogsViaSyslog) {
  fuchsia_logging::LogSettingsBuilder()
      .WithMinLogSeverity(fuchsia_logging::LogSeverity::Debug)
      .BuildAndInitialize();

  async::Loop server_loop{&kAsyncLoopConfigNeverAttachToThread};
  async::Loop client_loop{&kAsyncLoopConfigNeverAttachToThread};
  ASSERT_OK(server_loop.StartThread("syslog-ep-server"));
  ASSERT_OK(client_loop.StartThread("syslog-ep-client"));

  auto endpoints = fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
  fake_usb_endpoint::FakeEndpoint server;
  server.Connect(server_loop.dispatcher(), std::move(endpoints.server));

  usb::EndpointClient<SyslogEndpointClientTest> client(
      usb::EndpointType::BULK, this, std::mem_fn(&SyslogEndpointClientTest::Complete));

  libsync::Completion unbound;
  client.SetOnUnbind([&]() { unbound.Signal(); });
  ASSERT_OK(client.Init(std::move(endpoints.client), client_loop.dispatcher()));

  constexpr size_t kSlotSize = 64;
  ASSERT_EQ(client.AddRequests(2, kSlotSize, fuchsia_hardware_usb_request::Buffer::Tag::kVmoId),
            2u);

  std::optional<usb::FidlRequest> req = client.GetRequest();
  ASSERT_TRUE(req.has_value());
  const uint8_t payload[] = {0xde, 0xad, 0xbe, 0xef};
  zx::result<std::vector<size_t>> copied =
      req->CachedCopyTo(0, payload, sizeof(payload), client.GetMapped());
  ASSERT_OK(copied.status_value());
  ASSERT_EQ(copied->size(), 1u);
  EXPECT_EQ((*copied)[0], sizeof(payload));

  uint8_t readback[sizeof(payload)] = {};
  zx::result<std::vector<size_t>> read_res =
      req->CachedCopyFrom(0, readback, sizeof(readback), client.GetMapped());
  ASSERT_OK(read_res.status_value());
  EXPECT_BYTES_EQ(readback, payload, sizeof(payload));
  client.PutRequest(std::move(*req));

  // Shutting down the server closes the peer channel and exercises on_fidl_error() logging on a
  // non-driver component without touching fdf::Logger::GlobalInstance().
  server_loop.Shutdown();
  unbound.Wait();

  EXPECT_OK(client.Close());
  client_loop.Shutdown();
}

}  // namespace
