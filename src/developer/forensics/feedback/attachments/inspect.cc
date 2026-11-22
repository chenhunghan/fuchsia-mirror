// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/attachments/inspect.h"

#include <fidl/fuchsia.diagnostics/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
#include <lib/fpromise/result.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <memory>
#include <string>
#include <vector>

#include "src/developer/forensics/feedback_data/constants.h"
#include "src/developer/forensics/utils/errors.h"
#include "src/lib/fsl/vmo/strings.h"
#include "src/lib/fxl/strings/join_strings.h"

namespace forensics::feedback {

Inspect::Inspect(async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services,
                 std::unique_ptr<backoff::Backoff> backoff,
                 feedback_data::InspectDataBudget* data_budget, RedactorBase* redactor)
    : dispatcher_(dispatcher),
      services_(std::move(services)),
      backoff_(std::move(backoff)),
      data_budget_(data_budget),
      redactor_(redactor) {
  Connect();
}

void Inspect::Connect() {
  auto [client, server] = fidl::Endpoints<fuchsia_diagnostics::ArchiveAccessor>::Create();
  services_->Connect(feedback_data::kArchiveAccessorName, server.TakeChannel());
  archive_accessor_ = fidl::Client(std::move(client), dispatcher_, this);
}

void Inspect::on_fidl_error(const fidl::UnbindInfo error) {
  FX_LOGS(WARNING) << "Lost connection to " << feedback_data::kArchiveAccessorName << ": " << error;
  archive_accessor_ = fidl::Client<fuchsia_diagnostics::ArchiveAccessor>();

  fxl::WeakPtr<Inspect> self = ptr_factory_.GetWeakPtr();
  async::PostDelayedTask(
      dispatcher_,
      [self] {
        if (self) {
          self->Connect();
        }
      },
      backoff_->GetNext());
}

void Inspect::handle_unknown_event(
    const fidl::UnknownEventMetadata<fuchsia_diagnostics::ArchiveAccessor> metadata) {
  FX_LOGS(WARNING) << "Unexpected event ordinal: " << metadata.event_ordinal;
}

namespace {

// Manages collecting Inspect data and calling |complete| with done.
class InspectCollector : public fidl::AsyncEventHandler<fuchsia_diagnostics::BatchIterator> {
 public:
  InspectCollector(async_dispatcher_t* dispatcher,
                   fidl::ClientEnd<fuchsia_diagnostics::BatchIterator> iterator,
                   ::fit::callback<void(std::optional<Error>)> complete);

  void Run();

  const std::vector<std::string>& Inspect() const;

  void on_fidl_error(fidl::UnbindInfo error) override;

  void handle_unknown_event(
      fidl::UnknownEventMetadata<fuchsia_diagnostics::BatchIterator> metadata) override;

 private:
  ::fit::callback<void(std::optional<Error>)> complete_;
  fidl::Client<fuchsia_diagnostics::BatchIterator> batch_iterator_;
  std::vector<std::string> inspect_;
};

InspectCollector::InspectCollector(async_dispatcher_t* dispatcher,
                                   fidl::ClientEnd<fuchsia_diagnostics::BatchIterator> iterator,
                                   ::fit::callback<void(std::optional<Error>)> complete)
    : complete_(std::move(complete)), batch_iterator_(std::move(iterator), dispatcher, this) {}

void InspectCollector::on_fidl_error(const fidl::UnbindInfo error) {
  if (complete_ != nullptr) {
    FX_PLOGS(WARNING, error.status()) << "Lost connection to fuchsia.diagnostics.BatchIterator";
    complete_(Error::kConnectionError);
  }
}

void InspectCollector::handle_unknown_event(
    const fidl::UnknownEventMetadata<fuchsia_diagnostics::BatchIterator> metadata) {
  FX_LOGS(WARNING) << "Unexpected event ordinal: " << metadata.event_ordinal;
}

const std::vector<std::string>& InspectCollector::Inspect() const { return inspect_; }

void InspectCollector::Run() {
  if (complete_ == nullptr) {
    return;
  }

  FX_CHECK(batch_iterator_.is_valid())
      << "Attempting to collect Inspect without BatchIterator connection";

  batch_iterator_->GetNext().Then(
      [this](fidl::Result<fuchsia_diagnostics::BatchIterator::GetNext>& result) mutable {
        // Inspect collection has completed elsewhere, stop.
        if (complete_ == nullptr) {
          return;
        }

        if (result.is_error() && result.error_value().is_framework_error()) {
          return;
        }

        // Get the next batch of Inspect data when done.
        auto get_next = ::fit::defer([this] { Run(); });

        // Try again if the result was an error.
        if (result.is_error()) {
          FX_LOGS(WARNING) << "Failed to retrieve Inspect batch: " << result.error_value();
          return;
        }

        const std::vector<fuchsia_diagnostics::FormattedContent>& batch = result->batch();

        // All of the Inspect data has been collected.
        if (batch.empty()) {
          complete_(/*error=*/std::nullopt);
          get_next.cancel();
          return;
        }

        for (const fuchsia_diagnostics::FormattedContent& chunk : batch) {
          if (!chunk.json().has_value()) {
            FX_LOGS(WARNING) << "Invalid JSON Inspect chunk, skipping";
            continue;
          }

          if (std::string json; fsl::StringFromVmo(chunk.json().value(), &json)) {
            inspect_.push_back(std::move(json));
          } else {
            FX_LOGS(WARNING) << "Failed to convert Inspect chunk to string, skipping";
          }
        }
      });
}

}  // namespace

::fpromise::promise<AttachmentData> Inspect::Get(const uint64_t ticket) {
  FX_CHECK(!completers_.contains(ticket)) << "Ticket used twice: " << ticket;

  if (!archive_accessor_.is_valid()) {
    return ::fpromise::make_ok_promise(AttachmentData(Error::kConnectionError));
  }

  ::fpromise::bridge<void, Error> bridge;

  // Construct a promise and an object that can be used to complete the promise with a value at a
  // later point in time.
  auto consume = bridge.consumer.promise_or(::fpromise::error(Error::kLogicError));
  ::fit::callback<void(std::optional<Error>)> complete =
      [completer = std::move(bridge.completer)](std::optional<Error> error) mutable {
        if (error.has_value()) {
          completer.complete_error(*error);
        } else {
          completer.complete_ok();
        }
      };

  auto [iterator_client, iterator_server] =
      fidl::Endpoints<fuchsia_diagnostics::BatchIterator>::Create();
  auto collector =
      std::make_unique<InspectCollector>(dispatcher_, std::move(iterator_client), complete.share());

  completers_[ticket] = std::move(complete);

  fuchsia_diagnostics::StreamParameters params;
  params.data_type(fuchsia_diagnostics::DataType::kInspect)
      .format(fuchsia_diagnostics::Format::kJson)
      .stream_mode(fuchsia_diagnostics::StreamMode::kSnapshot)
      .client_selector_configuration(
          fuchsia_diagnostics::ClientSelectorConfiguration::WithSelectAll(true));

  if (const std::optional<size_t> budget = data_budget_->SizeInBytes(); budget.has_value()) {
    fuchsia_diagnostics::PerformanceConfiguration performance;
    performance.max_aggregate_content_size_bytes(*budget);
    params.performance_configuration(std::move(performance));
  }

  if (const fit::result result = archive_accessor_->StreamDiagnostics({{
          .stream_parameters = std::move(params),
          .result_stream = std::move(iterator_server),
      }});
      result.is_error()) {
    FX_LOGS(WARNING) << "Failed to call StreamDiagnostics: " << result.error_value();
  }

  fxl::WeakPtr<Inspect> self = ptr_factory_.GetWeakPtr();

  collector->Run();

  // Keep |collector| alive until Inspect collection has completed (for any reason).
  return consume.then([self, ticket, collector = std::move(collector)](
                          const ::fpromise::result<void, Error>& result) mutable
                          -> ::fpromise::result<AttachmentData> {
    const std::vector<std::string>& inspect = collector->Inspect();
    if (inspect.empty()) {
      FX_LOGS(WARNING) << "Inspect data was empty";
      const Error error = result.is_ok() ? Error::kMissingValue : result.error();
      return ::fpromise::ok(AttachmentData(error));
    }

    if (self) {
      self->completers_.erase(ticket);
    }

    std::string joined_data = "[\n";
    joined_data += fxl::JoinStrings(inspect, ",\n");
    joined_data += "\n]";
    joined_data = self->redactor_->RedactJson(joined_data);

    AttachmentData value = (result.is_ok())
                               ? AttachmentData(std::move(joined_data))
                               : AttachmentData(std::move(joined_data), result.error());

    return ::fpromise::ok(std::move(value));
  });
}

void Inspect::ForceCompletion(const uint64_t ticket, const Error error) {
  if (completers_.contains(ticket) && completers_[ticket] != nullptr) {
    completers_[ticket](error);
  }
}

}  // namespace forensics::feedback
