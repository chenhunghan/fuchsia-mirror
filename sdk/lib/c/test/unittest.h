// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_C_TEST_UNITTEST_H_
#define LIB_C_TEST_UNITTEST_H_

#ifdef __Fuchsia__
#include <zircon/status.h>
#include <zircon/types.h>
#endif

#include <concepts>
#include <string_view>

#include "src/__support/macros/config.h"
#include "test/UnitTest/Test.h"  // IWYU pragma: export

// When using zxtest, <zxtest/zxtest.h> (via test/UnitTest/ZxTest.h) already
// provides streamable ASSERT_OK and EXPECT_OK.  When using gtest, define them
// via ASSERT_EQ / EXPECT_EQ with << status-string diagnostics.
#if defined(__Fuchsia__) && !defined(ASSERT_OK)

namespace LIBC_NAMESPACE_DECL {
namespace testing::internal {

template <typename T>
concept HasStatusValue = requires(const T& v) {
  { v.status_value() } -> std::convertible_to<zx_status_t>;
};

template <typename T>
concept HasStatusMethod = requires(const T& v) {
  { v.status() } -> std::convertible_to<zx_status_t>;
};

template <typename T>
concept StatusLike =
    std::convertible_to<const T&, zx_status_t> || HasStatusValue<T> || HasStatusMethod<T>;

template <StatusLike T>
constexpr zx_status_t StatusValue(const T& v) {
  if constexpr (std::convertible_to<const T&, zx_status_t>) {
    return v;
  } else if constexpr (HasStatusValue<T>) {
    return v.status_value();
  } else {
    return v.status();
  }
}

template <StatusLike T>
constexpr decltype(auto) StatusString(const T& v) {
  if constexpr (requires {
                  { v.status_string() } -> std::convertible_to<std::string_view>;
                }) {
    return v.status_string();
  } else {
    return zx_status_get_string(StatusValue(v));
  }
}

}  // namespace testing::internal
}  // namespace LIBC_NAMESPACE_DECL

#define LIBC_TEST_OK_CONCAT_INNER_(a, b) a##b
#define LIBC_TEST_OK_CONCAT_(a, b) LIBC_TEST_OK_CONCAT_INNER_(a, b)

#define LIBC_TEST_CHECK_OK_IMPL_(MACRO, val, var)                       \
  switch (0)                                                            \
  case 0:                                                               \
  default:                                                              \
    if (auto&& var = (val); false) {                                    \
    } else                                                              \
      MACRO(LIBC_NAMESPACE::testing::internal::StatusValue(var), ZX_OK) \
          << "(" #val "): " << LIBC_NAMESPACE::testing::internal::StatusString(var) << " "

#define LIBC_TEST_CHECK_OK_(MACRO, val) \
  LIBC_TEST_CHECK_OK_IMPL_(MACRO, val, LIBC_TEST_OK_CONCAT_(libc_test_ok_val_, __LINE__))

#define ASSERT_OK(val) LIBC_TEST_CHECK_OK_(ASSERT_EQ, val)
#define EXPECT_OK(val) LIBC_TEST_CHECK_OK_(EXPECT_EQ, val)

#endif  // defined(__Fuchsia__) && !defined(ASSERT_OK)

#endif  // LIB_C_TEST_UNITTEST_H_
