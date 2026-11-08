// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback_data/log_source.h"

#include <fuchsia/diagnostics/cpp/fidl.h>
#include <fuchsia/logger/cpp/fidl.h>
#include <lib/fit/defer.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/syslog/cpp/macros.h>

#include <memory>

#include "src/developer/forensics/feedback_data/constants.h"
#include "src/lib/diagnostics/fidl_log_message_parser/log_message.h"

namespace forensics::feedback_data {

LogSource::LogSource(async_dispatcher_t* dispatcher,
                     std::shared_ptr<sys::ServiceDirectory> services, LogSink* sink)
    : dispatcher_(dispatcher), services_(std::move(services)), sink_(sink) {
  FX_CHECK(dispatcher_ != nullptr);
  FX_CHECK(services_ != nullptr);
  FX_CHECK(sink_ != nullptr);

  archive_accessor_.set_error_handler([this](const zx_status_t status) {
    FX_LOGS(WARNING) << "Lost connection to " << kLogsArchiveAccessorName;

    // The batch iterator and archive accessor connections are not expected to close. Ensure both
    // are unbound at the same time.
    if (batch_iterator_) {
      batch_iterator_->Unbind();
    }
  });
}

void LogSource::Start() {
  services_->Connect(archive_accessor_.NewRequest(dispatcher_), kLogsArchiveAccessorName);

  auto format = fuchsia::diagnostics::Format::FXT;
  fuchsia::diagnostics::StreamParameters params;
  params.set_data_type(fuchsia::diagnostics::DataType::LOGS)
      .set_format(format)
      .set_stream_mode(fuchsia::diagnostics::StreamMode::SNAPSHOT_THEN_SUBSCRIBE)
      .set_client_selector_configuration(
          fuchsia::diagnostics::ClientSelectorConfiguration::WithSelectAll(true));

  fuchsia::diagnostics::BatchIteratorPtr batch_iterator;
  archive_accessor_->StreamDiagnostics(std::move(params), batch_iterator.NewRequest(dispatcher_));
  batch_iterator_ = std::make_unique<diagnostics::fidl_log_message_parser::LogBatchIterator>(
      std::move(batch_iterator), format);
  batch_iterator_->set_error_handler([this](const zx_status_t status) {
    FX_LOGS(WARNING) << "Lost connection to fuchsia.diagnostics.BatchIterator";

    // The batch iterator and archive accessor connections are not expected to close. Ensure both
    // are unbound at the same time.
    archive_accessor_.Unbind();
  });
  GetNext();
}

void LogSource::GetNext() {
  batch_iterator_->GetNext([this](auto result) {
    auto get_next = ::fit::defer([this] { GetNext(); });
    if (result.is_error()) {
      return;
    }

    for (fpromise::result<fuchsia::logger::LogMessage, std::string>& formatted : result.value()) {
      if (formatted.is_error()) {
        sink_->Add(::fpromise::error(std::move(formatted.error())));
        continue;
      }

      sink_->Add(std::move(formatted));
    }
  });
}

}  // namespace forensics::feedback_data
