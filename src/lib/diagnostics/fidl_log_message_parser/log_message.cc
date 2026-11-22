// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/diagnostics/fidl_log_message_parser/log_message.h"

#include <assert.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/stream.h>
#include <lib/zx/vmo.h>
#include <zircon/types.h>

#include <algorithm>
#include <mutex>
#include <span>
#include <vector>

#include "src/lib/diagnostics/log/message/rust/cpp-log-decoder/log_decoder_api.h"

using fuchsia_diagnostics::FormattedContent;

namespace diagnostics::fidl_log_message_parser {

namespace {

fpromise::result<void, std::string> ConvertFormattedFxtToLogMessagesInternal(
    std::span<const uint8_t> data, bool expect_extended_attribution, MessageParser* parser,
    std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>>& output) {
  auto log_messages = fuchsia_decode_log_messages_to_struct(data.data(), data.size(),
                                                            expect_extended_attribution, parser);
  if (log_messages.error_str) {
    // Copy the error string so that we can free it before returning
    std::string copied_error_str = log_messages.error_str;
    FX_LOG_KV(ERROR, "Failed to decode log from Archivist", FX_KV("message", copied_error_str));
    fuchsia_free_log_messages(log_messages);
    return fpromise::error(copied_error_str);
  }
  output.reserve(log_messages.messages.len + output.size());
  for (size_t i = 0; i < log_messages.messages.len; i++) {
    auto msg = log_messages.messages.ptr[i];
    output.emplace_back(log_decoder::ToFidlLogMessage(*msg));
  }
  fuchsia_free_log_messages(log_messages);
  return fpromise::ok();
}

fpromise::result<void, std::string> ConvertFormattedContentToLogMessagesInternal(
    FormattedContent content, MessageParser* parser,
    std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>>& output) {
  if (content.fxt().has_value()) {
    uint64_t size = 0;
    zx_status_t status = content.fxt()->get_prop_content_size(&size);
    if (status != ZX_OK) {
      return fpromise::error("Failed to get content size from FXT VMO: " + std::to_string(status));
    }
    std::unique_ptr<unsigned char[]> data = std::make_unique<unsigned char[]>(size);
    status = content.fxt()->read(data.get(), 0, size);
    if (status != ZX_OK) {
      return fpromise::error("Failed to read from FXT VMO: " + std::to_string(status));
    }
    // Note: If |parser| is non-null, |expect_extended_attribution| is ignored.
    // If |parser| is null, |expect_extended_attribution| is used to determine
    // whether extended attribution is expected.
    return ConvertFormattedFxtToLogMessagesInternal(std::span<const uint8_t>(data.get(), size),
                                                    true, parser, output);
  }
  return fpromise::error("Expected FXT content");
}

MessageParser* CreateMessageParserForFormat(fuchsia_diagnostics::Format format) {
  switch (format) {
    case fuchsia_diagnostics::Format::kJson:
    case fuchsia_diagnostics::Format::kText:
    case fuchsia_diagnostics::Format::kCbor:
      return nullptr;
    case fuchsia_diagnostics::Format::kFxt:
      return fuchsia_new_message_parser();
  }
}
}  // namespace

void MessageParserDeleter::operator()(MessageParser* parser) const {
  if (parser) {
    fuchsia_free_message_parser(parser);
  }
}

LogBatchIterator::LogBatchIterator(
    fidl::ClientEnd<fuchsia_diagnostics::BatchIterator> iterator,
    fuchsia_diagnostics::Format format, async_dispatcher_t* dispatcher,
    fidl::AsyncEventHandler<fuchsia_diagnostics::BatchIterator>* event_handler)
    : parser_(CreateMessageParserForFormat(format)) {
  if (iterator.is_valid()) {
    iterator_.Bind(std::move(iterator), dispatcher, event_handler);
  }
}

void LogBatchIterator::GetNext(GetNextCallback callback) {
  if (!iterator_.is_valid()) {
    return;
  }

  iterator_->GetNext().Then(
      [this, callback = std::move(callback)](
          fidl::Result<fuchsia_diagnostics::BatchIterator::GetNext>& result) mutable {
        if (result.is_error()) {
          // Terminal framework errors are reported to the event handler, if any. kUnknownMethod
          // leaves the client bound, so it is reported to the caller instead.
          if (result.error_value().is_framework_error() &&
              result.error_value().framework_error().reason() != fidl::Reason::kUnknownMethod) {
            return;
          }
          callback(fpromise::error(result.error_value().FormatDescription()));
          return;
        }

        std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>> all_messages;
        for (auto& content : result->batch()) {
          auto formatted = ConvertFormattedContentToLogMessagesInternal(
              std::move(content), parser_.get(), all_messages);
          if (formatted.is_error()) {
            callback(fpromise::error(formatted.error()));
            return;
          }
        }
        callback(fpromise::ok(std::move(all_messages)));
      });
}

fpromise::result<std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>>,
                 std::string>
ConvertFormattedFxtToLogMessages(std::span<const uint8_t> data, bool expect_extended_attribution) {
  std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>> messages;
  auto ret = ConvertFormattedFxtToLogMessagesInternal(data, expect_extended_attribution, nullptr,
                                                      messages);
  if (ret.is_error()) {
    return fpromise::error(ret.take_error());
  }
  return fpromise::ok(std::move(messages));
}

fpromise::result<std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>>,
                 std::string>
ConvertFormattedContentToLogMessages(FormattedContent content, MessageParser* parser) {
  std::unique_ptr<MessageParser, MessageParserDeleter> owned_parser;
  if (!parser) {
    owned_parser.reset(fuchsia_new_message_parser());
    parser = owned_parser.get();
  }
  std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>> messages;
  auto ret = ConvertFormattedContentToLogMessagesInternal(std::move(content), parser, messages);
  if (ret.is_error()) {
    return fpromise::error(ret.take_error());
  }
  return fpromise::ok(std::move(messages));
}

fuchsia_logging::RawLogSeverity GetSeverityFromVerbosity(uint8_t verbosity) {
  // Clamp verbosity scale to the interstitial space between INFO and DEBUG
  uint8_t max_verbosity =
      (fuchsia_logging::LogSeverity::Info - fuchsia_logging::LogSeverity::Debug);
  if (verbosity > max_verbosity) {
    verbosity = max_verbosity;
  }
  int severity = fuchsia_logging::LogSeverity::Info - verbosity;
  if (severity < fuchsia_logging::LogSeverity::Debug + 1) {
    return fuchsia_logging::LogSeverity::Debug + 1;
  }
  return static_cast<fuchsia_logging::RawLogSeverity>(severity);
}

}  // namespace diagnostics::fidl_log_message_parser
