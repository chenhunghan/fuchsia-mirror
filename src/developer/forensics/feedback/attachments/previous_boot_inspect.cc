// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/attachments/previous_boot_inspect.h"

#include <lib/async/cpp/task.h>
#include <lib/fdio/fd.h>
#include <lib/fit/defer.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
#include <lib/syslog/cpp/macros.h>
#include <unistd.h>
#include <zlib.h>  // nogncheck: zlib.h belongs to the private :zlib_static dep of :zlib

#include <cstddef>
#include <limits>
#include <optional>
#include <string>
#include <string_view>
#include <utility>

#include "src/developer/forensics/feedback/attachments/types.h"
#include "src/developer/forensics/utils/errors.h"
#include "src/developer/forensics/utils/storage_size.h"
#include "src/lib/files/file.h"

namespace forensics::feedback {
namespace {

// Upper bound on the size of the decompressed previous boot Inspect data. This is well above the
// size of real snapshots and bounds the memory used by a corrupt or malicious file.
constexpr StorageSize kMaxInspectSize = StorageSize::Megabytes(64);

constexpr StorageSize kChunkSize = StorageSize::Kilobytes(64);

// Decompresses a single, complete RFC 1950 zlib stream.
//
// Returns std::nullopt if |compressed| is empty, is not a zlib stream (for example gzip data or
// uncompressed text), is truncated, is corrupt, requires a preset dictionary, has data after the
// end of the stream, or decompresses to more than |max_inflated_size|. The limit protects
// against small inputs that decompress to very large outputs.
std::optional<std::string> InflateZlib(const std::string_view compressed,
                                       const StorageSize max_inflated_size) {
  if (compressed.empty() || compressed.size() > std::numeric_limits<uInt>::max()) {
    return std::nullopt;
  }

  z_stream stream{};
  // inflateInit() only accepts the zlib (RFC 1950) format, unlike inflateInit2() which can be
  // configured to also accept gzip.
  if (inflateInit(&stream) != Z_OK) {
    return std::nullopt;
  }
  auto end_stream = fit::defer([&stream] { inflateEnd(&stream); });

  // zlib does not modify the input buffer.
  stream.next_in = reinterpret_cast<Bytef*>(const_cast<char*>(compressed.data()));
  stream.avail_in = static_cast<uInt>(compressed.size());

  const size_t max_inflated_bytes = max_inflated_size.ToBytes();
  const size_t chunk_bytes = kChunkSize.ToBytes();
  std::string inflated;
  int status = Z_OK;
  while (status != Z_STREAM_END) {
    // Leave room for one byte more than the limit so that exceeding it can be detected.
    const size_t offset = inflated.size();
    const size_t room = max_inflated_bytes - offset;
    const size_t chunk_size = room < chunk_bytes ? room + 1 : chunk_bytes;
    inflated.resize(offset + chunk_size);
    stream.next_out = reinterpret_cast<Bytef*>(inflated.data() + offset);
    stream.avail_out = static_cast<uInt>(chunk_size);

    // Returns Z_BUF_ERROR if no progress is possible, e.g., because the stream is truncated.
    status = inflate(&stream, Z_NO_FLUSH);
    inflated.resize(offset + chunk_size - stream.avail_out);
    if (status != Z_OK && status != Z_STREAM_END) {
      return std::nullopt;
    }
    if (inflated.size() > max_inflated_bytes) {
      return std::nullopt;
    }
  }

  // Data after the end of the stream.
  if (stream.avail_in != 0) {
    return std::nullopt;
  }

  return inflated;
}

}  // namespace

PreviousBootInspect::PreviousBootInspect(async_dispatcher_t* dispatcher,
                                         std::shared_ptr<sys::ServiceDirectory> services,
                                         std::unique_ptr<backoff::Backoff> backoff,
                                         RedactorBase* redactor, std::string path)
    : dispatcher_(dispatcher),
      services_(std::move(services)),
      backoff_(std::move(backoff)),
      redactor_(redactor),
      path_(std::move(path)) {
  // Don't fetch previous boot Inspect if it has already been written to path_.
  if (files::IsFile(path_)) {
    return;
  }

  WatchPreviousBootData();
}

void PreviousBootInspect::on_fidl_error(const fidl::UnbindInfo error) {
  // The server closes the channel with ZX_ERR_NOT_SUPPORTED if it can't satisfy the requested
  // options. Reconnecting would get the same result.
  if (error.status() == ZX_ERR_NOT_SUPPORTED) {
    FX_LOGS(WARNING)
        << "fuchsia.diagnostics.persistence.PreviousBootDataProvider does not support the requested options";
    OnError();
    return;
  }

  FX_PLOGS(WARNING, error.status())
      << "Lost connection to fuchsia.diagnostics.persistence.PreviousBootDataProvider";
  data_provider_ = fidl::Client<fuchsia_diagnostics_persistence::PreviousBootDataProvider>();

  async::PostDelayedTask(
      dispatcher_,
      [self = ptr_factory_.GetWeakPtr()] {
        if (self) {
          self->WatchPreviousBootData();
        }
      },
      backoff_->GetNext());
}

void PreviousBootInspect::handle_unknown_event(
    const fidl::UnknownEventMetadata<fuchsia_diagnostics_persistence::PreviousBootDataProvider>
        metadata) {
  FX_LOGS(WARNING) << "Unexpected event ordinal: " << metadata.event_ordinal;
}

void PreviousBootInspect::WatchPreviousBootData() {
  auto [client, server] =
      fidl::Endpoints<fuchsia_diagnostics_persistence::PreviousBootDataProvider>::Create();
  services_->Connect(
      fidl::DiscoverableProtocolName<fuchsia_diagnostics_persistence::PreviousBootDataProvider>,
      server.TakeChannel());
  data_provider_ = fidl::Client(std::move(client), dispatcher_, this);

  fuchsia_diagnostics_persistence::PreviousBootDataProviderOptions options;
  options.format(fuchsia_diagnostics_persistence::InspectFormat::kJson);
  options.compression(fuchsia_diagnostics_persistence::Compression::kZlibRfc1950);

  data_provider_->WatchPreviousBootData({{.options = std::move(options)}})
      .Then(
          [this](fidl::Result<
                 fuchsia_diagnostics_persistence::PreviousBootDataProvider::WatchPreviousBootData>&
                     result) {
            // Errors other than kUnknownMethod unbind |data_provider_|, and on_fidl_error()
            // handles reconnecting.
            if (result.is_ok()) {
              OnDataReceived(std::move(result->data()));
            } else if (result.error_value().reason() == fidl::Reason::kUnknownMethod) {
              OnError();
            }
          });
}

void PreviousBootInspect::OnError() {
  FX_LOGS(WARNING) << "Failed to watch previous boot inspect data";
  error_ = Error::kMissingValue;
  data_provider_ = fidl::Client<fuchsia_diagnostics_persistence::PreviousBootDataProvider>();
  auto completers = std::move(completers_);
  for (auto& [ticket, completer] : completers) {
    if (completer != nullptr) {
      completer(AttachmentData(Error::kMissingValue));
    }
  }
}

void PreviousBootInspect::OnDataReceived(fuchsia_diagnostics_persistence::PreviousBootData data) {
  if (!data.inspect().has_value() || !data.inspect()->is_valid()) {
    FX_LOGS(WARNING) << "Previous boot inspect data is missing or invalid";
    error_ = Error::kMissingValue;
  } else if ((data.format().has_value() &&
              data.format() != fuchsia_diagnostics_persistence::InspectFormat::kJson) ||
             (data.compression().has_value() &&
              data.compression() != fuchsia_diagnostics_persistence::Compression::kZlibRfc1950)) {
    FX_LOGS(WARNING) << "Previous boot inspect data has an unexpected format or compression";
    error_ = Error::kInvalidFormat;
  } else {
    zx::channel channel = data.inspect()->TakeChannel();
    int fd = -1;
    if (const zx_status_t status = fdio_fd_create(channel.release(), &fd); status != ZX_OK) {
      FX_PLOGS(WARNING, status) << "Failed to create fd from previous boot inspect file";
      error_ = Error::kFileReadFailure;
    } else {
      std::string compressed;
      if (!files::ReadFileDescriptorToString(fd, &compressed) || compressed.empty()) {
        FX_LOGS(WARNING) << "Failed to read previous boot inspect file content";
        error_ = Error::kMissingValue;
      } else if (std::optional<std::string> inspect_json = InflateZlib(compressed, kMaxInspectSize);
                 !inspect_json.has_value()) {
        FX_LOGS(WARNING)
            << "Previous boot inspect file is not a valid zlib stream or is larger than "
            << kMaxInspectSize.ToMegabytes() << " MiB once decompressed";
        error_ = Error::kInvalidFormat;
      } else if (inspect_json->empty()) {
        FX_LOGS(WARNING) << "Previous boot inspect file content is empty";
        error_ = Error::kMissingValue;
      } else {
        redactor_->RedactJson(*inspect_json);
        if (!files::WriteFile(path_, *inspect_json)) {
          FX_LOGS(WARNING) << "Failed to write previous boot inspect to file: " << path_;
          error_ = Error::kFileWriteFailure;
        }
      }
      close(fd);
    }
  }

  data_provider_ = fidl::Client<fuchsia_diagnostics_persistence::PreviousBootDataProvider>();

  auto completers = std::move(completers_);
  for (auto& [ticket, completer] : completers) {
    if (completer != nullptr) {
      completer(error_.has_value() ? AttachmentData(*error_) : ReadAttachmentData());
    }
  }
}

AttachmentData PreviousBootInspect::ReadAttachmentData() const {
  std::string inspect_json;
  if (!files::ReadFileToString(path_, &inspect_json)) {
    FX_LOGS(WARNING) << "Failed to read previous boot inspect file: " << path_;
    return AttachmentData(Error::kFileReadFailure);
  }

  if (inspect_json.empty()) {
    FX_LOGS(WARNING) << "Previous boot inspect file was empty: " << path_;
    return AttachmentData(Error::kMissingValue);
  }

  return AttachmentData(std::move(inspect_json));
}

::fpromise::promise<AttachmentData> PreviousBootInspect::Get(const uint64_t ticket) {
  FX_CHECK(!completers_.contains(ticket)) << "Ticket used twice: " << ticket;

  if (files::IsFile(path_)) {
    return ::fpromise::make_ok_promise(ReadAttachmentData());
  }

  if (error_.has_value()) {
    return ::fpromise::make_ok_promise(AttachmentData(*error_));
  }

  ::fpromise::bridge<AttachmentData, void> bridge;
  completers_[ticket] = [completer = std::move(bridge.completer)](AttachmentData value) mutable {
    completer.complete_ok(std::move(value));
  };

  return bridge.consumer.promise_or(::fpromise::ok(AttachmentData(Error::kLogicError)));
}

void PreviousBootInspect::ForceCompletion(const uint64_t ticket, const Error error) {
  if (completers_.contains(ticket) && completers_[ticket] != nullptr) {
    auto completer = std::move(completers_[ticket]);
    completers_.erase(ticket);
    completer(AttachmentData(error));
  }
}

}  // namespace forensics::feedback
