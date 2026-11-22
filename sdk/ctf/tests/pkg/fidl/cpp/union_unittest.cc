// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/test/misc/cpp/fidl.h>
#include <lib/fidl/cpp/comparison.h>

#include <vector>

#include <zxtest/zxtest.h>

#include "test/test_util.h"

namespace fidl {
namespace {

TEST(Union, SetterReturnsSelf) {
  using test::misc::SampleUnion;
  using test::misc::SimpleTable;

  SimpleTable st;
  st.set_x(42);

  SampleUnion u;
  u.set_st(std::move(st));

  EXPECT_TRUE(u.is_st());
  EXPECT_EQ(42, u.st().x());

  EXPECT_TRUE(fidl::Equals(u, SampleUnion().set_st(std::move(SimpleTable().set_x(42)))));
}

TEST(Union, EmptyUnionEquality) {
  using test::misc::SampleUnion;

  EXPECT_TRUE(::fidl::Equals(SampleUnion(), SampleUnion()));
}

TEST(Union, UnionFactoryFunctions) {
  using test::misc::SampleUnion;
  using test::misc::SimpleTable;

  SampleUnion prim_u = SampleUnion::WithI(123);

  EXPECT_EQ(prim_u.i(), 123);
  EXPECT_EQ(prim_u.Which(), SampleUnion::Tag::kI);
  EXPECT_EQ(prim_u.Ordinal(), SampleUnion::Tag::kI);
  EXPECT_EQ(prim_u.UnknownBytes(), nullptr);

  // Test passing an object with no copy constructor (only move) to the
  // factory function.
  SimpleTable tbl;
  SampleUnion tbl_u = SampleUnion::WithSt(std::move(tbl));

  EXPECT_EQ(tbl_u.Which(), SampleUnion::Tag::kSt);
  EXPECT_EQ(tbl_u.Ordinal(), SampleUnion::Tag::kSt);
  EXPECT_EQ(tbl_u.UnknownBytes(), nullptr);
}

// Confirms that a union can have a variant with both type and name identifiers as "empty" and not
// collide with any tags, internal or otherwise, indicating the union is in its unknown monostate.
TEST(Union, UnionContainingEmptyStruct) {
  using test::misc::Empty;
  using test::misc::UnionContainingEmptyStruct;

  UnionContainingEmptyStruct u;

  EXPECT_EQ(u.Which(), UnionContainingEmptyStruct::Tag::Invalid);
  EXPECT_FALSE(u.is_empty());

  Empty empty;
  u.set_empty(std::move(empty));
  EXPECT_EQ(u.Which(), UnionContainingEmptyStruct::Tag::kEmpty);
  EXPECT_TRUE(u.is_empty());
}

}  // namespace
}  // namespace fidl
