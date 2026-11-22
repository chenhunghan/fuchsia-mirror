// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZXTEST_CPP_STREAMS_HELPER_H_
#define ZXTEST_CPP_STREAMS_HELPER_H_

#include <memory>
#include <ostream>
#include <span>
#include <sstream>
#include <string>
#include <string_view>
#include <utility>

#include <zxtest/base/assertion.h>
#include <zxtest/base/types.h>
#include <zxtest/cpp/internal.h>

#define LIB_ZXTEST_RETURN_TAG_true return zxtest::internal::Tag{}&

#define LIB_ZXTEST_RETURN_TAG_false

#define LIB_ZXTEST_RETURN_TAG(val) LIB_ZXTEST_RETURN_TAG_##val

namespace zxtest::internal {

struct Tag {};

class StreamableBase {
 public:
  explicit StreamableBase(zxtest::SourceLocation location);
  ~StreamableBase();

  // Lower precedence operator that returns void, such that the following
  // expressions are valid in functions that return void:
  //
  //  return Tag{} & StreamableBase{};
  //  return Tag{} & StreamableBase{} << "Stream operators are higher precedence than &";
  //
  friend void operator&(Tag, const StreamableBase&) {}

  template <typename T>
  StreamableBase& operator<<(T t) {
    EnsureStream() << t;
    return *this;
  }

  StreamableBase& operator<<(std::ostream& (*manip)(std::ostream&)) {
    manip(EnsureStream());
    return *this;
  }

 protected:
  std::ostream& EnsureStream();
  std::string GetText() const;
  const zxtest::SourceLocation& location() const { return location_; }

 private:
  std::unique_ptr<std::stringstream> stream_;
  const zxtest::SourceLocation location_;
};

class StreamableFail : public StreamableBase {
 public:
  StreamableFail(zxtest::SourceLocation location, bool is_fatal);
  StreamableFail(zxtest::SourceLocation location, bool is_fatal,
                 std::span<zxtest::Message*> traces);
  ~StreamableFail();

 private:
  bool is_fatal_ = false;
  std::span<zxtest::Message*> traces_;
  bool has_explicit_traces_ = false;
};

class StreamableAssertion : public StreamableBase {
 public:
  StreamableAssertion(std::string_view actual_val, std::string_view expected_val,
                      const char* actual_symbol, const char* expected_symbol,
                      zxtest::SourceLocation location, bool is_fatal,
                      std::span<zxtest::Message*> traces);
  ~StreamableAssertion();

 private:
  const std::string actual_value_;
  const std::string expected_value_;
  const char* actual_symbol_;
  const char* expected_symbol_;
  bool is_fatal_ = false;
  std::span<zxtest::Message*> traces_;
};

class StreamableSkip : public StreamableBase {
 public:
  explicit StreamableSkip(zxtest::SourceLocation location);
  ~StreamableSkip();
};

std::unique_ptr<StreamableAssertion> CreateStreamableAssertion(
    const char* actual_symbol, FormatFn actual_printer, const char* expected_symbol,
    FormatFn expected_printer, zxtest::SourceLocation location, bool is_fatal);

std::unique_ptr<StreamableAssertion> EvaluateBoolConditionForStream(bool actual, bool expected,
                                                                    const char* actual_symbol,
                                                                    const char* expected_symbol,
                                                                    zxtest::SourceLocation location,
                                                                    bool is_fatal);

std::unique_ptr<StreamableAssertion> EvaluateStatusConditionForStream(
    zx_status_t actual, zx_status_t expected, bool expect_eq, const char* actual_symbol,
    const char* expected_symbol, zxtest::SourceLocation location, bool is_fatal);

std::unique_ptr<StreamableAssertion> EvaluateBytesConditionForStream(
    const void* actual, const void* expected, size_t size, bool expect_eq,
    const char* actual_symbol, const char* expected_symbol, zxtest::SourceLocation location,
    bool is_fatal);

// Evaluates a condition and returns nullptr if it is satisfied. If it is not, creates a
// StreamableAssertion that notifies the global runner instance on destruction.
template <typename Actual, typename Expected, typename CompareOp, typename PrintActual,
          typename PrintExpected>
std::unique_ptr<StreamableAssertion> EvaluateConditionForStream(
    const Actual& actual, const Expected& expected, const char* actual_symbol,
    const char* expected_symbol, zxtest::SourceLocation location, bool is_fatal,
    const CompareOp& compare, const PrintActual& print_actual,
    const PrintExpected& print_expected) {
  if (compare(actual, expected)) {
    return nullptr;
  }

  return CreateStreamableAssertion(actual_symbol, FormatFn(actual, print_actual), expected_symbol,
                                   FormatFn(expected, print_expected), location, is_fatal);
}

}  // namespace zxtest::internal

#endif  // ZXTEST_CPP_STREAMS_HELPER_H_
