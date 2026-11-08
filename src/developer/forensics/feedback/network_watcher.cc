// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/network_watcher.h"

#include <lib/syslog/cpp/macros.h>

#include <format>

namespace forensics::feedback {
namespace {

constexpr const char* kMonitorName =
    fidl::DiscoverableProtocolName<fuchsia_net_reachability::Monitor>;

}  // namespace

NetworkWatcher::NetworkWatcher(async_dispatcher_t* dispatcher,
                               std::shared_ptr<sys::ServiceDirectory> services,
                               std::unique_ptr<backoff::Backoff> backoff)
    : dispatcher_(dispatcher), services_(std::move(services)), backoff_(std::move(backoff)) {
  Connect();
}

void NetworkWatcher::Connect() {
  auto [client, server] = fidl::Endpoints<fuchsia_net_reachability::Monitor>::Create();
  if (const zx_status_t status = services_->Connect(kMonitorName, server.TakeChannel());
      status != ZX_OK) {
    FX_PLOGS(ERROR, status) << std::format(
        "Failed to connect to {}; cannot watch for network reachability status", kMonitorName);
    return;
  }

  monitor_ = fidl::Client(std::move(client), dispatcher_, this);
  Watch();
}

void NetworkWatcher::on_fidl_error(const fidl::UnbindInfo error) {
  monitor_ = fidl::Client<fuchsia_net_reachability::Monitor>();

  if (error.status() == ZX_ERR_NOT_FOUND) {
    FX_LOGS(WARNING) << std::format("{} unavailable, will not retry", kMonitorName);
    return;
  }

  FX_PLOGS(WARNING, error.status()) << std::format("Lost connection to {}", kMonitorName);
  reconnect_task_.PostDelayed(dispatcher_, backoff_->GetNext());
}

void NetworkWatcher::Watch() {
  monitor_->Watch().Then([this](fidl::Result<fuchsia_net_reachability::Monitor::Watch>& result) {
    // Errors unbind |monitor_|, and on_fidl_error() decides whether to reconnect.
    if (result.is_error()) {
      return;
    }

    backoff_->Reset();
    Watch();

    const bool reachable = result->http_active().value_or(false);
    if (reachable_.has_value() && *reachable_ == reachable) {
      return;
    }

    reachable_ = reachable;
    for (const ::fit::function<void(bool)>& on_reachable : callbacks_) {
      on_reachable(reachable);
    }
  });
}

void NetworkWatcher::Register(fit::function<void(bool)> on_reachable) {
  if (reachable_.has_value()) {
    on_reachable(reachable_.value());
  }
  callbacks_.push_back(std::move(on_reachable));
}

}  // namespace forensics::feedback
