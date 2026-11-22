// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/elfldltl/dwarf/cfi-entry.h>
#include <lib/elfldltl/dwarf/eh-frame-hdr.h>
#include <lib/elfldltl/dwarf/encoding.h>
#include <lib/elfldltl/dwarf/section-data.h>
#include <lib/elfldltl/memory.h>
#include <lib/elfldltl/testing/diagnostics.h>
#include <lib/elfldltl/testing/typed-test.h>

#include <concepts>
#include <ranges>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

namespace {

using ::testing::AllOf;
using ::testing::ElementsAre;
using ::testing::ElementsAreArray;
using ::testing::Eq;
using ::testing::Field;
using ::testing::FieldsAre;
using ::testing::Optional;

FORMAT_TYPED_TEST_SUITE(ElfldltlDwarfTests);

template <class Elf>
struct InitialLength32 {
  typename Elf::Word length = 0;
};

template <class Elf>
struct [[gnu::packed]] InitialLength64 {
  constexpr InitialLength64() = default;

  constexpr explicit InitialLength64(uint64_t n) : length{n} {}

  [[gnu::packed]] const uint32_t dwarf64_marker = 0xffffffff;
  [[gnu::packed]] typename Elf::Xword length = 0;
};

template <class Length, typename T>
struct [[gnu::packed]] TestData {
  constexpr TestData() = default;

  explicit constexpr TestData(const T& data) : contents{data} {}

  [[gnu::packed]] Length initial_length{sizeof(T)};
  [[gnu::packed]] T contents{};
};

template <typename T>
constexpr std::span<const std::byte> AsBytes(T& data) {
  return std::as_bytes(std::span{&data, 1});
}

constexpr elfldltl::FileAddress kErrorArg{0x123u};

constexpr std::span<const std::byte> kNoBytes{};

TYPED_TEST(ElfldltlDwarfTests, SectionDataEmpty) {
  using Elf = typename TestFixture::Elf;

  elfldltl::dwarf::SectionData section_data;
  EXPECT_TRUE(section_data.contents().empty());

  auto diag = elfldltl::testing::ExpectOkDiagnostics();

  {
    constexpr InitialLength32<Elf> kEmpty;
    auto read = elfldltl::dwarf::SectionData::Read<Elf>(diag, AsBytes(kEmpty));
    ASSERT_TRUE(read);
    EXPECT_EQ(read->size_bytes(), 4u);
    EXPECT_EQ(read->contents().size_bytes(), 0u);
  }

  {
    constexpr InitialLength64<Elf> kEmpty;
    auto read = elfldltl::dwarf::SectionData::Read<Elf>(diag, AsBytes(kEmpty));
    ASSERT_TRUE(read);
    EXPECT_EQ(read->size_bytes(), 12u);
    EXPECT_EQ(read->contents().size_bytes(), 0u);
  }
}

TYPED_TEST(ElfldltlDwarfTests, SectionDataRead) {
  using Elf = typename TestFixture::Elf;

  auto diag = elfldltl::testing::ExpectOkDiagnostics();

  static constexpr std::byte kOneByte{17};
  {
    constexpr TestData<InitialLength32<Elf>, std::byte> kOneByteData{kOneByte};
    auto read = elfldltl::dwarf::SectionData::Read<Elf>(diag, AsBytes(kOneByteData));
    ASSERT_TRUE(read);
    EXPECT_EQ(read->size_bytes(), sizeof(kOneByteData));
    EXPECT_EQ(read->format(), elfldltl::dwarf::SectionData::Format::kDwarf32);
    EXPECT_EQ(read->initial_length_size(), 4u);
    EXPECT_EQ(read->offset_size(), 4u);
    EXPECT_EQ(read->contents().size(), 1u);
    EXPECT_THAT(read->contents(), ElementsAre(kOneByte));
  }
  {
    constexpr TestData<InitialLength64<Elf>, std::byte> kOneByteData{kOneByte};
    auto read = elfldltl::dwarf::SectionData::Read<Elf>(diag, AsBytes(kOneByteData));
    ASSERT_TRUE(read);
    EXPECT_EQ(read->size_bytes(), sizeof(kOneByteData));
    EXPECT_EQ(read->format(), elfldltl::dwarf::SectionData::Format::kDwarf64);
    EXPECT_EQ(read->initial_length_size(), 12u);
    EXPECT_EQ(read->offset_size(), 8u);
    EXPECT_EQ(read->contents().size(), 1u);
    EXPECT_THAT(read->contents(), ElementsAre(kOneByte));
  }
}

TYPED_TEST(ElfldltlDwarfTests, SectionDataConsume) {
  using Elf = typename TestFixture::Elf;

  auto diag = elfldltl::testing::ExpectOkDiagnostics();

  static constexpr std::byte kOneByte{17};
  constexpr struct {
    TestData<InitialLength32<Elf>, std::byte> data{kOneByte};
    std::array<std::byte, 3> rest{std::byte{1}, std::byte{2}, std::byte{3}};
  } kData;

  auto [read, rest] = elfldltl::dwarf::SectionData::Consume<Elf>(diag, AsBytes(kData));
  ASSERT_TRUE(read);
  EXPECT_EQ(read->contents().size(), 1u);
  EXPECT_THAT(read->contents(), ElementsAre(kOneByte));
  EXPECT_THAT(rest, ElementsAreArray(kData.rest));
}

TYPED_TEST(ElfldltlDwarfTests, SectionDataReadOffset) {
  using Elf = typename TestFixture::Elf;

  auto diag = elfldltl::testing::ExpectOkDiagnostics();

  {
    struct [[gnu::packed]] DataWithOffset {
      std::byte extra_byte{23};
      [[gnu::packed]] typename Elf::Word offset{1234};
    };
    constexpr TestData<InitialLength32<Elf>, DataWithOffset> kDataWithOffset;
    auto read = elfldltl::dwarf::SectionData::Read<Elf>(diag, AsBytes(kDataWithOffset));
    ASSERT_TRUE(read);
    EXPECT_EQ(read->contents().size_bytes(), 5u);
    auto offset = read->template read_offset<Elf>(1);
    EXPECT_THAT(offset, Optional(Eq(1234u)));
  }
  {
    struct [[gnu::packed]] DataWithOffset {
      std::byte extra_byte{23};
      [[gnu::packed]] typename Elf::Xword offset{1234567890};
    };
    constexpr TestData<InitialLength64<Elf>, DataWithOffset> kDataWithOffset;
    auto read = elfldltl::dwarf::SectionData::Read<Elf>(diag, AsBytes(kDataWithOffset));
    ASSERT_TRUE(read);
    EXPECT_EQ(read->contents().size_bytes(), 9u);
    auto offset = read->template read_offset<Elf>(1);
    EXPECT_THAT(offset, Optional(Eq(1234567890u)));
  }
}

TYPED_TEST(ElfldltlDwarfTests, SectionDataReadFail) {
  using Elf = typename TestFixture::Elf;

  // Empty buffer.
  {
    elfldltl::testing::ExpectedSingleError expected{
        "data size ",
        0,
        " too small for DWARF header",
        kErrorArg,
    };
    auto read = elfldltl::dwarf::SectionData::Read<Elf>(expected, kNoBytes, kErrorArg);
    EXPECT_FALSE(read);
  }

  // Buffer too short for any initial length.
  {
    constexpr std::array<std::byte, 3> kTooShortForHeader = {};
    elfldltl::testing::ExpectedSingleError expected{
        "data size ",
        kTooShortForHeader.size(),
        " too small for DWARF header",
        kErrorArg,
    };
    auto read =
        elfldltl::dwarf::SectionData::Read<Elf>(expected, AsBytes(kTooShortForHeader), kErrorArg);
    EXPECT_FALSE(read);
  }

  // Buffer too short for 64-bit initial length.
  {
    constexpr std::array<std::byte, 8> kTooShortForHeader64 = {
        std::byte{0xff},
        std::byte{0xff},
        std::byte{0xff},
        std::byte{0xff},
    };
    elfldltl::testing::ExpectedSingleError expected{
        "data size ",
        kTooShortForHeader64.size(),
        " too small for DWARF header",
        kErrorArg,
    };
    auto read =
        elfldltl::dwarf::SectionData::Read<Elf>(expected, AsBytes(kTooShortForHeader64), kErrorArg);
    EXPECT_FALSE(read);
  }

  // Reserved initial length values.
  for (uint32_t value = elfldltl::dwarf::kDwarf32Limit; value < elfldltl::dwarf::kDwarf64Length;
       ++value) {
    const InitialLength32<Elf> kReserved{value};
    elfldltl::testing::ExpectedSingleError expected{
        "Reserved initial-length value ",
        value,
        " used in DWARF header",
        kErrorArg,
    };
    auto read = elfldltl::dwarf::SectionData::Read<Elf>(expected, AsBytes(kReserved), kErrorArg);
    EXPECT_FALSE(read);
  }

  // Truncated contents for 32-bit length.
  {
    constexpr InitialLength32<Elf> kTooShortForLength{1};
    elfldltl::testing::ExpectedSingleError expected{
        "data size ", 4, " < ", 5, " required by DWARF header", kErrorArg,
    };
    auto read =
        elfldltl::dwarf::SectionData::Read<Elf>(expected, AsBytes(kTooShortForLength), kErrorArg);
    EXPECT_FALSE(read);
  }

  // Truncated contents for 64-bit length.
  {
    constexpr InitialLength64<Elf> kTooShortForLength64{1};
    elfldltl::testing::ExpectedSingleError expected{
        "data size ", 12, " < ", 13, " required by DWARF header", kErrorArg,
    };
    auto read =
        elfldltl::dwarf::SectionData::Read<Elf>(expected, AsBytes(kTooShortForLength64), kErrorArg);
    EXPECT_FALSE(read);
  }
}

TEST(ElfldltlDwarfTests, Uleb128) {
  EXPECT_EQ(elfldltl::dwarf::Uleb128::Read(kNoBytes), std::nullopt);

  constexpr uint8_t kOneByte{17};
  EXPECT_THAT(elfldltl::dwarf::Uleb128::Read(AsBytes(kOneByte)), Optional(FieldsAre(17u, 1u)));

  constexpr uint8_t kTwoByte[] = {0x80 | (0xaau & 0x7f), 0xaau >> 7};
  EXPECT_THAT(elfldltl::dwarf::Uleb128::Read(AsBytes(kTwoByte)), Optional(FieldsAre(0xaau, 2u)));

  constexpr uint8_t kThreeByte[] = {
      0x80 | (0xaabbu & 0x7f),
      0x80 | ((0xaabbu >> 7) & 0x7f),
      ((0xaabbu >> 14) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Uleb128::Read(AsBytes(kThreeByte)),
              Optional(FieldsAre(0xaabbu, 3u)));

  constexpr uint8_t kFourByte[] = {
      0x80 | (0xaabbccu & 0x7f),
      0x80 | ((0xaabbccu >> 7) & 0x7f),
      0x80 | ((0xaabbccu >> 14) & 0x7f),
      ((0xaabbccu >> 21) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Uleb128::Read(AsBytes(kFourByte)),
              Optional(FieldsAre(0xaabbccu, 4u)));

  constexpr uint8_t kFiveByte[] = {
      0x80 | (0xaabbccddu & 0x7f),         0x80 | ((0xaabbccddu >> 7) & 0x7f),
      0x80 | ((0xaabbccddu >> 14) & 0x7f), 0x80 | ((0xaabbccddu >> 21) & 0x7f),
      ((0xaabbccdd >> 28) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Uleb128::Read(AsBytes(kFiveByte)),
              Optional(FieldsAre(0xaabbccddu, 5u)));

  constexpr uint8_t kSixByte[] = {
      0x80 | (0xaabbccddeeu & 0x7f),         0x80 | ((0xaabbccddeeu >> 7) & 0x7f),
      0x80 | ((0xaabbccddeeu >> 14) & 0x7f), 0x80 | ((0xaabbccddeeu >> 21) & 0x7f),
      0x80 | ((0xaabbccddeeu >> 28) & 0x7f), ((0xaabbccddeeu >> 35) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Uleb128::Read(AsBytes(kSixByte)),
              Optional(FieldsAre(0xaabbccddeeu, 6u)));

  constexpr uint8_t kSevenByte[] = {
      0x80 | (0xaabbccddeeffu & 0x7f),         0x80 | ((0xaabbccddeeffu >> 7) & 0x7f),
      0x80 | ((0xaabbccddeeffu >> 14) & 0x7f), 0x80 | ((0xaabbccddeeffu >> 21) & 0x7f),
      0x80 | ((0xaabbccddeeffu >> 28) & 0x7f), 0x80 | ((0xaabbccddeeffu >> 35) & 0x7f),
      ((0xaabbccddeeffu >> 42) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Uleb128::Read(AsBytes(kSevenByte)),
              Optional(FieldsAre(0xaabbccddeeffu, 7u)));

  constexpr uint8_t kEightByte[] = {
      0x80 | (0xaabbccddeeff11u & 0x7f),         0x80 | ((0xaabbccddeeff11u >> 7) & 0x7f),
      0x80 | ((0xaabbccddeeff11u >> 14) & 0x7f), 0x80 | ((0xaabbccddeeff11u >> 21) & 0x7f),
      0x80 | ((0xaabbccddeeff11u >> 28) & 0x7f), 0x80 | ((0xaabbccddeeff11u >> 35) & 0x7f),
      0x80 | ((0xaabbccddeeff11u >> 42) & 0x7f), ((0xaabbccddeeff11u >> 49) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Uleb128::Read(AsBytes(kEightByte)),
              Optional(FieldsAre(0xaabbccddeeff11u, 8u)));

  constexpr uint8_t kNineByte[] = {
      0x80 | (0x77bbccddeeff1122u & 0x7f),         0x80 | ((0x77bbccddeeff1122u >> 7) & 0x7f),
      0x80 | ((0x77bbccddeeff1122u >> 14) & 0x7f), 0x80 | ((0x77bbccddeeff1122u >> 21) & 0x7f),
      0x80 | ((0x77bbccddeeff1122u >> 28) & 0x7f), 0x80 | ((0x77bbccddeeff1122u >> 35) & 0x7f),
      0x80 | ((0x77bbccddeeff1122u >> 42) & 0x7f), 0x80 | ((0x77bbccddeeff1122u >> 49) & 0x7f),
      ((0x77bbccddeeff1122u >> 56) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Uleb128::Read(AsBytes(kNineByte)),
              Optional(FieldsAre(0x77bbccddeeff1122u, 9u)));

  constexpr uint8_t kTenByte[] = {
      0x80 | (0xffffffffffffffffu & 0x7f),         0x80 | ((0xffffffffffffffffu >> 7) & 0x7f),
      0x80 | ((0xffffffffffffffffu >> 14) & 0x7f), 0x80 | ((0xffffffffffffffffu >> 21) & 0x7f),
      0x80 | ((0xffffffffffffffffu >> 28) & 0x7f), 0x80 | ((0xffffffffffffffffu >> 35) & 0x7f),
      0x80 | ((0xffffffffffffffffu >> 42) & 0x7f), 0x80 | ((0xffffffffffffffffu >> 49) & 0x7f),
      0x80 | ((0xffffffffffffffffu >> 56) & 0x7f), ((0xffffffffffffffffu >> 63) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Uleb128::Read(AsBytes(kTenByte)),
              Optional(FieldsAre(0xffffffffffffffffu, 10u)));

  constexpr uint8_t kTooManyBytes[17] = {0x80 | 23, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80,
                                         0x80,      0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0};
  EXPECT_EQ(elfldltl::dwarf::Uleb128::Read(AsBytes(kTooManyBytes)), std::nullopt);
}

TEST(ElfldltlDwarfTests, Sleb128) {
  EXPECT_EQ(elfldltl::dwarf::Sleb128::Read(kNoBytes), std::nullopt);

  constexpr uint8_t kOneByte{17};
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kOneByte)), Optional(FieldsAre(17, 1u)));

  constexpr uint8_t kTwoByte[] = {0x80 | (0xaau & 0x7f), 0xaau >> 7};
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kTwoByte)), Optional(FieldsAre(0xaa, 2u)));

  constexpr uint8_t kThreeByte[] = {
      0x80 | (0xaabbu & 0x7f),
      0x80 | ((0xaabbu >> 7) & 0x7f),
      ((0xaabbu >> 14) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kThreeByte)), Optional(FieldsAre(0xaabb, 3u)));

  constexpr uint8_t kFourByte[] = {
      0x80 | (0xaabbcc & 0x7f),
      0x80 | ((0xaabbcc >> 7) & 0x7f),
      0x80 | ((0xaabbcc >> 14) & 0x7f),
      ((0xaabbcc >> 21) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kFourByte)), Optional(FieldsAre(0xaabbcc, 4)));

  constexpr uint8_t kFiveByte[] = {
      0x80 | (0xaabbccdd & 0x7f),         0x80 | ((0xaabbccdd >> 7) & 0x7f),
      0x80 | ((0xaabbccdd >> 14) & 0x7f), 0x80 | ((0xaabbccdd >> 21) & 0x7f),
      ((0xaabbccdd >> 28) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kFiveByte)),
              Optional(FieldsAre(0xaabbccdd, 5u)));

  constexpr uint8_t kSixByte[] = {
      0x80 | (0xaabbccddee & 0x7f),         0x80 | ((0xaabbccddee >> 7) & 0x7f),
      0x80 | ((0xaabbccddee >> 14) & 0x7f), 0x80 | ((0xaabbccddee >> 21) & 0x7f),
      0x80 | ((0xaabbccddee >> 28) & 0x7f), ((0xaabbccddee >> 35) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kSixByte)),
              Optional(FieldsAre(0xaabbccddee, 6u)));

  constexpr uint8_t kSevenByte[] = {
      0x80 | (0xaabbccddeeff & 0x7f),         0x80 | ((0xaabbccddeeff >> 7) & 0x7f),
      0x80 | ((0xaabbccddeeff >> 14) & 0x7f), 0x80 | ((0xaabbccddeeff >> 21) & 0x7f),
      0x80 | ((0xaabbccddeeff >> 28) & 0x7f), 0x80 | ((0xaabbccddeeff >> 35) & 0x7f),
      ((0xaabbccddeeff >> 42) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kSevenByte)),
              Optional(FieldsAre(0xaabbccddeeff, 7u)));

  constexpr uint8_t kEightByte[] = {
      0x80 | (0x7abbccddeeff11 & 0x7f),         0x80 | ((0x7abbccddeeff11 >> 7) & 0x7f),
      0x80 | ((0x7abbccddeeff11 >> 14) & 0x7f), 0x80 | ((0x7abbccddeeff11 >> 21) & 0x7f),
      0x80 | ((0x7abbccddeeff11 >> 28) & 0x7f), 0x80 | ((0x7abbccddeeff11 >> 35) & 0x7f),
      0x80 | ((0x7abbccddeeff11 >> 42) & 0x7f), ((0x7abbccddeeff11 >> 49) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kEightByte)),
              Optional(FieldsAre(0x7abbccddeeff11, 8u)));

  constexpr uint8_t kNineByte[] = {
      0x80 | (0x33bbccddeeff1122 & 0x7f),         0x80 | ((0x33bbccddeeff1122 >> 7) & 0x7f),
      0x80 | ((0x33bbccddeeff1122 >> 14) & 0x7f), 0x80 | ((0x33bbccddeeff1122 >> 21) & 0x7f),
      0x80 | ((0x33bbccddeeff1122 >> 28) & 0x7f), 0x80 | ((0x33bbccddeeff1122 >> 35) & 0x7f),
      0x80 | ((0x33bbccddeeff1122 >> 42) & 0x7f), 0x80 | ((0x33bbccddeeff1122 >> 49) & 0x7f),
      ((0x33bbccddeeff1122 >> 56) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kNineByte)),
              Optional(FieldsAre(0x33bbccddeeff1122, 9u)));

  constexpr uint8_t kOneByteNegative{-17 & 0x7f};
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kOneByteNegative)),
              Optional(FieldsAre(-17, 1u)));

  constexpr uint8_t kTwoByteNegative[] = {0x80 | (-0xaa & 0x7f), (-0xaau >> 7) & 0x7f};
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kTwoByteNegative)),
              Optional(FieldsAre(-0xaa, 2u)));

  constexpr uint8_t kThreeByteNegative[] = {
      0x80 | (-0xaabb & 0x7f),
      0x80 | ((-0xaabb >> 7) & 0x7f),
      ((-0xaabb >> 14) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kThreeByteNegative)),
              Optional(FieldsAre(-0xaabb, 3u)));

  constexpr uint8_t kFourByteNegative[] = {
      0x80 | (-0xaabbcc & 0x7f),
      0x80 | ((-0xaabbcc >> 7) & 0x7f),
      0x80 | ((-0xaabbcc >> 14) & 0x7f),
      ((-0xaabbcc >> 21) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kFourByteNegative)),
              Optional(FieldsAre(-0xaabbcc, 4)));

  constexpr uint8_t kFiveByteNegative[] = {
      0x80 | (-0xaabbccdd & 0x7f),         0x80 | ((-0xaabbccdd >> 7) & 0x7f),
      0x80 | ((-0xaabbccdd >> 14) & 0x7f), 0x80 | ((-0xaabbccdd >> 21) & 0x7f),
      ((-0xaabbccdd >> 28) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kFiveByteNegative)),
              Optional(FieldsAre(-0xaabbccdd, 5u)));

  constexpr uint8_t kSixByteNegative[] = {
      0x80 | (-0x33bbccddee & 0x7f),         0x80 | ((-0x33bbccddee >> 7) & 0x7f),
      0x80 | ((-0x33bbccddee >> 14) & 0x7f), 0x80 | ((-0x33bbccddee >> 21) & 0x7f),
      0x80 | ((-0x33bbccddee >> 28) & 0x7f), ((-0x33bbccddee >> 35) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kSixByteNegative)),
              Optional(FieldsAre(-0x33bbccddee, 6u)));

  constexpr uint8_t kSevenByteNegative[] = {
      0x80 | (-0x33bbccddeeff & 0x7f),         0x80 | ((-0x33bbccddeeff >> 7) & 0x7f),
      0x80 | ((-0x33bbccddeeff >> 14) & 0x7f), 0x80 | ((-0x33bbccddeeff >> 21) & 0x7f),
      0x80 | ((-0x33bbccddeeff >> 28) & 0x7f), 0x80 | ((-0x33bbccddeeff >> 35) & 0x7f),
      ((-0x33bbccddeeff >> 42) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kSevenByteNegative)),
              Optional(FieldsAre(-0x33bbccddeeff, 7u)));

  constexpr uint8_t kEightByteNegative[] = {
      0x80 | (-0x7abbccddeeff11 & 0x7f),         0x80 | ((-0x7abbccddeeff11 >> 7) & 0x7f),
      0x80 | ((-0x7abbccddeeff11 >> 14) & 0x7f), 0x80 | ((-0x7abbccddeeff11 >> 21) & 0x7f),
      0x80 | ((-0x7abbccddeeff11 >> 28) & 0x7f), 0x80 | ((-0x7abbccddeeff11 >> 35) & 0x7f),
      0x80 | ((-0x7abbccddeeff11 >> 42) & 0x7f), ((-0x7abbccddeeff11 >> 49) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kEightByteNegative)),
              Optional(FieldsAre(-0x7abbccddeeff11, 8u)));

  constexpr uint8_t kNineByteNegative[] = {
      0x80 | (-0x33bbccddeeff1122 & 0x7f),         0x80 | ((-0x33bbccddeeff1122 >> 7) & 0x7f),
      0x80 | ((-0x33bbccddeeff1122 >> 14) & 0x7f), 0x80 | ((-0x33bbccddeeff1122 >> 21) & 0x7f),
      0x80 | ((-0x33bbccddeeff1122 >> 28) & 0x7f), 0x80 | ((-0x33bbccddeeff1122 >> 35) & 0x7f),
      0x80 | ((-0x33bbccddeeff1122 >> 42) & 0x7f), 0x80 | ((-0x33bbccddeeff1122 >> 49) & 0x7f),
      ((-0x33bbccddeeff1122 >> 56) & 0x7f),
  };
  EXPECT_THAT(elfldltl::dwarf::Sleb128::Read(AsBytes(kNineByteNegative)),
              Optional(FieldsAre(-0x33bbccddeeff1122, 9u)));

  constexpr uint8_t kTooManyBytes[17] = {0x80 | (-23 & 0x7f),
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0x80,
                                         0};
  EXPECT_EQ(elfldltl::dwarf::Sleb128::Read(AsBytes(kTooManyBytes)), std::nullopt);
}

template <typename T>
constexpr uint8_t kSize = sizeof(T);

TYPED_TEST(ElfldltlDwarfTests, EncodedPtr) {
  using elfldltl::dwarf::EncodedPtr;
  using Elf = typename TestFixture::Elf;
  using Addr = typename Elf::Addr;
  using Half = typename Elf::Half;
  using Word = typename Elf::Word;
  using Xword = typename Elf::Xword;

  EncodedPtr ptr;
  EXPECT_EQ(ptr.ptr, 0u);
  EXPECT_EQ(ptr.encoded_size, 0u);
  EXPECT_EQ(ptr.encoding, EncodedPtr::kOmit);
  EXPECT_EQ(EncodedPtr::Type(ptr.encoding), EncodedPtr::kOmit);
  EXPECT_EQ(EncodedPtr::Modifier(ptr.encoding), EncodedPtr::kAbs);
  EXPECT_FALSE(EncodedPtr::Signed(ptr.encoding));
  EXPECT_FALSE(EncodedPtr::Indirect(ptr.encoding));

  EXPECT_EQ(EncodedPtr::Normalize<Elf>(EncodedPtr::kPtr),
            kSize<Addr> == 4 ? EncodedPtr::kUdata4 : EncodedPtr::kUdata8);

  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kOmit, kSize<Addr>), 0u);
  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kPtr, kSize<Addr>), kSize<Addr>);
  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kSigned, kSize<Addr>), kSize<Addr>);
  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kUdata2, kSize<Addr>), 2u);
  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kUdata4, kSize<Addr>), 4u);
  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kUdata8, kSize<Addr>), 8u);
  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kSdata2, kSize<Addr>), 2u);
  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kSdata4, kSize<Addr>), 4u);
  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kSdata8, kSize<Addr>), 8u);
  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kUleb128, kSize<Addr>), EncodedPtr::kDynamicSize);
  EXPECT_EQ(EncodedPtr::EncodedSize(EncodedPtr::kSleb128, kSize<Addr>), EncodedPtr::kDynamicSize);

  EXPECT_EQ(EncodedPtr::Read<Elf>(EncodedPtr::kPtr, kNoBytes), std::nullopt);

  constexpr Addr addr = 0x12345678;
  EXPECT_THAT(
      EncodedPtr::Read<Elf>(EncodedPtr::kPtr, AsBytes(addr)),
      Optional(AllOf(Field(&EncodedPtr::ptr, addr), Field(&EncodedPtr::encoding, EncodedPtr::kPtr),
                     Field(&EncodedPtr::encoded_size, kSize<Addr>))));

  constexpr Half half = 0x1234;
  EXPECT_THAT(EncodedPtr::Read<Elf>(EncodedPtr::kUdata2, AsBytes(half)),
              Optional(AllOf(Field(&EncodedPtr::ptr, half),
                             Field(&EncodedPtr::encoding, EncodedPtr::kUdata2),
                             Field(&EncodedPtr::encoded_size, kSize<Half>))));

  constexpr Word word = 0x12345678;
  EXPECT_THAT(EncodedPtr::Read<Elf>(EncodedPtr::kUdata4, AsBytes(word)),
              Optional(AllOf(Field(&EncodedPtr::ptr, word),
                             Field(&EncodedPtr::encoding, EncodedPtr::kUdata4),
                             Field(&EncodedPtr::encoded_size, kSize<Word>))));

  constexpr Xword xword = 0x12345678abcdef12;
  EXPECT_THAT(EncodedPtr::Read<Elf>(EncodedPtr::kUdata8, AsBytes(xword)),
              Optional(AllOf(Field(&EncodedPtr::ptr, xword),
                             Field(&EncodedPtr::encoding, EncodedPtr::kUdata8),
                             Field(&EncodedPtr::encoded_size, kSize<Xword>))));

  constexpr typename Half::Signed shalf = -0x1234;
  EXPECT_THAT(EncodedPtr::Read<Elf>(EncodedPtr::kSdata2, AsBytes(shalf)),
              Optional(AllOf(Field(&EncodedPtr::sptr, shalf),
                             Field(&EncodedPtr::encoding, EncodedPtr::kSdata2),
                             Field(&EncodedPtr::encoded_size, kSize<Half>))));

  constexpr typename Word::Signed sword = -0x12345678;
  EXPECT_THAT(EncodedPtr::Read<Elf>(EncodedPtr::kSdata4, AsBytes(sword)),
              Optional(AllOf(Field(&EncodedPtr::sptr, sword),
                             Field(&EncodedPtr::encoding, EncodedPtr::kSdata4),
                             Field(&EncodedPtr::encoded_size, kSize<Word>))));

  constexpr typename Xword::Signed sxword = -0x12345678abcdef12;
  EXPECT_THAT(EncodedPtr::Read<Elf>(EncodedPtr::kSdata8, AsBytes(sxword)),
              Optional(AllOf(Field(&EncodedPtr::sptr, sxword),
                             Field(&EncodedPtr::encoding, EncodedPtr::kSdata8),
                             Field(&EncodedPtr::encoded_size, kSize<Xword>))));

  constexpr uint8_t kThreeByte[] = {
      0x80 | (0xaabbu & 0x7f),
      0x80 | ((0xaabbu >> 7) & 0x7f),
      ((0xaabbu >> 14) & 0x7f),
  };
  EXPECT_THAT(EncodedPtr::Read<Elf>(EncodedPtr::kUleb128, AsBytes(kThreeByte)),
              Optional(AllOf(Field(&EncodedPtr::ptr, 0xaabbu),
                             Field(&EncodedPtr::encoding, EncodedPtr::kUleb128),
                             Field(&EncodedPtr::encoded_size, uint8_t{3}))));

  constexpr uint8_t kThreeByteNegative[] = {
      0x80 | (-0xaabb & 0x7f),
      0x80 | ((-0xaabb >> 7) & 0x7f),
      ((-0xaabb >> 14) & 0x7f),
  };
  EXPECT_THAT(EncodedPtr::Read<Elf>(EncodedPtr::kSleb128, AsBytes(kThreeByte)),
              Optional(AllOf(Field(&EncodedPtr::ptr, 0xaabbu),
                             Field(&EncodedPtr::encoding, EncodedPtr::kSleb128),
                             Field(&EncodedPtr::encoded_size, uint8_t{3}))));
  EXPECT_THAT(EncodedPtr::Read<Elf>(EncodedPtr::kSleb128, AsBytes(kThreeByteNegative)),
              Optional(AllOf(Field(&EncodedPtr::ptr, -0xaabb),
                             Field(&EncodedPtr::encoding, EncodedPtr::kSleb128),
                             Field(&EncodedPtr::encoded_size, uint8_t{3}))));

  constexpr std::byte kTruncatedLeb[] = {std::byte{0x80}};
  EXPECT_EQ(EncodedPtr::Read<Elf>(EncodedPtr::kUleb128, kTruncatedLeb), std::nullopt);
  EXPECT_EQ(EncodedPtr::Read<Elf>(EncodedPtr::kSleb128, kTruncatedLeb), std::nullopt);
}

TYPED_TEST(ElfldltlDwarfTests, EncodedPtrFromMemory) {
  using elfldltl::dwarf::EncodedPtr;
  using Elf = typename TestFixture::Elf;
  using Addr = typename Elf::Addr;
  using size_type = typename Elf::size_type;

  static constexpr size_type kMemoryBase = 0x12345000;
  static constexpr size_type kEncodedAt = 0x12345ea0;
  static constexpr size_type kIndirectEncodedAt = 0x12345bb0;
  static constexpr size_type kRelIndirectEncodedAt = 0x12345bc0;
  static constexpr size_type kBadIndirectEncodedAt = 0x12345bd0;
  static constexpr size_type kIndirectedTo = 0x12345120;
  static constexpr size_type kDirectValue = 0xabcdef12;
  static constexpr Addr kIndirectValue = 0x1234abcd;
  static constexpr auto kData = []() {
    std::array<Addr, 0x1000 / sizeof(Addr)> words{};
    for (Addr& word : words) {
      word = 0xdeadbeef;
    }
    auto set = [&words](size_type addr, Addr value) {
      words[(addr - kMemoryBase) / sizeof(Addr)] = value;
    };
    set(kEncodedAt, kDirectValue);
    set(kIndirectEncodedAt, kIndirectedTo);
    set(kRelIndirectEncodedAt, kIndirectedTo - kRelIndirectEncodedAt);
    set(kBadIndirectEncodedAt, 0xbad1230 - kBadIndirectEncodedAt);
    set(kIndirectedTo, kIndirectValue);
    return words;
  }();
  const std::span<std::byte> kImage{
      const_cast<std::byte*>(std::as_bytes(std::span{kData}).data()),
      std::span(kData).size_bytes(),
  };

  elfldltl::DirectMemory memory{kImage, kMemoryBase};

  EXPECT_EQ(EncodedPtr::FromMemory<Elf>(EncodedPtr::kPtr, memory, 0xbad10000, kSize<Addr>),
            std::nullopt);

  EXPECT_THAT(EncodedPtr::FromMemory<Elf>(EncodedPtr::kPtr, memory, kEncodedAt, kSize<Addr>),
              Optional(kDirectValue));

  EXPECT_THAT(
      EncodedPtr::FromMemory<Elf>(static_cast<uint8_t>(EncodedPtr::kPtr) | EncodedPtr::kPcrel,
                                  memory, kEncodedAt, kSize<Addr>),
      Optional(kEncodedAt + kDirectValue));

  EXPECT_THAT(EncodedPtr::FromMemory<Elf>(EncodedPtr::kPtr | EncodedPtr::kIndirect, memory,
                                          kIndirectEncodedAt, kSize<Addr>),
              Optional(kIndirectValue));

  EXPECT_THAT(
      EncodedPtr::FromMemory<Elf>(EncodedPtr::kPtr | EncodedPtr::kIndirect | EncodedPtr::kPcrel,
                                  memory, kRelIndirectEncodedAt, kSize<Addr>),
      Optional(kIndirectValue));

  EXPECT_EQ(EncodedPtr::FromMemory<Elf>(EncodedPtr::kPtr | EncodedPtr::kIndirect, memory,
                                        kBadIndirectEncodedAt, kSize<Addr>),
            std::nullopt);

  // Test dynamic-sized LEB128 reads from a small buffer (< kDynamicSize bytes).
  constexpr uint8_t kSmallLeb[] = {0x80 | (0x1234u & 0x7f), (0x1234u >> 7) & 0x7f};
  std::byte small_buf[sizeof(kSmallLeb)];
  memcpy(small_buf, kSmallLeb, sizeof(kSmallLeb));
  elfldltl::DirectMemory small_memory{small_buf, 0x2000};
  EXPECT_THAT(EncodedPtr::FromMemory<Elf>(EncodedPtr::kUleb128, small_memory, 0x2000),
              Optional(uint64_t{0x1234}));
  EXPECT_THAT(
      EncodedPtr::FromMemory<Elf>(EncodedPtr::kUleb128 | EncodedPtr::kPcrel, small_memory, 0x2000),
      Optional(uint64_t{0x3234}));
}

TYPED_TEST(ElfldltlDwarfTests, EhFrameHdr) {
  using Elf = typename TestFixture::Elf;
  using size_type = typename Elf::size_type;
  using Word = typename Elf::Word;
  using Phdr = typename Elf::Phdr;
  using value_type = elfldltl::dwarf::EhFrameHdrEntry<size_type>;
  using EhFrameHdr = elfldltl::dwarf::EhFrameHdr<Elf>;

  static_assert(std::ranges::random_access_range<EhFrameHdr>);
  static_assert(std::same_as<value_type, std::ranges::range_value_t<EhFrameHdr>>);

  EhFrameHdr eh_frame_hdr;
  EXPECT_TRUE(eh_frame_hdr.empty());

  auto diag = elfldltl::testing::ExpectOkDiagnostics();

  constexpr std::array kEntries{
      value_type{0x1000, 0x370},
      value_type{0x1005, 0x3f0},
      value_type{0x1011, 0x46c},
  };

  std::stringstream os;
  for (const value_type& entry : kEntries) {
    os << entry << "\n";
  }
  EXPECT_EQ(os.str(), R"""([PC 0x1000 -> FDE 0x370]
[PC 0x1005 -> FDE 0x3f0]
[PC 0x1011 -> FDE 0x46c]
)""");

  constexpr size_type kVaddr = 0x334;
  constexpr size_type kEhFrameVaddr = 0x358;
  static constexpr std::array<Word, 9> kData = {
      Word{std::array{
          std::byte{0x01},  // version 1
          std::byte{0x1b},  // eh_frame_ptr sdata pcrel
          std::byte{0x03},  // fde_count udata4
          std::byte{0x3b},  // fde_table sdata4 datarel
      }},
      0x20,  // eh_frame_ptr
      3,     // FDE count
      0xccc,
      0x3c,
      0xcd1,
      0xbc,
      0xcdd,
      0x138,
  };
  const std::span<std::byte> kImage{
      const_cast<std::byte*>(std::as_bytes(std::span{kData}).data()),
      std::span(kData).size_bytes(),
  };

  elfldltl::DirectMemory memory{kImage, kVaddr};

  constexpr Phdr kPhdr = {
      .type = elfldltl::ElfPhdrType::kEhFrameHdr,
      .vaddr = kVaddr,
      .filesz = sizeof(kData),
  };

  ASSERT_TRUE(eh_frame_hdr.Init(diag, memory, kPhdr));

  EXPECT_EQ(eh_frame_hdr.eh_frame_ptr(), kEhFrameVaddr);

  EXPECT_EQ(eh_frame_hdr.size(), 3u);
  EXPECT_EQ(eh_frame_hdr.size_bytes(), sizeof(kData));

  const EhFrameHdr& const_hdr = eh_frame_hdr;
  EXPECT_EQ(const_hdr[0], kEntries[0]);
  EXPECT_EQ(const_hdr[1], kEntries[1]);
  EXPECT_EQ(const_hdr[2], kEntries[2]);
  EXPECT_EQ(const_hdr.begin()[2], kEntries[2]);
  EXPECT_EQ(*(const_hdr.end() - 1), kEntries[2]);
  EXPECT_EQ(*(const_hdr.end() - 2), kEntries[1]);
  EXPECT_EQ(*(const_hdr.end() - 3), kEntries[0]);

  EXPECT_EQ(const_hdr.find(0x0fff), const_hdr.end());
  EXPECT_EQ(const_hdr.find(0x1000), const_hdr.begin());
  EXPECT_EQ(const_hdr.find(0x1004), const_hdr.begin());
  EXPECT_EQ(const_hdr.find(0x1005), const_hdr.begin() + 1);
  EXPECT_EQ(const_hdr.find(0x1010), const_hdr.begin() + 1);
  EXPECT_EQ(const_hdr.find(0x1011), const_hdr.begin() + 2);
  EXPECT_EQ(const_hdr.find(0x2000), const_hdr.begin() + 2);

  EXPECT_THAT(eh_frame_hdr, ElementsAreArray(kEntries));
}

constexpr uint32_t kFdeVaddr = 0x370;

template <class Elf>
struct [[gnu::packed]] TestEhFrame {
  using Addr = typename Elf::Addr;
  using Word = typename Elf::Word;

  struct TestCie {
    [[gnu::packed]] Word cie_id = elfldltl::dwarf::kEhFrameCieId;
    uint8_t version = 1;
    char augmentation[4] = "zLR";
    uint8_t code_alignment_factor = 1;
    uint8_t data_alignment_factor = -8 & 0x7f;
    uint8_t return_address_register = 16;
    uint8_t augmentation_data[3] = {2, 0, 0x1b};
    uint8_t instructions[5] = {0x0c, 0x07, 0x08, 0x90, 0x01};
  };
  using CieData = TestData<InitialLength32<Elf>, TestCie>;

  struct TestFde {
    [[gnu::packed]] Word cie_pointer = sizeof(CieData) + sizeof(Word);
    [[gnu::packed]] Word initial_location =  // sdata4 | pcrel encoded:
        0x1000 -                             // Absolute address.
        (kFdeVaddr +                         // FDE start address.
         sizeof(Word) +                      // Initial length
         sizeof(Word));                      // CIE_pointer
    [[gnu::packed]] Word address_range = 5;
    uint8_t augmentation_data_size = sizeof(Addr);
    [[gnu::packed]] Addr lsda = 0x534c55;
    uint8_t instructions[3] = {0x08, 0x00, 0};
  };
  using FdeData = TestData<InitialLength32<Elf>, TestFde>;

  [[gnu::packed]] CieData cie;
  [[gnu::packed]] FdeData fde;
};

template <class Elf>
constexpr TestEhFrame<Elf> kTestEhFrame{};

TYPED_TEST(ElfldltlDwarfTests, CfiEntry) {
  using Elf = typename TestFixture::Elf;

  auto diag = elfldltl::testing::ExpectOkDiagnostics();

  std::span kData = AsBytes(kTestEhFrame<Elf>);
  const std::span<std::byte> kImage{
      const_cast<std::byte*>(kData.data()),
      std::span(kData).size_bytes(),
  };
  elfldltl::DirectMemory memory{kImage, 0x358};

  auto fde = elfldltl::dwarf::CfiEntry::ReadEhFrameFromMemory<Elf>(diag, memory, kFdeVaddr);
  ASSERT_TRUE(fde);

  auto cie = fde->template ReadEhFrameCieFromMemory<Elf>(diag, memory);
  ASSERT_TRUE(cie);

  auto cie_info = cie->template DecodeCie<Elf>(diag, *fde->cie_pointer());
  ASSERT_TRUE(cie_info);

  EXPECT_EQ(cie_info->return_address_register, 16u);
  EXPECT_EQ(cie_info->initial_instructions.size_bytes(), 5u);
  EXPECT_FALSE(cie_info->signal_frame);

  auto fde_info = fde->template DecodeFde<Elf>(diag, kFdeVaddr, *cie_info);
  ASSERT_TRUE(fde_info);

  EXPECT_EQ(fde_info->initial_location, 0x1000u);
  EXPECT_EQ(fde_info->address_range, 5u);
  EXPECT_EQ(fde_info->lsda, 0x534c55u);
  EXPECT_EQ(fde_info->instructions.size_bytes(), 3u);
}

TYPED_TEST(ElfldltlDwarfTests, CfiEntryPersonalityAndPcrelLsda) {
  using Elf = typename TestFixture::Elf;
  using Word = typename Elf::Word;
  using EncodedPtr = elfldltl::dwarf::EncodedPtr;

  constexpr uint64_t kCieVaddr = 0x500;
  constexpr uint64_t kPersonalityVaddr = 0x2000;
  constexpr uint64_t kLsdaVaddr = 0x3000;

  struct [[gnu::packed]] PlrCie {
    [[gnu::packed]] Word cie_id = elfldltl::dwarf::kEhFrameCieId;
    uint8_t version = 4;
    char augmentation[5] = "zPLR";
    uint8_t address_size = 4;
    uint8_t segment_selector_size = 0;
    uint8_t code_alignment_factor = 1;
    uint8_t data_alignment_factor = -8 & 0x7f;
    uint8_t return_address_register = 16;
    uint8_t augmentation_size = 1 + sizeof(Word) + 1 + 1;
    uint8_t personality_encoding = EncodedPtr::kSdata4 | EncodedPtr::kPcrel;
    // Offset of personality field from start of CIEData:
    // 4 (initial_length) + 4 (cie_id) + 1 (ver) + 5 ("zPLR\0") + 2 (addr/seg) +
    // 3 (factors+rar) + 1 (aug_size) + 1 (personality_encoding) = 21.
    [[gnu::packed]] typename Word::Signed personality =
        static_cast<int32_t>(kPersonalityVaddr - (kCieVaddr + 21));
    uint8_t lsda_encoding = EncodedPtr::kSdata4 | EncodedPtr::kPcrel;
    uint8_t fde_encoding = EncodedPtr::kSdata4 | EncodedPtr::kPcrel;
    uint8_t instructions[2] = {0x0c, 0x07};
  };
  using CieData = TestData<InitialLength32<Elf>, PlrCie>;
  constexpr uint64_t kPlrFdeVaddr = kCieVaddr + sizeof(CieData);

  struct [[gnu::packed]] PlrFde {
    [[gnu::packed]] Word cie_pointer = sizeof(CieData) + sizeof(Word);
    [[gnu::packed]] typename Word::Signed initial_location =
        static_cast<int32_t>(0x1500 - (kPlrFdeVaddr + 8));
    [[gnu::packed]] Word address_range = 0x20;
    uint8_t augmentation_size = sizeof(Word);
    // Offset of lsda field from start of FdeData:
    // 4 (initial_length) + 4 (cie_pointer) + 4 (initial_location) + 4 (range) +
    // 1 (augmentation_size) = 17.
    [[gnu::packed]] typename Word::Signed lsda =
        static_cast<int32_t>(kLsdaVaddr - (kPlrFdeVaddr + 17));
    uint8_t instructions[1] = {0};
  };
  using FdeData = TestData<InitialLength32<Elf>, PlrFde>;

  struct [[gnu::packed]] Frame {
    CieData cie;
    FdeData fde;
  } constexpr kFrame{};

  auto diag = elfldltl::testing::ExpectOkDiagnostics();
  std::span kData = AsBytes(kFrame);
  elfldltl::DirectMemory memory{
      {const_cast<std::byte*>(kData.data()), kData.size_bytes()},
      kCieVaddr,
  };

  const elfldltl::dwarf::SectionData const_sec{kData.subspan(4),
                                               elfldltl::dwarf::SectionData::Format::kDwarf32};
  EXPECT_EQ(const_sec.read_offset<Elf>(0), uint64_t{0});

  auto fde = elfldltl::dwarf::CfiEntry::ReadEhFrameFromMemory<Elf>(diag, memory, kPlrFdeVaddr);
  ASSERT_TRUE(fde);
  auto cie = fde->template ReadEhFrameCieFromMemory<Elf>(diag, memory);
  ASSERT_TRUE(cie);

  auto cie_info = cie->template DecodeCie<Elf>(diag, kCieVaddr);
  ASSERT_TRUE(cie_info);
  EXPECT_EQ(cie_info->personality, kPersonalityVaddr);
  EXPECT_EQ(cie_info->lsda_encoding, uint8_t{EncodedPtr::kSdata4 | EncodedPtr::kPcrel});
  EXPECT_EQ(cie_info->fde_encoding, uint8_t{EncodedPtr::kSdata4 | EncodedPtr::kPcrel});
  EXPECT_EQ(cie_info->initial_instructions.size_bytes(), 2u);

  auto fde_info = fde->template DecodeFde<Elf>(diag, kPlrFdeVaddr, *cie_info);
  ASSERT_TRUE(fde_info);
  EXPECT_EQ(fde_info->initial_location, 0x1500u);
  EXPECT_EQ(fde_info->address_range, 0x20u);
  EXPECT_EQ(fde_info->lsda, kLsdaVaddr);
  EXPECT_EQ(fde_info->instructions.size_bytes(), 1u);

  // Also test truncated non-'z' FDE augmentation data bounds check.
  elfldltl::dwarf::CfiCie unsized_cie = *cie_info;
  unsized_cie.fde_augmentation_sized = false;
  unsized_cie.fde_augmentation_data_size = 64;
  elfldltl::testing::ExpectedSingleError trunc_err{
      "DWARF CFI has invalid encoding ",
      unsigned{unsized_cie.lsda_encoding},
      " or truncated FDE",
      elfldltl::FileAddress{kPlrFdeVaddr},
  };
  EXPECT_EQ(fde->template DecodeFde<Elf>(trunc_err, kPlrFdeVaddr, unsized_cie), std::nullopt);
}

}  // namespace
