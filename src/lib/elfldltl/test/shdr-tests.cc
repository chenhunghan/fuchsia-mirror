// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/elfldltl/constants.h>
#include <lib/elfldltl/diagnostics.h>
#include <lib/elfldltl/layout.h>
#include <lib/elfldltl/memory.h>
#include <lib/elfldltl/shdr.h>
#include <lib/elfldltl/testing/diagnostics.h>
#include <lib/elfldltl/testing/typed-test.h>

#include <array>
#include <cstddef>
#include <span>
#include <string_view>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

namespace {

using ::testing::ElementsAre;
using ::testing::Pair;

FORMAT_TYPED_TEST_SUITE(ElfldltlShdrTests);

template <class Elf>
constexpr Elf::Ehdr MakeEhdr() {
  using Ehdr = Elf::Ehdr;
  return Ehdr{
      .magic = Ehdr::kMagic,
      .elfclass = Elf::kClass,
      .elfdata = Elf::kData,
      .ident_version = elfldltl::ElfVersion::kCurrent,
      .version = elfldltl::ElfVersion::kCurrent,
      .ehsize = sizeof(Ehdr),
  };
}

TYPED_TEST(ElfldltlShdrTests, ReadShdrsFromFileBadSize) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  elfldltl::testing::ExpectedSingleError diag("e_shentsize has unexpected value",
                                              static_cast<uint16_t>(sizeof(Shdr) + 1));
  Ehdr ehdr = MakeEhdr<Elf>();
  ehdr.shentsize = sizeof(Shdr) + 1;
  ehdr.shnum = 1;
  elfldltl::DirectMemory file;
  auto result = elfldltl::ReadShdrsFromFile(diag, file, elfldltl::NoArrayFromFile<Shdr>(), ehdr);
  EXPECT_FALSE(result.has_value());
}

TYPED_TEST(ElfldltlShdrTests, ReadShdrsFromFileBadOffset) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  using size_type = Elf::size_type;

  elfldltl::testing::ExpectedSingleError diag("e_shoff overlaps with ELF file header",
                                              elfldltl::FileOffset{size_type{0}});
  Ehdr ehdr = MakeEhdr<Elf>();
  ehdr.shoff = 0;
  ehdr.shentsize = sizeof(Shdr);
  ehdr.shnum = 1;
  elfldltl::DirectMemory file;
  auto result = elfldltl::ReadShdrsFromFile(diag, file, elfldltl::NoArrayFromFile<Shdr>(), ehdr);
  EXPECT_FALSE(result.has_value());
}

TYPED_TEST(ElfldltlShdrTests, ReadShdrsFromFileBadAlign) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  elfldltl::testing::ExpectedSingleError diag(
      "e_shoff has insufficient alignment",
      elfldltl::FileOffset{static_cast<Elf::size_type>(sizeof(Ehdr) + 1)});
  Ehdr ehdr = MakeEhdr<Elf>();
  ehdr.shoff = sizeof(Ehdr) + 1;
  ehdr.shentsize = sizeof(Shdr);
  ehdr.shnum = 1;
  elfldltl::DirectMemory file;
  auto result = elfldltl::ReadShdrsFromFile(diag, file, elfldltl::NoArrayFromFile<Shdr>(), ehdr);
  EXPECT_FALSE(result.has_value());
}

TYPED_TEST(ElfldltlShdrTests, ReadShdrsFromFileCantReadShdr) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  elfldltl::testing::ExpectedSingleError diag(
      "cannot read section headers from ELF file",
      elfldltl::FileOffset{static_cast<Elf::size_type>(sizeof(Ehdr))});
  Ehdr ehdr = MakeEhdr<Elf>();
  ehdr.shoff = sizeof(Ehdr);
  ehdr.shentsize = sizeof(Shdr);
  ehdr.shnum = 1;
  elfldltl::DirectMemory file;
  auto result = elfldltl::ReadShdrsFromFile(diag, file, elfldltl::NoArrayFromFile<Shdr>(), ehdr);
  EXPECT_FALSE(result.has_value());
}

TYPED_TEST(ElfldltlShdrTests, ReadShdrsFromFileExtendedCantReadShdr0) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  elfldltl::testing::ExpectedSingleError diag(
      "cannot read section header 0 from ELF file",
      elfldltl::FileOffset{static_cast<Elf::size_type>(sizeof(Ehdr))});
  Ehdr ehdr = MakeEhdr<Elf>();
  ehdr.shoff = sizeof(Ehdr);
  ehdr.shentsize = sizeof(Shdr);
  ehdr.shnum = 0;
  elfldltl::DirectMemory file;
  auto result = elfldltl::ReadShdrsFromFile(diag, file, elfldltl::NoArrayFromFile<Shdr>(), ehdr);
  EXPECT_FALSE(result.has_value());
}

TYPED_TEST(ElfldltlShdrTests, ReadShdrsFromFileNoShdrs) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  elfldltl::testing::ExpectOkDiagnostics diag;
  Ehdr ehdr = MakeEhdr<Elf>();
  ehdr.shoff = 0;
  ehdr.shnum = 0;
  elfldltl::DirectMemory file;
  auto result = elfldltl::ReadShdrsFromFile(diag, file, elfldltl::NoArrayFromFile<Shdr>(), ehdr);
  ASSERT_TRUE(result.has_value());
  std::span<const Shdr> shdrs = *result;
  EXPECT_TRUE(shdrs.empty());
}

TYPED_TEST(ElfldltlShdrTests, ReadShdrsFromFileExtendedZeroSize) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  struct {
    Ehdr ehdr;
    Shdr shdrs[1];
  } elfbytes{
      .ehdr = MakeEhdr<Elf>(),
      .shdrs = {{.size = 0}},
  };
  elfbytes.ehdr.shoff = sizeof(Ehdr);
  elfbytes.ehdr.shentsize = sizeof(Shdr);
  elfbytes.ehdr.shnum = 0;

  elfldltl::DirectMemory file{std::as_writable_bytes(std::span{&elfbytes, 1})};
  elfldltl::testing::ExpectOkDiagnostics diag;
  auto result =
      elfldltl::ReadShdrsFromFile(diag, file, elfldltl::NoArrayFromFile<Shdr>(), elfbytes.ehdr);
  ASSERT_TRUE(result.has_value());
  std::span<const Shdr> shdrs = *result;
  EXPECT_TRUE(shdrs.empty());
}

TYPED_TEST(ElfldltlShdrTests, ReadShdrsFromFile) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  struct {
    Ehdr ehdr;
    Shdr shdrs[2];
  } elfbytes{
      .ehdr = MakeEhdr<Elf>(),
      .shdrs =
          {
              {.type = elfldltl::ElfShdrType::kNull},
              {.name = 1, .type = elfldltl::ElfShdrType::kStrtab},
          },
  };
  elfbytes.ehdr.shoff = sizeof(Ehdr);
  elfbytes.ehdr.shentsize = sizeof(Shdr);
  elfbytes.ehdr.shnum = 2;

  elfldltl::DirectMemory file{std::as_writable_bytes(std::span{&elfbytes, 1})};
  elfldltl::testing::ExpectOkDiagnostics diag;
  auto result =
      elfldltl::ReadShdrsFromFile(diag, file, elfldltl::NoArrayFromFile<Shdr>(), elfbytes.ehdr);
  ASSERT_TRUE(result.has_value());
  std::span<const Shdr> shdrs = *result;
  ASSERT_EQ(shdrs.size(), 2u);
  EXPECT_EQ(shdrs[0].type, elfldltl::ElfShdrType::kNull);
  EXPECT_EQ(shdrs[1].name, 1u);
  EXPECT_EQ(shdrs[1].type, elfldltl::ElfShdrType::kStrtab);
}

TYPED_TEST(ElfldltlShdrTests, ReadShdrsFromFileExtendedCount) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  struct {
    Ehdr ehdr;
    Shdr shdrs[2];
  } elfbytes{
      .ehdr = MakeEhdr<Elf>(),
      .shdrs =
          {
              {.type = elfldltl::ElfShdrType::kNull, .size = 2},
              {.name = 7, .type = elfldltl::ElfShdrType::kProgbits},
          },
  };
  elfbytes.ehdr.shoff = sizeof(Ehdr);
  elfbytes.ehdr.shentsize = sizeof(Shdr);
  elfbytes.ehdr.shnum = 0;

  elfldltl::DirectMemory file{std::as_writable_bytes(std::span{&elfbytes, 1})};
  elfldltl::testing::ExpectOkDiagnostics diag;
  auto result =
      elfldltl::ReadShdrsFromFile(diag, file, elfldltl::NoArrayFromFile<Shdr>(), elfbytes.ehdr);
  ASSERT_TRUE(result.has_value());
  std::span<const Shdr> shdrs = *result;
  ASSERT_EQ(shdrs.size(), 2u);
  EXPECT_EQ(shdrs[0].size(), 2u);
  EXPECT_EQ(shdrs[1].name, 7u);
}

TYPED_TEST(ElfldltlShdrTests, LoadAndWithShdrsFromFile) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  // Use ET_REL to verify LoadShdrsFromFile and WithShdrsFromFile accept
  // non-ET_DYN valid ELF files (unlike LoadHeadersFromFile / WithLoadHeadersFromFile).
  struct {
    Ehdr ehdr;
    Shdr shdrs[2];
  } elfbytes{
      .ehdr = MakeEhdr<Elf>(),
      .shdrs =
          {
              {.type = elfldltl::ElfShdrType::kNull},
              {.name = 1, .type = elfldltl::ElfShdrType::kProgbits},
          },
  };
  elfbytes.ehdr.type = elfldltl::ElfType::kRel;
  elfbytes.ehdr.machine = elfldltl::ElfMachine::kAarch64;
  elfbytes.ehdr.shoff = sizeof(Ehdr);
  elfbytes.ehdr.shentsize = sizeof(Shdr);
  elfbytes.ehdr.shnum = 2;

  elfldltl::DirectMemory file{std::as_writable_bytes(std::span{&elfbytes, 1})};

  {
    elfldltl::testing::ExpectOkDiagnostics diag;
    auto headers = elfldltl::LoadShdrsFromFile<Elf>(diag, file, elfldltl::NoArrayFromFile<Shdr>());
    ASSERT_TRUE(headers.has_value());
    auto [ehdr_owner, shdrs_owner] = *headers;
    const Ehdr& ehdr = ehdr_owner;
    const std::span<const Shdr> shdrs = shdrs_owner;
    EXPECT_EQ(ehdr.type, elfldltl::ElfType::kRel);
    ASSERT_EQ(shdrs.size(), 2u);
    EXPECT_EQ(shdrs[1].name, 1u);
  }

  {
    elfldltl::testing::ExpectedSingleError diag("wrong e_machine for architecture");
    auto headers = elfldltl::LoadShdrsFromFile<Elf>(diag, file, elfldltl::NoArrayFromFile<Shdr>(),
                                                    elfldltl::ElfMachine::kX86_64);
    EXPECT_FALSE(headers.has_value());
  }

  {
    elfldltl::testing::ExpectOkDiagnostics diag;
    bool callback_called = false;
    auto meta_allocator = []<typename T>(size_t count) {
      return elfldltl::NoArrayFromFile<T>()(count);
    };
    bool ok = elfldltl::WithShdrsFromFile(
        diag, file, meta_allocator,
        [&](const auto& ehdr, auto shdrs) {
          using CallbackElf = std::decay_t<decltype(ehdr)>::ElfLayout;
          if constexpr (std::same_as<CallbackElf, Elf>) {
            callback_called = true;
            EXPECT_EQ(shdrs.size(), 2u);
            EXPECT_EQ(shdrs[1].name, 1u);
            return true;
          } else {
            ADD_FAILURE() << "unexpected Elf format dispatched";
            return false;
          }
        },
        std::nullopt, elfldltl::ElfMachine::kAarch64);
    EXPECT_TRUE(ok);
    EXPECT_TRUE(callback_called);
  }
}

TYPED_TEST(ElfldltlShdrTests, ReadSectionAndShstrtabAndShdrWithName) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  static constexpr char kShstrtab[] = "\0.shstrtab\0.rodata";
  static constexpr std::array<uint32_t, 2> kRodata = {0x11223344, 0x55667788};

  struct ElfImage {
    Ehdr ehdr;
    Shdr shdrs[4];
    char shstrtab[sizeof(kShstrtab)];
    std::array<uint32_t, 2> rodata;
  } elfbytes{
      .ehdr = MakeEhdr<Elf>(),
      .shdrs =
          {
              {.name = 0, .type = elfldltl::ElfShdrType::kNull},
              {
                  .name = 1,
                  .type = elfldltl::ElfShdrType::kStrtab,
                  .offset = offsetof(ElfImage, shstrtab),
                  .size = sizeof(kShstrtab),
              },
              {
                  .name = 11,
                  .type = elfldltl::ElfShdrType::kProgbits,
                  .offset = offsetof(ElfImage, rodata),
                  .size = sizeof(kRodata),
              },
              {
                  .name = 999,
                  .type = elfldltl::ElfShdrType::kNull,
              },
          },
      .shstrtab = {},
      .rodata = kRodata,
  };
  elfbytes.ehdr.shoff = offsetof(ElfImage, shdrs);
  elfbytes.ehdr.shentsize = sizeof(Shdr);
  elfbytes.ehdr.shnum = 4;
  elfbytes.ehdr.shstrndx = 1;
  std::ranges::copy(kShstrtab, elfbytes.shstrtab);

  elfldltl::DirectMemory file{std::as_writable_bytes(std::span{&elfbytes, 1})};
  auto read_array = [&file]<typename T>(auto offset, size_t count) {
    return file.template ReadArrayFromFile<T>(offset, elfldltl::NoArrayFromFile<T>(), count);
  };

  elfldltl::testing::ExpectOkDiagnostics diag;
  auto headers = elfldltl::LoadShdrsFromFile<Elf>(diag, file, elfldltl::NoArrayFromFile<Shdr>());
  ASSERT_TRUE(headers.has_value());
  auto [ehdr, shdrs_owner] = *headers;
  std::span<const Shdr> shdrs = shdrs_owner;

  auto shstrtab_owner = elfldltl::ReadShstrtab(diag, ehdr, shdrs, [&](auto offset, size_t count) {
    return read_array.template operator()<char>(offset, count);
  });
  ASSERT_TRUE(shstrtab_owner.has_value());
  std::span<const char> shstrtab = *shstrtab_owner;

  std::vector<std::pair<std::string_view, elfldltl::ElfShdrType>> named;
  for (auto [name, shdr] : shdrs | elfldltl::ShdrWithName(shstrtab)) {
    named.emplace_back(name, shdr.type);
  }
  EXPECT_THAT(named, ElementsAre(Pair("", elfldltl::ElfShdrType::kNull),
                                 Pair(".shstrtab", elfldltl::ElfShdrType::kStrtab),
                                 Pair(".rodata", elfldltl::ElfShdrType::kProgbits),
                                 Pair("", elfldltl::ElfShdrType::kNull)));

  auto rodata_owner = elfldltl::ReadSection<uint32_t>(shdrs[2], [&](auto offset, size_t count) {
    return read_array.template operator()<uint32_t>(offset, count);
  });
  ASSERT_TRUE(rodata_owner.has_value());
  EXPECT_THAT(*rodata_owner, ElementsAre(kRodata[0], kRodata[1]));
}

TYPED_TEST(ElfldltlShdrTests, ReadShstrtabErrorsAndShnXindex) {
  using Elf = TestFixture::Elf;
  using Ehdr = Elf::Ehdr;
  using Shdr = Elf::Shdr;

  static constexpr char kGoodShstrtab[] = "\0.shstrtab";
  static constexpr char kBadShstrtab[] = {'.', 'f', 'o', 'o'};

  struct ElfImage {
    Ehdr ehdr;
    Shdr shdrs[2];
    char good[sizeof(kGoodShstrtab)];
    char bad[sizeof(kBadShstrtab)];
  } elfbytes{
      .ehdr = MakeEhdr<Elf>(),
      .shdrs =
          {
              {.type = elfldltl::ElfShdrType::kNull, .link = 1},
              {
                  .name = 1,
                  .type = elfldltl::ElfShdrType::kStrtab,
                  .offset = offsetof(ElfImage, good),
                  .size = sizeof(kGoodShstrtab),
              },
          },
      .good = {},
      .bad = {},
  };
  elfbytes.ehdr.shstrndx = Ehdr::kShnXindex;
  std::ranges::copy(kGoodShstrtab, elfbytes.good);
  std::ranges::copy(kBadShstrtab, elfbytes.bad);

  elfldltl::DirectMemory file{std::as_writable_bytes(std::span{&elfbytes, 1})};
  auto read_chars = [&file](auto offset, size_t count) {
    return file.template ReadArrayFromFile<char>(offset, elfldltl::NoArrayFromFile<char>(), count);
  };

  // SHN_XINDEX success.
  {
    elfldltl::testing::ExpectOkDiagnostics diag;
    auto res = elfldltl::ReadShstrtab(diag, elfbytes.ehdr, std::span{elfbytes.shdrs}, read_chars);
    ASSERT_TRUE(res.has_value());
    EXPECT_EQ((std::string_view{res->data(), res->size()}),
              (std::string_view{kGoodShstrtab, sizeof(kGoodShstrtab)}));
  }

  // SHN_XINDEX with no sections.
  {
    elfldltl::testing::ExpectedSingleError diag(
        "ELF header has SHN_XINDEX in e_shstrndx with no sections");
    auto res = elfldltl::ReadShstrtab(diag, elfbytes.ehdr, std::span<const Shdr>{}, read_chars);
    EXPECT_FALSE(res.has_value());
  }

  // e_shstrndx == 0.
  {
    Ehdr ehdr_zero = MakeEhdr<Elf>();
    ehdr_zero.shstrndx = 0;
    elfldltl::testing::ExpectedSingleError diag("ELF header has no e_shstrndx");
    auto res = elfldltl::ReadShstrtab(diag, ehdr_zero, std::span{elfbytes.shdrs}, read_chars);
    EXPECT_FALSE(res.has_value());
  }

  // Out-of-bounds e_shstrndx.
  {
    Ehdr ehdr_oob = MakeEhdr<Elf>();
    ehdr_oob.shstrndx = 5;
    elfldltl::testing::ExpectedSingleError diag("ELF header has invalid e_shstrndx ", uint32_t{5},
                                                " with only ", size_t{2}, " sections");
    auto res = elfldltl::ReadShstrtab(diag, ehdr_oob, std::span{elfbytes.shdrs}, read_chars);
    EXPECT_FALSE(res.has_value());
  }

  // Empty .shstrtab.
  {
    Ehdr ehdr1 = MakeEhdr<Elf>();
    ehdr1.shstrndx = 1;
    Shdr shdrs_empty[2] = {
        {},
        {.offset = offsetof(ElfImage, good), .size = 0},
    };
    elfldltl::testing::ExpectedSingleError diag("e_shstrndx section is empty");
    auto res = elfldltl::ReadShstrtab(diag, ehdr1, std::span{shdrs_empty}, read_chars);
    EXPECT_FALSE(res.has_value());
  }

  // Non-NUL-terminated .shstrtab.
  {
    Ehdr ehdr1 = MakeEhdr<Elf>();
    ehdr1.shstrndx = 1;
    Shdr shdrs_bad[2] = {
        {},
        {.offset = offsetof(ElfImage, bad), .size = sizeof(kBadShstrtab)},
    };
    elfldltl::testing::ExpectedSingleError diag("e_shstrndx section is not NUL-terminated");
    auto res = elfldltl::ReadShstrtab(diag, ehdr1, std::span{shdrs_bad}, read_chars);
    EXPECT_FALSE(res.has_value());
  }

  // Unreadable .shstrtab.
  {
    Ehdr ehdr1 = MakeEhdr<Elf>();
    ehdr1.shstrndx = 1;
    Shdr shdrs_eof[2] = {
        {},
        {.offset = sizeof(ElfImage) + 100, .size = 8},
    };
    elfldltl::testing::ExpectedSingleError diag("cannot read .shstrtab section from ELF file");
    auto res = elfldltl::ReadShstrtab(diag, ehdr1, std::span{shdrs_eof}, read_chars);
    EXPECT_FALSE(res.has_value());
  }
}

}  // namespace
