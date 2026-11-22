// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/test.types/cpp/wire.h>
#include <lib/zx/event.h>
#include <lib/zx/eventpair.h>
#include <zircon/fidl.h>

#include <iostream>
#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/fidl/llcpp/tests/types_test_utils.h"

namespace llcpp_test = ::test_types;

TEST(UnionPayload, Primitive) {
  {
    llcpp_test::wire::TestUnion test_union;
    EXPECT_TRUE(test_union.has_invalid_tag());
    int32_t primitive = 5;
    test_union = llcpp_test::wire::TestUnion::WithPrimitive(primitive);
    EXPECT_EQ(llcpp_test::wire::TestUnion::Tag::kPrimitive, test_union.Which());
    EXPECT_EQ(5, test_union.primitive());
  }
  {
    llcpp_test::wire::TestUnion test_union;
    EXPECT_TRUE(test_union.has_invalid_tag());
    test_union = llcpp_test::wire::TestUnion::WithPrimitive(5);
    EXPECT_EQ(llcpp_test::wire::TestUnion::Tag::kPrimitive, test_union.Which());
    EXPECT_EQ(5, test_union.primitive());
  }
  {
    int32_t primitive = 5;
    auto test_union = llcpp_test::wire::TestUnion::WithPrimitive(primitive);
    EXPECT_EQ(llcpp_test::wire::TestUnion::Tag::kPrimitive, test_union.Which());
    EXPECT_EQ(5, test_union.primitive());
  }
}

TEST(UnionPayload, WhichDisallowedWhenUninitialized) {
  llcpp_test::wire::TestUnion test_union;
  ASSERT_DEATH({ test_union.Which(); }, "!has_invalid_tag()");
}

TEST(UnionPayload, Struct) {
  llcpp_test::wire::CopyableStruct copyable{.x = 5};
  auto test_flexible_union = llcpp_test::wire::TestFlexibleUnion::WithCopyable(copyable);
  EXPECT_EQ(llcpp_test::wire::TestFlexibleUnion::Tag::kCopyable, test_flexible_union.Which());
}

TEST(UnionPayload, CopyableStruct) {
  {
    llcpp_test::wire::TestUnion test_union;
    EXPECT_TRUE(test_union.has_invalid_tag());
    llcpp_test::wire::CopyableStruct copyable_struct{.x = 5};
    test_union = llcpp_test::wire::TestUnion::WithCopyable(copyable_struct);
    EXPECT_EQ(llcpp_test::wire::TestUnion::Tag::kCopyable, test_union.Which());
  }
  {
    llcpp_test::wire::CopyableStruct copyable_struct{.x = 5};
    auto test_union = llcpp_test::wire::TestUnion::WithCopyable(copyable_struct);
    EXPECT_EQ(llcpp_test::wire::TestUnion::Tag::kCopyable, test_union.Which());
  }
}

TEST(UnionPayload, MoveOnlyStruct) {
  {
    llcpp_test::wire::TestUnion test_union;
    EXPECT_TRUE(test_union.has_invalid_tag());
    llcpp_test::wire::MoveOnlyStruct move_only_struct{.h = zx::handle()};
    test_union = llcpp_test::wire::TestUnion::WithMoveOnly(std::move(move_only_struct));
    EXPECT_EQ(llcpp_test::wire::TestUnion::Tag::kMoveOnly, test_union.Which());
  }
  {
    llcpp_test::wire::TestUnion test_union;
    EXPECT_TRUE(test_union.has_invalid_tag());
    zx::event event;
    ASSERT_EQ(ZX_OK, zx::event::create(0, &event));
    llcpp_test::wire::MoveOnlyStruct move_only_struct{.h = std::move(event)};
    EXPECT_NE(ZX_HANDLE_INVALID, move_only_struct.h.get());
    test_union = llcpp_test::wire::TestUnion::WithMoveOnly(std::move(move_only_struct));
    EXPECT_EQ(llcpp_test::wire::TestUnion::Tag::kMoveOnly, test_union.Which());
    EXPECT_EQ(ZX_HANDLE_INVALID, move_only_struct.h.get());
    EXPECT_NE(ZX_HANDLE_INVALID, test_union.move_only().h.get());
  }
  {
    llcpp_test::wire::MoveOnlyStruct move_only_struct{.h = zx::handle()};
    auto test_union = llcpp_test::wire::TestUnion::WithMoveOnly(std::move(move_only_struct));
    EXPECT_EQ(llcpp_test::wire::TestUnion::Tag::kMoveOnly, test_union.Which());
  }
}

bool IsPeerValid(const zx::unowned_eventpair& handle) {
  zx_signals_t observed_signals = {};
  switch (handle->wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::deadline_after(zx::msec(0)),
                           &observed_signals)) {
    case ZX_ERR_TIMED_OUT:
      // timeout implies peer-closed was not observed
      return true;
    case ZX_OK:
      return (observed_signals & ZX_EVENTPAIR_PEER_CLOSED) == 0;
    default:
      return false;
  }
}

TEST(MoveUnion, NoDoubleDestructPayload) {
  zx::eventpair canary_a, canary_b;
  ASSERT_EQ(zx::eventpair::create(0, &canary_a, &canary_b), ZX_OK);
  ASSERT_TRUE(IsPeerValid(zx::unowned_eventpair(canary_a)));
  // Initialize the union such that the handle |h| within the |MoveOnlyStruct|
  // payload overlaps with the eventpair.
  zx_handle_t h = canary_b.release();
  static_assert(sizeof(llcpp_test::wire::TestUnion) == sizeof(fidl_union_t));
  uint8_t dangerous_buffer[sizeof(llcpp_test::wire::TestUnion)] = {};
  memcpy(&dangerous_buffer[4], &h, sizeof(h));
  {
    llcpp_test::wire::TestUnion* test_union =
        reinterpret_cast<llcpp_test::wire::TestUnion*>(dangerous_buffer);
    llcpp_test::wire::TestUnion union_with_absent_handle;
    llcpp_test::wire::MoveOnlyStruct move_only_struct{.h = zx::handle()};
    union_with_absent_handle =
        llcpp_test::wire::TestUnion::WithMoveOnly(std::move(move_only_struct));
    // Manually running the move constructor.
    new (test_union) llcpp_test::wire::TestUnion(std::move(union_with_absent_handle));
  }
  // |canary_b| should not be closed.
  EXPECT_TRUE(IsPeerValid(zx::unowned_eventpair(canary_a)));
  zx_handle_close(h);
}

TEST(Union, InitialTag) {
  llcpp_test::wire::TestFlexibleUnion flexible_union;
  EXPECT_TRUE(flexible_union.has_invalid_tag());

  llcpp_test::wire::TestStrictUnion strict_union;
  EXPECT_TRUE(strict_union.has_invalid_tag());
}
