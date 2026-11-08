// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_NETWORK_WATCHER_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_NETWORK_WATCHER_H_

#include <fidl/fuchsia.net.reachability/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>
#include <lib/sys/cpp/service_directory.h>

#include <memory>
#include <optional>
#include <vector>

#include "src/lib/backoff/backoff.h"

namespace forensics::feedback {

// Watches for changes to the network reachability status and calls registered callbacks whenever
// this occurs.
//
// The network is considered reachable when fuchsia.net.reachability/Monitor reports that HTTP
// requests are expected to succeed. If the connection to Monitor is lost, NetworkWatcher reconnects
// after |backoff| and keeps reporting the last known status until Monitor replies again.
class NetworkWatcher : public fidl::AsyncEventHandler<fuchsia_net_reachability::Monitor> {
 public:
  NetworkWatcher(async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services,
                 std::unique_ptr<backoff::Backoff> backoff);

  // Register a callback to be called when the network reachability status changes.
  void Register(::fit::function<void(bool)> on_reachable);

  // |fidl::AsyncEventHandler<fuchsia_net_reachability::Monitor>|
  void on_fidl_error(fidl::UnbindInfo error) override;

 private:
  void Connect();
  void Watch();

  async_dispatcher_t* dispatcher_;
  std::shared_ptr<sys::ServiceDirectory> services_;
  std::unique_ptr<backoff::Backoff> backoff_;

  fidl::Client<fuchsia_net_reachability::Monitor> monitor_;
  async::TaskClosureMethod<NetworkWatcher, &NetworkWatcher::Connect> reconnect_task_{this};

  std::vector<::fit::function<void(bool)>> callbacks_;

  std::optional<bool> reachable_;
};

}  // namespace forensics::feedback

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_NETWORK_WATCHER_H_
