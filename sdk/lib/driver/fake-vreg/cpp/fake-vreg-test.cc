// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/driver/fake-vreg/cpp/fake-vreg.h>

#include <optional>

#include <gtest/gtest.h>

namespace {

using fuchsia_hardware_vreg::Vreg;

class FakeVregTest : public ::testing::Test {
 protected:
  fidl::WireClient<Vreg> Connect() {
    auto [client_end, server_end] = fidl::Endpoints<Vreg>::Create();
    vreg_.Bind(loop_.dispatcher(), std::move(server_end));
    return {std::move(client_end), loop_.dispatcher()};
  }

  // Sends `Enable()` on `client` and returns whether it succeeded.
  bool Enable(fidl::WireClient<Vreg>& client) {
    std::optional<bool> ok;
    client->Enable().ThenExactlyOnce([&](auto& result) { ok = result.ok() && result->is_ok(); });
    loop_.RunUntilIdle();
    return ok.value_or(false);
  }

  // Sends `Disable()` on `client` and returns whether it succeeded.
  bool Disable(fidl::WireClient<Vreg>& client) {
    std::optional<bool> ok;
    client->Disable().ThenExactlyOnce([&](auto& result) { ok = result.ok() && result->is_ok(); });
    loop_.RunUntilIdle();
    return ok.value_or(false);
  }

  // Sends `SetState({enable})` on `client` and returns whether it succeeded.
  bool SetEnable(fidl::WireClient<Vreg>& client, bool enable) {
    fidl::Arena arena;
    std::optional<bool> ok;
    client
        ->SetState(
            fuchsia_hardware_vreg::wire::VregSetStateRequest::Builder(arena).enable(enable).Build())
        .ThenExactlyOnce([&](auto& result) { ok = result.ok() && result->is_ok(); });
    loop_.RunUntilIdle();
    return ok.value_or(false);
  }

  async::Loop loop_{&kAsyncLoopConfigAttachToCurrentThread};
  fdf_fake::FakeVreg vreg_{loop_.dispatcher()};
};

TEST_F(FakeVregTest, VotesAreIdempotentPerConnection) {
  fidl::WireClient<Vreg> client = Connect();
  EXPECT_FALSE(vreg_.enabled());
  EXPECT_EQ(vreg_.take_enable_request(), std::nullopt);

  ASSERT_TRUE(Enable(client));
  ASSERT_TRUE(Enable(client));
  EXPECT_TRUE(vreg_.enabled());
  EXPECT_EQ(vreg_.take_enable_request(), true);

  ASSERT_TRUE(Disable(client));
  EXPECT_FALSE(vreg_.enabled());
  EXPECT_EQ(vreg_.take_enable_request(), false);

  ASSERT_TRUE(SetEnable(client, true));
  EXPECT_TRUE(vreg_.enabled());
  ASSERT_TRUE(SetEnable(client, false));
  EXPECT_FALSE(vreg_.enabled());
}

TEST_F(FakeVregTest, ConnectionsVoteIndependently) {
  fidl::WireClient<Vreg> client1 = Connect();
  fidl::WireClient<Vreg> client2 = Connect();

  ASSERT_TRUE(Enable(client1));
  ASSERT_TRUE(SetEnable(client2, true));
  ASSERT_TRUE(Disable(client1));
  EXPECT_TRUE(vreg_.enabled());

  ASSERT_TRUE(SetEnable(client2, false));
  EXPECT_FALSE(vreg_.enabled());
}

TEST_F(FakeVregTest, ClosedConnectionOrphansVoteUntilReclaimed) {
  {
    fidl::WireClient<Vreg> client = Connect();
    ASSERT_TRUE(Enable(client));
  }
  loop_.RunUntilIdle();
  EXPECT_TRUE(vreg_.enabled());
  EXPECT_EQ(vreg_.votes().orphan_count(), 1u);

  fidl::WireClient<Vreg> client = Connect();
  ASSERT_TRUE(SetEnable(client, true));
  EXPECT_EQ(vreg_.votes().orphan_count(), 0u);
  ASSERT_TRUE(Disable(client));
  EXPECT_FALSE(vreg_.enabled());
}

TEST_F(FakeVregTest, FailedRequestsDoNotVote) {
  fidl::WireClient<Vreg> client = Connect();
  vreg_.set_enable_result(zx::error(ZX_ERR_IO));
  EXPECT_FALSE(Enable(client));
  vreg_.set_set_state_result(zx::error(ZX_ERR_IO));
  EXPECT_FALSE(SetEnable(client, true));
  EXPECT_FALSE(vreg_.enabled());
  EXPECT_EQ(vreg_.take_enable_request(), std::nullopt);
}

TEST_F(FakeVregTest, InstanceHandlerConnectionsVotePerConnection) {
  auto handlers = vreg_.CreateInstanceHandler().TakeMemberHandlers();
  auto connect = [&]() {
    auto [client_end, server_end] = fidl::Endpoints<Vreg>::Create();
    handlers.at("vreg")(server_end.TakeChannel());
    return fidl::WireClient<Vreg>(std::move(client_end), loop_.dispatcher());
  };

  fidl::WireClient<Vreg> client1 = connect();
  fidl::WireClient<Vreg> client2 = connect();
  ASSERT_TRUE(Enable(client1));
  ASSERT_TRUE(Disable(client2));
  EXPECT_TRUE(vreg_.enabled());
  ASSERT_TRUE(Disable(client1));
  EXPECT_FALSE(vreg_.enabled());
}

TEST_F(FakeVregTest, DirectlyBoundConnectionsShareOneVote) {
  auto [client_end1, server_end1] = fidl::Endpoints<Vreg>::Create();
  fidl::BindServer(loop_.dispatcher(), std::move(server_end1), &vreg_);
  auto [client_end2, server_end2] = fidl::Endpoints<Vreg>::Create();
  fidl::BindServer(loop_.dispatcher(), std::move(server_end2), &vreg_);
  fidl::WireClient<Vreg> client1(std::move(client_end1), loop_.dispatcher());
  fidl::WireClient<Vreg> client2(std::move(client_end2), loop_.dispatcher());

  ASSERT_TRUE(Enable(client1));
  ASSERT_TRUE(Disable(client2));
  EXPECT_FALSE(vreg_.enabled());
}

}  // namespace
