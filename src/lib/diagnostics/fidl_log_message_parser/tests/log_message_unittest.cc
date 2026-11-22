// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.diagnostics/cpp/fidl.h>
#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/stdcompat/string_view.h>
#include <lib/syslog/cpp/log_message_impl.h>
#include <lib/syslog/cpp/macros.h>

#include <cinttypes>
#include <optional>
#include <string_view>

#include <gtest/gtest.h>
#include <src/lib/diagnostics/fidl_log_message_parser/log_message.h>
#include <src/lib/fsl/vmo/strings.h>
#include <src/lib/fxl/strings/string_printf.h>

#include "lib/syslog/cpp/log_level.h"
#include "src/lib/diagnostics/fidl_log_message_parser/test_support.h"
#include "src/lib/fxl/strings/join_strings.h"

using fuchsia_diagnostics::FormattedContent;

namespace {

FormattedContent JsonFormattedContent(std::string_view json) {
  fsl::SizedVmo vmo;
  FX_CHECK(fsl::VmoFromString(json, &vmo));
  return FormattedContent::WithJson(
      fuchsia_mem::Buffer{{.vmo = std::move(vmo.vmo()), .size = vmo.size()}});
}

zx::vmo BuildFxtVmoFromBuffer(fuchsia_logging::LogBuffer& buffer) {
  auto span = buffer.EndRecord();
  zx::vmo vmo;
  FX_CHECK(zx::vmo::create(span.size(), 0, &vmo) == ZX_OK);
  FX_CHECK(vmo.write(span.data(), 0, span.size()) == ZX_OK);
  FX_CHECK(vmo.set_prop_content_size(span.size()) == ZX_OK);
  return vmo;
}

// Test compatibility shim: routes ConvertFormattedContentToLogMessages through
// ConvertFormattedContentForTest which transcodes JSON payloads to FXT and decodes
// them through the diagnostics hierarchy.
inline fpromise::result<std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>>,
                        std::string>
ConvertFormattedContentToLogMessages(FormattedContent content) {
  return diagnostics::fidl_log_message_parser::testing::ConvertFormattedContentForTest(
      std::move(content));
}

class FakeBatchIterator : public fidl::Server<fuchsia_diagnostics::BatchIterator> {
 public:
  FakeBatchIterator()
      : should_error_(false),
        error_to_return_(fuchsia_diagnostics::ReaderError::kIo),
        hold_completer_(false) {}

  void GetNext(GetNextCompleter::Sync& completer) override {
    if (hold_completer_) {
      held_completers_.push_back(completer.ToAsync());
      return;
    }

    if (should_error_) {
      completer.Reply(fit::error(error_to_return_));
      return;
    }

    completer.Reply(
        fit::ok(fuchsia_diagnostics::BatchIteratorGetNextResponse{{.batch = std::move(batch_)}}));
  }

  void WaitForReady(WaitForReadyCompleter::Sync& completer) override { completer.Reply(); }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_diagnostics::BatchIterator> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

  void SetBatch(std::vector<FormattedContent> batch) {
    batch_ = std::move(batch);
    should_error_ = false;
  }

  void SetError(fuchsia_diagnostics::ReaderError error) {
    error_to_return_ = error;
    should_error_ = true;
  }

  void HoldCompleter(bool hold) { hold_completer_ = hold; }

  void ClearHeld() { held_completers_.clear(); }

 private:
  std::vector<FormattedContent> batch_;
  bool should_error_;
  fuchsia_diagnostics::ReaderError error_to_return_;
  bool hold_completer_;
  std::vector<GetNextCompleter::Async> held_completers_;
};

class LogBatchIteratorTestHelper {
 public:
  LogBatchIteratorTestHelper() : loop_(&kAsyncLoopConfigNoAttachToCurrentThread) {
    auto [client_end, server_end] = fidl::Endpoints<fuchsia_diagnostics::BatchIterator>::Create();
    binding_.emplace(loop_.dispatcher(), std::move(server_end), &fake_iterator_,
                     fidl::kIgnoreBindingClosure);
    iterator_ = std::make_unique<diagnostics::fidl_log_message_parser::LogBatchIterator>(
        std::move(client_end), fuchsia_diagnostics::Format::kFxt, loop_.dispatcher());
  }

  void SetBatch(std::vector<FormattedContent> batch) { fake_iterator_.SetBatch(std::move(batch)); }

  void SetError(fuchsia_diagnostics::ReaderError error) { fake_iterator_.SetError(error); }

  fpromise::result<std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>>,
                   std::string>
  GetNext() {
    fpromise::result<std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>>,
                     std::string>
        out_result;
    bool done = false;
    iterator_->GetNext([&](auto result) {
      out_result = std::move(result);
      done = true;
    });

    while (!done) {
      loop_.Run(zx::deadline_after(zx::msec(10)), true);
    }
    return out_result;
  }

 private:
  async::Loop loop_;
  FakeBatchIterator fake_iterator_;
  std::optional<fidl::ServerBinding<fuchsia_diagnostics::BatchIterator>> binding_;
  std::unique_ptr<diagnostics::fidl_log_message_parser::LogBatchIterator> iterator_;
};

class FxtToLogMessageTranscoderForTest {
 public:
  explicit FxtToLogMessageTranscoderForTest(LogBatchIteratorTestHelper* helper) : helper_(helper) {}
  explicit FxtToLogMessageTranscoderForTest(std::nullptr_t)
      : owned_helper_(std::make_unique<LogBatchIteratorTestHelper>()) {}

  fpromise::result<std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>>,
                   std::string>
  Transcode(FormattedContent content) {
    if (content.IsUnknown()) {
      return fpromise::error("Expected json or FXT content");
    }
    LogBatchIteratorTestHelper* helper = helper_ ? helper_ : owned_helper_.get();
    std::vector<FormattedContent> batch;
    batch.push_back(std::move(content));
    helper->SetBatch(std::move(batch));
    return helper->GetNext();
  }

 private:
  LogBatchIteratorTestHelper* helper_ = nullptr;
  std::unique_ptr<LogBatchIteratorTestHelper> owned_helper_;
};

TEST(LogMessage, StatePersistence) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  constexpr char kTestMoniker[] = "some_moniker";
  constexpr char kTestUrl[] = "fuchsia-pkg://fuchsia.com/test#test.cm";

  // 1. Build Manifest Record
  fuchsia_logging::LogBufferBuilder manifest_builder(fuchsia_logging::LogSeverity::Info);
  auto manifest_buffer = manifest_builder.Build();
  manifest_buffer.WriteKeyValue("moniker", kTestMoniker);
  manifest_buffer.WriteKeyValue("url", kTestUrl);
  std::span<const uint8_t> manifest_span = manifest_buffer.EndRecord();

  // 2. Build Log Record
  fuchsia_logging::LogBufferBuilder log_builder(fuchsia_logging::LogSeverity::Info);
  auto log_buffer = log_builder.WithMsg("test message").Build();
  std::span<const uint8_t> log_span = log_buffer.EndRecord();

  // 3. Prep Manifest Content (with LOG_CONTROL_BIT flipped)
  std::vector<uint8_t> manifest_raw(manifest_span.size());
  memcpy(manifest_raw.data(), manifest_span.data(), manifest_span.size());
  uint64_t* header_ptr = reinterpret_cast<uint64_t*>(manifest_raw.data());
  *header_ptr |= (1ULL << 47);

  LogBatchIteratorTestHelper helper;

  // 4. Send Manifest
  {
    zx::vmo vmo;
    zx::vmo::create(manifest_raw.size(), 0, &vmo);
    vmo.write(manifest_raw.data(), 0, manifest_raw.size());
    uint64_t size = manifest_raw.size();
    vmo.set_property(ZX_PROP_VMO_CONTENT_SIZE, &size, sizeof(size));

    auto results = FxtToLogMessageTranscoderForTest(&helper).Transcode(
        FormattedContent::WithFxt(std::move(vmo)));
    ASSERT_TRUE(results.is_ok());
    // Manifest records don't produce log messages, they just update state.
    EXPECT_EQ(0u, results.value().size());
  }

  // 5. Send Log Message (it should use the persisted state)
  {
    zx::vmo vmo;
    zx::vmo::create(log_span.size(), 0, &vmo);
    vmo.write(log_span.data(), 0, log_span.size());
    uint64_t size = log_span.size();
    vmo.set_property(ZX_PROP_VMO_CONTENT_SIZE, &size, sizeof(size));

    auto results = FxtToLogMessageTranscoderForTest(&helper).Transcode(
        FormattedContent::WithFxt(std::move(vmo)));
    ASSERT_TRUE(results.is_ok());
    ASSERT_EQ(1u, results.value().size());
    ASSERT_TRUE(results.value()[0].is_ok());
    auto& msg = results.value()[0].value();
    EXPECT_EQ("test message", msg.msg());
    ASSERT_EQ(1u, msg.tags().size());
    EXPECT_EQ(kTestMoniker, msg.tags()[0]);
  }
}

TEST(LogMessage, Empty) {
  auto messages = ConvertFormattedContentToLogMessages(JsonFormattedContent("[]"));
  ASSERT_TRUE(messages.is_ok());
  EXPECT_EQ(0u, messages.value().size());
}

TEST(LogMessage, WrongType) {
  zx::vmo vmo;
  ASSERT_EQ(ZX_OK, zx::vmo::create(0, 0, &vmo));
  EXPECT_TRUE(
      ConvertFormattedContentToLogMessages(FormattedContent::WithCbor(std::move(vmo))).is_error());
}

std::string SeverityToString(const int32_t severity) {
  if (severity == fuchsia_logging::LogSeverity::Trace) {
    return "TRACE";
  } else if (severity == fuchsia_logging::LogSeverity::Debug) {
    return "DEBUG";
  } else if (severity > fuchsia_logging::LogSeverity::Debug &&
             severity < fuchsia_logging::LogSeverity::Info) {
    return fxl::StringPrintf("VLOG(%d)", fuchsia_logging::LogSeverity::Info - severity);
  } else if (severity == fuchsia_logging::LogSeverity::Info) {
    return "INFO";
  } else if (severity == fuchsia_logging::LogSeverity::Warn) {
    return "WARN";
  } else if (severity == fuchsia_logging::LogSeverity::Error) {
    return "ERROR";
  } else if (severity == fuchsia_logging::LogSeverity::Fatal) {
    return "FATAL";
  }
  return "INVALID";
}

TEST(LogMessage, MonikerStringification) {
  constexpr char kSampleLogPayload[] =
      R"JSON([
      {
        "data_source": "Logs",
        "metadata": {
          "errors": null,
          "component_url": "",
          "timestamp": 8331458424750,
          "severity": "INFO",
          "size_bytes": 2048,
          "tags": [],
          "pid": 1102869,
          "tid": 1102871,
          "file": "src/diagnostics/lib/cpp-log-decoder/test.cc",
          "line": 24,
          "dropped": 0
        },
        "moniker": "test/path/test_moniker",
        "payload": {
          "root": {
            "keys": {
              "user property": 5.2
            },
            "message": {
              "value": "test message"
            }
          }
        },
        "version": 1
      }
    ])JSON";
  auto messages =
      ConvertFormattedContentToLogMessages(JsonFormattedContent(kSampleLogPayload)).take_value();
  auto message = messages[0].value();
  auto encoded_message = fxl::StringPrintf(
      "[%05d.%03d][%05" PRIu64 "][%05" PRIu64 "][%s] %s: %s\n",
      static_cast<int>(message.time().get() / 1000000000ULL),
      static_cast<int>((message.time().get() / 1000000ULL) % 1000ULL), message.pid(), message.tid(),
      fxl::JoinStrings(message.tags(), ", ").c_str(), SeverityToString(message.severity()).c_str(),
      message.msg().c_str());
  ASSERT_TRUE(cpp23::contains(encoded_message,
                              "[test_moniker] INFO: "
                              "[src/diagnostics/lib/cpp-log-decoder/test.cc(24)] test "
                              "message user property=5.2"));
}

TEST(LogMessage, LegacyHostEncoding) {
  constexpr char kSampleLogPayload[] =
      R"JSON([
      {
        "data_source": "Logs",
        "metadata": {
          "errors": null,
          "component_url": "",
          "timestamp": 8331458424750,
          "severity": "INFO",
          "size_bytes": 2048,
          "tags": [
            "some tag",
            "some other tag"
          ],
          "pid": 1102869,
          "tid": 1102871,
          "file": "src/diagnostics/lib/cpp-log-decoder/test.cc",
          "line": 24,
          "dropped": 0
        },
        "moniker": "test_moniker",
        "payload": {
          "root": {
            "keys": {
              "user property": 5.2
            },
            "message": {
              "value": "test message"
            }
          }
        },
        "version": 1
      }
    ])JSON";
  auto messages =
      ConvertFormattedContentToLogMessages(JsonFormattedContent(kSampleLogPayload)).take_value();
  auto message = messages[0].value();
  auto encoded_message = fxl::StringPrintf(
      "[%05d.%03d][%05" PRIu64 "][%05" PRIu64 "][%s] %s: %s\n",
      static_cast<int>(message.time().get() / 1000000000ULL),
      static_cast<int>((message.time().get() / 1000000ULL) % 1000ULL), message.pid(), message.tid(),
      fxl::JoinStrings(message.tags(), ", ").c_str(), SeverityToString(message.severity()).c_str(),
      message.msg().c_str());
  ASSERT_TRUE(cpp23::contains(encoded_message,
                              "[test_moniker, some tag, some other tag] INFO: "
                              "[src/diagnostics/lib/cpp-log-decoder/test.cc(24)] test "
                              "message user property=5.2"));
}

struct ValidationTestCase {
  std::string input;

  // If set, check that the conversion function returned this error instead of a vector.
  std::optional<std::string> expected_overall_error = std::nullopt;

  // If set, assert on the exact number of messages returned.
  std::optional<int> expected_count = std::nullopt;

  // If set, assert that message has the given error.
  std::optional<std::string> expected_error = std::nullopt;

  // If set, assert that message is OK and matches the given value.
  std::optional<std::string> expected_message = std::nullopt;

  // If set, assert that message is OK and tags match the given value.
  std::optional<std::vector<std::string>> expected_tags = std::nullopt;

  // If set, assert that message is OK and dropped logs match the given value.
  std::optional<uint32_t> dropped_logs = std::nullopt;
};

void RunValidationCases(std::vector<ValidationTestCase> cases) {
  for (const auto& test_case : cases) {
    auto results = ConvertFormattedContentToLogMessages(JsonFormattedContent(test_case.input));
    EXPECT_EQ(!test_case.expected_overall_error.has_value(), results.is_ok()) << test_case.input;
    if (test_case.expected_overall_error.has_value()) {
      ASSERT_TRUE(results.is_error());
      EXPECT_EQ(test_case.expected_overall_error.value(), results.error());
    } else if (results.is_ok()) {
      if (test_case.expected_count.has_value()) {
        EXPECT_EQ(static_cast<size_t>(test_case.expected_count.value()), results.value().size())
            << test_case.input;
      }

      if (test_case.expected_message.has_value()) {
        ASSERT_LE(1u, results.value().size())
            << "Must have at least one output to check expected message against";
        for (const auto& res : results.value()) {
          ASSERT_TRUE(res.is_ok()) << test_case.input;
          EXPECT_EQ(test_case.expected_message.value(), res.value().msg()) << test_case.input;
        }
      }
      if (test_case.expected_tags.has_value()) {
        ASSERT_LE(1u, results.value().size())
            << "Must have at least one output to check expected tags against";
        for (const auto& res : results.value()) {
          ASSERT_TRUE(res.is_ok()) << test_case.input;
          ASSERT_EQ(test_case.expected_tags.value().size(), res.value().tags().size())
              << test_case.input;
          for (size_t i = 0; i < res.value().tags().size(); i++) {
            EXPECT_EQ(test_case.expected_tags.value()[i], res.value().tags()[i])
                << "tag index " << i << " " << test_case.input;
          }
        }
      }
      if (test_case.dropped_logs.has_value()) {
        ASSERT_LE(1u, results.value().size())
            << "Must have at least one output to check expected dropped logs";
        for (const auto& res : results.value()) {
          ASSERT_TRUE(res.is_ok()) << test_case.input;
          EXPECT_EQ(test_case.dropped_logs.value(), res.value().dropped_logs()) << test_case.input;
        }
      }
      if (test_case.expected_error.has_value()) {
        ASSERT_LE(1u, results.value().size())
            << "Must have at least one output to check expected errors";
        for (const auto& res : results.value()) {
          ASSERT_TRUE(res.is_error()) << test_case.input;
          EXPECT_EQ(test_case.expected_error.value(), res.error()) << test_case.input;
        }
      }
    }
  }
}

const char PAYLOAD_TEMPLATE[] = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO"
    },
    "payload": %s
  }
]
)JSON";

const char VALID_MESSAGE_FOR_SEVERITY[] = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "%s",
      "pid": 200,
      "tid": 300,
      "tags": ["a"]
    },
    "payload": {
      "root": {
        "message": {
          "value": "Hello world"
        },
        "keys": {
          "arbitrary_kv": 1024
        }
      }
    }
  }
]
)JSON";

const char TWO_FLAT_VALID_INFO_MESSAGES[] = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "pid": 200,
      "tid": 300,
      "tags": ["a"]
    },
    "payload": {
      "root": {
        "message": {
          "value": "Hello world"
        },
        "keys": {
          "arbitrary_kv": 1024
        }
      }
    }
  },
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "pid": 200,
      "tid": 300,
      "tags": ["a"]
    },
    "payload": {
      "root": {
        "message": {
            "value": "Hello world"
        },
        "keys": {
          "arbitrary_kv": 1024
        }
      }
    }
  }
]
)JSON";

TEST(LogMessage, Valid) {
  // Ensure that both flat and nested (under "root") messages work.
  auto one_message_result = ConvertFormattedContentToLogMessages(
      JsonFormattedContent(fxl::StringPrintf(VALID_MESSAGE_FOR_SEVERITY, "INFO")));

  auto two_message_result =
      ConvertFormattedContentToLogMessages(JsonFormattedContent(TWO_FLAT_VALID_INFO_MESSAGES));

  ASSERT_TRUE(one_message_result.is_ok());
  ASSERT_TRUE(two_message_result.is_ok());

  std::vector<fpromise::result<fuchsia_logger::LogMessage, std::string>> messages;
  messages.insert(messages.end(), one_message_result.value().begin(),
                  one_message_result.value().end());
  messages.insert(messages.end(), two_message_result.value().begin(),
                  two_message_result.value().end());
  ASSERT_EQ(3u, messages.size());
  for (const auto& val : messages) {
    ASSERT_TRUE(val.is_ok());
    EXPECT_EQ("Hello world arbitrary_kv=1024", val.value().msg());
    EXPECT_EQ(200u, val.value().pid());
    EXPECT_EQ(300u, val.value().tid());
    ASSERT_EQ(1u, val.value().tags().size());
    EXPECT_EQ("a", val.value().tags()[0]);
    EXPECT_EQ(1000u, val.value().time().get());
    EXPECT_EQ(0u, val.value().dropped_logs());
    EXPECT_EQ(static_cast<int32_t>(fuchsia_logger::LogLevelFilter::kInfo), val.value().severity());
  }
}

TEST(LogMessage, ValidSeverityTests) {
  struct TestCase {
    std::string input;
    fuchsia_logger::LogLevelFilter severity;
  };

  std::vector<TestCase> cases;
  cases.emplace_back(TestCase{
      .input = fxl::StringPrintf(VALID_MESSAGE_FOR_SEVERITY, "INFO"),
      .severity = fuchsia_logger::LogLevelFilter::kInfo,
  });
  cases.emplace_back(TestCase{
      .input = fxl::StringPrintf(VALID_MESSAGE_FOR_SEVERITY, "info"),
      .severity = fuchsia_logger::LogLevelFilter::kInfo,
  });
  cases.emplace_back(TestCase{
      .input = fxl::StringPrintf(VALID_MESSAGE_FOR_SEVERITY, "INFO"),
      .severity = fuchsia_logger::LogLevelFilter::kInfo,
  });
  cases.emplace_back(TestCase{
      .input = fxl::StringPrintf(VALID_MESSAGE_FOR_SEVERITY, "TRACE"),
      .severity = fuchsia_logger::LogLevelFilter::kTrace,
  });
  cases.emplace_back(TestCase{
      .input = fxl::StringPrintf(VALID_MESSAGE_FOR_SEVERITY, "DEBUG"),
      .severity = fuchsia_logger::LogLevelFilter::kDebug,
  });
  cases.emplace_back(TestCase{
      .input = fxl::StringPrintf(VALID_MESSAGE_FOR_SEVERITY, "WARN"),
      .severity = fuchsia_logger::LogLevelFilter::kWarn,
  });
  cases.emplace_back(TestCase{
      .input = fxl::StringPrintf(VALID_MESSAGE_FOR_SEVERITY, "ERROR"),
      .severity = fuchsia_logger::LogLevelFilter::kError,
  });
  cases.emplace_back(TestCase{
      .input = fxl::StringPrintf(VALID_MESSAGE_FOR_SEVERITY, "FATAL"),
      .severity = fuchsia_logger::LogLevelFilter::kFatal,
  });
  cases.emplace_back(TestCase{
      .input = fxl::StringPrintf(VALID_MESSAGE_FOR_SEVERITY, "unknown"),
      .severity = fuchsia_logger::LogLevelFilter::kInfo,
  });

  for (const auto& test_case : cases) {
    auto messages = ConvertFormattedContentToLogMessages(JsonFormattedContent(test_case.input));
    ASSERT_TRUE(messages.is_ok());
    ASSERT_EQ(1u, messages.value().size());
    ASSERT_TRUE(messages.value()[0].is_ok());
    EXPECT_EQ(static_cast<int32_t>(test_case.severity), messages.value()[0].value().severity());
  }
}

const char META_TEMPLATE[] = R"JSON(
[
  {
    "metadata": {
      "pid": 200,
      "tid": 300,
      %s
    },
    "payload": {
      "root": {
        "message": {
          "value": "Hello world"
        },
        "keys": {
          "arbitrary_kv": 1024
        }
      }
    }
  }
]
)JSON";

const char MONIKER_TEMPLATE[] = R"JSON(
[
  {
    "moniker": "%s",
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "pid": 200,
      "tid": 300
    },
    "payload": {
      "root": {
        "message": {
          "value": "Hello world"
        },
        "keys": {
          "arbitrary_kv": 1024
        }
      }
    }
  }
]
)JSON";

TEST(LogMessage, MetadataValidation) {
  std::vector<ValidationTestCase> cases;

  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(META_TEMPLATE, R"("severity": "INFO", "timestamp": 1000)"),
      .expected_count = 1});
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(META_TEMPLATE, R"("severity": "INFO", "timestamp": "string")"),
      .expected_error = "Expected metadata.timestamp key",
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(META_TEMPLATE, R"("severity": "INFO")"),
      .expected_error = "Expected metadata.timestamp key",
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(META_TEMPLATE, R"("timestamp": 1000)"),
      .expected_error = "Expected metadata.severity key",
  });

  RunValidationCases(std::move(cases));
}

const char ROOT_TEMPLATE[] = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO"
    },
    "payload": {
      "root": %s
    }
  }
]
)JSON";

TEST(LogMessage, PayloadValidation) {
  std::vector<ValidationTestCase> cases;
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(ROOT_TEMPLATE, R"({"message": "Hello"})"),
      .expected_count = 1,
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(ROOT_TEMPLATE, R"("invalid type")"),
      .expected_error = "Expected payload.root to be an object if present",
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(ROOT_TEMPLATE, R"(1000)"),
      .expected_error = "Expected payload.root to be an object if present",
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(PAYLOAD_TEMPLATE, R"({"message": "Hello"})"),
      .expected_count = 1,
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(PAYLOAD_TEMPLATE, R"("invalid type")"),
      .expected_error = "Expected metadata and payload objects",
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(PAYLOAD_TEMPLATE, R"(1000)"),
      .expected_error = "Expected metadata and payload objects",
  });

  RunValidationCases(std::move(cases));
}

TEST(LogMessage, JsonValidation) {
  std::vector<ValidationTestCase> cases;
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(ROOT_TEMPLATE, R"({"message": "Hello"})"),
      .expected_count = 1,
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(ROOT_TEMPLATE, R"({"message": "Hello"},)"),
      .expected_overall_error =
          "Failed to parse content as JSON. Offset 139: Missing a name for object member.",
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(PAYLOAD_TEMPLATE, R"({"message": "Hello"})"),
      .expected_count = 1,
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(PAYLOAD_TEMPLATE, R"({"message": "Hello"},)"),
      .expected_overall_error =
          "Failed to parse content as JSON. Offset 121: Missing a name for object member.",
  });

  RunValidationCases(std::move(cases));
}

TEST(LogMessage, FileValidation) {
  std::vector<ValidationTestCase> cases;
  cases.emplace_back(ValidationTestCase{
      .input = "[]",
      .expected_count = 0,
  });
  cases.emplace_back(ValidationTestCase{
      .input = "[3]",
      .expected_count = 1,
      .expected_error = "Value is not an object",
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"(["a", "b"])",
      .expected_count = 2,
      .expected_error = "Value is not an object",
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"({"payload": {}})",
      .expected_overall_error = "Expected content to contain an array",
  });

  RunValidationCases(std::move(cases));
}

TEST(LogMessage, MessageFormatting) {
  std::vector<ValidationTestCase> cases;
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "file": "test.cc",
      "line": 420
    },
    "payload": {
      "root": {
        "message": {
          "value": "Hello, world"
        }
      }
    }
  }
]
)JSON",
      .expected_message = "[test.cc(420)] Hello, world",
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO"
    },
    "payload": {
      "root": {
        "message": {
          "value": "Hello, world"
        },
        "keys": {
          "kv": "ok"
        }
      }
    }
  }
]
)JSON",
      .expected_message = "Hello, world kv=\"ok\"",
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO"
    },
    "payload": {
      "root": {
        "message": {
          "value": "Hello, world"
        },
        "keys": {
          "int": -5,
          "intp": 5,
          "repeat": 2,
          "uint": 9223400000000000000,
          "float": 5.25
        }
      }
    }
  }
]
)JSON",
      .expected_message = "Hello, world int=-5 intp=5 repeat=2 uint=9223400000000000000 float=5.25",
  });

  RunValidationCases(std::move(cases));
}

TEST(LogMessage, Tags) {
  std::vector<ValidationTestCase> cases;
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "tags": ["hello"]
    },
    "payload": {
      "root": {
        "message": {
          "value": ""
        }
      }
    }
  }
]
)JSON",
      .expected_tags = std::vector<std::string>{"hello"},
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "tags": "hello"
    },
    "payload": {
      "root": {
        "message": {
          "value": ""
        }
      }
    }
  }
]
)JSON",
      .expected_tags = std::vector<std::string>{"hello"},
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "tags": ["hello", "world"]
    },
    "payload": {
      "root": {
        "message": {
          "value": ""
        }
      }
    }
  }
]
)JSON",
      .expected_tags = std::vector<std::string>{"hello", "world"},
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "tags": ["hello", "world"]
    },
    "moniker": "world/hello",
    "payload": {
      "root": {
        "message": {
          "value": ""
        }
      }
    }
  }
]
)JSON",
      .expected_tags = std::vector<std::string>{"hello", "world"},
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "tags": ["foo"]
    },
    "moniker": "world/hello",
    "payload": {
      "root": {
        "message": {
          "value": ""
        }
      }
    }
  }
]
)JSON",
      .expected_tags = std::vector<std::string>{"hello", "foo"},
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "tags": ["world"]
    },
    "moniker": "hello",
    "payload": {
      "root": {
        "message": {
          "value": ""
        }
      }
    }
  }
]
)JSON",
      .expected_tags = std::vector<std::string>{"hello", "world"},
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "tags": ["hello", 3]
    },
    "payload": {
      "root": {
        "message": {
          "value": ""
        }
      }
    }
  }
]
)JSON",
      .expected_error = "Tags array must contain strings",
  });
  cases.emplace_back(ValidationTestCase{
      .input = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "tags": 3
    },
    "payload": {
      "root": {
        "message": {
          "value": ""
        }
      }
    }
  }
]
)JSON",
      .expected_error = "Tags must be a string or array of strings",
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(MONIKER_TEMPLATE, "test"),
      .expected_tags = std::vector<std::string>{"test"},
  });

  RunValidationCases(std::move(cases));
}

TEST(LogMessage, DroppedLogs) {
  std::vector<ValidationTestCase> cases;
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(META_TEMPLATE,
                                 R"("timestamp": 1000, "severity": "INFO", "errors": ["test"])"),
      .dropped_logs = 0,
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(
          META_TEMPLATE,
          R"("timestamp": 1000, "severity": "INFO", "errors": [{"dropped_logs": {"count": 100}}])"),
      .dropped_logs = 100,
  });
  cases.emplace_back(ValidationTestCase{
      .input = fxl::StringPrintf(META_TEMPLATE,
                                 R"(
              "timestamp": 1000,
              "severity": "INFO",
              "errors": [
                {"dropped_logs": {"count": 100}},
                {"dropped_logs": {"count": 200}}
              ])"),
      .dropped_logs = 300,
  });

  RunValidationCases(std::move(cases));
}

TEST(LogMessage, MissingRootMemberOutOfBoundsRead) {
  constexpr char kSampleJson[] = R"JSON([
    {
      "metadata": {
        "timestamp": 1000,
        "severity": "INFO"
      },
      "payload": {
        "not_root": {}
      }
    }
  ])JSON";

  auto results = ConvertFormattedContentToLogMessages(JsonFormattedContent(kSampleJson));
  ASSERT_TRUE(results.is_ok());
  ASSERT_EQ(1u, results.value().size());
  ASSERT_TRUE(results.value()[0].is_error());
  EXPECT_EQ("Expected payload.root to be an object if present", results.value()[0].error());
}

TEST(LogMessage, MissingMessageMemberOutOfBoundsRead) {
  constexpr char kSampleJson[] = R"JSON([
    {
      "metadata": {
        "timestamp": 1000,
        "severity": "INFO"
      },
      "payload": {
        "root": {
          "not_message": {}
        }
      }
    }
  ])JSON";

  auto results = ConvertFormattedContentToLogMessages(JsonFormattedContent(kSampleJson));
  ASSERT_TRUE(results.is_ok());
  ASSERT_EQ(1u, results.value().size());
  ASSERT_TRUE(results.value()[0].is_error());
  EXPECT_EQ("Expected payload.root.message to be an object if present", results.value()[0].error());
}

class TestBatchIteratorEventHandler
    : public fidl::AsyncEventHandler<fuchsia_diagnostics::BatchIterator> {
 public:
  TestBatchIteratorEventHandler() : status_(ZX_OK) {}

  void on_fidl_error(fidl::UnbindInfo error) override { status_ = error.status(); }

  void handle_unknown_event(
      fidl::UnknownEventMetadata<fuchsia_diagnostics::BatchIterator> metadata) override {}

  zx_status_t status() const { return status_; }

 private:
  zx_status_t status_;
};

TEST(LogMessage, MoveConstructAndAssignLogBatchIterator) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  FakeBatchIterator fake_iterator;
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_diagnostics::BatchIterator>::Create();
  fidl::ServerBinding<fuchsia_diagnostics::BatchIterator> binding(
      loop.dispatcher(), std::move(server_end), &fake_iterator, fidl::kIgnoreBindingClosure);

  auto iterator1 = std::make_unique<diagnostics::fidl_log_message_parser::LogBatchIterator>(
      std::move(client_end), fuchsia_diagnostics::Format::kFxt, loop.dispatcher());

  // Move-construct
  diagnostics::fidl_log_message_parser::LogBatchIterator iterator2 = std::move(*iterator1);
  iterator1.reset();

  // Move-assign
  diagnostics::fidl_log_message_parser::LogBatchIterator iterator3(
      fidl::ClientEnd<fuchsia_diagnostics::BatchIterator>{}, fuchsia_diagnostics::Format::kFxt,
      loop.dispatcher());
  iterator3 = std::move(iterator2);
}

TEST(LogMessage, GetNextDomainError) {
  LogBatchIteratorTestHelper helper;
  helper.SetError(fuchsia_diagnostics::ReaderError::kIo);

  auto result = helper.GetNext();
  EXPECT_TRUE(result.is_error());
}

TEST(LogMessage, ChannelClosureInvokesEventHandler) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  FakeBatchIterator fake_iterator;
  TestBatchIteratorEventHandler event_handler;
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_diagnostics::BatchIterator>::Create();
  std::optional<fidl::ServerBinding<fuchsia_diagnostics::BatchIterator>> binding(
      std::in_place, loop.dispatcher(), std::move(server_end), &fake_iterator,
      fidl::kIgnoreBindingClosure);

  diagnostics::fidl_log_message_parser::LogBatchIterator iterator(
      std::move(client_end), fuchsia_diagnostics::Format::kFxt, loop.dispatcher(), &event_handler);

  binding.reset();
  bool callback_called = false;
  iterator.GetNext([&](auto result) { callback_called = true; });
  loop.RunUntilIdle();

  // The error is reported to the event handler, not the GetNext callback.
  EXPECT_FALSE(callback_called);
  EXPECT_EQ(ZX_ERR_PEER_CLOSED, event_handler.status());
}

TEST(LogMessage, DestroyFromCallbackOnTeardown) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  FakeBatchIterator fake_iterator;
  fake_iterator.HoldCompleter(true);

  auto [client_end, server_end] = fidl::Endpoints<fuchsia_diagnostics::BatchIterator>::Create();
  std::optional<fidl::ServerBinding<fuchsia_diagnostics::BatchIterator>> binding(
      std::in_place, loop.dispatcher(), std::move(server_end), &fake_iterator,
      fidl::kIgnoreBindingClosure);

  auto iterator = std::make_shared<diagnostics::fidl_log_message_parser::LogBatchIterator>(
      std::move(client_end), fuchsia_diagnostics::Format::kFxt, loop.dispatcher());
  std::weak_ptr<diagnostics::fidl_log_message_parser::LogBatchIterator> weak_iterator = iterator;

  // Issue a pending GetNext whose callback holds the last strong reference to the
  // LogBatchIterator (with no event_handler, matching debug_agent's TestLauncher), then close the
  // server channel so releasing the callback destroys the LogBatchIterator during FIDL teardown.
  bool callback_called = false;
  iterator->GetNext([keep_alive = iterator, &callback_called](auto) { callback_called = true; });
  iterator.reset();
  loop.RunUntilIdle();
  EXPECT_FALSE(weak_iterator.expired());

  // Close binding before dropping held completers so the server sends PEER_CLOSED cleanly.
  binding.reset();
  fake_iterator.ClearHeld();
  loop.RunUntilIdle();

  // The LogBatchIterator was destroyed during teardown without the callback being called (or
  // crashing on a use-after-free).
  EXPECT_FALSE(callback_called);
  EXPECT_TRUE(weak_iterator.expired());
}

TEST(LogMessage, DecodeFxtVmo) {
  constexpr zx::duration kTimestamp = zx::sec(1234);
  auto vmo = diagnostics::fidl_log_message_parser::testing::BuildFxtVmo(
      zx::basic_time<ZX_CLOCK_BOOT>(kTimestamp.get()), FUCHSIA_LOG_INFO, "hello fxt world", 123,
      456, {"tag1", "tag2"});
  ASSERT_TRUE(vmo.is_valid());

  auto messages = ConvertFormattedContentToLogMessages(FormattedContent::WithFxt(std::move(vmo)));
  ASSERT_TRUE(messages.is_ok());
  ASSERT_EQ(1u, messages.value().size());
  ASSERT_TRUE(messages.value()[0].is_ok());

  const auto& log = messages.value()[0].value();
  EXPECT_EQ(123u, log.pid());
  EXPECT_EQ(456u, log.tid());
  EXPECT_EQ(kTimestamp.get(), log.time().get());
  EXPECT_EQ(FUCHSIA_LOG_INFO, log.severity());
  EXPECT_EQ("hello fxt world", log.msg());
  ASSERT_EQ(2u, log.tags().size());
  EXPECT_EQ("tag1", log.tags()[0]);
  EXPECT_EQ("tag2", log.tags()[1]);
}

// Validates https://fxbug.dev/586102325 (parent https://fxbug.dev/306199648):
// production FXT decoding preserves wire insertion order for structured KVPs
// (including non-alphabetical keys, mixed types, and duplicate keys).
TEST(LogMessage, FxtPreservesKvpOrdering) {
  constexpr zx::duration kTimestamp = zx::sec(1234);
  auto make_vmo = [&]() {
    fuchsia_logging::LogBuffer buffer;
    buffer.BeginRecord(zx::basic_time<ZX_CLOCK_BOOT>(kTimestamp.get()), FUCHSIA_LOG_INFO,
                       std::nullopt, 0, "ordered fxt", 0, 123, 456);
    buffer.WriteKeyValue("zebra", "last");
    buffer.WriteKeyValue("alpha", int64_t{-42});
    buffer.WriteKeyValue("middle", uint64_t{7});
    buffer.WriteKeyValue("beta", true);
    buffer.WriteKeyValue("alpha", int64_t{99});
    return BuildFxtVmoFromBuffer(buffer);
  };
  constexpr std::string_view kExpectedMsg =
      "ordered fxt zebra=\"last\" alpha=-42 middle=7 beta=true alpha=99";

  // Verify production ConvertFormattedContentToLogMessages directly (bypassing the test shim).
  auto direct_messages = diagnostics::fidl_log_message_parser::ConvertFormattedContentToLogMessages(
      FormattedContent::WithFxt(make_vmo()));
  ASSERT_TRUE(direct_messages.is_ok());
  ASSERT_EQ(1u, direct_messages.value().size());
  ASSERT_TRUE(direct_messages.value()[0].is_ok());
  EXPECT_EQ(kExpectedMsg, direct_messages.value()[0].value().msg());

  // Also verify through LogBatchIterator (Format::kFxt).
  LogBatchIteratorTestHelper helper;
  std::vector<FormattedContent> batch;
  batch.push_back(FormattedContent::WithFxt(make_vmo()));
  helper.SetBatch(std::move(batch));
  auto batch_messages = helper.GetNext();
  ASSERT_TRUE(batch_messages.is_ok());
  ASSERT_EQ(1u, batch_messages.value().size());
  ASSERT_TRUE(batch_messages.value()[0].is_ok());
  EXPECT_EQ(kExpectedMsg, batch_messages.value()[0].value().msg());
}

// Validates https://fxbug.dev/586102325 (parent https://fxbug.dev/306199648):
// legacy JSON test transcoding preserves JSON member order for structured KVPs,
// while production ConvertFormattedContentToLogMessages is FXT-only.
TEST(LogMessage, JsonKvpOrdering) {
  constexpr std::string_view kOrderedJson = R"JSON(
[
  {
    "metadata": {
      "timestamp": 1000,
      "severity": "INFO",
      "pid": 200,
      "tid": 300,
      "tags": ["a"]
    },
    "payload": {
      "root": {
        "message": {
          "value": "ordered json"
        },
        "keys": {
          "zebra": "last",
          "alpha": -42,
          "middle": 7,
          "beta": 3.5
        }
      }
    }
  }
]
)JSON";

  // Legacy JSON path via ConvertFormattedContentForTest preserves JSON member order.
  // Note: test_support.cc supports int64, uint64, double, and string JSON KVP values
  // (booleans format as <unknown>), so we exercise string, int, uint, and double here.
  auto test_messages = ConvertFormattedContentToLogMessages(JsonFormattedContent(kOrderedJson));
  ASSERT_TRUE(test_messages.is_ok());
  ASSERT_EQ(1u, test_messages.value().size());
  ASSERT_TRUE(test_messages.value()[0].is_ok());
  EXPECT_EQ("ordered json zebra=\"last\" alpha=-42 middle=7 beta=3.5",
            test_messages.value()[0].value().msg());

  // Production fidl_log_message_parser no longer accepts JSON FormattedContent.
  auto prod_messages = diagnostics::fidl_log_message_parser::ConvertFormattedContentToLogMessages(
      JsonFormattedContent(kOrderedJson));
  ASSERT_TRUE(prod_messages.is_error());
  EXPECT_EQ("Expected FXT content", prod_messages.error());
}

}  // namespace
