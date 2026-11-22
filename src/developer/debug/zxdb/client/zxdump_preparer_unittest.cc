// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/zxdump_preparer.h"

#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <filesystem>
#include <string>

#include <gtest/gtest.h>
#include <zstd/zstd.h>

#include "src/developer/debug/zxdb/common/scoped_temp_file.h"
#include "src/lib/files/file.h"

namespace zxdb {

namespace {

std::string ReadFd(int fd) {
  struct stat st = {};
  if (fstat(fd, &st) < 0 || st.st_size <= 0) {
    return {};
  }
  void* mapped = mmap(nullptr, static_cast<size_t>(st.st_size), PROT_READ, MAP_PRIVATE, fd, 0);
  if (mapped == MAP_FAILED) {
    ADD_FAILURE() << "mmap failed";
    return {};
  }
  std::string out(static_cast<const char*>(mapped), static_cast<size_t>(st.st_size));
  munmap(mapped, static_cast<size_t>(st.st_size));
  return out;
}

std::string Compress(const std::string& data) {
  std::string out(ZSTD_compressBound(data.size()), '\0');
  size_t size = ZSTD_compress(out.data(), out.size(), data.data(), data.size(), 3);
  EXPECT_FALSE(ZSTD_isError(size));
  out.resize(size);
  return out;
}

// A minimal little-endian ELF header with e_type == ET_CORE, padded with a body.
std::string FakeCore(size_t body_size) {
  std::string data(64 + body_size, '\0');
  data[0] = 0x7f;
  data[1] = 'E';
  data[2] = 'L';
  data[3] = 'F';
  data[4] = 2;   // ELFCLASS64.
  data[5] = 1;   // ELFDATA2LSB.
  data[16] = 4;  // ET_CORE.
  for (size_t i = 64; i < data.size(); i++) {
    data[i] = static_cast<char>(i * 7);
  }
  return data;
}

DumpFormat Detect(const std::string& bytes) {
  return DetectDumpFormat(
      std::span<const uint8_t>(reinterpret_cast<const uint8_t*>(bytes.data()), bytes.size()));
}

TEST(ZxdumpPreparer, DetectDumpFormat) {
  EXPECT_EQ(DumpFormat::kElfCore, Detect(FakeCore(0)));
  EXPECT_EQ(DumpFormat::kArchive, Detect("!<arch>\nrest"));
  EXPECT_EQ(DumpFormat::kZstd, Detect(std::string("\x28\xb5\x2f\xfd", 4)));
  EXPECT_EQ(DumpFormat::kZstd, Detect(Compress("hello")));

  // An ELF file of another type (ET_DYN).
  std::string so = FakeCore(0);
  so[16] = 3;
  EXPECT_EQ(DumpFormat::kUnknown, Detect(so));

  // A big-endian ET_CORE.
  std::string big = FakeCore(0);
  big[5] = 2;
  big[16] = 0;
  big[17] = 4;
  EXPECT_EQ(DumpFormat::kElfCore, Detect(big));

  // Too short to hold e_type.
  EXPECT_EQ(DumpFormat::kUnknown, Detect(FakeCore(0).substr(0, 17)));
  EXPECT_EQ(DumpFormat::kUnknown, Detect("MDMP minidump"));
  EXPECT_EQ(DumpFormat::kUnknown, Detect(""));
  EXPECT_EQ(DumpFormat::kUnknown, Detect("!<arch"));
}

TEST(ZxdumpPreparer, IsZxdumpFile) {
  ScopedTempFile core;
  ScopedTempFile text;
  ASSERT_TRUE(files::WriteFile(core.name(), FakeCore(16)));
  ASSERT_TRUE(files::WriteFile(text.name(), "not a dump at all"));
  EXPECT_TRUE(IsZxdumpFile(core.name()));
  EXPECT_FALSE(IsZxdumpFile(text.name()));
  EXPECT_FALSE(IsZxdumpFile(core.name() + ".missing"));
}

TEST(ZxdumpPreparer, UncompressedIsOpenedInPlaceAndNotDeleted) {
  ScopedTempFile input;
  const std::string original = FakeCore(32);
  ASSERT_TRUE(files::WriteFile(input.name(), original));

  {
    auto result = PrepareDump(input.name());
    ASSERT_TRUE(result.ok()) << result.err().msg();
    EXPECT_TRUE(result.value().is_valid());
    EXPECT_EQ(0, lseek(result.value().get(), 0, SEEK_CUR));
    EXPECT_EQ(original, ReadFd(result.value().get()));
  }
  // Closing the descriptor never deletes an uncompressed input.
  EXPECT_TRUE(std::filesystem::exists(input.name()));
  std::string disk_contents;
  ASSERT_TRUE(files::ReadFileToString(input.name(), &disk_contents));
  EXPECT_EQ(original, disk_contents);
}

TEST(ZxdumpPreparer, ArchiveInput) {
  ScopedTempFile input;
  const std::string archive = "!<arch>\n";
  ASSERT_TRUE(files::WriteFile(input.name(), archive));

  auto uncompressed = PrepareDump(input.name());
  ASSERT_TRUE(uncompressed.ok()) << uncompressed.err().msg();
  EXPECT_EQ(0, lseek(uncompressed.value().get(), 0, SEEK_CUR));
  EXPECT_EQ(archive, ReadFd(uncompressed.value().get()));

  ASSERT_TRUE(files::WriteFile(input.name(), Compress(archive)));
  auto compressed = PrepareDump(input.name());
  ASSERT_TRUE(compressed.ok()) << compressed.err().msg();
  EXPECT_EQ(0, lseek(compressed.value().get(), 0, SEEK_CUR));
  EXPECT_EQ(archive, ReadFd(compressed.value().get()));
}

TEST(ZxdumpPreparer, CompressedDecompressesIntoUnlinkedMmapableFd) {
  ScopedTempFile input;
  const std::string original = FakeCore(300000);
  const std::string compressed = Compress(original);
  ASSERT_TRUE(files::WriteFile(input.name(), compressed));

  auto first = PrepareDump(input.name());
  ASSERT_TRUE(first.ok()) << first.err().msg();
  EXPECT_EQ(0, lseek(first.value().get(), 0, SEEK_CUR));
  struct stat st = {};
  ASSERT_EQ(0, fstat(first.value().get(), &st));
  // The temporary file is already unlinked while its descriptor stays open and mmap-able.
  EXPECT_EQ(0u, static_cast<unsigned>(st.st_nlink));
  EXPECT_EQ(original, ReadFd(first.value().get()));

  auto second = PrepareDump(input.name());
  ASSERT_TRUE(second.ok()) << second.err().msg();
  EXPECT_EQ(0, lseek(second.value().get(), 0, SEEK_CUR));
  EXPECT_EQ(original, ReadFd(second.value().get()));
  std::string disk_contents;
  ASSERT_TRUE(files::ReadFileToString(input.name(), &disk_contents));
  EXPECT_EQ(compressed, disk_contents);
}

TEST(ZxdumpPreparer, ConcatenatedFrames) {
  ScopedTempFile input;
  const std::string a = FakeCore(1000);
  const std::string b(5000, 'x');
  ASSERT_TRUE(files::WriteFile(input.name(), Compress(a) + Compress(b)));

  auto result = PrepareDump(input.name());
  ASSERT_TRUE(result.ok()) << result.err().msg();
  EXPECT_EQ(a + b, ReadFd(result.value().get()));
}

// Regression test: a frame that ends exactly when the output buffer fills must not be reported
// as truncated.
TEST(ZxdumpPreparer, FrameEndsAtOutputBufferBoundary) {
  const size_t out_size = ZSTD_DStreamOutSize();
  for (size_t multiple : {1u, 2u, 3u}) {
    SCOPED_TRACE(multiple);
    ScopedTempFile input;
    const std::string original = FakeCore((multiple * out_size) - 64);
    ASSERT_EQ(multiple * out_size, original.size());
    ASSERT_TRUE(files::WriteFile(input.name(), Compress(original) + Compress(original)));

    auto result = PrepareDump(input.name());
    ASSERT_TRUE(result.ok()) << result.err().msg();
    EXPECT_EQ(original + original, ReadFd(result.value().get()));

    ASSERT_TRUE(files::WriteFile(input.name(), Compress(original)));
    auto single = PrepareDump(input.name());
    ASSERT_TRUE(single.ok()) << single.err().msg();
    EXPECT_EQ(original, ReadFd(single.value().get()));
  }
}

TEST(ZxdumpPreparer, TruncatedCompressedInputFails) {
  ScopedTempFile input;
  std::string compressed = Compress(FakeCore(100000));
  compressed.resize(compressed.size() / 2);
  ASSERT_TRUE(files::WriteFile(input.name(), compressed));

  auto result = PrepareDump(input.name());
  EXPECT_TRUE(result.has_error());
}

TEST(ZxdumpPreparer, CorruptCompressedPayloadFails) {
  ScopedTempFile input;
  std::string corrupt = Compress(FakeCore(100000));
  ASSERT_GT(corrupt.size(), 8u);
  corrupt.replace(5, 3, "\xff\xff\xff", 3);
  ASSERT_TRUE(files::WriteFile(input.name(), corrupt));

  auto result = PrepareDump(input.name());
  ASSERT_TRUE(result.has_error());
  EXPECT_NE(std::string::npos, result.err().msg().find("zstd decompression failed"));
}

TEST(ZxdumpPreparer, CompressedNonZxdumpFails) {
  ScopedTempFile input;
  ASSERT_TRUE(files::WriteFile(input.name(), Compress("MDMP minidump payload")));
  EXPECT_TRUE(PrepareDump(input.name()).has_error());

  ASSERT_TRUE(files::WriteFile(input.name(), Compress(Compress(FakeCore(64)))));
  EXPECT_TRUE(PrepareDump(input.name()).has_error());
}

TEST(ZxdumpPreparer, UnknownAndMissingInputFails) {
  ScopedTempFile input;
  ASSERT_TRUE(files::WriteFile(input.name(), "hello"));
  auto unknown = PrepareDump(input.name());
  ASSERT_TRUE(unknown.has_error());
  EXPECT_NE(std::string::npos, unknown.err().msg().find("Not a zxdump file"));

  auto missing = PrepareDump(input.name() + ".missing");
  ASSERT_TRUE(missing.has_error());
  EXPECT_NE(std::string::npos, missing.err().msg().find("Could not open"));
}

}  // namespace
}  // namespace zxdb
