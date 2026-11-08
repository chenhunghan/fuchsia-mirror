// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_TESTING_CPP_HEALTH_TEST_UTILS_H_
#define LIB_DRIVER_TESTING_CPP_HEALTH_TEST_UTILS_H_

#include <fidl/fuchsia.driver.framework/cpp/natural_types.h>
#include <lib/stdcompat/span.h>
#include <lib/zx/time.h>

#include <cstdint>
#include <optional>
#include <string_view>

namespace fdf_testing {

// Derives the overall `HealthStatus` (`kUnhealthy > kDegraded > kOk`) across `conditions`.
// Per `fuchsia.driver.framework/HealthCondition`, an omitted condition status is treated as
// `kUnhealthy`. Returns `kOk` when `conditions` is empty.
inline fuchsia_driver_framework::HealthStatus OverallHealthStatus(
    cpp20::span<const fuchsia_driver_framework::HealthCondition> conditions) {
  auto overall = fuchsia_driver_framework::HealthStatus::kOk;
  for (const auto& condition : conditions) {
    if (!condition.status().has_value()) {
      return fuchsia_driver_framework::HealthStatus::kUnhealthy;
    }
    switch (*condition.status()) {
      case fuchsia_driver_framework::HealthStatus::kOk:
        break;
      case fuchsia_driver_framework::HealthStatus::kDegraded:
        overall = fuchsia_driver_framework::HealthStatus::kDegraded;
        break;
      case fuchsia_driver_framework::HealthStatus::kUnhealthy:
        return fuchsia_driver_framework::HealthStatus::kUnhealthy;
    }
  }
  return overall;
}

// Derives the overall `HealthStatus` (`kUnhealthy > kDegraded > kOk`) of `response` from its
// `conditions` list, or `kOk` if `conditions` is absent or empty.
inline fuchsia_driver_framework::HealthStatus OverallHealthStatus(
    const fuchsia_driver_framework::HealthCheckResponse& response) {
  if (!response.conditions().has_value()) {
    return fuchsia_driver_framework::HealthStatus::kOk;
  }
  return OverallHealthStatus(*response.conditions());
}

// Returns a pointer to the first condition in `conditions` whose `reason_code` equals
// `reason_code`, or `nullptr` if none matches.
inline const fuchsia_driver_framework::HealthCondition* FindCondition(
    cpp20::span<const fuchsia_driver_framework::HealthCondition> conditions,
    std::string_view reason_code) {
  for (const auto& condition : conditions) {
    if (condition.reason_code().has_value() && *condition.reason_code() == reason_code) {
      return &condition;
    }
  }
  return nullptr;
}

// Returns a pointer to the first condition in `response.conditions()` whose `reason_code` equals
// `reason_code`, or `nullptr` if `conditions` is absent or no condition matches.
inline const fuchsia_driver_framework::HealthCondition* FindCondition(
    const fuchsia_driver_framework::HealthCheckResponse& response, std::string_view reason_code) {
  if (!response.conditions().has_value()) {
    return nullptr;
  }
  return FindCondition(*response.conditions(), reason_code);
}

// Returns a pointer to the `HealthDiagnosticValue` for the first item in `items` with `key`, or
// `nullptr` if not found.
inline const fuchsia_driver_framework::HealthDiagnosticValue* FindDiagnostic(
    cpp20::span<const fuchsia_driver_framework::HealthDiagnosticItem> items, std::string_view key) {
  for (const auto& item : items) {
    if (item.key() == key) {
      return &item.value();
    }
  }
  return nullptr;
}

inline const fuchsia_driver_framework::HealthDiagnosticValue* FindDiagnostic(
    const fuchsia_driver_framework::HealthCheckResponse& response, std::string_view key) {
  if (!response.diagnostics().has_value()) {
    return nullptr;
  }
  return FindDiagnostic(*response.diagnostics(), key);
}

inline const fuchsia_driver_framework::HealthDiagnosticValue* FindDiagnostic(
    const fuchsia_driver_framework::HealthCondition& condition, std::string_view key) {
  if (!condition.diagnostics().has_value()) {
    return nullptr;
  }
  return FindDiagnostic(*condition.diagnostics(), key);
}

inline std::optional<bool> GetDiagnosticBool(
    cpp20::span<const fuchsia_driver_framework::HealthDiagnosticItem> items, std::string_view key) {
  const auto* val = FindDiagnostic(items, key);
  if (val != nullptr && val->bool_value().has_value()) {
    return val->bool_value().value();
  }
  return std::nullopt;
}

inline std::optional<bool> GetDiagnosticBool(
    const fuchsia_driver_framework::HealthCheckResponse& response, std::string_view key) {
  if (!response.diagnostics().has_value()) {
    return std::nullopt;
  }
  return GetDiagnosticBool(*response.diagnostics(), key);
}

inline std::optional<bool> GetDiagnosticBool(
    const fuchsia_driver_framework::HealthCondition& condition, std::string_view key) {
  if (!condition.diagnostics().has_value()) {
    return std::nullopt;
  }
  return GetDiagnosticBool(*condition.diagnostics(), key);
}

inline std::optional<uint64_t> GetDiagnosticUint(
    cpp20::span<const fuchsia_driver_framework::HealthDiagnosticItem> items, std::string_view key) {
  const auto* val = FindDiagnostic(items, key);
  if (val != nullptr && val->uint_value().has_value()) {
    return val->uint_value().value();
  }
  return std::nullopt;
}

inline std::optional<uint64_t> GetDiagnosticUint(
    const fuchsia_driver_framework::HealthCheckResponse& response, std::string_view key) {
  if (!response.diagnostics().has_value()) {
    return std::nullopt;
  }
  return GetDiagnosticUint(*response.diagnostics(), key);
}

inline std::optional<uint64_t> GetDiagnosticUint(
    const fuchsia_driver_framework::HealthCondition& condition, std::string_view key) {
  if (!condition.diagnostics().has_value()) {
    return std::nullopt;
  }
  return GetDiagnosticUint(*condition.diagnostics(), key);
}

inline std::optional<int64_t> GetDiagnosticInt(
    cpp20::span<const fuchsia_driver_framework::HealthDiagnosticItem> items, std::string_view key) {
  const auto* val = FindDiagnostic(items, key);
  if (val != nullptr && val->int_value().has_value()) {
    return val->int_value().value();
  }
  return std::nullopt;
}

inline std::optional<int64_t> GetDiagnosticInt(
    const fuchsia_driver_framework::HealthCheckResponse& response, std::string_view key) {
  if (!response.diagnostics().has_value()) {
    return std::nullopt;
  }
  return GetDiagnosticInt(*response.diagnostics(), key);
}

inline std::optional<int64_t> GetDiagnosticInt(
    const fuchsia_driver_framework::HealthCondition& condition, std::string_view key) {
  if (!condition.diagnostics().has_value()) {
    return std::nullopt;
  }
  return GetDiagnosticInt(*condition.diagnostics(), key);
}

// The returned view borrows from `items`; it is only valid while the diagnostics it was
// read from are alive.
inline std::optional<std::string_view> GetDiagnosticString(
    cpp20::span<const fuchsia_driver_framework::HealthDiagnosticItem> items, std::string_view key) {
  const auto* val = FindDiagnostic(items, key);
  if (val != nullptr && val->string_value().has_value()) {
    return std::string_view(val->string_value().value());
  }
  return std::nullopt;
}

inline std::optional<std::string_view> GetDiagnosticString(
    const fuchsia_driver_framework::HealthCheckResponse& response, std::string_view key) {
  if (!response.diagnostics().has_value()) {
    return std::nullopt;
  }
  return GetDiagnosticString(*response.diagnostics(), key);
}

inline std::optional<std::string_view> GetDiagnosticString(
    const fuchsia_driver_framework::HealthCondition& condition, std::string_view key) {
  if (!condition.diagnostics().has_value()) {
    return std::nullopt;
  }
  return GetDiagnosticString(*condition.diagnostics(), key);
}

inline std::optional<zx::time_boot> GetDiagnosticTimestamp(
    cpp20::span<const fuchsia_driver_framework::HealthDiagnosticItem> items, std::string_view key) {
  const auto* val = FindDiagnostic(items, key);
  if (val != nullptr && val->timestamp().has_value()) {
    return val->timestamp().value();
  }
  return std::nullopt;
}

inline std::optional<zx::time_boot> GetDiagnosticTimestamp(
    const fuchsia_driver_framework::HealthCheckResponse& response, std::string_view key) {
  if (!response.diagnostics().has_value()) {
    return std::nullopt;
  }
  return GetDiagnosticTimestamp(*response.diagnostics(), key);
}

inline std::optional<zx::time_boot> GetDiagnosticTimestamp(
    const fuchsia_driver_framework::HealthCondition& condition, std::string_view key) {
  if (!condition.diagnostics().has_value()) {
    return std::nullopt;
  }
  return GetDiagnosticTimestamp(*condition.diagnostics(), key);
}

}  // namespace fdf_testing

#endif  // LIB_DRIVER_TESTING_CPP_HEALTH_TEST_UTILS_H_
