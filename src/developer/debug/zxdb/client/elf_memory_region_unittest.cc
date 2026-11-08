// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/elf_memory_region.h"

#include <string.h>

#include <filesystem>
#include <limits>
#include <optional>
#include <vector>

#include <gtest/gtest.h>
#include <llvm/BinaryFormat/ELF.h>

#include "src/developer/debug/zxdb/common/host_util.h"
#include "src/lib/elflib/elflib.h"

namespace zxdb {

namespace {
std::filesystem::path GetTestElfFile() {
  return std::filesystem::path(GetSelfPath()).parent_path() /
         "test_data/zxdb/libzxdb_symbol_test.targetso";
}

struct CompressedSectionInfo {
  uint64_t file_offset = 0;
  uint64_t decompressed_size = 0;
};

std::optional<CompressedSectionInfo> GetCompressedSectionInfo(elflib::ElfLib* elflib,
                                                              const char* target_section_name) {
  auto data = elflib->GetSectionData(".shstrtab");
  for (const auto& shdr : elflib->GetSectionHeaders()) {
    const char* section_name = reinterpret_cast<const char*>(&data.ptr[shdr.sh_name]);
    if (strcmp(section_name, target_section_name) == 0) {
      if ((shdr.sh_flags & elflib::SHF_COMPRESSED) == 0) {
        return std::nullopt;
      }
      auto section_data = elflib->GetSectionData(target_section_name);
      elflib::Elf64_Chdr chdr;
      memcpy(&chdr, section_data.ptr, sizeof(chdr));
      return CompressedSectionInfo{
          .file_offset = shdr.sh_offset,
          .decompressed_size = chdr.ch_size,
      };
    }
  }
  return std::nullopt;
}

TEST(ElfMemoryRegion, RandomAccess) {
  // Fake load address that will be offset for any addresses we want to read from the ELF file.
  constexpr uint64_t kLoadAddress = 0x1000;

  auto elflib = elflib::ElfLib::Create(GetTestElfFile());
  ElfMemoryRegion elf_memory_region(kLoadAddress, GetTestElfFile());

  // We should be able to transparently read the ELF header.
  elflib::Elf64_Ehdr ehdr_read;
  elf_memory_region.ReadBytes(kLoadAddress, sizeof(ehdr_read), &ehdr_read);
  ASSERT_TRUE(ehdr_read.checkMagic());

  auto ehdr = elflib->GetEhdr();

  // The ELF header we read should be identical to elflib's.
  EXPECT_EQ(memcmp(ehdr, &ehdr_read, sizeof(ehdr_read)), 0);
}

}  // namespace

TEST(ElfMemoryRegion, CompressedAccess) {
  constexpr uint64_t kLoadAddress = 0xdeadbeef;

  auto elflib = elflib::ElfLib::Create(GetTestElfFile());
  ElfMemoryRegion elf_memory_region(kLoadAddress, GetTestElfFile());

  std::optional<elflib::Elf64_Shdr> debug_info_section_header = std::nullopt;
  auto data = elflib->GetSectionData(".shstrtab");
  for (const auto& shdr : elflib->GetSectionHeaders()) {
    constexpr char kDebugInfoSectionName[] = ".debug_info";

    const char* section_name = reinterpret_cast<const char*>(&data.ptr[shdr.sh_name]);
    if (strncmp(section_name, kDebugInfoSectionName, strlen(kDebugInfoSectionName)) == 0) {
      debug_info_section_header = shdr;
      break;
    }
  }

  ASSERT_NE(debug_info_section_header, std::nullopt);

  // The debug_info section should be compressed.
  ASSERT_NE(debug_info_section_header->sh_flags & elflib::SHF_COMPRESSED, 0u);

  auto debug_info_offset = debug_info_section_header->sh_offset;

  // Requesting a read at the offset into the file should trigger the debug_info section to be
  // decompressed by the ElfMemoryRegion reader.
  constexpr size_t kBufferSize = 128;
  uint8_t buf[kBufferSize];  // Contents don't matter.
  auto err = elf_memory_region.ReadBytes(kLoadAddress + debug_info_offset, kBufferSize, buf);
  ASSERT_TRUE(err.ok()) << err.msg();

  // There should be exactly one decompressed section (.debug_info).
  EXPECT_EQ(elf_memory_region.decompressed_sections_.size(), 1u);

  // The whole section should be decompressed and loaded into the objects cache, the decompressed
  // section should be larger than what the ELF file section size is, since that's the compressed
  // size.
  EXPECT_GT(elf_memory_region.decompressed_sections_.begin()->second.size(),
            debug_info_section_header->sh_size);

  // The compressed section header will report the exact decompressed size.
  elflib::Elf64_Chdr compressed_header;
  auto debug_info_memory = elflib->GetSectionData(".debug_info");
  memcpy(&compressed_header, debug_info_memory.ptr, sizeof(compressed_header));

  EXPECT_EQ(compressed_header.ch_size,
            elf_memory_region.decompressed_sections_.begin()->second.size());

  // The purpose of this test isn't to validate the debug_info, that can be evaluated elsewhere, but
  // we should at least make sure that the first few bytes of the decompressed data is not the same
  // as the compressed header.
  EXPECT_NE(memcmp(reinterpret_cast<elflib::Elf64_Chdr*>(buf), &compressed_header,
                   sizeof(compressed_header)),
            0);

  // Because the decompressed section is larger than the section size reported by the section
  // header, we need to make sure that we can read from the end of the decompressed section, which
  // would appear like it belongs to the next section if it is simply compared against the section
  // size as reported by the header.
}

namespace {

TEST(ElfMemoryRegion, ReadBytesDecompressedSectionWithOverflowingSizeReturnsError) {
  constexpr uint64_t kLoadAddress = 0xdeadbeef;

  auto elflib = elflib::ElfLib::Create(GetTestElfFile());
  ElfMemoryRegion elf_memory_region(kLoadAddress, GetTestElfFile());

  auto info = GetCompressedSectionInfo(elflib.get(), ".debug_info");
  ASSERT_TRUE(info.has_value());

  // Trigger decompression of the compressed section before testing cached reads.
  constexpr size_t kBufferSize = 128;
  uint8_t buf[kBufferSize];
  auto err = elf_memory_region.ReadBytes(kLoadAddress + info->file_offset, kBufferSize, buf);
  ASSERT_TRUE(err.ok()) << err.msg();

  // Read at a non-zero offset within the decompressed section with a size that wraps offset + size.
  constexpr uint64_t kRelativeOffset = 0x10;
  constexpr uint64_t kOverflowSize = std::numeric_limits<uint64_t>::max() - 0x8;
  uint8_t canary[32];
  memset(canary, 0xaa, sizeof(canary));
  uint8_t expected_canary[32];
  memset(expected_canary, 0xaa, sizeof(expected_canary));

  err = elf_memory_region.ReadBytes(kLoadAddress + info->file_offset + kRelativeOffset,
                                    kOverflowSize, canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);
}

TEST(ElfMemoryRegion, ReadBytesAddressBelowLoadAddressReturnsError) {
  constexpr uint64_t kLoadAddress = 0x1000;
  ElfMemoryRegion elf_memory_region(kLoadAddress, GetTestElfFile());

  uint8_t canary[8];
  memset(canary, 0xaa, sizeof(canary));
  uint8_t expected_canary[8];
  memset(expected_canary, 0xaa, sizeof(expected_canary));

  // Address strictly less than load_address.
  auto err = elf_memory_region.ReadBytes(kLoadAddress - 1, 1, canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);

  // Address 0 when load_address is non-zero.
  err = elf_memory_region.ReadBytes(0, 1, canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);

  // Address spanning across the load_address boundary.
  err = elf_memory_region.ReadBytes(kLoadAddress - 4, 8, canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);
}

TEST(ElfMemoryRegion, ReadBytesAtDecompressedSectionEndBoundarySucceeds) {
  constexpr uint64_t kLoadAddress = 0xdeadbeef;

  auto elflib = elflib::ElfLib::Create(GetTestElfFile());
  ElfMemoryRegion elf_memory_region(kLoadAddress, GetTestElfFile());

  auto info = GetCompressedSectionInfo(elflib.get(), ".debug_info");
  ASSERT_TRUE(info.has_value());

  // Trigger decompression of the compressed section before testing boundary reads.
  uint8_t warmup_buf[16];
  auto err =
      elf_memory_region.ReadBytes(kLoadAddress + info->file_offset, sizeof(warmup_buf), warmup_buf);
  ASSERT_TRUE(err.ok()) << err.msg();

  // Read a chunk that starts within the section and ends exactly at the decompressed section
  // boundary.
  constexpr uint64_t kRelativeOffset = 0x10;
  ASSERT_GT(info->decompressed_size, kRelativeOffset);
  const uint64_t exact_fit_size = info->decompressed_size - kRelativeOffset;
  std::vector<uint8_t> buf(exact_fit_size);
  err = elf_memory_region.ReadBytes(kLoadAddress + info->file_offset + kRelativeOffset,
                                    exact_fit_size, buf.data());
  EXPECT_TRUE(err.ok()) << err.msg();

  // Read the full decompressed section from start to end.
  buf.resize(info->decompressed_size);
  err = elf_memory_region.ReadBytes(kLoadAddress + info->file_offset, info->decompressed_size,
                                    buf.data());
  EXPECT_TRUE(err.ok()) << err.msg();
}

TEST(ElfMemoryRegion, ReadBytesPastDecompressedSectionEndBoundaryReturnsError) {
  constexpr uint64_t kLoadAddress = 0xdeadbeef;

  auto elflib = elflib::ElfLib::Create(GetTestElfFile());
  ElfMemoryRegion elf_memory_region(kLoadAddress, GetTestElfFile());

  auto info = GetCompressedSectionInfo(elflib.get(), ".debug_info");
  ASSERT_TRUE(info.has_value());

  // Trigger decompression of the compressed section before testing boundary reads.
  uint8_t warmup_buf[16];
  auto err =
      elf_memory_region.ReadBytes(kLoadAddress + info->file_offset, sizeof(warmup_buf), warmup_buf);
  ASSERT_TRUE(err.ok()) << err.msg();

  uint8_t canary[32];
  memset(canary, 0xaa, sizeof(canary));
  uint8_t expected_canary[32];
  memset(expected_canary, 0xaa, sizeof(expected_canary));

  // Requesting 1 byte past the end of the decompressed section from offset 0 must fail.
  err = elf_memory_region.ReadBytes(kLoadAddress + info->file_offset, info->decompressed_size + 1,
                                    canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);

  // Requesting 1 byte past the end of the decompressed section from non-zero offset must fail.
  constexpr uint64_t kRelativeOffset = 0x10;
  ASSERT_GT(info->decompressed_size, kRelativeOffset);
  const uint64_t over_read_size = (info->decompressed_size - kRelativeOffset) + 1;
  err = elf_memory_region.ReadBytes(kLoadAddress + info->file_offset + kRelativeOffset,
                                    over_read_size, canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);

  // Requesting 16 bytes past the end of the decompressed section from non-zero offset must fail.
  const uint64_t large_over_read_size = (info->decompressed_size - kRelativeOffset) + 16;
  err = elf_memory_region.ReadBytes(kLoadAddress + info->file_offset + kRelativeOffset,
                                    large_over_read_size, canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);
}

TEST(ElfMemoryRegion, ReadBytesDecompressedSectionWithMaxUint64SizeReturnsError) {
  constexpr uint64_t kLoadAddress = 0xdeadbeef;

  auto elflib = elflib::ElfLib::Create(GetTestElfFile());
  ElfMemoryRegion elf_memory_region(kLoadAddress, GetTestElfFile());

  auto info = GetCompressedSectionInfo(elflib.get(), ".debug_info");
  ASSERT_TRUE(info.has_value());

  // Trigger decompression of the compressed section before testing cached reads.
  constexpr size_t kBufferSize = 128;
  uint8_t buf[kBufferSize];
  auto err = elf_memory_region.ReadBytes(kLoadAddress + info->file_offset, kBufferSize, buf);
  ASSERT_TRUE(err.ok()) << err.msg();

  uint8_t canary[32];
  memset(canary, 0xaa, sizeof(canary));
  uint8_t expected_canary[32];
  memset(expected_canary, 0xaa, sizeof(expected_canary));

  // Requesting maximum uint64_t size from the start of the decompressed section must fail.
  err = elf_memory_region.ReadBytes(kLoadAddress + info->file_offset,
                                    std::numeric_limits<uint64_t>::max(), canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);
}

TEST(ElfMemoryRegion, ReadBytesDecompressedSectionNearMaxUint64BoundaryReturnsError) {
  constexpr uint64_t kLoadAddress = 0xdeadbeef;

  auto elflib = elflib::ElfLib::Create(GetTestElfFile());
  ElfMemoryRegion elf_memory_region(kLoadAddress, GetTestElfFile());

  auto info = GetCompressedSectionInfo(elflib.get(), ".debug_info");
  ASSERT_TRUE(info.has_value());

  // Trigger decompression of the compressed section before testing cached reads.
  constexpr size_t kBufferSize = 128;
  uint8_t buf[kBufferSize];
  auto err = elf_memory_region.ReadBytes(kLoadAddress + info->file_offset, kBufferSize, buf);
  ASSERT_TRUE(err.ok()) << err.msg();

  uint8_t canary[32];
  memset(canary, 0xaa, sizeof(canary));
  uint8_t expected_canary[32];
  memset(expected_canary, 0xaa, sizeof(expected_canary));

  // Section-relative offset where relative offset and size wrap to 0.
  constexpr uint64_t kRelativeOffset = 0x20;
  const uint64_t file_offset = info->file_offset + kRelativeOffset;
  const uint64_t wrap_to_zero_size = std::numeric_limits<uint64_t>::max() - kRelativeOffset + 1;

  err = elf_memory_region.ReadBytes(kLoadAddress + file_offset, wrap_to_zero_size, canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);

  // Section-relative offset where relative offset and size equal maximum uint64_t.
  const uint64_t max_boundary_size = std::numeric_limits<uint64_t>::max() - kRelativeOffset;

  err = elf_memory_region.ReadBytes(kLoadAddress + file_offset, max_boundary_size, canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);

  // Address arithmetic overflow where address and size wrap around uint64_t.
  constexpr uint64_t kNearMaxAddr = std::numeric_limits<uint64_t>::max() - 0x10;
  constexpr uint64_t kOverflowSpan = 0x20;

  err = elf_memory_region.ReadBytes(kNearMaxAddr, kOverflowSpan, canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);

  ElfMemoryRegion near_max_region(kNearMaxAddr, GetTestElfFile());
  err = near_max_region.ReadBytes(kNearMaxAddr, kOverflowSpan, canary);
  EXPECT_TRUE(err.has_err());
  EXPECT_EQ(memcmp(canary, expected_canary, sizeof(canary)), 0);
}

}  // namespace

}  // namespace zxdb
