// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/bin/system_monitor/system_monitor.h"

#include <fidl/fuchsia.diagnostics/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/async/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/stdcompat/string_view.h>
#include <lib/syslog/cpp/macros.h>

#include <algorithm>
#include <iostream>
#include <iterator>
#include <string>

#include "rapidjson/schema.h"
#include "src/lib/json_parser/json_parser.h"

namespace system_monitor {

constexpr char kTarget[] = "platform_metrics";
constexpr zx::duration kPrintFrequency = zx::sec(10);
constexpr char kCpuData[] = "CPU MEAN ";

SystemMonitor::SystemMonitor() {
  params_ = fuchsia_diagnostics::StreamParameters{{
      .data_type = fuchsia_diagnostics::DataType::kInspect,
      .stream_mode = fuchsia_diagnostics::StreamMode::kSnapshot,
      .format = fuchsia_diagnostics::Format::kJson,
      .client_selector_configuration =
          fuchsia_diagnostics::ClientSelectorConfiguration::WithSelectAll(true),
  }};
}

void SystemMonitor::ConnectToArchiveAccessor() {
  zx::result client_end = component::Connect<fuchsia_diagnostics::ArchiveAccessor>();
  if (client_end.is_error()) {
    FX_PLOGS(ERROR, client_end.status_value()) << "Failed to connect to ArchiveAccessor";
    return;
  }
  accessor_ = fidl::SyncClient(std::move(*client_end));
}

void SystemMonitor::InitializeRenderer() { renderer_.Initialize(); }

void SystemMonitor::UpdateRecentDiagnostic() {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_diagnostics::BatchIterator>::Create();
  fit::result<fidl::OneWayError> stream_result = accessor_->StreamDiagnostics(
      {{.stream_parameters = params_, .result_stream = std::move(server_end)}});
  if (stream_result.is_error()) {
    FX_LOGS(WARNING) << "Failed to call StreamDiagnostics: " << stream_result.error_value();
  }
  iterator_ = fidl::SyncClient(std::move(client_end));
  fidl::Result<fuchsia_diagnostics::BatchIterator::GetNext> result = iterator_->GetNext();
  if (result.is_error()) {
    // Keep the previous batch, if any.
    FX_LOGS(WARNING) << "Failed to get the next batch of Inspect data: " << result.error_value();
    return;
  }
  batch_ = std::move(result->batch());
}

std::vector<std::string> SystemMonitor::ParseBatch(
    const std::vector<fuchsia_diagnostics::FormattedContent>& batch) {
  std::vector<std::string> recent_diagnostics;
  for (auto& content : batch) {
    if (content.Which() != fuchsia_diagnostics::FormattedContent::Tag::kJson) {
      FX_LOGS(WARNING) << "Invalid JSON Inspect content, skipping";
      continue;
    }
    const fuchsia_mem::Buffer& buffer = content.json().value();
    std::string json(buffer.size(), '\0');
    if (zx_status_t status = buffer.vmo().read(json.data(), 0, buffer.size()); status == ZX_OK) {
      recent_diagnostics.push_back(std::move(json));
    } else {
      FX_PLOGS(WARNING, status) << "Failed to convert Inspect content to string, skipping";
    }
  }
  return recent_diagnostics;
}

std::string SystemMonitor::GetTargetFromDiagnostics(std::vector<std::string> recent_diagnostics) {
  for (auto& content : recent_diagnostics) {
    if (cpp23::contains(content, kTarget)) {
      return content;
    }
  }
  return "";
}

std::string SystemMonitor::GetCPUData() {
  if (batch_.empty()) {
    return "";
  }
  return GetTargetFromDiagnostics(ParseBatch(batch_));
}

void SystemMonitor::PrintRecentDiagnostic() {
  UpdateRecentDiagnostic();
  const std::string& recent_diag = GetCPUData();
  FX_LOGS(INFO) << "Recent Diagnostics: " << recent_diag;
  json_parser::JSONParser stringParser;
  auto jsonDoc = stringParser.ParseFromString(recent_diag, "");
  // Extract the mean values
  const rapidjson::Value& meanValues =
      jsonDoc["payload"]["root"]["platform_metrics"]["cpu"]["mean"];

  // Print the mean values
  double sum = 0.0;
  for (rapidjson::SizeType i = 0; i < meanValues.Size(); ++i) {
    sum += meanValues[i].GetDouble();
  }
  FX_LOGS(INFO) << "CPU mean: " << sum / meanValues.Size();
  std::string cpu_data = kCpuData + std::to_string(sum / meanValues.Size());
  renderer_.RenderFrame(cpu_data);
  async::PostDelayedTask(
      async_get_default_dispatcher(), [&] { PrintRecentDiagnostic(); }, kPrintFrequency);
}
}  // namespace system_monitor
