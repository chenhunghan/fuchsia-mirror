// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback_data/log_source.h"

#include <fidl/fuchsia.diagnostics/cpp/fidl.h>
#include <fidl/fuchsia.logger/cpp/fidl.h>
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
}

void LogSource::on_fidl_error(const fidl::UnbindInfo error) {
  FX_LOGS(WARNING) << "Lost connection to diagnostics: " << error;

  // The batch iterator and archive accessor connections are not expected to close. Ensure both
  // are unbound at the same time.
  archive_accessor_ = {};
  if (batch_iterator_) {
    batch_iterator_->Unbind();
  }
}

void LogSource::handle_unknown_event(
    const fidl::UnknownEventMetadata<fuchsia_diagnostics::ArchiveAccessor> metadata) {
  FX_LOGS(WARNING) << "Unexpected event ordinal: " << metadata.event_ordinal;
}

void LogSource::handle_unknown_event(
    const fidl::UnknownEventMetadata<fuchsia_diagnostics::BatchIterator> metadata) {
  FX_LOGS(WARNING) << "Unexpected event ordinal: " << metadata.event_ordinal;
}

void LogSource::Start() {
  auto [archive_client, archive_server] =
      fidl::Endpoints<fuchsia_diagnostics::ArchiveAccessor>::Create();
  services_->Connect(kLogsArchiveAccessorName, archive_server.TakeChannel());
  archive_accessor_.Bind(std::move(archive_client), dispatcher_, this);

  constexpr auto kFormat = fuchsia_diagnostics::Format::kFxt;
  fuchsia_diagnostics::StreamParameters params;
  params.data_type(fuchsia_diagnostics::DataType::kLogs)
      .format(kFormat)
      .stream_mode(fuchsia_diagnostics::StreamMode::kSnapshotThenSubscribe)
      .client_selector_configuration(
          fuchsia_diagnostics::ClientSelectorConfiguration::WithSelectAll(true));

  auto [iterator_client, iterator_server] =
      fidl::Endpoints<fuchsia_diagnostics::BatchIterator>::Create();
  if (const fit::result result = archive_accessor_->StreamDiagnostics({{
          .stream_parameters = std::move(params),
          .result_stream = std::move(iterator_server),
      }});
      result.is_error()) {
    FX_LOGS(WARNING) << "Failed to call StreamDiagnostics: " << result.error_value();
    archive_accessor_ = {};
    return;
  }

  batch_iterator_ = std::make_unique<diagnostics::fidl_log_message_parser::LogBatchIterator>(
      std::move(iterator_client), kFormat, dispatcher_, this);
  GetNext();
}

void LogSource::GetNext() {
  batch_iterator_->GetNext([this](auto result) {
    auto get_next = ::fit::defer([this] { GetNext(); });
    if (result.is_error()) {
      return;
    }

    for (fpromise::result<fuchsia_logger::LogMessage, std::string>& formatted : result.value()) {
      if (formatted.is_error()) {
        sink_->Add(::fpromise::error(std::move(formatted.error())));
        continue;
      }

      sink_->Add(std::move(formatted));
    }
  });
}

}  // namespace forensics::feedback_data
