// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.diagnostics/cpp/fidl.h>
#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include <algorithm>
#include <iterator>

#include <gtest/gtest.h>
#include <src/lib/diagnostics/fidl_log_message_parser/log_message.h>

const char* TAG = "integration_test";

void WriteLogs() {
  fuchsia_logging::LogSettingsBuilder builder;
  builder.WithTags({TAG}).BuildAndInitialize();
  // WARNING: Test is sensitive to line numbers and file name.
  // These log lines are added to the top of the file to prevent changes to the test below from
  // changing the expected outputs.
  FX_LOGS(INFO) << "Hello info";        // Line 25
  FX_LOGS(WARNING) << "Hello warning";  // Line 26
  FX_LOGS(ERROR) << "Hello error";      // Line 27
}

std::vector<fuchsia_logger::LogMessage> GetLogs() {
  // Synchronously get the logs from ArchiveAccessor.
  zx::result accessor_end = component::Connect<fuchsia_diagnostics::ArchiveAccessor>();
  EXPECT_TRUE(accessor_end.is_ok()) << accessor_end.status_string();
  fidl::SyncClient accessor(std::move(*accessor_end));

  auto [iterator_client, iterator_server] =
      fidl::Endpoints<fuchsia_diagnostics::BatchIterator>::Create();
  fidl::SyncClient iterator(std::move(iterator_client));

  fuchsia_diagnostics::StreamParameters params;
  params.stream_mode(fuchsia_diagnostics::StreamMode::kSnapshot)
      .data_type(fuchsia_diagnostics::DataType::kLogs)
      .format(fuchsia_diagnostics::Format::kFxt)
      .client_selector_configuration(
          fuchsia_diagnostics::ClientSelectorConfiguration::WithSelectAll(true));

  EXPECT_TRUE(accessor
                  ->StreamDiagnostics({{
                      .stream_parameters = std::move(params),
                      .result_stream = std::move(iterator_server),
                  }})
                  .is_ok());

  auto get_next_result = iterator->GetNext();
  if (!get_next_result.is_ok()) {
    ADD_FAILURE() << "GetNext failed: " << get_next_result.error_value();
    return {};
  }
  std::vector<fuchsia_logger::LogMessage> ret;
  for (auto& content : get_next_result->batch()) {
    auto result = diagnostics::fidl_log_message_parser::ConvertFormattedContentToLogMessages(
        std::move(content));
    if (result.is_error()) {
      EXPECT_TRUE(false) << "Received an error: " << result.error();
      return {};
    }

    for (auto& value : result.take_value()) {
      if (value.is_error()) {
        EXPECT_TRUE(false) << "Value had an error: " << value.error();
        return {};
      }
      ret.emplace_back(value.take_value());
    }
  }

  return ret;
}

const size_t EXPECTED_LOGS = 3;

TEST(fidl_log_message_parser, ConversionWorks) {
  WriteLogs();
  auto logs = GetLogs();
  while (logs.size() != EXPECTED_LOGS) {
    // Repeat with 1s wait if logs do not appear yet.
    // This can happen when the test Archivist has not yet finished reading the logs off of the
    // wire.
    sleep(1);
    printf("Retrying reading logs\n");
    logs = GetLogs();
  }

  ASSERT_EQ(logs.size(), EXPECTED_LOGS);

  EXPECT_GT(logs[0].time().get(), 0u);
  EXPECT_GT(logs[0].pid(), 0u);
  EXPECT_GT(logs[0].tid(), 0u);
  EXPECT_EQ(logs[0].msg(), "[test.cc(25)] Hello info");
  ASSERT_EQ(logs[0].tags().size(), 1u);
  EXPECT_EQ(logs[0].tags()[0], TAG);
  EXPECT_EQ(logs[0].severity(), static_cast<int8_t>(fuchsia_logger::LogLevelFilter::kInfo));

  EXPECT_GT(logs[1].time().get(), 0u);
  EXPECT_GT(logs[1].pid(), 0u);
  EXPECT_GT(logs[1].tid(), 0u);
  EXPECT_EQ(logs[1].msg(),
            "[src/lib/diagnostics/fidl_log_message_parser/integration/test.cc(26)] Hello warning");
  ASSERT_EQ(logs[1].tags().size(), 1u);
  EXPECT_EQ(logs[1].tags()[0], TAG);
  EXPECT_EQ(logs[1].severity(), static_cast<int8_t>(fuchsia_logger::LogLevelFilter::kWarn));

  EXPECT_GT(logs[2].time().get(), 0u);
  EXPECT_GT(logs[2].pid(), 0u);
  EXPECT_GT(logs[2].tid(), 0u);
  EXPECT_EQ(logs[2].msg(),
            "[src/lib/diagnostics/fidl_log_message_parser/integration/test.cc(27)] Hello error");
  ASSERT_EQ(logs[2].tags().size(), 1u);
  EXPECT_EQ(logs[2].tags()[0], TAG);
  EXPECT_EQ(logs[2].severity(), static_cast<int8_t>(fuchsia_logger::LogLevelFilter::kError));
}
