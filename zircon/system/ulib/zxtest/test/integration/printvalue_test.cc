// Copyright 2020 The Fuchsia Authors. All rights reserved. Use of this source code is governed by a
// BSD-style license that can be found in the LICENSE file.

#include <arpa/inet.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <string>
#include <tuple>

#include <zxtest/zxtest.h>

#include "helper.h"

namespace {

// Tests the formatted output value returned by the various PrintValue<> specializations.

// Test printing of primitive types.
TEST(PrintValueTest, PrimitiveTypes) {
  TEST_EXPECTATION(CHECKPOINT_REACHED, NO_ERRORS, "PrintValue printed primitive types wrong.");
  EXPECT_EQ("2147483647", zxtest::PrintValue(static_cast<int32_t>(2147483647)));
  EXPECT_EQ("4294967295", zxtest::PrintValue(static_cast<uint32_t>(4294967295u)));
  EXPECT_EQ("9223372036854775807", zxtest::PrintValue(static_cast<int64_t>(9223372036854775807)));
  EXPECT_EQ("18446744073709551615",
            zxtest::PrintValue(static_cast<uint64_t>(18446744073709551615u)));
  EXPECT_EQ("1024.000000", zxtest::PrintValue(1024.0f));
  EXPECT_EQ("-0.531250", zxtest::PrintValue(-0.53125));

  uint64_t u64 = 18446744073709551615u;
  const uint64_t const_u64 = 18446744073709551615u;
  EXPECT_EQ("18446744073709551615", zxtest::PrintValue(u64));
  EXPECT_EQ("18446744073709551615", zxtest::PrintValue(const_u64));
  TEST_CHECKPOINT();
}

// Test printing of string types.
TEST(PrintValueTest, StringTypes) {
  TEST_EXPECTATION(CHECKPOINT_REACHED, NO_ERRORS, "PrintValue printed string types wrong.");
  const char* foo = nullptr;
  EXPECT_EQ("<nullptr>", zxtest::PrintValue(foo));
  EXPECT_EQ("bar", zxtest::PrintValue("bar"));
  EXPECT_EQ("baz", zxtest::PrintValue(std::string("baz")));
  EXPECT_EQ("qux", zxtest::PrintValue(std::string("qux")));
  TEST_CHECKPOINT();
}

#if !defined(__Fuchsia__)

// Test printing of status types.
TEST(PrintValueTest, StatusType) {
  TEST_EXPECTATION(CHECKPOINT_REACHED, NO_ERRORS, "PrintStatus printed wrong.");
  EXPECT_EQ("0", zxtest::PrintStatus(ZX_OK));
  TEST_CHECKPOINT();
}

#endif

// Test printing of tuple types.
TEST(PrintValueTest, TupleType) {
  TEST_EXPECTATION(CHECKPOINT_REACHED, NO_ERRORS, "PrintValue printed tuple types wrong.");
  const auto tuple = std::make_tuple(3, "rabbits", ZX_OK);
  EXPECT_EQ("{ 3, rabbits, 0 }", zxtest::PrintValue(tuple));
  EXPECT_EQ("{ 1 }", zxtest::PrintValue(std::make_tuple(1)));
  EXPECT_EQ("{ }", zxtest::PrintValue(std::tuple<>{}));
  TEST_CHECKPOINT();
}

// Test that an unknown value type is printed as hex.
TEST(PrintValueTest, ValueAsHex) {
  TEST_EXPECTATION(CHECKPOINT_REACHED, NO_ERRORS, "PrintValue printed hex wrong.");
  struct Foo {
    uint32_t foo = 0;
  };
  Foo foo{htonl(0xDEADBEEF)};
  EXPECT_EQ("DE AD BE EF", zxtest::PrintValue(foo));
  TEST_CHECKPOINT();
}

}  // namespace
