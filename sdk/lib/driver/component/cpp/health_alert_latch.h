// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_COMPONENT_CPP_HEALTH_ALERT_LATCH_H_
#define LIB_DRIVER_COMPONENT_CPP_HEALTH_ALERT_LATCH_H_

#include <fidl/fuchsia.driver.framework/cpp/natural_types.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <zircon/compiler.h>

#include <cstddef>
#include <memory>
#include <mutex>
#include <string>
#include <string_view>
#include <vector>

namespace fdf {

// Thread-safe helper that maintains a driver's active self-fault condition set and emits
// edge-triggered `OnHealthAlert` (`kSelfFault`) events via `DriverBase2::ReportHealthAlert`.
//
// `HealthAlertLatch` is thread-safe, allowing any execution context to record or clear faults
// without per-driver locks or manual synchronization.
//
// An alert carrying the full active condition set is emitted whenever that set changes in a way a
// consumer can observe: a `reason_code` is added or removed, or an active condition's status
// changes. This includes changes that leave the overall status unchanged (for example, a second
// `kDegraded` condition, or clearing one of several conditions), so `driver_manager` always holds
// the current set. Re-raising an active `reason_code` at the same status only updates its
// timestamps, `occurrence_count`, `message`, and `diagnostics` in memory, and emits nothing; those
// updates are returned by `Snapshot()` and `MergeInto()`.
//
// Lifetime requirements:
// - `HealthAlertLatch` must not outlive the `DriverBase2` instance passed to its constructor (it
//   is typically held as a member variable of the driver).
// - Construction and destruction must occur on the driver's default synchronized dispatcher.
//   Pending alerts are safely dropped once the driver stops or the latch is destroyed.
class HealthAlertLatch final {
 public:
  // Matches the FIDL `vector<HealthCondition>:16` bound in `HealthCheckResponse`.
  static constexpr size_t kMaxConditions = 16;

  explicit HealthAlertLatch(DriverBase2& driver);

  ~HealthAlertLatch();

  HealthAlertLatch(const HealthAlertLatch&) = delete;
  HealthAlertLatch& operator=(const HealthAlertLatch&) = delete;
  HealthAlertLatch(HealthAlertLatch&&) = delete;
  HealthAlertLatch& operator=(HealthAlertLatch&&) = delete;

  // Inserts or updates an active fault condition keyed by `condition.reason_code()`.
  //
  // - On the first raise of an active `reason_code`, `first_observed` and `last_observed` default
  //   to the current boot instant if unset, and `occurrence_count` defaults to 1 if unset or 0.
  // - On subsequent raises of an already-active `reason_code`, `first_observed` is preserved from
  //   the initial raise, `last_observed` updates to the current boot instant, and
  //   `occurrence_count` increments (or takes the supplied value if greater).
  // - `occurrence_count` counts occurrences within the current episode (since the `reason_code`
  //   was last raised from a cleared state). Leave it unset to let the latch count; do not pass a
  //   lifetime counter, which would hide the episode count. Report lifetime counters in
  //   `diagnostics` instead.
  // - Absent/empty `reason_code` maps to `"UNKNOWN"`.
  // - Unset statuses map to `kUnhealthy`. `kOk` clears the condition.
  // - Evicts an existing `kDegraded` condition to make room if `kMaxConditions` (16) is reached
  //   and an incoming `kUnhealthy` requires insertion. Logs a warning naming the evicted or
  //   dropped `reason_code`.
  void Raise(fuchsia_driver_framework::HealthCondition condition);

  // Raises a `kDegraded` self-fault condition with the given `reason_code` and optional `message`.
  //
  // Convenience wrapper around `Raise(HealthCondition)` that populates `first_observed`,
  // `last_observed`, and `occurrence_count` automatically.
  void RaiseDegraded(std::string_view reason_code, std::string_view message = {});

  // Raises a `kUnhealthy` self-fault condition with the given `reason_code` and optional `message`.
  //
  // Convenience wrapper around `Raise(HealthCondition)` that populates `first_observed`,
  // `last_observed`, and `occurrence_count` automatically.
  void RaiseUnhealthy(std::string_view reason_code, std::string_view message = {});

  // Removes the condition with `reason_code` if active.
  //
  // If a condition was removed, emits a `kSelfFault` alert with the remaining active conditions
  // (or an empty `conditions` list to report `HealthStatus::kOk` recovery when none remain).
  void Clear(std::string_view reason_code);

  // Clears all active conditions. If at least one condition was active, emits a single recovery
  // `kSelfFault` alert with an empty `conditions` list (`HealthStatus::kOk`).
  void ClearAll();

  // Returns a snapshot of all currently active conditions, suitable for returning from
  // `DriverBase2::HealthCheck` (`kPing`).
  std::vector<fuchsia_driver_framework::HealthCondition> Snapshot() const;

  // Builds a `HealthCheckResponse` containing all currently active conditions (or omitted
  // `conditions` when none are active, reporting `HealthStatus::kOk`), suitable for returning
  // directly from `DriverBase2::HealthCheck` on `HealthCheckRequest::kPing`.
  fuchsia_driver_framework::HealthCheckResponse ToResponse() const;

  // Appends any active conditions whose `reason_code` is not already present in
  // `response.conditions()` into `response` (up to `kMaxConditions`). Once `response` is full, an
  // active `kUnhealthy` condition replaces a `kDegraded` entry (preferring one this call merged
  // over one the caller measured live) rather than being dropped, so merging never downgrades the
  // overall status the response reports.
  void MergeInto(fuchsia_driver_framework::HealthCheckResponse& response) const;

  // Returns the overall status derived from the active condition set
  // (`kUnhealthy > kDegraded > kOk`).
  fuchsia_driver_framework::HealthStatus status() const;

 private:
  struct State {
    explicit State(DriverBase2* d) : driver(d), dispatcher(d->dispatcher()) {}

    std::vector<fuchsia_driver_framework::HealthCondition>::iterator FindCondition(
        std::string_view code) __TA_REQUIRES(mutex);

    // Must be called with `mutex` held so tasks are posted onto the driver dispatcher in the exact
    // linearizable order of state transitions (`async::PostTask` queues onto the dispatcher FIFO).
    void PostSelfFaultLocked(const std::shared_ptr<State>& self) const __TA_REQUIRES(mutex);

    // Posts a warning log onto the driver dispatcher. Must be called with `mutex` held.
    void PostWarningLocked(const std::shared_ptr<State>& self, std::string message) const
        __TA_REQUIRES(mutex);

    std::mutex mutex;
    DriverBase2* driver __TA_GUARDED(mutex);
    async_dispatcher_t* const dispatcher;
    std::vector<fuchsia_driver_framework::HealthCondition> conditions __TA_GUARDED(mutex);
  };

  std::shared_ptr<State> state_;
};

}  // namespace fdf

#endif  // LIB_DRIVER_COMPONENT_CPP_HEALTH_ALERT_LATCH_H_
