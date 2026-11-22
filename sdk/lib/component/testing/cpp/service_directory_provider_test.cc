// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.examples/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/component/testing/cpp/service_directory_provider.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fidl/cpp/wire/server.h>
#include <lib/fit/defer.h>
#include <lib/zx/result.h>
#include <zircon/errors.h>

#include <memory>
#include <optional>
#include <string>
#include <utility>

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "src/lib/testing/predicates/status.h"

namespace component::testing {
namespace {

using fuchsia_examples::Echo;

constexpr char kHello[] = "hello";
constexpr char kOtherName[] = "other.Echo";

class EchoImpl final : public fidl::Server<Echo> {
 public:
  explicit EchoImpl(std::string suffix = "") : suffix_(std::move(suffix)) {}

  void EchoString(EchoStringRequest& request, EchoStringCompleter::Sync& completer) override {
    ++num_calls_;
    completer.Reply(request.value() + suffix_);
  }

  void SendString(SendStringRequest& request, SendStringCompleter::Sync& completer) override {}

  size_t num_calls() const { return num_calls_; }

 private:
  std::string suffix_;
  size_t num_calls_ = 0;
};

class ServiceDirectoryProviderTest : public gtest::TestLoopFixture {
 protected:
  // Calls `EchoString` over `client` and runs the loop until the call completes. Returns the
  // response, or the status the call failed with.
  zx::result<std::string> EchoString(const fidl::Client<Echo>& client,
                                     const std::string& value = kHello) {
    std::optional<zx::result<std::string>> result;
    client->EchoString({{.value = value}})
        .ThenExactlyOnce([&result](fidl::Result<Echo::EchoString>& call_result) {
          if (call_result.is_ok()) {
            result = zx::ok(call_result->response());
          } else {
            result = zx::error(call_result.error_value().status());
          }
        });
    RunLoopUntilIdle();
    if (!result.has_value()) {
      ADD_FAILURE() << "EchoString did not complete";
      return zx::error(ZX_ERR_TIMED_OUT);
    }
    return std::move(result).value();
  }

  // Connects to `name` in `svc`, calls `EchoString` and returns the result.
  zx::result<std::string> EchoStringAt(
      fidl::UnownedClientEnd<fuchsia_io::Directory> svc,
      std::string_view name = fidl::DiscoverableProtocolName<Echo>) {
    zx::result client_end = component::ConnectAt<Echo>(svc, name);
    if (client_end.is_error()) {
      return client_end.take_error();
    }
    fidl::Client<Echo> client(std::move(client_end.value()), dispatcher());
    return EchoString(client);
  }

  EchoImpl echo_impl_;
  ServiceDirectoryProvider svc_provider_{dispatcher()};
};

TEST_F(ServiceDirectoryProviderTest, AddProtocolWithHandler) {
  fidl::ServerBindingGroup<Echo> bindings;
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(
      bindings.CreateHandler(&echo_impl_, dispatcher(), fidl::kIgnoreBindingClosure)));

  zx::result client_end = svc_provider_.Connect<Echo>();
  ASSERT_OK(client_end);
  fidl::Client<Echo> client(std::move(client_end.value()), dispatcher());

  // The call is pipelined behind the connection request; nothing happens until the loop runs.
  EXPECT_EQ(echo_impl_.num_calls(), 0u);
  zx::result response = EchoString(client);
  ASSERT_OK(response);
  EXPECT_EQ(response.value(), kHello);
  EXPECT_EQ(echo_impl_.num_calls(), 1u);
  EXPECT_EQ(bindings.size(), 1u);
}

TEST_F(ServiceDirectoryProviderTest, AddProtocolWithServerImpl) {
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&echo_impl_));

  // Each connection gets its own binding to the same implementation.
  zx::result first = svc_provider_.Connect<Echo>();
  ASSERT_OK(first);
  zx::result second = svc_provider_.Connect<Echo>();
  ASSERT_OK(second);
  fidl::Client<Echo> first_client(std::move(first.value()), dispatcher());
  fidl::Client<Echo> second_client(std::move(second.value()), dispatcher());

  zx::result response = EchoString(first_client, "first");
  ASSERT_OK(response);
  EXPECT_EQ(response.value(), "first");
  response = EchoString(second_client, "second");
  ASSERT_OK(response);
  EXPECT_EQ(response.value(), "second");
  EXPECT_EQ(echo_impl_.num_calls(), 2u);
}

TEST_F(ServiceDirectoryProviderTest, AddProtocolWithServerEnd) {
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&echo_impl_));

  auto [client_end, server_end] = fidl::Endpoints<Echo>::Create();
  ASSERT_OK(svc_provider_.Connect<Echo>(std::move(server_end)));
  fidl::Client<Echo> client(std::move(client_end), dispatcher());

  zx::result response = EchoString(client);
  ASSERT_OK(response);
  EXPECT_EQ(response.value(), kHello);
}

TEST_F(ServiceDirectoryProviderTest, AddProtocolWithCustomName) {
  EchoImpl other_impl("!");
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&echo_impl_));
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&other_impl, kOtherName));

  zx::result response = EchoStringAt(svc_provider_.client_end());
  ASSERT_OK(response);
  EXPECT_EQ(response.value(), kHello);

  response = EchoStringAt(svc_provider_.client_end(), kOtherName);
  ASSERT_OK(response);
  EXPECT_EQ(response.value(), "hello!");

  EXPECT_EQ(echo_impl_.num_calls(), 1u);
  EXPECT_EQ(other_impl.num_calls(), 1u);

  // Entries added with a custom name are removed by that name.
  ASSERT_OK(svc_provider_.RemoveProtocol<Echo>(kOtherName));
  ASSERT_OK(svc_provider_.RemoveProtocol<Echo>());
}

TEST_F(ServiceDirectoryProviderTest, AddProtocolRejectsDuplicatesAndInvalidArguments) {
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&echo_impl_));

  EXPECT_STATUS(svc_provider_.AddProtocol<Echo>(&echo_impl_).status_value(), ZX_ERR_ALREADY_EXISTS);
  EXPECT_STATUS(
      svc_provider_.AddProtocol<Echo>([](fidl::ServerEnd<Echo> server_end) {}).status_value(),
      ZX_ERR_ALREADY_EXISTS);

  EXPECT_STATUS(
      svc_provider_.AddProtocol<Echo>(static_cast<EchoImpl*>(nullptr), kOtherName).status_value(),
      ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(
      svc_provider_.AddProtocol<Echo>(fidl::ProtocolHandler<Echo>(), kOtherName).status_value(),
      ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(svc_provider_.AddProtocol<Echo>(&echo_impl_, "").status_value(),
                ZX_ERR_INVALID_ARGS);

  // The directory still only has the original entry.
  zx::result response = EchoStringAt(svc_provider_.client_end());
  ASSERT_OK(response);
  EXPECT_EQ(response.value(), kHello);
  EXPECT_STATUS(EchoStringAt(svc_provider_.client_end(), kOtherName).status_value(),
                ZX_ERR_NOT_FOUND);
}

TEST_F(ServiceDirectoryProviderTest, ConnectToMissingProtocolFails) {
  EXPECT_STATUS(EchoStringAt(svc_provider_.client_end()).status_value(), ZX_ERR_NOT_FOUND);
}

TEST_F(ServiceDirectoryProviderTest, RemoveProtocol) {
  EXPECT_STATUS(svc_provider_.RemoveProtocol<Echo>().status_value(), ZX_ERR_NOT_FOUND);

  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&echo_impl_));
  ASSERT_OK(EchoStringAt(svc_provider_.client_end()));

  ASSERT_OK(svc_provider_.RemoveProtocol<Echo>());
  EXPECT_STATUS(EchoStringAt(svc_provider_.client_end()).status_value(), ZX_ERR_NOT_FOUND);
  EXPECT_STATUS(svc_provider_.RemoveProtocol<Echo>().status_value(), ZX_ERR_NOT_FOUND);
  EXPECT_EQ(echo_impl_.num_calls(), 1u);

  // The name is free again.
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&echo_impl_));
  ASSERT_OK(EchoStringAt(svc_provider_.client_end()));
  EXPECT_EQ(echo_impl_.num_calls(), 2u);
}

TEST_F(ServiceDirectoryProviderTest, RemoveProtocolClosesOwnedBindings) {
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&echo_impl_));

  zx::result client_end = svc_provider_.Connect<Echo>();
  ASSERT_OK(client_end);
  fidl::Client<Echo> client(std::move(client_end.value()), dispatcher());
  ASSERT_OK(EchoString(client));

  ASSERT_OK(svc_provider_.RemoveProtocol<Echo>());
  zx::result response = EchoString(client);
  ASSERT_TRUE(response.is_error());
  EXPECT_STATUS(response.status_value(), ZX_ERR_PEER_CLOSED);
  EXPECT_EQ(echo_impl_.num_calls(), 1u);
}

TEST_F(ServiceDirectoryProviderTest, RemoveProtocolKeepsHandlerBindings) {
  fidl::ServerBindingGroup<Echo> bindings;
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(
      bindings.CreateHandler(&echo_impl_, dispatcher(), fidl::kIgnoreBindingClosure)));

  zx::result client_end = svc_provider_.Connect<Echo>();
  ASSERT_OK(client_end);
  fidl::Client<Echo> client(std::move(client_end.value()), dispatcher());
  ASSERT_OK(EchoString(client));

  // The test owns the bindings, so the existing connection survives the removal.
  ASSERT_OK(svc_provider_.RemoveProtocol<Echo>());
  ASSERT_OK(EchoString(client));
  EXPECT_EQ(echo_impl_.num_calls(), 2u);
  EXPECT_EQ(bindings.size(), 1u);
}

TEST_F(ServiceDirectoryProviderTest, CloneReturnsIndependentConnections) {
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&echo_impl_));

  zx::result first = svc_provider_.Clone();
  ASSERT_OK(first);
  zx::result second = svc_provider_.Clone();
  ASSERT_OK(second);
  EXPECT_NE(first->channel().get(), second->channel().get());
  EXPECT_NE(first->channel().get(), svc_provider_.client_end().channel()->get());

  ASSERT_OK(EchoStringAt(first.value()));
  ASSERT_OK(EchoStringAt(second.value()));

  // Closing one connection leaves the others usable.
  first.value().reset();
  RunLoopUntilIdle();
  ASSERT_OK(EchoStringAt(second.value()));
  ASSERT_OK(EchoStringAt(svc_provider_.client_end()));
  EXPECT_EQ(echo_impl_.num_calls(), 4u);
}

TEST_F(ServiceDirectoryProviderTest, CloneSeesLaterChanges) {
  zx::result svc = svc_provider_.Clone();
  ASSERT_OK(svc);
  EXPECT_STATUS(EchoStringAt(svc.value()).status_value(), ZX_ERR_NOT_FOUND);

  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&echo_impl_));
  ASSERT_OK(EchoStringAt(svc.value()));

  ASSERT_OK(svc_provider_.RemoveProtocol<Echo>());
  EXPECT_STATUS(EchoStringAt(svc.value()).status_value(), ZX_ERR_NOT_FOUND);
}

TEST_F(ServiceDirectoryProviderTest, Serve) {
  ASSERT_OK(svc_provider_.AddProtocol<Echo>(&echo_impl_));

  auto [client_end, server_end] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  ASSERT_OK(svc_provider_.Serve(std::move(server_end)));
  ASSERT_OK(EchoStringAt(client_end));
}

TEST_F(ServiceDirectoryProviderTest, DestructionClosesConnections) {
  auto svc_provider = std::make_unique<ServiceDirectoryProvider>(dispatcher());
  ASSERT_OK(svc_provider->AddProtocol<Echo>(&echo_impl_));

  zx::result svc = svc_provider->Clone();
  ASSERT_OK(svc);
  zx::result client_end = svc_provider->Connect<Echo>();
  ASSERT_OK(client_end);
  fidl::Client<Echo> client(std::move(client_end.value()), dispatcher());
  ASSERT_OK(EchoString(client));

  svc_provider.reset();

  // Both the protocol connection bound by the provider and the directory connections are closed.
  EXPECT_STATUS(EchoString(client).status_value(), ZX_ERR_PEER_CLOSED);
  EXPECT_STATUS(EchoStringAt(svc.value()).status_value(), ZX_ERR_PEER_CLOSED);
  EXPECT_EQ(echo_impl_.num_calls(), 1u);
}

TEST(ServiceDirectoryProviderBackgroundLoopTest, ServesOnBackgroundDispatcher) {
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);

  EchoImpl echo_impl;
  fidl::ServerBindingGroup<Echo> bindings;
  ServiceDirectoryProvider svc_provider(loop.dispatcher());
  ASSERT_OK(svc_provider.AddProtocol<Echo>(
      bindings.CreateHandler(&echo_impl, loop.dispatcher(), fidl::kIgnoreBindingClosure)));

  // Start serving after mutating `svc_provider`, and stop serving before `svc_provider` and
  // `bindings` are destroyed, as required by the class contract.
  ASSERT_OK(loop.StartThread("svc-provider"));
  auto shutdown_loop = fit::defer([&loop] { loop.Shutdown(); });

  zx::result client_end = svc_provider.Connect<Echo>();
  ASSERT_OK(client_end);
  fidl::SyncClient<Echo> client(std::move(client_end.value()));
  fidl::Result result = client->EchoString({{.value = kHello}});
  ASSERT_TRUE(result.is_ok()) << result.error_value();
  EXPECT_EQ(result->response(), kHello);
}

}  // namespace
}  // namespace component::testing
