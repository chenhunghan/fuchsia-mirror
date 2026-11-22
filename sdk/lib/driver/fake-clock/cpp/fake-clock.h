// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_FAKE_CLOCK_CPP_FAKE_CLOCK_H_
#define LIB_DRIVER_FAKE_CLOCK_CPP_FAKE_CLOCK_H_

#include <fidl/fuchsia.hardware.clock/cpp/fidl.h>
#include <fidl/fuchsia.hardware.clock/cpp/wire_test_base.h>
#include <lib/driver/resource-vote/cpp/vote-set.h>

#include <memory>
#include <optional>
#include <unordered_map>

namespace fdf_fake {

// A fake `fuchsia.hardware.clock/Clock` server.
//
// Enable and disable requests are tracked as per-connection votes, like a real clock provider
// (see //sdk/lib/driver/resource-vote): every connection made through `Bind()`, `Connect()` or
// `CreateInstanceHandler()` owns its own counted vote (each `Enable()` increments it, each
// `Disable()` decrements it, saturating at zero), and the clock is enabled while any vote is
// held. When a connection closes while holding a vote, the vote is kept as an orphan for the
// consumer's node (NodeKey "default") and is reclaimed by the next `Enable()` on a new connection.
//
// Connections bound directly to this object (e.g. `fidl::BindServer(dispatcher, server, &fake)`)
// share a single vote that is never closed.
class FakeClock final : public fidl::testing::WireTestBase<fuchsia_hardware_clock::Clock> {
 public:
  // The NodeKey of every vote cast through this fake.
  static constexpr char kNodeKey[] = "default";

  explicit FakeClock(async_dispatcher_t* dispatcher = nullptr);
  ~FakeClock() override;

  // fuchsia.hardware.clock/Clock protocol implementation.
  void Enable(EnableCompleter::Sync& completer) override;
  void Disable(DisableCompleter::Sync& completer) override;
  void IsEnabled(IsEnabledCompleter::Sync& completer) override;
  void SetRate(SetRateRequestView request, SetRateCompleter::Sync& completer) override;
  void QuerySupportedRate(QuerySupportedRateRequestView request,
                          QuerySupportedRateCompleter::Sync& completer) override;
  void GetRate(GetRateCompleter::Sync& completer) override;
  void SetInput(SetInputRequestView request, SetInputCompleter::Sync& completer) override;
  void GetNumInputs(GetNumInputsCompleter::Sync& completer) override;
  void GetInput(GetInputCompleter::Sync& completer) override;
  void GetProperties(GetPropertiesCompleter::Sync& completer) override;

  void NotImplemented_(const std::string& name, ::fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  // Helper methods to inspect state and configure results.

  // Whether the clock is enabled, i.e. whether any vote (live or orphaned) is held.
  bool enabled() const { return votes_.enabled(); }
  uint64_t rate() const { return rate_.value_or(0); }
  uint32_t input_idx() const { return input_idx_.value_or(0); }

  // The votes cast on this clock, e.g. to check for orphaned votes.
  const fdf_resource_vote::BoolVoteSet& votes() const { return votes_; }

  // Returns whether the clock was enabled after the last successful `Enable()` or `Disable()`
  // call, or `std::nullopt` if there was none since the last call to this method.
  std::optional<bool> take_enabled();
  std::optional<uint64_t> take_rate();
  std::optional<uint32_t> take_input_idx();
  std::optional<uint64_t> take_max_rate();

  void set_enable_result(zx::result<> result) { enable_result_ = result; }
  void set_disable_result(zx::result<> result) { disable_result_ = result; }
  void set_set_rate_result(zx::result<> result) { set_rate_result_ = result; }
  void set_set_input_result(zx::result<> result) { set_input_result_ = result; }
  void set_get_rate_result(std::optional<zx::result<uint64_t>> result) {
    get_rate_result_ = result;
  }

  void set_supported_rates(std::vector<uint64_t> rates) { supported_rates_ = std::move(rates); }
  void set_id(uint32_t id) { id_ = id; }
  void set_name(std::string name) { name_ = std::move(name); }
  void set_max_rate(uint64_t max_hz) { max_rate_ = max_hz; }

  // Method to manually update rate without triggering FIDL calls/results (setup).
  void set_rate(uint64_t hz) { rate_ = hz; }
  void set_input_idx(uint32_t idx) { input_idx_ = idx; }

  void Bind(async_dispatcher_t* dispatcher,
            fidl::ServerEnd<fuchsia_hardware_clock::Clock> server_end);

  fidl::ClientEnd<fuchsia_hardware_clock::Clock> Connect(async_dispatcher_t* dispatcher = nullptr);

  fuchsia_hardware_clock::Service::InstanceHandler CreateInstanceHandler(
      async_dispatcher_t* dispatcher = nullptr);

 private:
  class Connection;

  void EnableForVote(const fdf_resource_vote::Vote& vote, EnableCompleter::Sync& completer);
  void DisableForVote(const fdf_resource_vote::Vote& vote, DisableCompleter::Sync& completer);
  void OnConnectionClosed(Connection* connection);

  // Declared before `connections_` and `bindings_` so that it outlives them.
  fdf_resource_vote::BoolVoteSet votes_{"fake-clock"};
  // The vote shared by connections bound directly to this object.
  fdf_resource_vote::Vote direct_vote_{kNodeKey, fdf_resource_vote::CloseBehavior::kOrphanOnClose};

  std::optional<bool> enabled_;
  std::optional<uint64_t> rate_;
  std::optional<uint32_t> input_idx_;

  std::vector<uint64_t> supported_rates_;
  uint32_t id_ = 0;
  std::string name_ = "fake-clock";
  std::optional<uint64_t> max_rate_ = std::nullopt;

  zx::result<> enable_result_ = zx::ok();
  zx::result<> disable_result_ = zx::ok();
  zx::result<> set_rate_result_ = zx::ok();
  zx::result<> set_input_result_ = zx::ok();
  std::optional<zx::result<uint64_t>> get_rate_result_;

  std::unordered_map<Connection*, std::unique_ptr<Connection>> connections_;
  // Destroyed first, so that no close handler runs during destruction.
  fidl::ServerBindingGroup<fuchsia_hardware_clock::Clock> bindings_;
  async_dispatcher_t* dispatcher_;
};

}  // namespace fdf_fake

#endif  // LIB_DRIVER_FAKE_CLOCK_CPP_FAKE_CLOCK_H_
