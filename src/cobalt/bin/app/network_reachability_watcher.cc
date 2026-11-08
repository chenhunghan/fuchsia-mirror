// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/cobalt/bin/app/network_reachability_watcher.h"

#include <fidl/fuchsia.net.reachability/cpp/markers.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/status.h>

#include <utility>

namespace cobalt {

NetworkReachabilityWatcher::NetworkReachabilityWatcher(
    async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services,
    fit::function<void()> on_network_reconnect)
    : dispatcher_(dispatcher),
      services_(std::move(services)),
      on_network_reconnect_(std::move(on_network_reconnect)) {
  FX_CHECK(dispatcher_ != nullptr);
  FX_CHECK(services_ != nullptr);
  FX_CHECK(on_network_reconnect_ != nullptr);
}

NetworkReachabilityWatcher::~NetworkReachabilityWatcher() {
  reconnect_task_.Cancel();
  monitor_ = {};
}

void NetworkReachabilityWatcher::StartWatching() {
  reconnect_task_.Cancel();
  http_active_ = false;
  backoff_.Reset();
  ConnectToMonitor();
}

void NetworkReachabilityWatcher::ConnectToMonitor() {
  monitor_ = {};
  http_active_ = false;

  zx::result endpoints = fidl::CreateEndpoints<fuchsia_net_reachability::Monitor>();
  if (endpoints.is_error()) {
    FX_PLOGS(WARNING, endpoints.error_value())
        << "NetworkReachabilityWatcher: Failed to create fuchsia.net.reachability.Monitor "
           "endpoints";
    ScheduleReconnect();
    return;
  }
  const zx_status_t status =
      services_->Connect(fidl::DiscoverableProtocolName<fuchsia_net_reachability::Monitor>,
                         endpoints->server.TakeChannel());
  if (status != ZX_OK) {
    FX_PLOGS(WARNING, status)
        << "NetworkReachabilityWatcher: Failed to connect to fuchsia.net.reachability.Monitor";
    ScheduleReconnect();
    return;
  }

  monitor_.Bind(std::move(endpoints->client), dispatcher_);
  Watch();
}

void NetworkReachabilityWatcher::ScheduleReconnect() {
  monitor_ = {};
  http_active_ = false;
  if (reconnect_task_.is_pending()) {
    return;
  }
  const zx_status_t status = reconnect_task_.PostDelayed(dispatcher_, backoff_.GetNext());
  if (status != ZX_OK && status != ZX_ERR_BAD_STATE) {
    FX_PLOGS(ERROR, status) << "NetworkReachabilityWatcher: Failed to post reconnect task";
  }
}

void NetworkReachabilityWatcher::Watch() {
  monitor_->Watch().Then([this](fidl::Result<fuchsia_net_reachability::Monitor::Watch>& result) {
    if (result.is_error()) {
      if (!result.error_value().is_canceled()) {
        FX_LOGS(WARNING) << "NetworkReachabilityWatcher: Watch failed: "
                         << result.error_value().FormatDescription();
        ScheduleReconnect();
      }
      return;
    }

    const bool should_notify = HandleSnapshot(*result);
    if (monitor_.is_valid()) {
      Watch();
    }
    if (should_notify) {
      on_network_reconnect_();
    }
  });
}

bool NetworkReachabilityWatcher::HandleSnapshot(
    const fuchsia_net_reachability::Snapshot& snapshot) {
  backoff_.Reset();
  const bool was_http_active = http_active_;
  http_active_ = snapshot.http_active().value_or(false);
  return !was_http_active && http_active_;
}

}  // namespace cobalt
