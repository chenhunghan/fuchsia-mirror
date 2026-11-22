// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/fake-vreg/cpp/fake-vreg.h>

#include <cstdio>

namespace fdf_fake {

// Serves one connection. Owns that connection's vote; everything else is forwarded to the
// `FakeVreg`.
class FakeVreg::Connection final : public fidl::WireServer<fuchsia_hardware_vreg::Vreg> {
 public:
  explicit Connection(FakeVreg& parent)
      : parent_(parent), vote_(kNodeKey, fdf_resource_vote::CloseBehavior::kOrphanOnClose) {}

  const fdf_resource_vote::Vote& vote() const { return vote_; }

  void SetVoltageStep(SetVoltageStepRequestView request,
                      SetVoltageStepCompleter::Sync& completer) override {
    parent_.SetVoltageStep(request, completer);
  }
  void GetVoltageStep(GetVoltageStepCompleter::Sync& completer) override {
    parent_.GetVoltageStep(completer);
  }
  void SetState(SetStateRequestView request, SetStateCompleter::Sync& completer) override {
    parent_.SetStateForVote(vote_, request, completer);
  }
  void Enable(EnableCompleter::Sync& completer) override {
    parent_.EnableForVote(vote_, completer);
  }
  void Disable(DisableCompleter::Sync& completer) override {
    parent_.DisableForVote(vote_, completer);
  }
  void GetRegulatorParams(GetRegulatorParamsCompleter::Sync& completer) override {
    parent_.GetRegulatorParams(completer);
  }

 private:
  FakeVreg& parent_;
  fdf_resource_vote::Vote vote_;
};

FakeVreg::FakeVreg(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {
  // Log to stderr rather than to the global driver logger, which may belong to (and be torn
  // down with) a driver under test.
  votes_.SetLogSinkForTesting([](fdf_resource_vote::LogSeverity, const std::string& message) {
    fprintf(stderr, "[fake-vreg] %s\n", message.c_str());
  });
}

FakeVreg::~FakeVreg() = default;

void FakeVreg::SetVoltageStep(SetVoltageStepRequestView request,
                              SetVoltageStepCompleter::Sync& completer) {
  if (set_voltage_step_result_.is_error()) {
    completer.ReplyError(set_voltage_step_result_.status_value());
    return;
  }
  if (request->step >= num_steps_) {
    // Optionally handle invalid step
  }
  voltage_step_ = request->step;
  completer.ReplySuccess();
}

void FakeVreg::GetVoltageStep(GetVoltageStepCompleter::Sync& completer) {
  if (get_voltage_step_result_.is_error()) {
    completer.ReplyError(get_voltage_step_result_.status_value());
    return;
  }
  completer.ReplySuccess(voltage_step_);
}

void FakeVreg::SetState(SetStateRequestView request, SetStateCompleter::Sync& completer) {
  SetStateForVote(direct_vote_, request, completer);
}

void FakeVreg::Enable(EnableCompleter::Sync& completer) { EnableForVote(direct_vote_, completer); }

void FakeVreg::Disable(DisableCompleter::Sync& completer) {
  DisableForVote(direct_vote_, completer);
}

void FakeVreg::SetStateForVote(const fdf_resource_vote::Vote& vote, SetStateRequestView request,
                               SetStateCompleter::Sync& completer) {
  if (set_state_result_.is_error()) {
    completer.ReplyError(set_state_result_.status_value());
    return;
  }
  if (request->has_step()) {
    voltage_step_ = request->step();
  }
  if (request->has_enable()) {
    votes_.Commit(request->enable() ? votes_.PrepareSet(vote, true) : votes_.PrepareClear(vote));
    last_enable_request_ = request->enable();
  }
  if (request->has_mode()) {
    last_mode_request_ = request->mode();
  }
  completer.ReplySuccess();
}

void FakeVreg::EnableForVote(const fdf_resource_vote::Vote& vote,
                             EnableCompleter::Sync& completer) {
  if (enable_result_.is_error()) {
    completer.ReplyError(enable_result_.status_value());
    return;
  }
  votes_.Commit(votes_.PrepareSet(vote, true));
  last_enable_request_ = true;
  completer.ReplySuccess();
}

void FakeVreg::DisableForVote(const fdf_resource_vote::Vote& vote,
                              DisableCompleter::Sync& completer) {
  if (disable_result_.is_error()) {
    completer.ReplyError(disable_result_.status_value());
    return;
  }
  votes_.Commit(votes_.PrepareClear(vote));
  last_enable_request_ = false;
  completer.ReplySuccess();
}

void FakeVreg::OnConnectionClosed(Connection* connection) {
  // Closing an orphan-on-close vote never changes the aggregate.
  votes_.Commit(votes_.PrepareClose(connection->vote()));
  connections_.erase(connection);
}

void FakeVreg::GetRegulatorParams(GetRegulatorParamsCompleter::Sync& completer) {
  if (get_regulator_params_result_.is_error()) {
    completer.ReplyError(get_regulator_params_result_.status_value());
    return;
  }
  completer.ReplySuccess(min_uv_, step_size_uv_, num_steps_);
}

void FakeVreg::SetRegulatorParams(uint32_t min_uv, uint32_t step_size_uv, uint32_t num_steps) {
  min_uv_ = min_uv;
  step_size_uv_ = step_size_uv;
  num_steps_ = num_steps;
}

std::optional<bool> FakeVreg::take_enable_request() {
  std::optional<bool> result = last_enable_request_;
  last_enable_request_.reset();
  return result;
}

std::optional<fuchsia_hardware_vreg::RegulatorMode> FakeVreg::take_mode_request() {
  std::optional<fuchsia_hardware_vreg::RegulatorMode> result = last_mode_request_;
  last_mode_request_.reset();
  return result;
}

void FakeVreg::Bind(async_dispatcher_t* dispatcher,
                    fidl::ServerEnd<fuchsia_hardware_vreg::Vreg> server_end) {
  auto connection = std::make_unique<Connection>(*this);
  Connection* connection_ptr = connection.get();
  connections_.emplace(connection_ptr, std::move(connection));
  bindings_.AddBinding(
      dispatcher, std::move(server_end), connection_ptr,
      [this](Connection* closed, fidl::UnbindInfo) { OnConnectionClosed(closed); });
}

fuchsia_hardware_vreg::Service::InstanceHandler FakeVreg::CreateInstanceHandler() {
  return fuchsia_hardware_vreg::Service::InstanceHandler({
      .vreg =
          [this](fidl::ServerEnd<fuchsia_hardware_vreg::Vreg> server_end) {
            Bind(dispatcher_, std::move(server_end));
          },
  });
}

}  // namespace fdf_fake
