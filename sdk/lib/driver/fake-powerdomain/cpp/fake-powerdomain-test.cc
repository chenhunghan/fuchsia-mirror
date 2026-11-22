// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/driver/fake-powerdomain/cpp/fake-powerdomain.h>
#include <lib/fit/function.h>
#include <lib/zx/channel.h>

#include <map>
#include <optional>
#include <string>

#include <gtest/gtest.h>

namespace {

using fuchsia_hardware_powerdomain::Domain;

class FakePowerDomainTest : public ::testing::Test {
 protected:
  void SetUp() override { handlers_ = domain_.CreateInstanceHandler().TakeMemberHandlers(); }

  fidl::WireClient<Domain> Connect() {
    auto [client_end, server_end] = fidl::Endpoints<Domain>::Create();
    handlers_.at("domain")(server_end.TakeChannel());
    return {std::move(client_end), loop_.dispatcher()};
  }

  // Sends `Enable()` on `client` and returns whether it succeeded.
  bool Enable(fidl::WireClient<Domain>& client) {
    std::optional<bool> ok;
    client->Enable().ThenExactlyOnce([&](auto& result) { ok = result.ok() && result->is_ok(); });
    loop_.RunUntilIdle();
    return ok.value_or(false);
  }

  // Sends `Disable()` on `client` and returns whether it succeeded.
  bool Disable(fidl::WireClient<Domain>& client) {
    std::optional<bool> ok;
    client->Disable().ThenExactlyOnce([&](auto& result) { ok = result.ok() && result->is_ok(); });
    loop_.RunUntilIdle();
    return ok.value_or(false);
  }

  bool IsEnabled(fidl::WireClient<Domain>& client) {
    std::optional<bool> enabled;
    client->IsEnabled().ThenExactlyOnce([&](auto& result) {
      ASSERT_TRUE(result.ok());
      ASSERT_TRUE(result->is_ok());
      enabled = result.value()->enabled;
    });
    loop_.RunUntilIdle();
    return enabled.value();
  }

  async::Loop loop_{&kAsyncLoopConfigAttachToCurrentThread};
  fdf_fake::FakePowerDomain domain_{loop_.dispatcher()};
  std::map<std::string, fit::function<void(zx::channel)>> handlers_;
};

TEST_F(FakePowerDomainTest, VotesAreCountedPerConnection) {
  fidl::WireClient<Domain> client = Connect();
  EXPECT_FALSE(domain_.is_enabled());

  ASSERT_TRUE(Enable(client));
  ASSERT_TRUE(Enable(client));
  EXPECT_TRUE(domain_.is_enabled());
  EXPECT_TRUE(IsEnabled(client));

  ASSERT_TRUE(Disable(client));
  EXPECT_TRUE(domain_.is_enabled());

  ASSERT_TRUE(Disable(client));
  EXPECT_FALSE(domain_.is_enabled());
  EXPECT_FALSE(IsEnabled(client));

  // Disabling without a vote is a no-op.
  ASSERT_TRUE(Disable(client));
  EXPECT_FALSE(domain_.is_enabled());
}

TEST_F(FakePowerDomainTest, ConnectionsVoteIndependently) {
  fidl::WireClient<Domain> client1 = Connect();
  fidl::WireClient<Domain> client2 = Connect();

  ASSERT_TRUE(Enable(client1));
  ASSERT_TRUE(Disable(client2));
  EXPECT_TRUE(domain_.is_enabled());

  ASSERT_TRUE(Disable(client1));
  EXPECT_FALSE(domain_.is_enabled());
}

TEST_F(FakePowerDomainTest, ClosedConnectionOrphansVoteUntilReclaimed) {
  {
    fidl::WireClient<Domain> client = Connect();
    ASSERT_TRUE(Enable(client));
  }
  loop_.RunUntilIdle();
  EXPECT_TRUE(domain_.is_enabled());
  EXPECT_EQ(domain_.votes().orphan_count(), 1u);

  fidl::WireClient<Domain> client = Connect();
  ASSERT_TRUE(Enable(client));
  EXPECT_EQ(domain_.votes().orphan_count(), 0u);
  ASSERT_TRUE(Disable(client));
  EXPECT_FALSE(domain_.is_enabled());
}

TEST_F(FakePowerDomainTest, DirectlyBoundConnectionsShareOneVote) {
  auto [client_end1, server_end1] = fidl::Endpoints<Domain>::Create();
  fidl::BindServer(loop_.dispatcher(), std::move(server_end1), &domain_);
  auto [client_end2, server_end2] = fidl::Endpoints<Domain>::Create();
  fidl::BindServer(loop_.dispatcher(), std::move(server_end2), &domain_);
  fidl::WireClient<Domain> client1(std::move(client_end1), loop_.dispatcher());
  fidl::WireClient<Domain> client2(std::move(client_end2), loop_.dispatcher());

  ASSERT_TRUE(Enable(client1));
  ASSERT_TRUE(Disable(client2));
  EXPECT_FALSE(domain_.is_enabled());
}

}  // namespace
