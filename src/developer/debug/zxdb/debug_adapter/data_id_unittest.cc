// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/data_id.h"

#include <cstdint>
#include <limits>
#include <string>

#include <gtest/gtest.h>

#include "src/developer/debug/shared/arch.h"
#include "src/developer/debug/zxdb/client/breakpoint_settings.h"
#include "src/developer/debug/zxdb/expr/expr_value.h"
#include "src/developer/debug/zxdb/symbols/base_type.h"
#include "src/developer/debug/zxdb/symbols/type_test_support.h"

namespace zxdb {

namespace {

TEST(DataIdTest, FromExprValueAndRoundTrip) {
  auto int32_type = MakeInt32Type();
  ExprValue valid_val(int32_type, {1, 2, 3, 4}, ExprValueSource(0x7ffee1234000));
  ErrOr<DataId> res = DataId::FromExprValue(debug::Arch::kX64, 17601, "counter", valid_val);
  ASSERT_TRUE(res.ok()) << res.err().msg();
  EXPECT_EQ(res.value().process_koid(), 17601u);
  EXPECT_EQ(res.value().address(), 0x7ffee1234000u);
  EXPECT_EQ(res.value().byte_size(), 4u);
  EXPECT_EQ(res.value().ToString(), "17601:0x7ffee1234000:4");

  ErrOr<DataId> parsed = DataId::Parse(res.value().ToString());
  ASSERT_TRUE(parsed.ok());
  EXPECT_EQ(parsed.value().process_koid(), 17601u);
  EXPECT_EQ(parsed.value().address(), 0x7ffee1234000u);
  EXPECT_EQ(parsed.value().byte_size(), 4u);
}

TEST(DataIdTest, FromExprValueRejectsInvalidLocationsAndSizes) {
  auto int32_type = MakeInt32Type();

  // Register-backed values cannot be watched.
  ExprValue reg_val(int32_type, {1, 2, 3, 4}, ExprValueSource(debug::RegisterID::kX64_rax));
  EXPECT_TRUE(DataId::FromExprValue(debug::Arch::kX64, 100, "rax_var", reg_val).has_error());

  // Bitfields cannot be watched.
  ExprValue bitfield_val(int32_type, {1, 0, 0, 0}, ExprValueSource(0x2000, 3, 1));
  ErrOr<DataId> bitfield_res = DataId::FromExprValue(debug::Arch::kX64, 100, "bf", bitfield_val);
  ASSERT_TRUE(bitfield_res.has_error());
  EXPECT_NE(bitfield_res.err().msg().find("Bitfields"), std::string::npos);

  // Zero-byte and unsupported 3-byte sizes fail architectural validation.
  auto empty_type = fxl::MakeRefCounted<BaseType>(BaseType::kBaseTypeUnsigned, 0, "empty");
  ExprValue zero_val(empty_type, {}, ExprValueSource(0x2000));
  EXPECT_TRUE(DataId::FromExprValue(debug::Arch::kX64, 100, "empty", zero_val).has_error());

  auto three_byte_type = fxl::MakeRefCounted<BaseType>(BaseType::kBaseTypeUnsigned, 3, "u24");
  ExprValue three_val(three_byte_type, {1, 2, 3}, ExprValueSource(0x2000));
  EXPECT_TRUE(DataId::FromExprValue(debug::Arch::kX64, 100, "u24", three_val).has_error());

  // Address range right at UINT64_MAX - 4 succeeds, while UINT64_MAX - 3 overflows.
  ExprValue max_ok(int32_type, {1, 2, 3, 4},
                   ExprValueSource(std::numeric_limits<uint64_t>::max() - 4));
  EXPECT_TRUE(DataId::FromExprValue(debug::Arch::kX64, 100, "max_ok", max_ok).ok());

  ExprValue overflow_val(int32_type, {1, 2, 3, 4},
                         ExprValueSource(std::numeric_limits<uint64_t>::max() - 3));
  ErrOr<DataId> overflow_res =
      DataId::FromExprValue(debug::Arch::kX64, 100, "overflow", overflow_val);
  ASSERT_TRUE(overflow_res.has_error());
  EXPECT_NE(overflow_res.err().msg().find("overflows"), std::string::npos);
}

TEST(DataIdTest, ParseValidAndBoundaryRejections) {
  // Uppercase 0X prefix and exact upper bound (UINT64_MAX - byte_size) succeed.
  ErrOr<DataId> upper = DataId::Parse("42:0X1000:8");
  ASSERT_TRUE(upper.ok());
  EXPECT_EQ(upper.value().process_koid(), 42u);
  EXPECT_EQ(upper.value().address(), 0x1000u);
  EXPECT_EQ(upper.value().byte_size(), 8u);
  EXPECT_TRUE(upper.value().ValidateSize(debug::Arch::kX64, BreakpointSettings::Type::kWrite).ok());

  ErrOr<DataId> edge_ok = DataId::Parse("1:0xfffffffffffffffb:4");
  ASSERT_TRUE(edge_ok.ok());
  EXPECT_EQ(edge_ok.value().address(), 0xfffffffffffffffbu);

  // Zero koid, zero byte_size, missing/extra colons, empty tokens, non-hex prefix, and overflows.
  EXPECT_TRUE(DataId::Parse("0:0x1000:4").has_error());
  EXPECT_TRUE(DataId::Parse("1234:0x1000:0").has_error());
  EXPECT_TRUE(DataId::Parse("1234:0xffffffffffffffff:0").has_error());
  EXPECT_TRUE(DataId::Parse("1234:0xfffffffffffffffc:4").has_error());
  EXPECT_TRUE(DataId::Parse("1234:0x1000").has_error());
  EXPECT_TRUE(DataId::Parse("1234:0x1000:4:extra").has_error());
  EXPECT_TRUE(DataId::Parse(":0x1000:4").has_error());
  EXPECT_TRUE(DataId::Parse("1234:0x:4").has_error());
  EXPECT_TRUE(DataId::Parse("1234:1000:4").has_error());
  EXPECT_TRUE(DataId::Parse("1234:0x1000:").has_error());
  EXPECT_TRUE(DataId::Parse("18446744073709551616:0x1000:4").has_error());
  EXPECT_TRUE(DataId::Parse("1234:0x1000:4294967296").has_error());

  // Parse accepts positive non-hardware sizes, while ValidateSize rejects them.
  ErrOr<DataId> non_hw = DataId::Parse("1234:0x1000:3");
  ASSERT_TRUE(non_hw.ok());
  EXPECT_TRUE(
      non_hw.value().ValidateSize(debug::Arch::kX64, BreakpointSettings::Type::kWrite).has_error());
}

TEST(DataIdTest, ParseAccessType) {
  ErrOr<BreakpointSettings::Type> write = DataId::ParseAccessType("write");
  ASSERT_TRUE(write.ok());
  EXPECT_EQ(write.value(), BreakpointSettings::Type::kWrite);

  ErrOr<BreakpointSettings::Type> rw = DataId::ParseAccessType("readWrite");
  ASSERT_TRUE(rw.ok());
  EXPECT_EQ(rw.value(), BreakpointSettings::Type::kReadWrite);

  ErrOr<BreakpointSettings::Type> ro = DataId::ParseAccessType("read");
  ASSERT_TRUE(ro.has_error());
  EXPECT_EQ(ro.err().msg(), "Read-only data breakpoints are not supported on this platform.");

  ErrOr<BreakpointSettings::Type> unknown = DataId::ParseAccessType("sideways");
  ASSERT_TRUE(unknown.has_error());
  EXPECT_EQ(unknown.err().msg(), "Unknown accessType: sideways");
}

}  // namespace

}  // namespace zxdb
