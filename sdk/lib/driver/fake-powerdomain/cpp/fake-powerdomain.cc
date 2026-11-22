// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/fake-powerdomain/cpp/fake-powerdomain.h>

#include <cstdio>

namespace fdf_fake {

// Serves one connection. Owns that connection's vote; everything else is forwarded to the
// `FakePowerDomain`.
class FakePowerDomain::Connection final
    : public fidl::WireServer<fuchsia_hardware_powerdomain::Domain> {
 public:
  explicit Connection(FakePowerDomain& parent)
      : parent_(parent), vote_(kNodeKey, fdf_resource_vote::CloseBehavior::kOrphanOnClose) {}

  const fdf_resource_vote::Vote& vote() const { return vote_; }

  void Enable(EnableCompleter::Sync& completer) override {
    parent_.EnableForVote(vote_, completer);
  }
  void Disable(DisableCompleter::Sync& completer) override {
    parent_.DisableForVote(vote_, completer);
  }
  void IsEnabled(IsEnabledCompleter::Sync& completer) override { parent_.IsEnabled(completer); }
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_powerdomain::Domain> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    parent_.handle_unknown_method(metadata, completer);
  }

 private:
  FakePowerDomain& parent_;
  fdf_resource_vote::Vote vote_;
};

FakePowerDomain::FakePowerDomain(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {
  // Log to stderr rather than to the global driver logger, which may belong to (and be torn
  // down with) a driver under test.
  votes_.SetLogSinkForTesting([](fdf_resource_vote::LogSeverity, const std::string& message) {
    fprintf(stderr, "[fake-powerdomain] %s\n", message.c_str());
  });
}

FakePowerDomain::~FakePowerDomain() = default;

fuchsia_hardware_powerdomain::Service::InstanceHandler FakePowerDomain::CreateInstanceHandler() {
  return fuchsia_hardware_powerdomain::Service::InstanceHandler({
      .domain =
          [this](fidl::ServerEnd<fuchsia_hardware_powerdomain::Domain> server_end) {
            auto connection = std::make_unique<Connection>(*this);
            Connection* connection_ptr = connection.get();
            connections_.emplace(connection_ptr, std::move(connection));
            bindings_.AddBinding(
                dispatcher_, std::move(server_end), connection_ptr,
                [this](Connection* closed, fidl::UnbindInfo) { OnConnectionClosed(closed); });
          },
  });
}

void FakePowerDomain::Enable(EnableCompleter::Sync& completer) {
  EnableForVote(direct_vote_, completer);
}

void FakePowerDomain::Disable(DisableCompleter::Sync& completer) {
  DisableForVote(direct_vote_, completer);
}

void FakePowerDomain::IsEnabled(IsEnabledCompleter::Sync& completer) {
  completer.ReplySuccess(votes_.enabled());
}

void FakePowerDomain::EnableForVote(const fdf_resource_vote::Vote& vote,
                                    EnableCompleter::Sync& completer) {
  votes_.Commit(votes_.PrepareEnable(vote));
  completer.ReplySuccess();
}

void FakePowerDomain::DisableForVote(const fdf_resource_vote::Vote& vote,
                                     DisableCompleter::Sync& completer) {
  votes_.Commit(votes_.PrepareDisable(vote));
  completer.ReplySuccess();
}

void FakePowerDomain::OnConnectionClosed(Connection* connection) {
  // Closing an orphan-on-close vote never changes the aggregate.
  votes_.Commit(votes_.PrepareClose(connection->vote()));
  connections_.erase(connection);
}

}  // namespace fdf_fake
