// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async/cpp/task.h>
#include <lib/driver/component/cpp/health_alert_latch.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/zx/clock.h>

#include <algorithm>
#include <cstdint>
#include <limits>
#include <optional>
#include <utility>

namespace fdf {

namespace {

fuchsia_driver_framework::HealthStatus NormalizeFaultStatus(
    std::optional<fuchsia_driver_framework::HealthStatus> status) {
  // An omitted status counts as kUnhealthy (see HealthCondition in health.fidl).
  if (status == fuchsia_driver_framework::HealthStatus::kOk ||
      status == fuchsia_driver_framework::HealthStatus::kDegraded) {
    return *status;
  }
  return fuchsia_driver_framework::HealthStatus::kUnhealthy;
}

}  // namespace

std::vector<fuchsia_driver_framework::HealthCondition>::iterator
HealthAlertLatch::State::FindCondition(std::string_view code) {
  return std::find_if(conditions.begin(), conditions.end(),
                      [code](const fuchsia_driver_framework::HealthCondition& cond) {
                        return cond.reason_code().has_value() && *cond.reason_code() == code;
                      });
}

void HealthAlertLatch::State::PostSelfFaultLocked(const std::shared_ptr<State>& self) const {
  if (driver == nullptr || dispatcher == nullptr) {
    return;
  }

  fuchsia_driver_framework::HealthCheckResponse response;
  if (!conditions.empty()) {
    response.conditions(conditions);
  }

  async::PostTask(dispatcher, [self, response = std::move(response)]() mutable {
    DriverBase2* current_driver = nullptr;
    {
      std::lock_guard lock(self->mutex);
      current_driver = self->driver;
    }
    // 1) Unlocking before ReportHealthAlert avoids stalling IRQ/worker threads calling
    //    Raise()/Clear() across FIDL channel writes and Inspect VMO updates.
    // 2) `current_driver` cannot be destroyed after releasing `mutex` while this task runs because
    //    DriverBase2 (and its member HealthAlertLatch) must be destroyed on this same
    //    synchronized driver_dispatcher, which serializes all tasks.
    if (current_driver != nullptr) {
      current_driver->ReportHealthAlert(
          fuchsia_driver_framework::HealthAlert::WithSelfFault(std::move(response)));
    }
  });
}

void HealthAlertLatch::State::PostWarningLocked(const std::shared_ptr<State>& self,
                                                std::string message) const {
  if (driver == nullptr || dispatcher == nullptr) {
    return;
  }
  async::PostTask(dispatcher, [self, message = std::move(message)]() {
    DriverBase2* current_driver = nullptr;
    {
      std::lock_guard lock(self->mutex);
      current_driver = self->driver;
    }
    if (current_driver != nullptr && current_driver->has_logger()) {
      FDF_LOGL(WARNING, current_driver->logger(), "%s", message.c_str());
    }
  });
}

HealthAlertLatch::HealthAlertLatch(DriverBase2& driver)
    : state_(std::make_shared<State>(&driver)) {}

HealthAlertLatch::~HealthAlertLatch() {
  std::lock_guard lock(state_->mutex);
  state_->driver = nullptr;
}

void HealthAlertLatch::Raise(fuchsia_driver_framework::HealthCondition condition) {
  const zx::time_boot now = zx::clock::get_boot();
  const auto normalized_status = NormalizeFaultStatus(condition.status());

  std::string code = condition.reason_code().has_value() && !condition.reason_code()->empty()
                         ? *condition.reason_code()
                         : "UNKNOWN";

  if (normalized_status == fuchsia_driver_framework::HealthStatus::kOk) {
    Clear(code);
    return;
  }

  std::lock_guard lock(state_->mutex);

  auto it = state_->FindCondition(code);
  if (it != state_->conditions.end()) {
    auto& existing = *it;
    const auto prev_status = NormalizeFaultStatus(existing.status());

    const zx::time_boot first = existing.first_observed().value_or(now);
    const zx::time_boot last = condition.last_observed().value_or(now);

    const uint64_t supplied_count = condition.occurrence_count().value_or(0);
    const uint64_t existing_count = existing.occurrence_count().value_or(1);
    const uint64_t incremented_count =
        existing_count < std::numeric_limits<uint64_t>::max() ? existing_count + 1 : existing_count;
    const uint64_t next_count = std::max(incremented_count, supplied_count);

    existing.status(normalized_status);
    existing.first_observed(first);
    existing.last_observed(last);
    existing.occurrence_count(next_count);
    if (condition.message().has_value()) {
      existing.message(*std::move(condition.message()));
    }
    if (condition.diagnostics().has_value()) {
      existing.diagnostics(*std::move(condition.diagnostics()));
    }

    if (normalized_status != prev_status) {
      state_->PostSelfFaultLocked(state_);
    }
    return;
  }

  if (state_->conditions.size() >= kMaxConditions) {
    if (normalized_status == fuchsia_driver_framework::HealthStatus::kUnhealthy) {
      auto degraded_it =
          std::find_if(state_->conditions.begin(), state_->conditions.end(),
                       [](const fuchsia_driver_framework::HealthCondition& entry) {
                         return entry.status() == fuchsia_driver_framework::HealthStatus::kDegraded;
                       });
      if (degraded_it != state_->conditions.end()) {
        std::string evicted_code = degraded_it->reason_code().value_or("UNKNOWN");
        state_->conditions.erase(degraded_it);
        state_->PostWarningLocked(
            state_, "HealthAlertLatch: evicting DEGRADED condition '" + evicted_code +
                        "' to make room for UNHEALTHY condition '" + code + "' (kMaxConditions)");
      }
    }
    if (state_->conditions.size() >= kMaxConditions) {
      state_->PostWarningLocked(state_, "HealthAlertLatch: dropping condition '" + code +
                                            "': kMaxConditions active conditions already");
      return;
    }
  }

  condition.status(normalized_status);
  condition.reason_code(std::move(code));
  if (!condition.first_observed().has_value()) {
    condition.first_observed(now);
  }
  if (!condition.last_observed().has_value()) {
    condition.last_observed(now);
  }
  if (!condition.occurrence_count().has_value() || *condition.occurrence_count() == 0) {
    condition.occurrence_count(1);
  }

  state_->conditions.push_back(std::move(condition));
  state_->PostSelfFaultLocked(state_);
}

void HealthAlertLatch::RaiseDegraded(std::string_view reason_code, std::string_view message) {
  fuchsia_driver_framework::HealthCondition condition;
  condition.status(fuchsia_driver_framework::HealthStatus::kDegraded);
  condition.reason_code(std::string(reason_code));
  if (!message.empty()) {
    condition.message(std::string(message));
  }
  Raise(std::move(condition));
}

void HealthAlertLatch::RaiseUnhealthy(std::string_view reason_code, std::string_view message) {
  fuchsia_driver_framework::HealthCondition condition;
  condition.status(fuchsia_driver_framework::HealthStatus::kUnhealthy);
  condition.reason_code(std::string(reason_code));
  if (!message.empty()) {
    condition.message(std::string(message));
  }
  Raise(std::move(condition));
}

void HealthAlertLatch::Clear(std::string_view reason_code) {
  std::string_view normalized_code = reason_code.empty() ? "UNKNOWN" : reason_code;
  std::lock_guard lock(state_->mutex);
  auto it = state_->FindCondition(normalized_code);
  if (it == state_->conditions.end()) {
    return;
  }
  state_->conditions.erase(it);
  state_->PostSelfFaultLocked(state_);
}

void HealthAlertLatch::ClearAll() {
  std::lock_guard lock(state_->mutex);
  if (state_->conditions.empty()) {
    return;
  }
  state_->conditions.clear();
  state_->PostSelfFaultLocked(state_);
}

std::vector<fuchsia_driver_framework::HealthCondition> HealthAlertLatch::Snapshot() const {
  std::lock_guard lock(state_->mutex);
  return state_->conditions;
}

fuchsia_driver_framework::HealthCheckResponse HealthAlertLatch::ToResponse() const {
  std::lock_guard lock(state_->mutex);
  fuchsia_driver_framework::HealthCheckResponse response;
  if (!state_->conditions.empty()) {
    response.conditions(state_->conditions);
  }
  return response;
}

void HealthAlertLatch::MergeInto(fuchsia_driver_framework::HealthCheckResponse& response) const {
  std::lock_guard lock(state_->mutex);
  if (state_->conditions.empty()) {
    return;
  }
  if (!response.conditions().has_value()) {
    response.conditions().emplace();
  }
  auto& out = *response.conditions();
  const size_t merged_begin = out.size();
  auto is_degraded = [](const fuchsia_driver_framework::HealthCondition& entry) {
    return entry.status() == fuchsia_driver_framework::HealthStatus::kDegraded;
  };
  for (const auto& cond : state_->conditions) {
    if (!cond.reason_code().has_value()) {
      continue;
    }
    const std::string_view code = *cond.reason_code();
    const bool already_present = std::any_of(
        out.begin(), out.end(), [code](const fuchsia_driver_framework::HealthCondition& existing) {
          return existing.reason_code().has_value() && *existing.reason_code() == code;
        });
    if (already_present) {
      continue;
    }
    if (out.size() < kMaxConditions) {
      out.push_back(cond);
      continue;
    }
    if (NormalizeFaultStatus(cond.status()) != fuchsia_driver_framework::HealthStatus::kUnhealthy) {
      continue;
    }
    const auto merged_first = out.begin() + static_cast<std::ptrdiff_t>(merged_begin);
    auto degraded_it = std::find_if(merged_first, out.end(), is_degraded);
    if (degraded_it == out.end()) {
      degraded_it = std::find_if(out.begin(), merged_first, is_degraded);
      if (degraded_it == merged_first) {
        continue;
      }
    }
    *degraded_it = cond;
  }
}

fuchsia_driver_framework::HealthStatus HealthAlertLatch::status() const {
  std::lock_guard lock(state_->mutex);
  auto overall = fuchsia_driver_framework::HealthStatus::kOk;
  for (const auto& cond : state_->conditions) {
    const auto s = NormalizeFaultStatus(cond.status());
    if (s == fuchsia_driver_framework::HealthStatus::kUnhealthy) {
      return fuchsia_driver_framework::HealthStatus::kUnhealthy;
    }
    if (s == fuchsia_driver_framework::HealthStatus::kDegraded) {
      overall = fuchsia_driver_framework::HealthStatus::kDegraded;
    }
  }
  return overall;
}

}  // namespace fdf
