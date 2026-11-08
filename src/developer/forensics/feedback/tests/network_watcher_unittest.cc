// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/network_watcher.h"

#include <fidl/fuchsia.net.reachability/cpp/fidl.h>
#include <fidl/fuchsia.net.reachability/cpp/test_base.h>

#include <optional>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/testing/backoff.h"
#include "src/developer/forensics/testing/stubs/fidl_server.h"
#include "src/developer/forensics/testing/unit_test_fixture.h"

namespace forensics::feedback {
namespace {

using ::testing::ElementsAre;
using ::testing::IsEmpty;

// Serves fuchsia.net.reachability/Monitor and replies to the pending hanging get once a snapshot
// is set. A snapshot set while disconnected is held until the client reconnects and calls Watch.
class FakeMonitor : public stubs::SingleBindingFidlServer<fuchsia_net_reachability::Monitor> {
 public:
  void SetSnapshot(fuchsia_net_reachability::Snapshot snapshot) {
    snapshot_ = std::move(snapshot);
    MaybeReply();
  }

  // |fuchsia_net_reachability::Monitor|
  void Watch(WatchCompleter::Sync& completer) override {
    completer_ = completer.ToAsync();
    MaybeReply();
  }

 private:
  void MaybeReply() {
    // |completer_| may be from a closed connection, in which case it's replaced by the next Watch.
    if (!IsBound() || !completer_.has_value() || !snapshot_.has_value()) {
      return;
    }

    completer_->Reply(std::move(*snapshot_));
    completer_ = std::nullopt;
    snapshot_ = std::nullopt;
  }

  std::optional<WatchCompleter::Async> completer_;
  std::optional<fuchsia_net_reachability::Snapshot> snapshot_;
};

fuchsia_net_reachability::Snapshot HttpActive(const bool http_active) {
  fuchsia_net_reachability::Snapshot snapshot;
  snapshot.http_active(http_active);
  return snapshot;
}

class NetworkWatcherTest : public UnitTestFixture {
 public:
  NetworkWatcherTest() {
    InjectServiceProvider(&monitor_);
    network_watcher_.emplace(dispatcher(), services(), std::make_unique<MonotonicBackoff>());
    network_watcher_->Register([this](const bool reachable) { reachable_.push_back(reachable); });
  }

 protected:
  FakeMonitor monitor_;
  std::optional<NetworkWatcher> network_watcher_;
  std::vector<bool> reachable_;
};

TEST_F(NetworkWatcherTest, ReportsInitialSnapshot) {
  monitor_.SetSnapshot(HttpActive(false));
  RunLoopUntilIdle();

  EXPECT_THAT(reachable_, ElementsAre(false));
}

TEST_F(NetworkWatcherTest, ReportsChanges) {
  monitor_.SetSnapshot(HttpActive(false));
  RunLoopUntilIdle();

  monitor_.SetSnapshot(HttpActive(true));
  RunLoopUntilIdle();

  monitor_.SetSnapshot(HttpActive(false));
  RunLoopUntilIdle();

  EXPECT_THAT(reachable_, ElementsAre(false, true, false));
}

TEST_F(NetworkWatcherTest, IgnoresUnchangedHttpActive) {
  monitor_.SetSnapshot(HttpActive(true));
  RunLoopUntilIdle();

  fuchsia_net_reachability::Snapshot snapshot = HttpActive(true);
  snapshot.gateway_reachable(true);
  monitor_.SetSnapshot(std::move(snapshot));
  RunLoopUntilIdle();

  EXPECT_THAT(reachable_, ElementsAre(true));
}

TEST_F(NetworkWatcherTest, MissingHttpActiveIsUnreachable) {
  monitor_.SetSnapshot(fuchsia_net_reachability::Snapshot());
  RunLoopUntilIdle();

  EXPECT_THAT(reachable_, ElementsAre(false));
}

TEST_F(NetworkWatcherTest, RegisterAfterSnapshot) {
  monitor_.SetSnapshot(HttpActive(true));
  RunLoopUntilIdle();

  std::vector<bool> late_reachable;
  network_watcher_->Register(
      [&late_reachable](const bool reachable) { late_reachable.push_back(reachable); });
  EXPECT_THAT(late_reachable, ElementsAre(true));

  monitor_.SetSnapshot(HttpActive(false));
  RunLoopUntilIdle();

  EXPECT_THAT(reachable_, ElementsAre(true, false));
  EXPECT_THAT(late_reachable, ElementsAre(true, false));
}

TEST_F(NetworkWatcherTest, ReconnectsOnConnectionLoss) {
  monitor_.SetSnapshot(HttpActive(true));
  RunLoopUntilIdle();

  monitor_.CloseConnection(ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();

  // Sent once NetworkWatcher reconnects and calls Watch again.
  monitor_.SetSnapshot(HttpActive(false));

  // The connection should stay closed until the backoff allows it to reconnect.
  RunLoopFor(zx::msec(500));
  EXPECT_FALSE(monitor_.IsBound());
  EXPECT_THAT(reachable_, ElementsAre(true));

  RunLoopFor(zx::msec(500));
  EXPECT_TRUE(monitor_.IsBound());
  EXPECT_THAT(reachable_, ElementsAre(true, false));
}

TEST_F(NetworkWatcherTest, ResetsBackoffOnSuccess) {
  RunLoopUntilIdle();

  // The first reconnect is after 1 second, the second after 2 seconds.
  monitor_.CloseConnection(ZX_ERR_PEER_CLOSED);
  RunLoopFor(zx::sec(1));
  ASSERT_TRUE(monitor_.IsBound());

  monitor_.CloseConnection(ZX_ERR_PEER_CLOSED);
  RunLoopFor(zx::sec(1));
  EXPECT_FALSE(monitor_.IsBound());
  RunLoopFor(zx::sec(1));
  ASSERT_TRUE(monitor_.IsBound());

  // A reply resets the backoff, so the next reconnect is after 1 second again.
  monitor_.SetSnapshot(HttpActive(true));
  RunLoopUntilIdle();

  monitor_.CloseConnection(ZX_ERR_PEER_CLOSED);
  RunLoopFor(zx::sec(1));
  EXPECT_TRUE(monitor_.IsBound());

  EXPECT_THAT(reachable_, ElementsAre(true));
}

TEST_F(NetworkWatcherTest, KeepsLastStatusWhileDisconnected) {
  monitor_.SetSnapshot(HttpActive(true));
  RunLoopUntilIdle();

  monitor_.CloseConnection(ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();

  std::vector<bool> late_reachable;
  network_watcher_->Register(
      [&late_reachable](const bool reachable) { late_reachable.push_back(reachable); });
  EXPECT_THAT(late_reachable, ElementsAre(true));
  EXPECT_THAT(reachable_, ElementsAre(true));
}

TEST_F(NetworkWatcherTest, StopsIfMonitorNotFound) {
  RunLoopUntilIdle();

  monitor_.CloseConnection(ZX_ERR_NOT_FOUND);
  RunLoopFor(zx::hour(1));

  EXPECT_FALSE(monitor_.IsBound());
  EXPECT_THAT(reachable_, IsEmpty());
}

TEST_F(NetworkWatcherTest, DestroyedWhileWatchPending) {
  RunLoopUntilIdle();

  network_watcher_ = std::nullopt;
  monitor_.SetSnapshot(HttpActive(true));
  RunLoopUntilIdle();

  EXPECT_THAT(reachable_, IsEmpty());
}

TEST_F(NetworkWatcherTest, DestroyedWhileReconnectPending) {
  RunLoopUntilIdle();

  monitor_.CloseConnection(ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();

  network_watcher_ = std::nullopt;
  RunLoopFor(zx::sec(1));

  EXPECT_FALSE(monitor_.IsBound());
}

}  // namespace
}  // namespace forensics::feedback
