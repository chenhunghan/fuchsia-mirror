// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback_data/system_log_recorder/log_collector.h"

#include <lib/async/cpp/task.h>
#include <lib/syslog/cpp/macros.h>

#include <utility>

#include "src/developer/forensics/feedback_data/constants.h"
#include "src/developer/forensics/utils/vmo.h"

namespace forensics::feedback_data::system_log_recorder {

LogCollector::LogCollector(async_dispatcher_t* dispatcher,
                           std::shared_ptr<sys::ServiceDirectory> services, StorageSize buffer_size,
                           RedactorBase* redactor)
    : dispatcher_(dispatcher),
      services_(std::move(services)),
      buffer_(buffer_size, redactor),
      started_(false) {}

void LogCollector::on_fidl_error(const fidl::UnbindInfo error) {
  FX_LOGS(WARNING) << "Lost connection to diagnostics: " << error;
  OnError(Error::kStreamError);
}

void LogCollector::handle_unknown_event(
    const fidl::UnknownEventMetadata<fuchsia_diagnostics::ArchiveAccessor> metadata) {
  FX_LOGS(WARNING) << "Unexpected event ordinal: " << metadata.event_ordinal;
}

void LogCollector::handle_unknown_event(
    const fidl::UnknownEventMetadata<fuchsia_diagnostics::BatchIterator> metadata) {
  FX_LOGS(WARNING) << "Unexpected event ordinal: " << metadata.event_ordinal;
}

void LogCollector::Start(Callback callback) {
  FX_CHECK(!started_) << "Start can only be called once";
  started_ = true;
  callback_ = std::move(callback);

  auto [archive_client, archive_server] =
      fidl::Endpoints<fuchsia_diagnostics::ArchiveAccessor>::Create();
  services_->Connect(kLogsArchiveAccessorName, archive_server.TakeChannel());
  archive_accessor_.Bind(std::move(archive_client), dispatcher_, this);

  constexpr auto kFormat = fuchsia_diagnostics::Format::kFxt;
  fuchsia_diagnostics::StreamParameters params;
  params.data_type(fuchsia_diagnostics::DataType::kLogs)
      .format(kFormat)
      .stream_mode(fuchsia_diagnostics::StreamMode::kSnapshot)
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
    OnError(Error::kStreamError);
    return;
  }

  iterator_ = std::make_unique<diagnostics::fidl_log_message_parser::LogBatchIterator>(
      std::move(iterator_client), kFormat, dispatcher_, this);

  GetNext();
}

void LogCollector::OnError(LogCollector::Error error) { Complete(fit::error(error)); }

void LogCollector::Complete(fit::result<LogCollector::Error, Logs> result) {
  if (!callback_) {
    return;
  }

  // Defer invoking the callback to the next dispatcher loop to ensure that any caller destroying
  // this LogCollector from within the callback will not trigger a use-after-free while unwinding
  // the current call stack.
  async::PostTask(dispatcher_, [cb = std::move(callback_), result = std::move(result)]() mutable {
    cb(std::move(result));
  });
}

void LogCollector::GetNext() {
  if (!callback_) {
    return;
  }

  iterator_->GetNext([this](auto result) {
    if (!callback_) {
      return;
    }

    if (result.is_error()) {
      FX_LOGS(ERROR) << "BatchIterator GetNext failed: " << result.error();
      OnError(Error::kStreamError);
      return;
    }

    if (result.value().empty()) {
      zx::result<zx::vmo> vmo = VmoFromString(buffer_.ToString());
      if (vmo.is_error()) {
        Complete(fit::error(Error::kVmoError));
        return;
      }

      Logs logs{
          .vmo = std::move(*vmo),
          .first_timestamp = buffer_.FirstTimestamp(),
          .last_timestamp = buffer_.LastTimestamp(),
      };
      Complete(fit::ok(std::move(logs)));
      return;
    }

    for (fpromise::result<fuchsia_logger::LogMessage, std::string>& msg : result.value()) {
      buffer_.Add(std::move(msg));
    }

    GetNext();
  });
}

}  // namespace forensics::feedback_data::system_log_recorder
