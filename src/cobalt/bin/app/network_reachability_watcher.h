// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_COBALT_BIN_APP_NETWORK_REACHABILITY_WATCHER_H_
#define SRC_COBALT_BIN_APP_NETWORK_REACHABILITY_WATCHER_H_

#include <fidl/fuchsia.net.reachability/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/zx/time.h>

#include <cstdint>
#include <memory>

#include "src/lib/backoff/exponential_backoff.h"

namespace cobalt {

// Watches fuchsia.net.reachability/Monitor for Snapshot.http_active becoming
// true and invokes a callback on |dispatcher|.
//
// If the Monitor channel closes, NetworkReachabilityWatcher reconnects to
// fuchsia.net.reachability.Monitor after an exponential backoff.
class NetworkReachabilityWatcher {
 public:
  // |dispatcher|, |services|, and |on_network_reconnect| must be non-null.
  NetworkReachabilityWatcher(async_dispatcher_t* dispatcher,
                             std::shared_ptr<sys::ServiceDirectory> services,
                             fit::function<void()> on_network_reconnect);

  ~NetworkReachabilityWatcher();

  NetworkReachabilityWatcher(const NetworkReachabilityWatcher&) = delete;
  NetworkReachabilityWatcher& operator=(const NetworkReachabilityWatcher&) = delete;
  NetworkReachabilityWatcher(NetworkReachabilityWatcher&&) = delete;
  NetworkReachabilityWatcher& operator=(NetworkReachabilityWatcher&&) = delete;

  // Connects to fuchsia.net.reachability.Monitor and begins watching
  // reachability snapshots on |dispatcher|.
  void StartWatching();

 private:
  static constexpr zx::duration kInitialReconnectDelay = zx::msec(100);
  static constexpr uint32_t kReconnectBackoffFactor = 2u;
  static constexpr zx::duration kMaxReconnectDelay = zx::hour(1);

  void ConnectToMonitor();
  void ScheduleReconnect();
  void Watch();
  [[nodiscard]] bool HandleSnapshot(const fuchsia_net_reachability::Snapshot& snapshot);

  async_dispatcher_t* const dispatcher_;
  const std::shared_ptr<sys::ServiceDirectory> services_;
  const fit::function<void()> on_network_reconnect_;

  bool http_active_ = false;
  backoff::ExponentialBackoff backoff_{kInitialReconnectDelay, kReconnectBackoffFactor,
                                       kMaxReconnectDelay};
  fidl::Client<fuchsia_net_reachability::Monitor> monitor_;
  async::TaskClosureMethod<NetworkReachabilityWatcher,
                           &NetworkReachabilityWatcher::ConnectToMonitor>
      reconnect_task_{this};
};

}  // namespace cobalt

#endif  // SRC_COBALT_BIN_APP_NETWORK_REACHABILITY_WATCHER_H_
