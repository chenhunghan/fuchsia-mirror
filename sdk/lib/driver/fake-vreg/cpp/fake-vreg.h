// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_FAKE_VREG_CPP_FAKE_VREG_H_
#define LIB_DRIVER_FAKE_VREG_CPP_FAKE_VREG_H_

#include <fidl/fuchsia.hardware.vreg/cpp/fidl.h>
#include <fidl/fuchsia.hardware.vreg/cpp/wire_test_base.h>
#include <lib/driver/resource-vote/cpp/vote-set.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/zx/result.h>
#include <zircon/errors.h>

#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <unordered_map>

namespace fdf_fake {

// A fake `fuchsia.hardware.vreg/Vreg` server.
//
// Enable requests are tracked as per-connection votes, like a real regulator provider (see
// //sdk/lib/driver/resource-vote): every connection made through `Bind()` or
// `CreateInstanceHandler()` owns its own idempotent vote, which `Enable()` and
// `SetState({enable: true})` set and `Disable()` and `SetState({enable: false})` clear, and the
// regulator is enabled while any vote is held. When a connection closes while holding a vote, the
// vote is kept as an orphan for the consumer's node (NodeKey "default") and is reclaimed by the
// next enable request on a new connection. Voltage and mode requests are not per-connection: the
// last request wins.
//
// Connections bound directly to this object (e.g. `fidl::BindServer(dispatcher, server, &fake)`)
// share a single vote that is never closed.
class FakeVreg final : public fidl::testing::WireTestBase<fuchsia_hardware_vreg::Vreg> {
 public:
  // The NodeKey of every vote cast through this fake.
  static constexpr char kNodeKey[] = "default";

  explicit FakeVreg(
      async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher());
  ~FakeVreg() override;

  void SetVoltageStep(SetVoltageStepRequestView request,
                      SetVoltageStepCompleter::Sync& completer) override;
  void GetVoltageStep(GetVoltageStepCompleter::Sync& completer) override;
  void SetState(SetStateRequestView request, SetStateCompleter::Sync& completer) override;
  void Enable(EnableCompleter::Sync& completer) override;
  void Disable(DisableCompleter::Sync& completer) override;
  void GetRegulatorParams(GetRegulatorParamsCompleter::Sync& completer) override;

  void NotImplemented_(const std::string& name, ::fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void SetRegulatorParams(uint32_t min_uv, uint32_t step_size_uv, uint32_t num_steps);

  void set_set_voltage_step_result(zx::result<> result) { set_voltage_step_result_ = result; }
  void set_get_voltage_step_result(zx::result<uint32_t> result) {
    get_voltage_step_result_ = result;
  }
  void set_set_state_result(zx::result<> result) { set_state_result_ = result; }
  void set_enable_result(zx::result<> result) { enable_result_ = result; }
  void set_disable_result(zx::result<> result) { disable_result_ = result; }
  void set_get_regulator_params_result(zx::result<> result) {
    get_regulator_params_result_ = result;
  }

  // Whether the regulator is enabled, i.e. whether any vote (live or orphaned) is held.
  bool enabled() const { return votes_.enabled(); }
  uint32_t voltage_step() const { return voltage_step_; }
  uint32_t voltage_uv() const {
    uint32_t result = 0;
    ZX_ASSERT(!__builtin_mul_overflow(voltage_step_, step_size_uv_, &result));
    ZX_ASSERT(!__builtin_add_overflow(min_uv_, result, &result));
    return result;
  }

  // The votes cast on this regulator, e.g. to check for orphaned votes.
  const fdf_resource_vote::BoolVoteSet& votes() const { return votes_; }

  // Returns the last successful enable request (from `Enable()`, `Disable()` or `SetState()`), or
  // `std::nullopt` if there was none since the last call to this method.
  std::optional<bool> take_enable_request();
  std::optional<fuchsia_hardware_vreg::RegulatorMode> take_mode_request();

  void Bind(async_dispatcher_t* dispatcher,
            fidl::ServerEnd<fuchsia_hardware_vreg::Vreg> server_end);

  fuchsia_hardware_vreg::Service::InstanceHandler CreateInstanceHandler();

 private:
  class Connection;

  void SetStateForVote(const fdf_resource_vote::Vote& vote, SetStateRequestView request,
                       SetStateCompleter::Sync& completer);
  void EnableForVote(const fdf_resource_vote::Vote& vote, EnableCompleter::Sync& completer);
  void DisableForVote(const fdf_resource_vote::Vote& vote, DisableCompleter::Sync& completer);
  void OnConnectionClosed(Connection* connection);

  // Declared before `connections_` and `bindings_` so that it outlives them.
  fdf_resource_vote::BoolVoteSet votes_{"fake-vreg"};
  // The vote shared by connections bound directly to this object.
  fdf_resource_vote::Vote direct_vote_{kNodeKey, fdf_resource_vote::CloseBehavior::kOrphanOnClose};

  uint32_t min_uv_ = 0;
  uint32_t step_size_uv_ = 0;
  uint32_t num_steps_ = 0;
  uint32_t voltage_step_ = 0;

  std::optional<bool> last_enable_request_;
  std::optional<fuchsia_hardware_vreg::RegulatorMode> last_mode_request_;

  zx::result<> set_voltage_step_result_ = zx::ok();
  zx::result<uint32_t> get_voltage_step_result_ = zx::ok(0);
  zx::result<> set_state_result_ = zx::ok();
  zx::result<> enable_result_ = zx::ok();
  zx::result<> disable_result_ = zx::ok();
  zx::result<> get_regulator_params_result_ = zx::ok();

  std::unordered_map<Connection*, std::unique_ptr<Connection>> connections_;
  // Destroyed first, so that no close handler runs during destruction.
  fidl::ServerBindingGroup<fuchsia_hardware_vreg::Vreg> bindings_;
  async_dispatcher_t* dispatcher_;
};

}  // namespace fdf_fake

#endif  // LIB_DRIVER_FAKE_VREG_CPP_FAKE_VREG_H_
