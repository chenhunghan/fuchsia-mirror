// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/driver/fake-clock/cpp/fake-clock.h>

#include <optional>

#include <gtest/gtest.h>

namespace {

using fuchsia_hardware_clock::Clock;

class FakeClockTest : public ::testing::Test {
 protected:
  // Sends `Enable()` on `client` and returns whether it succeeded.
  bool Enable(fidl::WireClient<Clock>& client) {
    std::optional<bool> ok;
    client->Enable().ThenExactlyOnce([&](auto& result) { ok = result.ok() && result->is_ok(); });
    loop_.RunUntilIdle();
    return ok.value_or(false);
  }

  // Sends `Disable()` on `client` and returns whether it succeeded.
  bool Disable(fidl::WireClient<Clock>& client) {
    std::optional<bool> ok;
    client->Disable().ThenExactlyOnce([&](auto& result) { ok = result.ok() && result->is_ok(); });
    loop_.RunUntilIdle();
    return ok.value_or(false);
  }

  bool IsEnabled(fidl::WireClient<Clock>& client) {
    std::optional<bool> enabled;
    client->IsEnabled().ThenExactlyOnce([&](auto& result) {
      ASSERT_TRUE(result.ok());
      ASSERT_TRUE(result->is_ok());
      enabled = result.value()->enabled;
    });
    loop_.RunUntilIdle();
    return enabled.value();
  }

  fidl::WireClient<Clock> Connect() { return {clock_.Connect(), loop_.dispatcher()}; }

  async::Loop loop_{&kAsyncLoopConfigAttachToCurrentThread};
  fdf_fake::FakeClock clock_{loop_.dispatcher()};
};

TEST_F(FakeClockTest, VotesAreCountedPerConnection) {
  fidl::WireClient<Clock> client = Connect();
  EXPECT_FALSE(clock_.enabled());
  EXPECT_EQ(clock_.take_enabled(), std::nullopt);

  ASSERT_TRUE(Enable(client));
  ASSERT_TRUE(Enable(client));
  EXPECT_TRUE(clock_.enabled());
  EXPECT_TRUE(IsEnabled(client));

  ASSERT_TRUE(Disable(client));
  EXPECT_TRUE(clock_.enabled());
  EXPECT_EQ(clock_.take_enabled(), true);

  ASSERT_TRUE(Disable(client));
  EXPECT_FALSE(clock_.enabled());
  EXPECT_FALSE(IsEnabled(client));
  EXPECT_EQ(clock_.take_enabled(), false);
  EXPECT_EQ(clock_.take_enabled(), std::nullopt);

  // Disabling without a vote is a no-op.
  ASSERT_TRUE(Disable(client));
  EXPECT_FALSE(clock_.enabled());
}

TEST_F(FakeClockTest, ConnectionsVoteIndependently) {
  fidl::WireClient<Clock> client1 = Connect();
  fidl::WireClient<Clock> client2 = Connect();

  ASSERT_TRUE(Enable(client1));
  ASSERT_TRUE(Enable(client2));
  ASSERT_TRUE(Disable(client1));
  EXPECT_TRUE(clock_.enabled());

  // Another connection cannot withdraw client2's vote.
  ASSERT_TRUE(Disable(client1));
  EXPECT_TRUE(clock_.enabled());

  ASSERT_TRUE(Disable(client2));
  EXPECT_FALSE(clock_.enabled());
}

TEST_F(FakeClockTest, ClosedConnectionOrphansVoteUntilReclaimed) {
  {
    fidl::WireClient<Clock> client = Connect();
    ASSERT_TRUE(Enable(client));
    ASSERT_TRUE(Enable(client));
  }
  loop_.RunUntilIdle();
  EXPECT_TRUE(clock_.enabled());
  EXPECT_EQ(clock_.votes().orphan_count(), 1u);

  // A new connection from the same node reclaims the orphan.
  fidl::WireClient<Clock> client = Connect();
  ASSERT_TRUE(Enable(client));
  EXPECT_EQ(clock_.votes().orphan_count(), 0u);
  EXPECT_TRUE(clock_.enabled());

  ASSERT_TRUE(Disable(client));
  EXPECT_FALSE(clock_.enabled());
}

TEST_F(FakeClockTest, ClosedConnectionWithoutVoteIsDropped) {
  {
    fidl::WireClient<Clock> client = Connect();
    ASSERT_TRUE(Enable(client));
    ASSERT_TRUE(Disable(client));
  }
  loop_.RunUntilIdle();
  EXPECT_FALSE(clock_.enabled());
  EXPECT_EQ(clock_.votes().orphan_count(), 0u);
  EXPECT_EQ(clock_.votes().orphaned_total(), 0u);
}

TEST_F(FakeClockTest, FailedEnableDoesNotVote) {
  fidl::WireClient<Clock> client = Connect();
  clock_.set_enable_result(zx::error(ZX_ERR_IO));
  EXPECT_FALSE(Enable(client));
  EXPECT_FALSE(clock_.enabled());
  EXPECT_EQ(clock_.take_enabled(), std::nullopt);

  clock_.set_enable_result(zx::ok());
  ASSERT_TRUE(Enable(client));
  clock_.set_disable_result(zx::error(ZX_ERR_IO));
  EXPECT_FALSE(Disable(client));
  EXPECT_TRUE(clock_.enabled());
}

TEST_F(FakeClockTest, InstanceHandlerConnectionsVotePerConnection) {
  auto handlers = clock_.CreateInstanceHandler().TakeMemberHandlers();
  auto connect = [&]() {
    auto [client_end, server_end] = fidl::Endpoints<Clock>::Create();
    handlers.at("clock")(server_end.TakeChannel());
    return fidl::WireClient<Clock>(std::move(client_end), loop_.dispatcher());
  };

  fidl::WireClient<Clock> client1 = connect();
  fidl::WireClient<Clock> client2 = connect();
  ASSERT_TRUE(Enable(client1));
  ASSERT_TRUE(Disable(client2));
  EXPECT_TRUE(clock_.enabled());
  ASSERT_TRUE(Disable(client1));
  EXPECT_FALSE(clock_.enabled());
}

TEST_F(FakeClockTest, DirectlyBoundConnectionsShareOneVote) {
  auto [client_end1, server_end1] = fidl::Endpoints<Clock>::Create();
  fidl::BindServer(loop_.dispatcher(), std::move(server_end1), &clock_);
  auto [client_end2, server_end2] = fidl::Endpoints<Clock>::Create();
  fidl::BindServer(loop_.dispatcher(), std::move(server_end2), &clock_);
  fidl::WireClient<Clock> client1(std::move(client_end1), loop_.dispatcher());
  fidl::WireClient<Clock> client2(std::move(client_end2), loop_.dispatcher());

  ASSERT_TRUE(Enable(client1));
  ASSERT_TRUE(Disable(client2));
  EXPECT_FALSE(clock_.enabled());
}

TEST_F(FakeClockTest, NonVoteMethodsAreServedOnEveryConnection) {
  fidl::WireClient<Clock> client = Connect();
  std::optional<uint64_t> rate;
  client->SetRate(1000).ThenExactlyOnce([&](auto& result) {
    ASSERT_TRUE(result.ok());
    ASSERT_TRUE(result->is_ok());
  });
  client->GetRate().ThenExactlyOnce([&](auto& result) {
    ASSERT_TRUE(result.ok());
    ASSERT_TRUE(result->is_ok());
    rate = result.value()->hz;
  });
  loop_.RunUntilIdle();
  EXPECT_EQ(rate, 1000u);
  EXPECT_EQ(clock_.take_rate(), 1000u);
}

}  // namespace
