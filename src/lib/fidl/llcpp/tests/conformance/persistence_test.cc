// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/manual.conformance.large/cpp/wire_types.h>
#include <fidl/test.types/cpp/wire_types.h>
#include <lib/zx/event.h>
#include <zircon/fidl.h>
#include <zircon/types.h>

#include <cstring>
#include <iostream>
#include <numeric>
#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

static std::vector<uint8_t> GetWireStructBytes() {
  // clang-format off
  std::vector<uint8_t> bytes = {
      // Wire format metadata.
      0, kFidlWireFormatMagicNumberInitial, FIDL_MESSAGE_HEADER_AT_REST_FLAGS_0_USE_VERSION_V2, 0,
      0, 0, 0, 0,
      // Payload, a single uint32_t.
      42, 0, 0, 0, 0, 0, 0, 0,
  };
  // clang-format on
  return bytes;
}

TEST(Unpersist, TooFewBytesError) {
  std::vector<uint8_t> bytes = {1, 2, 3};
  fit::result result =
      ::fidl::InplaceUnpersist<test_types::wire::CopyableStruct>(cpp20::span(bytes));
  ASSERT_FALSE(result.is_ok());
  ASSERT_EQ(fidl::Reason::kDecodeError, result.error_value().reason());
}

TEST(Unpersist, TooManyBytesError) {
  for (size_t n : {1, 8}) {
    std::vector<uint8_t> bytes = GetWireStructBytes();
    for (size_t i = 0; i < n; i++) {
      bytes.push_back(0);
    }
    fit::result result =
        ::fidl::InplaceUnpersist<test_types::wire::CopyableStruct>(cpp20::span(bytes));
    ASSERT_FALSE(result.is_ok()) << "Should fail with " << n << " extra bytes";
    ASSERT_EQ(fidl::Reason::kDecodeError, result.error_value().reason());
  }
}

TEST(Unpersist, WireStruct) {
  std::vector<uint8_t> bytes = GetWireStructBytes();
  EXPECT_EQ(bytes.size(), 16U);

  fit::result result =
      ::fidl::InplaceUnpersist<test_types::wire::CopyableStruct>(cpp20::span(bytes));
  ASSERT_TRUE(result.is_ok()) << "Error during unpersist: " << result.error_value();
  test_types::wire::CopyableStruct& obj = *result.value();

  // Check decoded value.
  EXPECT_EQ(42, obj.x);
}

TEST(Persist, WireStruct) {
  test_types::wire::CopyableStruct obj{.x = 42};

  fit::result result = ::fidl::Persist(obj);
  ASSERT_TRUE(result.is_ok()) << "Error during persist: " << result.error_value();

  // clang-format off
  const std::vector<uint8_t> golden_bytes = {
      // Wire format metadata.
      0, kFidlWireFormatMagicNumberInitial, FIDL_MESSAGE_HEADER_AT_REST_FLAGS_0_USE_VERSION_V2, 0,
      0, 0, 0, 0,
      // Payload, a single uint32_t.
      42, 0, 0, 0, 0, 0, 0, 0,
  };
  // clang-format on

  EXPECT_EQ(result.value(), golden_bytes);
}

TEST(Unpersist, WireUnion) {
  // clang-format off
  std::vector<uint8_t> bytes = {
      // Wire format metadata.
      0, kFidlWireFormatMagicNumberInitial, FIDL_MESSAGE_HEADER_AT_REST_FLAGS_0_USE_VERSION_V2, 0,
      0, 0, 0, 0,
      // Payload, a union with ordinal 1, and inlined int32.
      1, 0, 0, 0, 0, 0, 0, 0,
      42, 0, 0, 0, 0, 0, 1, 0,
  };
  // clang-format on
  EXPECT_EQ(bytes.size(), 24U);

  fit::result result =
      ::fidl::InplaceUnpersist<test_types::wire::TestStrictUnion>(cpp20::span(bytes));
  ASSERT_TRUE(result.is_ok()) << "Error during unpersist: " << result.error_value();
  test_types::wire::TestStrictUnion& obj = *result.value();

  // Check decoded value.
  EXPECT_TRUE(obj.is_primitive());
  EXPECT_EQ(42, obj.primitive());
}

TEST(Persist, WireUnion) {
  auto obj = test_types::wire::TestStrictUnion::WithPrimitive(42);

  fit::result result = ::fidl::Persist(obj);
  ASSERT_TRUE(result.is_ok()) << "Error during persist: " << result.error_value();

  // clang-format off
  const std::vector<uint8_t> golden_bytes = {
      // Wire format metadata.
      0, kFidlWireFormatMagicNumberInitial, FIDL_MESSAGE_HEADER_AT_REST_FLAGS_0_USE_VERSION_V2, 0,
      0, 0, 0, 0,
      // Payload, a union with ordinal 1, and inlined int32.
      1, 0, 0, 0, 0, 0, 0, 0,
      42, 0, 0, 0, 0, 0, 1, 0,
  };
  // clang-format on

  EXPECT_EQ(result.value(), golden_bytes);
}

TEST(Unpersist, WireTable) {
  // clang-format off
  std::vector<uint8_t> bytes = {
      // Wire format metadata.
      0, kFidlWireFormatMagicNumberInitial, FIDL_MESSAGE_HEADER_AT_REST_FLAGS_0_USE_VERSION_V2, 0,
      0, 0, 0, 0,
      // Payload, an empty table.
      0, 0, 0, 0, 0, 0, 0, 0,
      0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
  };
  // clang-format on
  EXPECT_EQ(bytes.size(), 24U);

  fit::result result =
      ::fidl::InplaceUnpersist<test_types::wire::SampleEmptyTable>(cpp20::span(bytes));
  ASSERT_TRUE(result.is_ok()) << "Error during unpersist: " << result.error_value();
  test_types::wire::SampleEmptyTable& obj = *result.value();

  // Check decoded value.
  EXPECT_TRUE(obj.IsEmpty());
}

TEST(Persist, WireTable) {
  test_types::wire::SampleEmptyTable obj;

  fit::result result = ::fidl::Persist(obj);
  ASSERT_TRUE(result.is_ok()) << "Error during persist: " << result.error_value();

  // clang-format off
  const std::vector<uint8_t> golden_bytes = {
      // Wire format metadata.
      0, kFidlWireFormatMagicNumberInitial, FIDL_MESSAGE_HEADER_AT_REST_FLAGS_0_USE_VERSION_V2, 0,
      0, 0, 0, 0,
      // Payload, an empty table.
      0, 0, 0, 0, 0, 0, 0, 0,
      0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
  };
  // clang-format on

  EXPECT_EQ(result.value(), golden_bytes);
}

TEST(PersistToBuffer, WireStruct) {
  const test_types::wire::CopyableStruct obj{.x = 42};
  FIDL_ALIGNDECL uint8_t storage[64];

  fit::result result = ::fidl::Persist(obj, cpp20::span(storage));
  ASSERT_TRUE(result.is_ok()) << "Error during persist: " << result.error_value();
  EXPECT_EQ(result.value().data(), storage);

  const std::vector<uint8_t> golden_bytes = GetWireStructBytes();
  EXPECT_EQ(std::vector<uint8_t>(result.value().begin(), result.value().end()), golden_bytes);
}

TEST(PersistToBuffer, WireUnion) {
  const auto obj = test_types::wire::TestStrictUnion::WithPrimitive(42);
  FIDL_ALIGNDECL uint8_t storage[64];

  fit::result expected = ::fidl::Persist(obj);
  ASSERT_TRUE(expected.is_ok());

  fit::result result = ::fidl::Persist(obj, cpp20::span(storage));
  ASSERT_TRUE(result.is_ok()) << "Error during persist: " << result.error_value();
  EXPECT_EQ(result.value().data(), storage);
  EXPECT_EQ(std::vector<uint8_t>(result.value().begin(), result.value().end()), expected.value());
}

TEST(PersistToBuffer, WireTableWithLargeOutOfLineVector) {
  // Use an unaligned length (2045 % 8 != 0) to exercise trailing-padding zeroing.
  std::vector<uint8_t> filler(2045);
  std::iota(filler.begin(), filler.end(), uint8_t{0});

  fidl::Arena<128> arena;
  const auto table = manual_conformance_large::wire::LargeTable::Builder(arena)
                         .filler(fidl::VectorView<uint8_t>::FromExternal(filler))
                         .Build();

  fit::result expected = ::fidl::Persist(table);
  ASSERT_TRUE(expected.is_ok());

  FIDL_ALIGNDECL uint8_t storage[4096];
  std::memset(storage, 0xAA, sizeof(storage));
  fit::result result = ::fidl::Persist(table, cpp20::span(storage));
  ASSERT_TRUE(result.is_ok()) << "Error during persist: " << result.error_value();
  EXPECT_EQ(result.value().data(), storage);
  EXPECT_EQ(std::vector<uint8_t>(result.value().begin(), result.value().end()), expected.value());

  fit::result decoded =
      ::fidl::InplaceUnpersist<manual_conformance_large::wire::LargeTable>(result.value());
  ASSERT_TRUE(decoded.is_ok()) << "Error during unpersist: " << decoded.error_value();
  ASSERT_TRUE(decoded.value()->has_filler());
  EXPECT_EQ(
      std::vector<uint8_t>(decoded.value()->filler().begin(), decoded.value()->filler().end()),
      filler);
}

TEST(PersistToBuffer, ExceedsChannelMaxMessageSize) {
  std::vector<uint8_t> filler(ZX_CHANNEL_MAX_MSG_BYTES + 13);
  std::iota(filler.begin(), filler.end(), uint8_t{0});

  fidl::Arena<128> arena;
  const auto table = manual_conformance_large::wire::LargeTable::Builder(arena)
                         .filler(fidl::VectorView<uint8_t>::FromExternal(filler))
                         .Build();

  std::vector<uint8_t> storage(ZX_CHANNEL_MAX_MSG_BYTES + 1024, 0xAA);

  fit::result result = ::fidl::Persist(table, cpp20::span(storage));
  ASSERT_TRUE(result.is_ok()) << "Error during persist: " << result.error_value();
  EXPECT_EQ(result.value().data(), storage.data());
  EXPECT_GT(result.value().size(), static_cast<size_t>(ZX_CHANNEL_MAX_MSG_BYTES));

  fit::result decoded =
      ::fidl::InplaceUnpersist<manual_conformance_large::wire::LargeTable>(result.value());
  ASSERT_TRUE(decoded.is_ok()) << "Error during unpersist: " << decoded.error_value();
  ASSERT_TRUE(decoded.value()->has_filler());
  EXPECT_EQ(
      std::vector<uint8_t>(decoded.value()->filler().begin(), decoded.value()->filler().end()),
      filler);
}

TEST(PersistToBuffer, BufferTooSmallError) {
  const test_types::wire::CopyableStruct obj{.x = 42};
  FIDL_ALIGNDECL uint8_t storage[16];
  std::memset(storage, 0xAA, sizeof(storage));

  for (cpp20::span<uint8_t> buf : {
           cpp20::span<uint8_t>{},
           cpp20::span(storage, 0),
           cpp20::span(storage, 4),
           cpp20::span(storage, 8),
           cpp20::span(storage, 15),
       }) {
    fit::result result = ::fidl::Persist(obj, buf);
    ASSERT_TRUE(result.is_error()) << "Should fail with capacity " << buf.size();
    EXPECT_EQ(result.error_value().reason(), fidl::Reason::kEncodeError);
    EXPECT_EQ(result.error_value().status(), ZX_ERR_BUFFER_TOO_SMALL);
  }

  fit::result exact = ::fidl::Persist(obj, cpp20::span(storage));
  ASSERT_TRUE(exact.is_ok()) << "Error during persist: " << exact.error_value();
  EXPECT_EQ(std::vector<uint8_t>(exact.value().begin(), exact.value().end()), GetWireStructBytes());

  uint8_t filler[5] = {1, 2, 3, 4, 5};
  fidl::Arena<128> arena;
  const auto table = manual_conformance_large::wire::LargeTable::Builder(arena)
                         .filler(fidl::VectorView<uint8_t>::FromExternal(filler))
                         .Build();
  fit::result expected = ::fidl::Persist(table);
  ASSERT_TRUE(expected.is_ok());

  FIDL_ALIGNDECL uint8_t table_storage[64];
  std::memset(table_storage, 0xAA, sizeof(table_storage));
  ASSERT_LE(expected->size(), sizeof(table_storage));
  for (size_t cap : {size_t{24}, expected->size() - 8, expected->size() - 1}) {
    fit::result result = ::fidl::Persist(table, cpp20::span(table_storage, cap));
    ASSERT_TRUE(result.is_error()) << "Should fail with capacity " << cap;
    EXPECT_EQ(result.error_value().reason(), fidl::Reason::kEncodeError);
    EXPECT_EQ(result.error_value().status(), ZX_ERR_BUFFER_TOO_SMALL);
  }

  fit::result exact_table = ::fidl::Persist(table, cpp20::span(table_storage, expected->size()));
  ASSERT_TRUE(exact_table.is_ok()) << "Error during persist: " << exact_table.error_value();
  EXPECT_EQ(std::vector<uint8_t>(exact_table->begin(), exact_table->end()), expected.value());
}

TEST(PersistToBuffer, UnalignedBufferError) {
  const test_types::wire::CopyableStruct obj{.x = 42};
  FIDL_ALIGNDECL uint8_t storage[32];

  fit::result result = ::fidl::Persist(obj, cpp20::span(storage + 1, sizeof(storage) - 1));
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.error_value().reason(), fidl::Reason::kEncodeError);
  EXPECT_EQ(result.error_value().status(), ZX_ERR_INVALID_ARGS);
}
