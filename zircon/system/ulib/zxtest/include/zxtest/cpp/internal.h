// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZXTEST_CPP_INTERNAL_H_
#define ZXTEST_CPP_INTERNAL_H_

#include <zircon/compiler.h>

#include <concepts>
#include <cstdint>
#include <cstring>
#include <memory>
#include <string>
#include <string_view>
#include <type_traits>

#include <zxtest/base/assertion.h>
#include <zxtest/base/runner.h>
#include <zxtest/base/types.h>

#define LIB_ZXTEST_CHECK_RUNNING() zxtest::internal::CheckRunning()

namespace zxtest::internal {

void CheckRunning();

// Lightweight type-erased callback for formatting values and descriptions only on assertion
// failure, avoiding std::string temporaries and address-taken scalar allocas in caller frames.
class FormatFn {
 public:
  template <typename F>
    requires(!std::same_as<std::remove_cvref_t<F>, FormatFn> &&
             std::is_empty_v<std::remove_cvref_t<F>> &&
             std::is_default_constructible_v<std::remove_cvref_t<F>>)
  constexpr explicit(false) FormatFn(F&&) noexcept
      : ctx_{.bits = 0}, fn_([](Context) -> std::string { return std::remove_cvref_t<F>{}(); }) {}

  template <typename F>
    requires(!std::same_as<std::remove_cvref_t<F>, FormatFn> &&
             !(std::is_empty_v<std::remove_cvref_t<F>> &&
               std::is_default_constructible_v<std::remove_cvref_t<F>>))
  explicit(false) FormatFn(const F& fn) noexcept
      : ctx_(Ref(fn)), fn_([](Context ctx) -> std::string { return Deref<F>(ctx)(); }) {}

  template <typename T, typename Printer>
    requires(std::is_empty_v<Printer> && std::is_scalar_v<T> && !std::is_volatile_v<T> &&
             sizeof(T) <= sizeof(uintptr_t))
  FormatFn(const T& value, const Printer&) noexcept
      : ctx_{.bits = PackScalar(value)},
        fn_([](Context ctx) -> std::string { return Printer{}(UnpackScalar<T>(ctx.bits)); }) {}

  template <typename T, typename Printer>
    requires(std::is_empty_v<Printer> &&
             !(std::is_scalar_v<T> && !std::is_volatile_v<T> && sizeof(T) <= sizeof(uintptr_t)))
  FormatFn(const T& value, const Printer&) noexcept
      : ctx_(Ref(value)),
        fn_([](Context ctx) -> std::string { return Printer{}(Deref<T>(ctx)); }) {}

  template <typename T, typename Printer>
    requires(!std::is_empty_v<Printer>)
  FormatFn(const T& value, const Printer& printer) noexcept
      : FormatFn([&] -> std::string { return printer(value); }) {}

  std::string operator()() const { return fn_(ctx_); }

 private:
  // Either the address of the callable or value to format, or the bits of a
  // small scalar value packed so that its address need not be taken.
  union Context {
    const void* ptr;
    uintptr_t bits;
  };

  template <typename T>
  static Context Ref(const T& object) noexcept {
    const volatile void* ptr = std::addressof(object);
    return {.ptr = const_cast<const void*>(ptr)};
  }

  template <typename T>
  static const T& Deref(Context ctx) noexcept {
    return *static_cast<const T*>(ctx.ptr);
  }

  template <typename T>
  static uintptr_t PackScalar(const T& value) noexcept {
    uintptr_t bits = 0;
    memcpy(&bits, static_cast<const void*>(&value), sizeof(T));
    return bits;
  }

  template <typename T>
  static T UnpackScalar(uintptr_t bits) noexcept {
    T value;
    memcpy(static_cast<void*>(&value), &bits, sizeof(T));
    return value;
  }

  Context ctx_;
  std::string (*fn_)(Context);
};

void ReportConditionFailure(const char* actual_symbol, FormatFn actual_printer,
                            const char* expected_symbol, FormatFn expected_printer,
                            FormatFn description, SourceLocation location, bool is_fatal);

bool EvaluateBoolCondition(bool actual, bool expected, const char* actual_symbol,
                           const char* expected_symbol, SourceLocation location, bool is_fatal,
                           FormatFn description);

bool EvaluateStatusCondition(zx_status_t actual, zx_status_t expected, bool expect_eq,
                             const char* actual_symbol, const char* expected_symbol,
                             SourceLocation location, bool is_fatal, FormatFn description);

bool EvaluateBytesCondition(const void* actual, const void* expected, size_t size, bool expect_eq,
                            const char* actual_symbol, const char* expected_symbol,
                            SourceLocation location, bool is_fatal, FormatFn description);

void FailCurrentTest(SourceLocation location, bool is_fatal, std::string_view desc, const char* fmt,
                     ...) __PRINTFLIKE(4, 5);

void SkipCurrentTest(SourceLocation location, std::string_view desc, const char* fmt, ...)
    __PRINTFLIKE(3, 4);

// Used for delegating an assertion evaluation to a lambda.
template <typename Actual, typename Expected, typename Comparer>
bool Compare(const Actual& actual, const Expected& expected, const Comparer& compare) {
  if constexpr (std::is_integral_v<Actual> && std::is_integral_v<Expected>) {
    return compare(static_cast<std::common_type_t<Actual, Expected>>(actual),
                   static_cast<std::common_type_t<Actual, Expected>>(expected));
  } else {
    return compare(actual, expected);
  }
}

// Alternative when there exists an implicit conversion to pointer, which allows
// VLAs to be compared.
template <typename Actual, typename Expected, typename Comparer>
bool Compare(const Actual* a, const Expected* e, const Comparer& compare) {
  return compare(a, e);
}

constexpr zx_status_t GetStatus(const std::convertible_to<zx_status_t> auto& status) {
  return status;
}

template <class T>
  requires requires(T obj) {
    { obj.status_value() } -> std::convertible_to<zx_status_t>;
  }
constexpr zx_status_t GetStatus(const T& obj) {
  return obj.status_value();
}

template <class T>
  requires requires(T obj) {
    { obj.status() } -> std::convertible_to<zx_status_t>;
  }
constexpr zx_status_t GetStatus(const T& obj) {
  return obj.status();
}

// Evaluates a condition and returns true if it is satisfied. If it is not, will create an assertion
// and notify the global runner instance.
template <typename Actual, typename Expected, typename CompareOp, typename PrintActual,
          typename PrintExpected, typename DescGenerator>
bool EvaluateStatusCondition(const Actual& actual, const Expected& expected,
                             const char* actual_symbol, const char* expected_symbol,
                             SourceLocation location, bool is_fatal,
                             const DescGenerator& description, const CompareOp& compare,
                             const PrintActual& print_actual, const PrintExpected& print_expected) {
  zx_status_t actual_status = GetStatus(actual);
  if (compare(actual_status, expected)) {
    return true;
  }

  ReportConditionFailure(actual_symbol, FormatFn(actual_status, print_actual), expected_symbol,
                         FormatFn(expected, print_expected), FormatFn(description), location,
                         is_fatal);
  return false;
}

// Evaluates a condition and returns true if it is satisfied. If it is not, will create an assertion
// and notify the global runner instance.
template <typename Actual, typename Expected, typename CompareOp, typename PrintActual,
          typename PrintExpected, typename DescGenerator>
bool EvaluateCondition(const Actual& actual, const Expected& expected, const char* actual_symbol,
                       const char* expected_symbol, SourceLocation location, bool is_fatal,
                       const DescGenerator& description, const CompareOp& compare,
                       const PrintActual& print_actual, const PrintExpected& print_expected) {
  if (compare(actual, expected)) {
    return true;
  }

  ReportConditionFailure(actual_symbol, FormatFn(actual, print_actual), expected_symbol,
                         FormatFn(expected, print_expected), FormatFn(description), location,
                         is_fatal);
  return false;
}

// Force Array to become a pointer, has no effect in pointers.
template <typename T>
const T* ToPointer(const T* value) {
  return value;
}

}  // namespace zxtest::internal

#endif  // ZXTEST_CPP_INTERNAL_H_
