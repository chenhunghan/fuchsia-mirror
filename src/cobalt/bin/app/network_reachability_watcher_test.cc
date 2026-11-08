// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/cobalt/bin/app/network_reachability_watcher.h"

#include <fidl/fuchsia.net.reachability/cpp/fidl.h>
#include <lib/sys/cpp/testing/service_directory_provider.h>
#include <zircon/errors.h>

#include <cstdint>
#include <memory>
#include <utility>

#include <gtest/gtest.h>

#include "src/cobalt/bin/app/fake_network_reachability.h"
#include "src/lib/testing/loop_fixture/test_loop_fixture.h"

namespace cobalt {
namespace {

fuchsia_net_reachability::Snapshot MakeSnapshot(bool http_active) {
  fuchsia_net_reachability::Snapshot snapshot;
  snapshot.http_active(http_active);
  return snapshot;
}

class NetworkReachabilityWatcherTest : public gtest::TestLoopFixture {
 protected:
  NetworkReachabilityWatcherTest()
      : service_directory_provider_(dispatcher()),
        watcher_(dispatcher(), service_directory_provider_.service_directory(),
                 [this] { ++reconnect_callback_count_; }) {}

  void RegisterFakeMonitorServer() {
    monitor_server_ = std::make_unique<FakeReachabilityMonitor>();
    ASSERT_EQ(service_directory_provider_.AddService<fuchsia_net_reachability::Monitor>(
                  [this](fidl::InterfaceRequest<fuchsia_net_reachability::Monitor> request) {
                    monitor_server_->Bind(
                        dispatcher(),
                        fidl::ServerEnd<fuchsia_net_reachability::Monitor>(request.TakeChannel()));
                  },
                  fidl::DiscoverableProtocolName<fuchsia_net_reachability::Monitor>),
              ZX_OK);
  }

  [[nodiscard]] FakeReachabilityMonitor& monitor_server() { return *monitor_server_; }
  [[nodiscard]] NetworkReachabilityWatcher& watcher() { return watcher_; }
  [[nodiscard]] uint32_t reconnect_callback_count() const { return reconnect_callback_count_; }
  [[nodiscard]] const std::shared_ptr<sys::ServiceDirectory>& service_directory() {
    return service_directory_provider_.service_directory();
  }

 private:
  sys::testing::ServiceDirectoryProvider service_directory_provider_;
  std::unique_ptr<FakeReachabilityMonitor> monitor_server_;
  uint32_t reconnect_callback_count_ = 0;
  NetworkReachabilityWatcher watcher_;
};

TEST_F(NetworkReachabilityWatcherTest, InitialHttpActiveSnapshotInvokesCallback) {
  RegisterFakeMonitorServer();
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));

  watcher().StartWatching();
  RunLoopUntilIdle();

  EXPECT_EQ(monitor_server().connection_count(), 1u);
  EXPECT_EQ(reconnect_callback_count(), 1u);
}

TEST_F(NetworkReachabilityWatcherTest, HttpActiveBecomingTrueAfterInitialSnapshotInvokesCallback) {
  RegisterFakeMonitorServer();
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));

  watcher().StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(reconnect_callback_count(), 0u);

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));
  RunLoopUntilIdle();
  EXPECT_EQ(reconnect_callback_count(), 1u);
}

TEST_F(NetworkReachabilityWatcherTest, HttpActiveBecomingFalseDoesNotInvokeCallback) {
  RegisterFakeMonitorServer();
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));

  watcher().StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(reconnect_callback_count(), 1u);

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));
  RunLoopUntilIdle();
  EXPECT_EQ(reconnect_callback_count(), 1u);
}

TEST_F(NetworkReachabilityWatcherTest, HttpActiveReturningTrueAfterBecomingFalseInvokesCallback) {
  RegisterFakeMonitorServer();
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));

  watcher().StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(reconnect_callback_count(), 1u);

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));
  RunLoopUntilIdle();
  ASSERT_EQ(reconnect_callback_count(), 1u);

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));
  RunLoopUntilIdle();
  EXPECT_EQ(reconnect_callback_count(), 2u);
}

TEST_F(NetworkReachabilityWatcherTest,
       UnrelatedSnapshotUpdateWhileHttpActiveRemainsTrueDoesNotInvokeCallback) {
  RegisterFakeMonitorServer();
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));

  watcher().StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(reconnect_callback_count(), 1u);

  fuchsia_net_reachability::Snapshot dns_change;
  dns_change.internet_available(true);
  dns_change.gateway_reachable(true);
  dns_change.dns_active(true);
  dns_change.http_active(true);
  monitor_server().EnqueueSnapshot(std::move(dns_change));
  RunLoopUntilIdle();
  EXPECT_EQ(reconnect_callback_count(), 1u);
}

TEST_F(NetworkReachabilityWatcherTest, SnapshotMissingHttpActiveFieldIsTreatedAsInactive) {
  RegisterFakeMonitorServer();
  fuchsia_net_reachability::Snapshot initial_without_http;
  initial_without_http.internet_available(true);
  initial_without_http.dns_active(true);
  monitor_server().EnqueueSnapshot(std::move(initial_without_http));

  watcher().StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(reconnect_callback_count(), 0u);

  // Subsequent snapshot still omitting http_active does not trigger the callback.
  fuchsia_net_reachability::Snapshot gateway_only;
  gateway_only.gateway_reachable(true);
  monitor_server().EnqueueSnapshot(std::move(gateway_only));
  RunLoopUntilIdle();
  EXPECT_EQ(reconnect_callback_count(), 0u);

  // When http_active is later reported true, the callback fires.
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));
  RunLoopUntilIdle();
  EXPECT_EQ(reconnect_callback_count(), 1u);
}

TEST_F(NetworkReachabilityWatcherTest,
       ReconnectsAfterMonitorChannelClosesAndNotifiesWhenHttpActiveIsTrue) {
  RegisterFakeMonitorServer();
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));

  watcher().StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(monitor_server().connection_count(), 1u);
  ASSERT_EQ(reconnect_callback_count(), 0u);

  monitor_server().Close(/*epitaph=*/ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();
  EXPECT_EQ(reconnect_callback_count(), 0u);

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));
  RunLoopFor(zx::msec(200));
  EXPECT_EQ(monitor_server().connection_count(), 2u);
  EXPECT_EQ(reconnect_callback_count(), 1u);
}

TEST_F(NetworkReachabilityWatcherTest,
       InitialMonitorDisconnectBeforeSnapshotNotifiesOnReconnectWhenHttpActiveIsTrue) {
  RegisterFakeMonitorServer();
  monitor_server().SetCloseOnConnect(true);

  watcher().StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(monitor_server().connection_count(), 1u);
  ASSERT_EQ(reconnect_callback_count(), 0u);

  monitor_server().SetCloseOnConnect(false);
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));

  RunLoopFor(zx::msec(200));
  EXPECT_EQ(monitor_server().connection_count(), 2u);
  EXPECT_EQ(reconnect_callback_count(), 1u);
}

TEST_F(NetworkReachabilityWatcherTest,
       ReconnectSnapshotWithHttpActiveFalseDoesNotNotifyUntilHttpActiveBecomesTrue) {
  RegisterFakeMonitorServer();
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));

  watcher().StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(monitor_server().connection_count(), 1u);

  monitor_server().Close(/*epitaph=*/ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));
  RunLoopFor(zx::msec(200));
  EXPECT_EQ(monitor_server().connection_count(), 2u);
  EXPECT_EQ(reconnect_callback_count(), 0u);

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));
  RunLoopUntilIdle();
  EXPECT_EQ(reconnect_callback_count(), 1u);
}

TEST_F(NetworkReachabilityWatcherTest,
       ExponentialReconnectBackoffAdvancesAcrossPreSnapshotDisconnectsAndResetsAfterSnapshot) {
  RegisterFakeMonitorServer();
  watcher().StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(monitor_server().connection_count(), 1u);

  // Failure 1 -> backoff in [100ms, 200ms].
  monitor_server().Close(/*epitaph=*/ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();
  RunLoopFor(zx::msec(99));
  EXPECT_EQ(monitor_server().connection_count(), 1u);
  RunLoopFor(zx::msec(101));
  EXPECT_EQ(monitor_server().connection_count(), 2u);

  // Failure 2 (before receiving a Snapshot) -> backoff in [200ms, 400ms].
  monitor_server().Close(/*epitaph=*/ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();
  RunLoopFor(zx::msec(199));
  EXPECT_EQ(monitor_server().connection_count(), 2u);
  RunLoopFor(zx::msec(201));
  EXPECT_EQ(monitor_server().connection_count(), 3u);

  // Failure 3 (before receiving a Snapshot) -> backoff in [400ms, 800ms]; allow this attempt to
  // receive a Snapshot.
  monitor_server().Close(/*epitaph=*/ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));
  RunLoopFor(zx::msec(399));
  EXPECT_EQ(monitor_server().connection_count(), 3u);
  RunLoopFor(zx::msec(401));
  EXPECT_EQ(monitor_server().connection_count(), 4u);

  // After receiving a Snapshot, a subsequent disconnect resets backoff to [100ms, 200ms].
  monitor_server().Close(/*epitaph=*/ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();
  RunLoopFor(zx::msec(99));
  EXPECT_EQ(monitor_server().connection_count(), 4u);
  RunLoopFor(zx::msec(101));
  EXPECT_EQ(monitor_server().connection_count(), 5u);
}

TEST_F(NetworkReachabilityWatcherTest,
       StartWatchingWhileReconnectTaskIsPendingCancelsTaskAndResetsBackoff) {
  RegisterFakeMonitorServer();
  monitor_server().SetCloseOnConnect(true);

  watcher().StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(monitor_server().connection_count(), 1u);

  RunLoopFor(zx::msec(50));
  monitor_server().SetCloseOnConnect(false);
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));
  watcher().StartWatching();
  RunLoopUntilIdle();
  EXPECT_EQ(monitor_server().connection_count(), 2u);

  RunLoopFor(zx::msec(100));
  EXPECT_EQ(monitor_server().connection_count(), 2u);
  EXPECT_EQ(reconnect_callback_count(), 0u);
}

TEST_F(NetworkReachabilityWatcherTest, DestructorWhileWatchIsPendingOnOpenChannelCancelsCleanly) {
  RegisterFakeMonitorServer();
  uint32_t local_callback_count = 0;
  {
    NetworkReachabilityWatcher local_watcher(dispatcher(), service_directory(),
                                             [&local_callback_count] { ++local_callback_count; });
    monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));
    local_watcher.StartWatching();
    RunLoopUntilIdle();
    ASSERT_EQ(monitor_server().connection_count(), 1u);
  }

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));
  RunLoopUntilIdle();
  EXPECT_EQ(local_callback_count, 0u);
}

TEST_F(NetworkReachabilityWatcherTest, DestructorWhileReconnectTaskIsPendingCancelsCleanly) {
  RegisterFakeMonitorServer();
  uint32_t local_callback_count = 0;
  {
    NetworkReachabilityWatcher local_watcher(dispatcher(), service_directory(),
                                             [&local_callback_count] { ++local_callback_count; });
    local_watcher.StartWatching();
    RunLoopUntilIdle();
    monitor_server().Close(/*epitaph=*/ZX_ERR_PEER_CLOSED);
    RunLoopUntilIdle();
  }

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));
  RunLoopFor(zx::sec(1));
  EXPECT_EQ(monitor_server().connection_count(), 1u);
  EXPECT_EQ(local_callback_count, 0u);
}

TEST_F(NetworkReachabilityWatcherTest, DestroyingWatcherInsideOnNetworkReconnectCallbackIsSafe) {
  RegisterFakeMonitorServer();
  uint32_t callback_count = 0;
  std::unique_ptr<NetworkReachabilityWatcher> local_watcher;
  local_watcher = std::make_unique<NetworkReachabilityWatcher>(dispatcher(), service_directory(),
                                                               [&local_watcher, &callback_count] {
                                                                 ++callback_count;
                                                                 local_watcher.reset();
                                                               });

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));
  local_watcher->StartWatching();
  RunLoopUntilIdle();

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));
  RunLoopUntilIdle();

  EXPECT_EQ(callback_count, 1u);
  EXPECT_EQ(local_watcher, nullptr);
}

TEST_F(NetworkReachabilityWatcherTest,
       CallingStartWatchingInsideOnNetworkReconnectCallbackRebindsCleanly) {
  RegisterFakeMonitorServer();
  uint32_t callback_count = 0;
  std::unique_ptr<NetworkReachabilityWatcher> local_watcher;
  local_watcher = std::make_unique<NetworkReachabilityWatcher>(dispatcher(), service_directory(),
                                                               [&local_watcher, &callback_count] {
                                                                 ++callback_count;
                                                                 if (callback_count == 1u) {
                                                                   local_watcher->StartWatching();
                                                                 }
                                                               });

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));
  local_watcher->StartWatching();
  RunLoopUntilIdle();
  ASSERT_EQ(monitor_server().connection_count(), 1u);

  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));
  RunLoopUntilIdle();
  EXPECT_EQ(callback_count, 1u);
  EXPECT_EQ(monitor_server().connection_count(), 2u);

  // Complete the re-initialized snapshot and verify subsequent snapshots still work.
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/false));
  monitor_server().EnqueueSnapshot(MakeSnapshot(/*http_active=*/true));
  RunLoopUntilIdle();
  EXPECT_EQ(callback_count, 2u);
}

}  // namespace
}  // namespace cobalt
