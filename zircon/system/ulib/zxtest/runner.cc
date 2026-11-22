// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>
#include <zircon/assert.h>

#include <cstdarg>
#include <cstring>
#include <memory>
#include <sstream>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <zxtest/base/json-reporter.h>
#include <zxtest/base/log-sink.h>
#include <zxtest/base/message.h>
#include <zxtest/base/runner.h>
#include <zxtest/cpp/internal.h>
#include <zxtest/cpp/scoped_trace.h>
#include <zxtest/cpp/streams_helper.h>

namespace zxtest {

const Runner::Options Runner::kDefaultOptions;

thread_local std::vector<std::unique_ptr<zxtest::Message>> Runner::owned_scoped_traces_;
thread_local std::vector<zxtest::Message*> Runner::scoped_traces_;

namespace internal {

TestDriverImpl::TestDriverImpl() = default;
TestDriverImpl::~TestDriverImpl() = default;

void TestDriverImpl::Skip() {
  skipped_ = true;
  // A skip after a failure leaves the test failed.
  TestStatus passed = TestStatus::kPassed;
  status_.compare_exchange_strong(passed, TestStatus::kSkipped);
}

bool TestDriverImpl::Continue() const { return !current_test_has_fatal_failures_ && !skipped_; }

void TestDriverImpl::OnTestStart(const TestCase& test_case, const TestInfo& test_info) {
  status_ = TestStatus::kPassed;
}

void TestDriverImpl::OnTestSkip(const TestCase& test_case, const TestInfo& test_info) { Reset(); }

void TestDriverImpl::OnTestSuccess(const TestCase& test_case, const TestInfo& test_info) {
  Reset();
}

void TestDriverImpl::OnTestFailure(const TestCase& test_case, const TestInfo& test_info) {
  Reset();
}

void TestDriverImpl::OnTestCaseEnd(const TestCase& test_case) {
  // A skip or fatal failure in SetUpTestSuite or TearDownTestSuite ends only
  // its own test suite.
  Reset();
}

void TestDriverImpl::OnAssertion(const Assertion& assertion) {
  if (asserts_enabled_) {
    status_ = TestStatus::kFailed;
    current_test_has_any_failures_ = true;
    if (assertion.is_fatal()) {
      current_test_has_fatal_failures_ = true;
    }
    had_any_failures_ = true;
  }
}

void TestDriverImpl::Reset() {
  current_test_has_fatal_failures_ = false;
  current_test_has_any_failures_ = false;
  skipped_ = false;
  status_ = TestStatus::kPassed;
}

}  // namespace internal

Runner::Runner(Reporter&& reporter) : reporter_(std::move(reporter)) {
  event_broadcaster_.Subscribe(&test_driver_);
  event_broadcaster_.Subscribe(&reporter_);
}
Runner::~Runner() = default;

TestRef Runner::RegisterTest(std::string_view test_case_name, std::string_view test_name,
                             const SourceLocation& location, internal::TestFactory factory,
                             internal::SetUpTestCaseFn set_up,
                             internal::TearDownTestCaseFn tear_down) {
  ZX_ASSERT_MSG(!test_case_name.empty(), "test_case_name cannot be an empty string.");
  ZX_ASSERT_MSG(!test_name.empty(), "test_name cannot be an empty string.");

  // TODO(gevalentino): replace by std::find.
  TestCase* target_test_case = nullptr;
  TestRef test_ref;

  for (auto& test_case : test_cases_) {
    if (test_case.name() == test_case_name) {
      target_test_case = &test_case;
      break;
    }
    test_ref.test_case_index++;
  }

  // If there is no existing test case with |test_case_name|, create a new one.
  if (target_test_case == nullptr) {
    test_cases_.push_back(TestCase(test_case_name, std::move(set_up), std::move(tear_down)));
    target_test_case = &test_cases_[test_cases_.size() - 1];
  }

  test_ref.test_index = target_test_case->TestCount();
  ZX_ASSERT_MSG(target_test_case->RegisterTest(test_name, location, std::move(factory)),
                "Test Registration failed.");
  summary_.registered_test_count++;
  summary_.registered_test_case_count = test_cases_.size();

  return test_ref;
}

int Runner::Run(const Runner::Options& options) {
  is_running_ = true;
  RegisterParameterizedTests();

  options_ = &options;
  auto reset_options = fit::defer([this]() { options_ = nullptr; });
  summary_.total_iterations = options.repeat;
  EnforceOptions(options);

  event_broadcaster_.OnProgramStart(*this);
  bool end_execution = false;
  for (int i = 0; (i < options.repeat || options.repeat == -1) && !end_execution; ++i) {
    event_broadcaster_.OnIterationStart(*this, i);
    event_broadcaster_.OnEnvironmentSetUp(*this);
    // Set them up in order.
    for (auto& environment : environments_) {
      environment->SetUp();
    }

    for (auto& test_case : test_cases_) {
      if (options.shuffle) {
        test_case.Shuffle(options.seed);
      }

      test_case.Run(&event_broadcaster_, &test_driver_);

      // If there was any kind of failure, we should stop executing any other
      // test case and just finish. TearDown do get called, this is treated
      // as if everything ended here.
      if ((options.break_on_failure && test_driver_.HadAnyFailures()) || fatal_error_) {
        end_execution = true;
        break;
      }

      if (options.shuffle) {
        test_case.UnShuffle();
      }
    }
    event_broadcaster_.OnEnvironmentTearDown(*this);

    // Tear them down in reverse order
    for (size_t i = environments_.size(); i > 0; --i) {
      environments_[i - 1]->TearDown();
    }
    event_broadcaster_.OnIterationEnd(*this, i);
  }
  event_broadcaster_.OnProgramEnd(*this);
  is_running_ = false;

  return test_driver_.HadAnyFailures() ? -1 : 0;
}

void Runner::List(const Runner::Options& options) {
  RegisterParameterizedTests();

  options_ = &options;
  auto reset_options = fit::defer([this]() { options_ = nullptr; });
  summary_.total_iterations = options.repeat;
  EnforceOptions(options);

  auto* maybe_json_reporter = json_reporter_.get();

  if (maybe_json_reporter) {
    maybe_json_reporter->set_list_mode(true);
    maybe_json_reporter->OnProgramStart(*this);
  }

  for (const auto& test_case : test_cases_) {
    if (maybe_json_reporter) {
      maybe_json_reporter->OnTestCaseStart(test_case);
    }
    if (test_case.MatchingTestCount() != 0) {
      reporter_.mutable_log_sink()->Write("%s\n", test_case.name().c_str());
      for (size_t i = 0; i < test_case.MatchingTestCount(); ++i) {
        auto& info = test_case.GetMatchingTestInfo(i);
        if (maybe_json_reporter) {
          maybe_json_reporter->OnTestStart(test_case, info);
          maybe_json_reporter->OnTestSuccess(test_case, info);
        }
        reporter_.mutable_log_sink()->Write("  .%s\n", info.name().c_str());
      }
    }
    if (maybe_json_reporter) {
      maybe_json_reporter->OnTestCaseEnd(test_case);
    }
  }

  if (maybe_json_reporter) {
    maybe_json_reporter->OnProgramEnd(*this);
  }
}

void Runner::EnforceOptions(const Runner::Options& options) {
  summary_.active_test_count = 0;
  summary_.active_test_case_count = 0;
  std::string filter_pattern = options.filter;
  const FilterOp filter_op = {.pattern = filter_pattern, .run_disabled = options.run_disabled};
  for (auto& test_case : test_cases_) {
    // TODO(gevalentino): replace with filter function.
    test_case.Filter(filter_op);
    if (test_case.MatchingTestCount() > 0) {
      summary_.active_test_case_count++;
      summary_.active_test_count += test_case.MatchingTestCount();
      test_case.SetReturnOnFailure(options.break_on_failure);
    }
  }

  // Set up JSON output if enabled.
  if (!options.output_path.empty()) {
    auto view = std::string_view(options.output_path);
    if (view.starts_with("json:")) {
      std::string path(view.substr(5));
      FILE* f = fopen(path.c_str(), "w");
      if (f == nullptr) {
        reporter_.mutable_log_sink()->Write("ERROR: Failed to open output file %s\n",
                                            options.output_path.c_str());
      } else {
        auto new_sink = std::make_unique<FileLogSink>(f, [](FILE* f) { fclose(f); });
        json_reporter_ = std::make_unique<internal::JsonReporter>(std::move(new_sink));
        event_broadcaster_.Subscribe(json_reporter_.get());
      }
    }
  }
}

void Runner::NotifyAssertion(const Assertion& assertion) {
  event_broadcaster_.OnAssertion(assertion);
}

void Runner::SkipCurrent(const Message& message) {
  event_broadcaster_.OnMessage(message);
  test_driver_.Skip();
}

void Runner::PushTrace(SourceLocation location, std::string_view message) {
  auto owned = std::make_unique<zxtest::Message>(message, location);
  scoped_traces_.push_back(owned.get());
  owned_scoped_traces_.push_back(std::move(owned));
}

void Runner::PopTrace() {
  zxtest::Message* popped = scoped_traces_.back();
  scoped_traces_.pop_back();
  if (!owned_scoped_traces_.empty() && owned_scoped_traces_.back().get() == popped) {
    owned_scoped_traces_.pop_back();
  }
}

ScopedTrace::ScopedTrace(SourceLocation location, std::string_view message) {
  Runner::PushTrace(location, message);
}

ScopedTrace::~ScopedTrace() { Runner::PopTrace(); }

namespace internal {

void CheckRunning() {
  ZX_ASSERT_MSG(Runner::GetInstance()->IsRunning(), "See Context Check in README.");
}

void ReportConditionFailure(const char* actual_symbol, FormatFn actual_printer,
                            const char* expected_symbol, FormatFn expected_printer,
                            FormatFn description, SourceLocation location, bool is_fatal) {
  Runner* runner = Runner::GetInstance();
  std::string actual_value = actual_printer();
  std::string expected_value = expected_printer();
  std::string desc = description();
  Assertion assertion(desc, expected_symbol, expected_value, actual_symbol, actual_value, location,
                      is_fatal, runner->GetScopedTraces());
  runner->NotifyAssertion(assertion);
}

bool EvaluateBoolCondition(bool actual, bool expected, const char* actual_symbol,
                           const char* expected_symbol, SourceLocation location, bool is_fatal,
                           FormatFn description) {
  if (actual == expected) {
    return true;
  }
  Runner* runner = Runner::GetInstance();
  std::string actual_value = PrintValue(actual);
  std::string expected_value = PrintValue(expected);
  std::string desc = description();
  Assertion assertion(desc, expected_symbol, expected_value, actual_symbol, actual_value, location,
                      is_fatal, runner->GetScopedTraces());
  runner->NotifyAssertion(assertion);
  return false;
}

bool EvaluateStatusCondition(zx_status_t actual, zx_status_t expected, bool expect_eq,
                             const char* actual_symbol, const char* expected_symbol,
                             SourceLocation location, bool is_fatal, FormatFn description) {
  if ((actual == expected) == expect_eq) {
    return true;
  }
  Runner* runner = Runner::GetInstance();
  std::string actual_value = PrintStatus(actual);
  std::string expected_value = PrintStatus(expected);
  std::string desc = description();
  Assertion assertion(desc, expected_symbol, expected_value, actual_symbol, actual_value, location,
                      is_fatal, runner->GetScopedTraces());
  runner->NotifyAssertion(assertion);
  return false;
}

bool EvaluateBytesCondition(const void* actual, const void* expected, size_t size, bool expect_eq,
                            const char* actual_symbol, const char* expected_symbol,
                            SourceLocation location, bool is_fatal, FormatFn description) {
  const bool equal = size == 0 || memcmp(actual, expected, size) == 0;
  if (equal == expect_eq) {
    return true;
  }
  Runner* runner = Runner::GetInstance();
  std::string actual_value = ToHex(actual, size);
  std::string expected_value = ToHex(expected, size);
  std::string desc = description();
  Assertion assertion(desc, expected_symbol, expected_value, actual_symbol, actual_value, location,
                      is_fatal, runner->GetScopedTraces());
  runner->NotifyAssertion(assertion);
  return false;
}

void FailCurrentTest(SourceLocation location, bool is_fatal, std::string_view desc, const char* fmt,
                     ...) {
  va_list args;
  va_start(args, fmt);
  std::string message = FormatDescriptionV(desc, fmt, args);
  va_end(args);
  Runner* runner = Runner::GetInstance();
  Assertion assertion(message, location, is_fatal, runner->GetScopedTraces());
  runner->NotifyAssertion(assertion);
}

void SkipCurrentTest(SourceLocation location, std::string_view desc, const char* fmt, ...) {
  va_list args;
  va_start(args, fmt);
  std::string text = FormatDescriptionV(desc, fmt, args);
  va_end(args);
  Message message(text, location);
  Runner::GetInstance()->SkipCurrent(message);
}

StreamableBase::StreamableBase(zxtest::SourceLocation location) : location_(location) {
  CheckRunning();
}

StreamableBase::~StreamableBase() = default;

std::ostream& StreamableBase::EnsureStream() {
  if (!stream_) {
    stream_ = std::make_unique<std::stringstream>();
  }
  return *stream_;
}

std::string StreamableBase::GetText() const { return stream_ ? stream_->str() : std::string{}; }

StreamableFail::StreamableFail(zxtest::SourceLocation location, bool is_fatal)
    : StreamableBase(location), is_fatal_(is_fatal) {}

StreamableFail::StreamableFail(zxtest::SourceLocation location, bool is_fatal,
                               std::span<zxtest::Message*> traces)
    : StreamableBase(location), is_fatal_(is_fatal), traces_(traces), has_explicit_traces_(true) {}

StreamableFail::~StreamableFail() {
  Runner* runner = Runner::GetInstance();
  std::span<zxtest::Message*> traces = has_explicit_traces_ ? traces_ : runner->GetScopedTraces();
  std::string desc = GetText();
  Assertion assertion(desc, location(), is_fatal_, traces);
  runner->NotifyAssertion(assertion);
}

StreamableAssertion::StreamableAssertion(std::string_view actual_val, std::string_view expected_val,
                                         const char* actual_symbol, const char* expected_symbol,
                                         zxtest::SourceLocation location, bool is_fatal,
                                         std::span<zxtest::Message*> traces)
    : StreamableBase(location),
      actual_value_(actual_val),
      expected_value_(expected_val),
      actual_symbol_(actual_symbol),
      expected_symbol_(expected_symbol),
      is_fatal_(is_fatal),
      traces_(traces) {}

StreamableAssertion::~StreamableAssertion() {
  std::string desc = GetText();
  Assertion assertion(desc, expected_symbol_, expected_value_, actual_symbol_, actual_value_,
                      location(), is_fatal_, traces_);
  Runner::GetInstance()->NotifyAssertion(assertion);
}

StreamableSkip::StreamableSkip(zxtest::SourceLocation location) : StreamableBase(location) {}

StreamableSkip::~StreamableSkip() {
  std::string text = GetText();
  Message message(text, location());
  Runner::GetInstance()->SkipCurrent(message);
}

std::unique_ptr<StreamableAssertion> CreateStreamableAssertion(
    const char* actual_symbol, FormatFn actual_printer, const char* expected_symbol,
    FormatFn expected_printer, zxtest::SourceLocation location, bool is_fatal) {
  std::string actual_value = actual_printer();
  std::string expected_value = expected_printer();
  return std::make_unique<StreamableAssertion>(actual_value, expected_value, actual_symbol,
                                               expected_symbol, location, is_fatal,
                                               Runner::GetInstance()->GetScopedTraces());
}

std::unique_ptr<StreamableAssertion> EvaluateBoolConditionForStream(bool actual, bool expected,
                                                                    const char* actual_symbol,
                                                                    const char* expected_symbol,
                                                                    zxtest::SourceLocation location,
                                                                    bool is_fatal) {
  CheckRunning();
  if (actual == expected) {
    return nullptr;
  }
  std::string actual_value = PrintValue(actual);
  std::string expected_value = PrintValue(expected);
  return std::make_unique<StreamableAssertion>(actual_value, expected_value, actual_symbol,
                                               expected_symbol, location, is_fatal,
                                               Runner::GetInstance()->GetScopedTraces());
}

std::unique_ptr<StreamableAssertion> EvaluateStatusConditionForStream(
    zx_status_t actual, zx_status_t expected, bool expect_eq, const char* actual_symbol,
    const char* expected_symbol, zxtest::SourceLocation location, bool is_fatal) {
  CheckRunning();
  if ((actual == expected) == expect_eq) {
    return nullptr;
  }
  std::string actual_value = PrintStatus(actual);
  std::string expected_value = PrintStatus(expected);
  return std::make_unique<StreamableAssertion>(actual_value, expected_value, actual_symbol,
                                               expected_symbol, location, is_fatal,
                                               Runner::GetInstance()->GetScopedTraces());
}

std::unique_ptr<StreamableAssertion> EvaluateBytesConditionForStream(
    const void* actual, const void* expected, size_t size, bool expect_eq,
    const char* actual_symbol, const char* expected_symbol, zxtest::SourceLocation location,
    bool is_fatal) {
  CheckRunning();
  const bool equal = size == 0 || memcmp(actual, expected, size) == 0;
  if (equal == expect_eq) {
    return nullptr;
  }
  std::string actual_value = ToHex(actual, size);
  std::string expected_value = ToHex(expected, size);
  return std::make_unique<StreamableAssertion>(actual_value, expected_value, actual_symbol,
                                               expected_symbol, location, is_fatal,
                                               Runner::GetInstance()->GetScopedTraces());
}

}  // namespace internal

Runner* Runner::GetInstance() {
  static Runner runner = Runner(Reporter(std::make_unique<FileLogSink>(stdout)));
  return &runner;
}

int RunAllTests(int argc, char** argv) {
  std::vector<std::string> errors;
  LogSink* log_sink = Runner::GetInstance()->mutable_reporter()->mutable_log_sink();
  Runner::Options options = Runner::Options::FromArgs(argc, argv, &errors);

  if (!errors.empty()) {
    for (const auto& error : errors) {
      log_sink->Write("%s\n", error.c_str());
    }
    options.help = true;
  }

  // Errors will always set help to true.
  if (options.help) {
    Runner::Options::Usage(argv[0], log_sink);
    return errors.empty() ? 0 : 1;
  }

  if (options.list) {
    Runner::GetInstance()->List(options);
    return 0;
  }

  return Runner::GetInstance()->Run(options);
}

namespace {

// Matches a single gTest glob pattern, where '?' matches any one character
// and '*' matches any sequence of characters.
bool MatchPattern(std::string_view pattern, std::string_view str) {
  if (pattern.empty()) {
    return str.empty();
  }
  switch (pattern.front()) {
    case '?':
      return !str.empty() && MatchPattern(pattern.substr(1), str.substr(1));
    case '*':
      return (!str.empty() && MatchPattern(pattern, str.substr(1))) ||
             MatchPattern(pattern.substr(1), str);
    default:
      return !str.empty() && str.front() == pattern.front() &&
             MatchPattern(pattern.substr(1), str.substr(1));
  }
}

// Matches a ':'-separated list of patterns.
bool MatchPatterns(std::string_view patterns, std::string_view str) {
  while (true) {
    size_t end = patterns.find(':');
    if (MatchPattern(patterns.substr(0, end), str)) {
      return true;
    }
    if (end == std::string_view::npos) {
      return false;
    }
    patterns.remove_prefix(end + 1);
  }
}

}  // namespace

bool FilterOp::operator()(std::string_view test_case, std::string_view test) const {
  if (!run_disabled) {
    if (test_case.starts_with(kDisabledTestPrefix) || test.starts_with(kDisabledTestPrefix)) {
      return false;
    }
  }

  std::string full_test_name{test_case};
  full_test_name += '.';
  full_test_name += test;

  // The filter is "POSITIVE-NEGATIVE", where either part can be empty.  An
  // empty (or all ':') positive part matches everything.
  std::string_view positive = pattern, negative;
  if (size_t dash = positive.find('-'); dash != std::string_view::npos) {
    negative = positive.substr(dash + 1);
    positive = positive.substr(0, dash);
  }
  if (positive.find_first_not_of(':') == std::string_view::npos) {
    positive = {};
  }
  return (positive.empty() || MatchPatterns(positive, full_test_name)) &&
         (negative.empty() || !MatchPatterns(negative, full_test_name));
}

}  // namespace zxtest
