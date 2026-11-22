// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/test.types/cpp/natural_types.h>

#include <gtest/gtest.h>

namespace {

zx::event MakeEvent() {
  zx::event event;
  zx_status_t status = zx::event::create(0, &event);
  ZX_ASSERT(status == ZX_OK);
  return event;
}

TEST(StrictUnion, Construction) {
  // Strict union is not default constructible.
  static_assert(!std::is_default_constructible_v<test_types::TestUnion>);
  static_assert(!std::is_default_constructible_v<test_types::TestStrictUnion>);

  // As are any aggregates thereof.
  static_assert(!std::is_default_constructible_v<test_types::TestStrictUnionInStruct>,
                "Struct of strict union is not default constructible");
  static_assert(!std::is_default_constructible_v<test_types::TestStrictUnionInArrayInStruct>,
                "Strict union is not default constructible");

  test_types::TestUnion value = test_types::TestUnion::WithCopyable({{.x = 42}});
  EXPECT_TRUE(value.copyable());
  EXPECT_TRUE(value.copyable().has_value());
  EXPECT_EQ(value.copyable().value().x(), 42);
  EXPECT_EQ(value.copyable()->x(), 42);
}

TEST(StrictUnion, Move) {
  // Moving a value union recursively moves fields instead of clearing the union.
  test_types::TestStrictUnion value = test_types::TestStrictUnion::WithCopyable({{.x = 42}});
  test_types::TestStrictUnion value2 = std::move(value);
  EXPECT_EQ(value.Which(), test_types::TestStrictUnion::Tag::kCopyable);
  EXPECT_EQ(value.copyable().value().x(), 42);
  EXPECT_EQ(value2.Which(), test_types::TestStrictUnion::Tag::kCopyable);
  EXPECT_EQ(value2.copyable().value().x(), 42);

  // They do not share the same storage.
  value.copyable().value().x() = 0;
  EXPECT_EQ(value2.copyable().value().x(), 42);

  // Moving a resource union recursively moves fields instead of clearing the union.
  test_types::TestUnion resource = test_types::TestUnion::WithMoveOnly({{.h = MakeEvent()}});
  test_types::TestUnion resource2 = std::move(resource);
  EXPECT_EQ(resource.Which(), test_types::TestUnion::Tag::kMoveOnly);
  EXPECT_TRUE(resource.move_only().has_value());
  EXPECT_FALSE(resource.move_only()->h().is_valid());
  EXPECT_EQ(resource2.Which(), test_types::TestUnion::Tag::kMoveOnly);
  EXPECT_TRUE(resource2.move_only().has_value());
  EXPECT_TRUE(resource2.move_only()->h().is_valid());

  // They do not share the same storage.
  resource.move_only()->h() = MakeEvent();
  EXPECT_NE(resource2.move_only()->h(), resource.move_only()->h());
}

TEST(FlexibleUnion, Construction) {
  // Flexible union is not default constructible.
  static_assert(!std::is_default_constructible_v<test_types::TestFlexibleUnion>);

  // As are any aggregates thereof.
  static_assert(!std::is_default_constructible_v<test_types::TestFlexibleUnionInStruct>,
                "Struct of flexible union is not default constructible");
  static_assert(!std::is_default_constructible_v<test_types::TestFlexibleUnionInArrayInStruct>,
                "Flexible union is not default constructible");

  test_types::TestFlexibleUnion value = test_types::TestFlexibleUnion::WithCopyable({{.x = 42}});
  EXPECT_TRUE(value.copyable());
  EXPECT_TRUE(value.copyable().has_value());
  EXPECT_EQ(value.copyable().value().x(), 42);
  EXPECT_EQ(value.copyable()->x(), 42);
}

TEST(FlexibleUnion, Unknown) {
  // Use an internal API to make an unknown union.
  test_types::TestFlexibleUnion unknown{fidl::internal::DefaultConstructPossiblyInvalidObjectTag{}};
  EXPECT_TRUE(unknown.IsUnknown());
}

// In flexible unions it might be tempting to reset the moved-from union to an
// unknown state. Here, we explicitly choose to only recursively move fields to
// align with strict unions.
TEST(FlexibleUnion, Move) {
  // Moving a value union recursively moves fields instead of clearing the union.
  test_types::TestNonResourceUnion value = test_types::TestNonResourceUnion::WithPrimitive(42);
  test_types::TestNonResourceUnion value2 = std::move(value);
  EXPECT_EQ(value.Which(), test_types::TestNonResourceUnion::Tag::kPrimitive);
  EXPECT_EQ(value.primitive().value(), 42);
  EXPECT_EQ(value2.Which(), test_types::TestNonResourceUnion::Tag::kPrimitive);
  EXPECT_EQ(value2.primitive().value(), 42);

  // They do not share the same storage.
  value.primitive().value() = 0;
  EXPECT_EQ(value2.primitive().value(), 42);

  // Moving a resource union recursively moves fields instead of clearing the union.
  test_types::TestFlexibleUnion resource = test_types::TestFlexibleUnion::WithH(MakeEvent());
  test_types::TestFlexibleUnion resource2 = std::move(resource);
  EXPECT_EQ(resource.Which(), test_types::TestFlexibleUnion::Tag::kH);
  EXPECT_TRUE(resource.h().has_value());
  EXPECT_FALSE(resource.h()->is_valid());
  EXPECT_EQ(resource2.Which(), test_types::TestFlexibleUnion::Tag::kH);
  EXPECT_TRUE(resource2.h().has_value());
  EXPECT_TRUE(resource2.h()->is_valid());

  // They do not share the same storage.
  resource.h().value() = MakeEvent();
  EXPECT_NE(resource2.h().value(), resource.h().value());
}

// These operations should be common across strict/flexible unions.
TEST(Union, SetAndGetFieldsUsingMutableAccessors) {
  test_types::TestFlexibleUnion u = test_types::TestFlexibleUnion::WithPrimitive(0);
  u.primitive() = 42;
  EXPECT_EQ(u.Which(), test_types::TestFlexibleUnion::Tag::kPrimitive);
  u.copyable() = test_types::CopyableStruct{42};
  EXPECT_EQ(u.Which(), test_types::TestFlexibleUnion::Tag::kCopyable);
  u.h() = MakeEvent();
  EXPECT_EQ(u.Which(), test_types::TestFlexibleUnion::Tag::kH);
  EXPECT_EQ(u.primitive().value_or(0), 0);
}

TEST(Union, SetFieldsUsingSetters) {
  test_types::TestFlexibleUnion u = test_types::TestFlexibleUnion::WithPrimitive(0);
  u.primitive(42);
  EXPECT_EQ(u.Which(), test_types::TestFlexibleUnion::Tag::kPrimitive);
  u.copyable(test_types::CopyableStruct{42});
  EXPECT_EQ(u.Which(), test_types::TestFlexibleUnion::Tag::kCopyable);
  u.h(MakeEvent());
  EXPECT_EQ(u.Which(), test_types::TestFlexibleUnion::Tag::kH);
  EXPECT_EQ(u.primitive().value_or(0), 0);
}

TEST(Union, IntoOptional) {
  test_types::TestFlexibleUnion u = test_types::TestFlexibleUnion::WithPrimitive(42);
  std::optional<int32_t> p = u.primitive();
  EXPECT_TRUE(p.has_value());
  EXPECT_EQ(p.value(), 42);

  u.h() = MakeEvent();
  std::optional<zx::handle> h = u.h().take();
  EXPECT_TRUE(h.has_value());
  EXPECT_TRUE(h.value().is_valid());
}

TEST(Union, Copy) {
  static_assert(!std::is_copy_constructible_v<test_types::TestFlexibleUnion>,
                "Resource type is not copyable");
  static_assert(!std::is_copy_assignable_v<test_types::TestFlexibleUnion>,
                "Resource type is not copyable");
  static_assert(std::is_copy_constructible_v<test_types::TestNonResourceUnion>,
                "Value type is copyable");
  static_assert(std::is_copy_assignable_v<test_types::TestNonResourceUnion>,
                "Value type is copyable");

  test_types::TestNonResourceUnion value = test_types::TestNonResourceUnion::WithPrimitive(42);
  test_types::TestNonResourceUnion value2 = value;
  EXPECT_EQ(value.Which(), test_types::TestNonResourceUnion::Tag::kPrimitive);
  EXPECT_EQ(value.primitive().value(), 42);
  EXPECT_EQ(value2.Which(), test_types::TestNonResourceUnion::Tag::kPrimitive);
  EXPECT_EQ(value2.primitive().value(), 42);

  // They do not share the same storage.
  value.primitive().value() = 0;
  EXPECT_EQ(value2.primitive().value(), 42);
}

TEST(Union, Equality) {
  test_types::TestStrictUnion u = test_types::TestStrictUnion::WithPrimitive(42);
  test_types::TestStrictUnion different1 = test_types::TestStrictUnion::WithPrimitive(0);
  test_types::TestStrictUnion different2 = test_types::TestStrictUnion::WithCopyable({{.x = 0}});

  EXPECT_EQ(u, u);
  EXPECT_EQ(u, test_types::TestStrictUnion{u});
  EXPECT_NE(u, different1);
  EXPECT_NE(u, different2);
}

TEST(Union, OptionalUnion) {
  test_types::TestStrictOptionalUnionInStruct us;
  us.xu(std::make_unique<test_types::TestStrictUnion>(
      test_types::TestStrictUnion::WithPrimitive(42)));
  EXPECT_TRUE(us.xu().has_value());
  test_types::TestStrictOptionalUnionInStruct us2 = std::move(us);
  EXPECT_FALSE(us.xu().has_value());
  EXPECT_TRUE(us2.xu().has_value());
  EXPECT_EQ(us2.xu().value(), test_types::TestStrictUnion::WithPrimitive(42));

  us2.xu().reset();
  EXPECT_FALSE(us2.xu().has_value());
  EXPECT_EQ(us2.xu().value_or(test_types::TestStrictUnion::WithPrimitive(1)),
            test_types::TestStrictUnion::WithPrimitive(1));
}

TEST(Union, Traits) {
  static_assert(fidl::IsFidlType<test_types::TestStrictUnion>::value);
  static_assert(fidl::IsUnion<test_types::TestStrictUnion>::value);
  static_assert(!fidl::IsUnion<int>::value);
  static_assert(!fidl::IsUnion<test_types::FlexibleBits>::value);
  static_assert(fidl::TypeTraits<test_types::TestStrictUnion>::kPrimarySize ==
                sizeof(fidl_union_t));
  static_assert(fidl::TypeTraits<test_types::TestStrictUnion>::kMaxOutOfLine ==
                0);  // Envelope inlining
}

}  // namespace
