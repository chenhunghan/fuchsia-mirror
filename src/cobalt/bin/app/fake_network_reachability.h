// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_COBALT_BIN_APP_FAKE_NETWORK_REACHABILITY_H_
#define SRC_COBALT_BIN_APP_FAKE_NETWORK_REACHABILITY_H_

#include <fidl/fuchsia.net.reachability/cpp/fidl.h>
#include <fidl/fuchsia.net.reachability/cpp/test_base.h>
#include <lib/async/dispatcher.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/types.h>

#include <cstddef>
#include <deque>
#include <optional>
#include <string>
#include <utility>

namespace cobalt {

class FakeReachabilityMonitor : public fidl::testing::TestBase<fuchsia_net_reachability::Monitor> {
 public:
  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    FX_NOTIMPLEMENTED() << name << " is not implemented";
  }

  void Watch(WatchCompleter::Sync& completer) override {
    if (!queued_snapshots_.empty()) {
      fuchsia_net_reachability::Snapshot snapshot = std::move(queued_snapshots_.front());
      queued_snapshots_.pop_front();
      completer.Reply(
          fidl::Response<fuchsia_net_reachability::Monitor::Watch>(std::move(snapshot)));
      return;
    }
    pending_completer_ = completer.ToAsync();
  }

  void Bind(async_dispatcher_t* dispatcher,
            fidl::ServerEnd<fuchsia_net_reachability::Monitor> server_end) {
    ++connection_count_;
    pending_completer_.reset();
    if (close_on_connect_) {
      server_end.Close(ZX_ERR_PEER_CLOSED);
      return;
    }
    binding_ = fidl::BindServer(dispatcher, std::move(server_end), this);
  }

  void EnqueueSnapshot(fuchsia_net_reachability::Snapshot snapshot) {
    if (pending_completer_.has_value()) {
      WatchCompleter::Async completer = std::move(*pending_completer_);
      pending_completer_.reset();
      completer.Reply(
          fidl::Response<fuchsia_net_reachability::Monitor::Watch>(std::move(snapshot)));
      return;
    }
    queued_snapshots_.push_back(std::move(snapshot));
  }

  void Close(zx_status_t epitaph) {
    if (pending_completer_.has_value()) {
      pending_completer_->Close(epitaph);
      pending_completer_.reset();
    }
    if (binding_.has_value()) {
      binding_->Close(epitaph);
      binding_.reset();
    }
  }

  void SetCloseOnConnect(bool close_on_connect) { close_on_connect_ = close_on_connect; }

  [[nodiscard]] size_t connection_count() const { return connection_count_; }

 private:
  std::optional<fidl::ServerBindingRef<fuchsia_net_reachability::Monitor>> binding_;
  std::optional<WatchCompleter::Async> pending_completer_;
  std::deque<fuchsia_net_reachability::Snapshot> queued_snapshots_;
  size_t connection_count_ = 0;
  bool close_on_connect_ = false;
};

}  // namespace cobalt

#endif  // SRC_COBALT_BIN_APP_FAKE_NETWORK_REACHABILITY_H_
