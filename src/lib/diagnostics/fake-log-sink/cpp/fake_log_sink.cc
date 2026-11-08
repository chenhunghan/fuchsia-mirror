// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/diagnostics/fake-log-sink/cpp/fake_log_sink.h"

#include <lib/async-loop/cpp/loop.h>
#include <zircon/assert.h>

#include <sdk/lib/syslog/cpp/log_settings.h>

#include "src/lib/diagnostics/fake-log-sink/rust/fake_log_sink.h"
#include "src/lib/diagnostics/log/message/rust/cpp-log-decoder/log_decoder.h"

namespace fuchsia_logging {
namespace {

struct ScopedLogMessages {
  LogMessages messages;
  explicit ScopedLogMessages(LogMessages m) : messages(m) {}
  ScopedLogMessages(const ScopedLogMessages&) = delete;
  ScopedLogMessages& operator=(const ScopedLogMessages&) = delete;
  ~ScopedLogMessages() { fuchsia_free_log_messages(messages); }
};

fuchsia_diagnostics_types::Severity RawSeverityToSeverity(uint8_t raw_severity) {
  if (raw_severity <= static_cast<uint8_t>(fuchsia_diagnostics_types::Severity::kTrace)) {
    return fuchsia_diagnostics_types::Severity::kTrace;
  }
  if (raw_severity <= static_cast<uint8_t>(fuchsia_diagnostics_types::Severity::kDebug)) {
    return fuchsia_diagnostics_types::Severity::kDebug;
  }
  if (raw_severity <= static_cast<uint8_t>(fuchsia_diagnostics_types::Severity::kInfo)) {
    return fuchsia_diagnostics_types::Severity::kInfo;
  }
  if (raw_severity <= static_cast<uint8_t>(fuchsia_diagnostics_types::Severity::kWarn)) {
    return fuchsia_diagnostics_types::Severity::kWarn;
  }
  if (raw_severity <= static_cast<uint8_t>(fuchsia_diagnostics_types::Severity::kError)) {
    return fuchsia_diagnostics_types::Severity::kError;
  }
  return fuchsia_diagnostics_types::Severity::kFatal;
}

diagnostics::reader::LogsData LogMessageToLogsData(const LogMessage& message) {
  diagnostics::reader::LogsData::Metadata metadata;
  metadata.timestamp = static_cast<uint64_t>(message.timestamp);
  metadata.severity = RawSeverityToSeverity(message.severity);

  metadata.tags.reserve(message.tags.len);
  for (size_t i = 0; i < message.tags.len; ++i) {
    const auto& tag = message.tags.ptr[i];
    if (tag.ptr && tag.len > 0) {
      metadata.tags.emplace_back(reinterpret_cast<const char*>(tag.ptr), tag.len);
    } else {
      metadata.tags.emplace_back();
    }
  }

  if (message.pid != 0) {
    metadata.pid = message.pid;
  }
  if (message.tid != 0) {
    metadata.tid = message.tid;
  }
  if (message.file.ptr && message.file.len > 0) {
    metadata.file = std::string(reinterpret_cast<const char*>(message.file.ptr), message.file.len);
  }
  metadata.line = message.line;

  if (message.dropped > 0) {
    metadata.errors.push_back(
        diagnostics::reader::LogsData::DroppedLogsError{.count = message.dropped});
  }

  std::string msg_str;
  if (message.message.ptr && message.message.len > 0) {
    msg_str = std::string(reinterpret_cast<const char*>(message.message.ptr), message.message.len);
  }

  std::vector<inspect::PropertyValue> keys;
  keys.reserve(message.kvps.len);
  for (size_t i = 0; i < message.kvps.len; ++i) {
    const auto& kvp = message.kvps.ptr[i];
    std::string key_name;
    if (kvp.key.ptr && kvp.key.len > 0) {
      key_name = std::string(reinterpret_cast<const char*>(kvp.key.ptr), kvp.key.len);
    }
    switch (kvp.value.tag) {
      case CppValue::Tag::CPP_VALUE_SIGNED_INT:
        keys.emplace_back(std::move(key_name), inspect::IntPropertyValue(kvp.value.SIGNED_INT._0));
        break;
      case CppValue::Tag::CPP_VALUE_UNSIGNED_INT:
        keys.emplace_back(std::move(key_name),
                          inspect::UintPropertyValue(kvp.value.UNSIGNED_INT._0));
        break;
      case CppValue::Tag::CPP_VALUE_FLOATING:
        keys.emplace_back(std::move(key_name), inspect::DoublePropertyValue(kvp.value.FLOATING._0));
        break;
      case CppValue::Tag::CPP_VALUE_BOOLEAN:
        keys.emplace_back(std::move(key_name), inspect::BoolPropertyValue(kvp.value.BOOLEAN._0));
        break;
      case CppValue::Tag::CPP_VALUE_TEXT: {
        std::string val_str;
        if (kvp.value.TEXT._0.ptr && kvp.value.TEXT._0.len > 0) {
          val_str = std::string(reinterpret_cast<const char*>(kvp.value.TEXT._0.ptr),
                                kvp.value.TEXT._0.len);
        }
        keys.emplace_back(std::move(key_name), inspect::StringPropertyValue(std::move(val_str)));
        break;
      }
    }
  }

  return diagnostics::reader::LogsData("test_moniker", std::move(metadata), std::move(msg_str),
                                       std::move(keys),
                                       /*version=*/1);
}

}  // namespace

FakeLogSink::FakeLogSink(RawLogSeverity severity,
                         fidl::ServerEnd<fuchsia_logger::LogSink> server_end)
    : impl_(internal::fake_log_sink_new()) {
  SetSeverity(severity);
  if (server_end.is_valid()) {
    internal::fake_log_sink_serve(impl_, server_end.TakeChannel().release());
  } else {
    auto endpoints = fidl::CreateEndpoints<fuchsia_logger::LogSink>();
    ZX_ASSERT(endpoints.is_ok());

    internal::fake_log_sink_serve(impl_, endpoints->server.TakeChannel().release());

    // Create a dispatcher for the global logger.
    static async::Loop* global_loop = [] {
      auto* loop = new async::Loop(&kAsyncLoopConfigNeverAttachToThread);
      loop->StartThread();
      return loop;
    }();

    LogSettingsBuilder builder;
    builder.WithLogSink(endpoints->client.TakeChannel().release())
        .WithDispatcher(global_loop->dispatcher())
        .BuildAndInitialize();
  }
}

FakeLogSink::FakeLogSink(FakeLogSink&& other) : impl_(other.impl_) { other.impl_ = nullptr; }

FakeLogSink& FakeLogSink::operator=(FakeLogSink&& other) {
  if (this == &other) {
    return *this;
  }
  if (impl_) {
    internal::fake_log_sink_delete(impl_);
  }
  impl_ = other.impl_;
  other.impl_ = nullptr;
  return *this;
}

FakeLogSink::~FakeLogSink() {
  if (impl_) {
    internal::fake_log_sink_delete(impl_);
  }
}

bool FakeLogSink::WaitForRecord(zx::time deadline) const {
  return internal::fake_log_sink_wait_for_record(impl_, deadline.get()) > 0;
}

std::vector<uint8_t> FakeLogSink::ReadRecord() {
  uintptr_t record_size = internal::fake_log_sink_wait_for_record(impl_, ZX_TIME_INFINITE);
  ZX_ASSERT(record_size > 0);
  std::vector<uint8_t> buf(record_size);
  internal::fake_log_sink_read_record(impl_, buf.data(), record_size);
  return buf;
}

std::optional<diagnostics::reader::LogsData> FakeLogSink::ReadLogsData() {
  auto record = ReadRecord();
  if (record.empty()) {
    return {};
  }
  ZX_DEBUG_ASSERT(record.data() != nullptr);
  ScopedLogMessages decoded(fuchsia_decode_log_messages_to_struct(
      record.data(), record.size(), /*expect_extended_attribution=*/false, nullptr));
  if (decoded.messages.error_str != nullptr || decoded.messages.messages.len == 0 ||
      decoded.messages.messages.ptr == nullptr || decoded.messages.messages.ptr[0] == nullptr) {
    return {};
  }
  return LogMessageToLogsData(*decoded.messages.messages.ptr[0]);
}

void FakeLogSink::SetSeverity(RawLogSeverity severity) {
  internal::fake_log_sink_set_min_severity(impl_, severity);
}

}  // namespace fuchsia_logging
