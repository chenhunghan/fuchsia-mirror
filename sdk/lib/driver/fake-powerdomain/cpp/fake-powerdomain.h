// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_FAKE_POWERDOMAIN_CPP_FAKE_POWERDOMAIN_H_
#define LIB_DRIVER_FAKE_POWERDOMAIN_CPP_FAKE_POWERDOMAIN_H_

#include <fidl/fuchsia.hardware.powerdomain/cpp/wire_test_base.h>
#include <lib/driver/resource-vote/cpp/vote-set.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <zircon/errors.h>

#include <memory>
#include <unordered_map>

namespace fdf_fake {

// A fake `fuchsia.hardware.powerdomain/Domain` server.
//
// Enable and disable requests are tracked as per-connection votes, like a real power domain
// provider (see //sdk/lib/driver/resource-vote): every connection made through
// `CreateInstanceHandler()` owns its own counted vote (each `Enable()` increments it, each
// `Disable()` decrements it, saturating at zero), and the domain is enabled while any vote is
// held. When a connection closes while holding a vote, the vote is kept as an orphan for the
// consumer's node (NodeKey "default") and is reclaimed by the next `Enable()` on a new connection.
//
// Connections bound directly to this object (e.g. `fidl::BindServer(dispatcher, server, &fake)`)
// share a single vote that is never closed.
class FakePowerDomain : public fidl::testing::WireTestBase<fuchsia_hardware_powerdomain::Domain> {
 public:
  // The NodeKey of every vote cast through this fake.
  static constexpr char kNodeKey[] = "default";

  explicit FakePowerDomain(
      async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher());
  ~FakePowerDomain() override;

  fuchsia_hardware_powerdomain::Service::InstanceHandler CreateInstanceHandler();

  // Whether the domain is enabled, i.e. whether any vote (live or orphaned) is held.
  bool is_enabled() const { return votes_.enabled(); }

  // The votes cast on this domain, e.g. to check for orphaned votes.
  const fdf_resource_vote::BoolVoteSet& votes() const { return votes_; }

 private:
  class Connection;

  void Enable(EnableCompleter::Sync& completer) override;
  void Disable(DisableCompleter::Sync& completer) override;
  void IsEnabled(IsEnabledCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_powerdomain::Domain> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void EnableForVote(const fdf_resource_vote::Vote& vote, EnableCompleter::Sync& completer);
  void DisableForVote(const fdf_resource_vote::Vote& vote, DisableCompleter::Sync& completer);
  void OnConnectionClosed(Connection* connection);

  async_dispatcher_t* const dispatcher_;

  // Declared before `connections_` and `bindings_` so that it outlives them.
  fdf_resource_vote::BoolVoteSet votes_{"fake-powerdomain"};
  // The vote shared by connections bound directly to this object.
  fdf_resource_vote::Vote direct_vote_{kNodeKey, fdf_resource_vote::CloseBehavior::kOrphanOnClose};

  std::unordered_map<Connection*, std::unique_ptr<Connection>> connections_;
  // Destroyed first, so that no close handler runs during destruction.
  fidl::ServerBindingGroup<fuchsia_hardware_powerdomain::Domain> bindings_;
};

}  // namespace fdf_fake

#endif  // LIB_DRIVER_FAKE_POWERDOMAIN_CPP_FAKE_POWERDOMAIN_H_
