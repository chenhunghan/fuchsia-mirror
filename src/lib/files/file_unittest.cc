// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/files/file.h"

#include <fcntl.h>
#include <limits.h>

#ifdef __Fuchsia__
#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/fdio/directory.h>
#include <zircon/availability.h>
#endif  // __Fuchsia__

#include <vector>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/files/directory.h"
#include "src/lib/files/path.h"
#include "src/lib/files/scoped_temp_dir.h"

namespace files {
namespace {

using ::testing::IsEmpty;
using ::testing::UnorderedElementsAre;

std::vector<std::string> GetNonDotDirContents(const std::string& dir_path) {
  std::vector<std::string> contents;
  EXPECT_TRUE(ReadDirContents(dir_path, &contents));
  std::erase_if(contents, [](const std::string& entry) { return entry == "." || entry == ".."; });
  return contents;
}

class WriteFileInTwoPhasesTest : public ::testing::TestWithParam<std::string_view> {};

TEST_P(WriteFileInTwoPhasesTest, WriteAndUpdate) {
  ScopedTempDir dir(GetParam());
  fbl::unique_fd dirfd(open(dir.path().c_str(), O_RDONLY | O_DIRECTORY));
  ASSERT_TRUE(dirfd.is_valid());

  std::string filename = "destination";
  std::string content = "Hello World";
  ASSERT_TRUE(WriteFileInTwoPhases(dirfd.get(), filename, content));
  std::string read_content;
  ASSERT_TRUE(ReadFileToStringAt(dirfd.get(), filename, &read_content));
  EXPECT_EQ(read_content, content);
  EXPECT_THAT(GetNonDotDirContents(dir.path()), UnorderedElementsAre("destination"));

  std::string updated_content = "Updated Hello World";
  ASSERT_TRUE(WriteFileInTwoPhases(dirfd.get(), filename, updated_content));
  ASSERT_TRUE(ReadFileToStringAt(dirfd.get(), filename, &read_content));
  EXPECT_EQ(read_content, updated_content);
  EXPECT_THAT(GetNonDotDirContents(dir.path()), UnorderedElementsAre("destination"));

  ASSERT_TRUE(WriteFileInTwoPhases(dirfd.get(), filename, ""));
  ASSERT_TRUE(ReadFileToStringAt(dirfd.get(), filename, &read_content));
  EXPECT_TRUE(read_content.empty());
  EXPECT_THAT(GetNonDotDirContents(dir.path()), UnorderedElementsAre("destination"));
}

TEST(File, WriteFileInTwoPhasesOverwritesLeftoverTempFile) {
  ScopedTempDir dir;
  fbl::unique_fd dirfd(open(dir.path().c_str(), O_RDONLY | O_DIRECTORY));
  ASSERT_TRUE(dirfd.is_valid());

  std::string path = dir.path() + "/destination";
  std::string leftover_temp_path = dir.path() + "/destination.tmp";

  // Simulate a prior fallback or Linux write that was interrupted (e.g. by a reboot) after
  // creating/linking "<filename>.tmp" but before renaming it over the destination.
  ASSERT_TRUE(WriteFile(path, "old content"));
  ASSERT_TRUE(WriteFile(leftover_temp_path, "interrupted temp content"));

  ASSERT_TRUE(WriteFileInTwoPhases(dirfd.get(), "destination", "new content"));

  std::string read_content;
  ASSERT_TRUE(ReadFileToString(path, &read_content));
  EXPECT_EQ(read_content, "new content");
  EXPECT_FALSE(IsFile(leftover_temp_path));
  EXPECT_THAT(GetNonDotDirContents(dir.path()), UnorderedElementsAre("destination"));
}

TEST_P(WriteFileInTwoPhasesTest, CleansUpTempFileOnRenameFailure) {
  ScopedTempDir dir(GetParam());
  fbl::unique_fd dirfd(open(dir.path().c_str(), O_RDONLY | O_DIRECTORY));
  ASSERT_TRUE(dirfd.is_valid());

  // Creating a directory at the target filename causes replacing it to fail.
  ASSERT_TRUE(CreateDirectoryAt(dirfd.get(), "destination"));

  EXPECT_FALSE(WriteFileInTwoPhases(dirfd.get(), "destination", "content"));
  EXPECT_THAT(GetNonDotDirContents(dir.path()), UnorderedElementsAre("destination"));
}

TEST_P(WriteFileInTwoPhasesTest, FailsOnNonDirectoryFd) {
  ScopedTempDir dir(GetParam());
  fbl::unique_fd dirfd(open(dir.path().c_str(), O_RDONLY | O_DIRECTORY));
  ASSERT_TRUE(dirfd.is_valid());

  ASSERT_TRUE(WriteFileAt(dirfd.get(), "regular_file", "hello", 5));
  fbl::unique_fd file_fd(openat(dirfd.get(), "regular_file", O_RDONLY));
  ASSERT_TRUE(file_fd.is_valid());

  EXPECT_FALSE(WriteFileInTwoPhases(file_fd.get(), "destination", "content"));
  EXPECT_THAT(GetNonDotDirContents(dir.path()), UnorderedElementsAre("regular_file"));
}

#ifdef __Fuchsia__
// Test both /tmp (memfs, which does not support FLAG_CREATE_AS_UNNAMED_TEMPORARY and exercises the
// fallback path) and /data (fxfs on standard builds, or minfs on core_with_minfs builds).
INSTANTIATE_TEST_SUITE_P(File, WriteFileInTwoPhasesTest, ::testing::Values("/tmp", "/data"));

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
TEST(File, WriteFileInTwoPhasesBypassesNamedTempFileWhenSupported) {
  ScopedTempDir dir("/data");
  fbl::unique_fd dirfd(open(dir.path().c_str(), O_RDONLY | O_DIRECTORY));
  ASSERT_TRUE(dirfd.is_valid());

  // Some configurations (e.g. core_with_minfs) back /data with a filesystem that does not support
  // unnamed temporary files.
  constexpr fuchsia_io::wire::Flags kProbeFlags =
      fuchsia_io::wire::Flags::kProtocolFile | fuchsia_io::wire::kPermReadable |
      fuchsia_io::wire::kPermWritable | fuchsia_io::wire::Flags::kFlagCreateAsUnnamedTemporary;
  int probe_raw_fd = -1;
  const zx_status_t probe_status =
      fdio_open3_fd_at(dirfd.get(), ".", uint64_t{kProbeFlags}, &probe_raw_fd);
  if (probe_status == ZX_ERR_NOT_SUPPORTED) {
    GTEST_SKIP() << "/data filesystem does not support FLAG_CREATE_AS_UNNAMED_TEMPORARY";
  }
  ASSERT_EQ(probe_status, ZX_OK);
  fbl::unique_fd probe_fd(probe_raw_fd);
  probe_fd.reset();

  // Occupy "destination.tmp" with a directory so the fallback path (opening "destination.tmp" for
  // writing) would fail with EISDIR. Direct linking via FLAG_CREATE_AS_UNNAMED_TEMPORARY and
  // LinkInto with REPLACE_EXISTING never opens "destination.tmp" and must still succeed.
  ASSERT_TRUE(CreateDirectoryAt(dirfd.get(), "destination.tmp"));

  ASSERT_TRUE(WriteFileInTwoPhases(dirfd.get(), "destination", "initial"));
  std::string read_content;
  ASSERT_TRUE(ReadFileToStringAt(dirfd.get(), "destination", &read_content));
  EXPECT_EQ(read_content, "initial");

  ASSERT_TRUE(WriteFileInTwoPhases(dirfd.get(), "destination", "overwritten"));
  ASSERT_TRUE(ReadFileToStringAt(dirfd.get(), "destination", &read_content));
  EXPECT_EQ(read_content, "overwritten");
  EXPECT_THAT(GetNonDotDirContents(dir.path()),
              UnorderedElementsAre("destination", "destination.tmp"));
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
#else
INSTANTIATE_TEST_SUITE_P(File, WriteFileInTwoPhasesTest, ::testing::Values(""));
#endif  // __Fuchsia__

TEST(File, WriteFileInTwoPhasesFailsOnInvalidArguments) {
  ScopedTempDir dir;
  fbl::unique_fd dirfd(open(dir.path().c_str(), O_RDONLY | O_DIRECTORY));
  ASSERT_TRUE(dirfd.is_valid());

  EXPECT_FALSE(WriteFileInTwoPhases(-1, "destination", "content"));
  EXPECT_FALSE(WriteFileInTwoPhases(AT_FDCWD, "destination", "content"));
  EXPECT_FALSE(WriteFileInTwoPhases(dirfd.get(), "", "content"));
  EXPECT_FALSE(WriteFileInTwoPhases(dirfd.get(), ".", "content"));
  EXPECT_FALSE(WriteFileInTwoPhases(dirfd.get(), "..", "content"));
  EXPECT_FALSE(WriteFileInTwoPhases(dirfd.get(), "subdir/destination", "content"));
  EXPECT_FALSE(WriteFileInTwoPhases(dirfd.get(), "/destination", "content"));
  EXPECT_FALSE(WriteFileInTwoPhases(dirfd.get(), std::string(NAME_MAX + 1, 'a'), "content"));
  EXPECT_THAT(GetNonDotDirContents(dir.path()), IsEmpty());
}

TEST(File, ReadWriteFileAt) {
  ScopedTempDir dir;
  std::string filename = "bar";
  std::string content = "content";
  fbl::unique_fd dirfd(open(dir.path().c_str(), O_RDONLY));

  EXPECT_TRUE(files::WriteFileAt(dirfd.get(), filename, content.c_str(), content.size()));

  std::string read_content;
  EXPECT_TRUE(files::ReadFileToStringAt(dirfd.get(), filename, &read_content));
  EXPECT_EQ(content, read_content);
}

TEST(File, ReadFileToVector) {
  ScopedTempDir dir;
  std::string filename = "bar";
  std::string content = "content";
  std::string path = dir.path() + filename;

  EXPECT_TRUE(files::WriteFile(path, content));

  std::vector<uint8_t> data;
  EXPECT_TRUE(files::ReadFileToVector(path, &data));

  std::string string_content(data.begin(), data.end());
  EXPECT_EQ(content, string_content);
}

TEST(File, ReadFileDescriptorToVector) {
  ScopedTempDir dir;
  std::string filename = "bar";
  std::string content = "content";
  std::string path = dir.path() + filename;

  EXPECT_TRUE(files::WriteFile(path, content));

  auto fd = open(path.c_str(), O_RDONLY);
  EXPECT_GT(fd, 0);
  std::vector<uint8_t> data;
  EXPECT_TRUE(files::ReadFileDescriptorToVector(fd, &data));

  std::string string_content(data.begin(), data.end());
  EXPECT_EQ(content, string_content);
}

TEST(File, IsFileAt) {
  ScopedTempDir dir;
  std::string path;

  ASSERT_TRUE(dir.NewTempFile(&path));

  fbl::unique_fd dirfd(open(dir.path().c_str(), O_RDONLY));
  ASSERT_TRUE(dirfd.get() != -1);
  EXPECT_TRUE(IsFileAt(dirfd.get(), GetBaseName(path)));
}

TEST(File, GetFileSize) {
  ScopedTempDir dir;
  std::string path;

  ASSERT_TRUE(dir.NewTempFile(&path));

  uint64_t size;
  EXPECT_TRUE(GetFileSize(path, &size));
  EXPECT_EQ(0u, size);

  std::string content = "Hello World";
  ASSERT_TRUE(WriteFile(path, content));
  EXPECT_TRUE(GetFileSize(path, &size));
  EXPECT_EQ(content.size(), size);
}

TEST(File, GetFileSizeAt) {
  ScopedTempDir dir;
  fbl::unique_fd dirfd(open(dir.path().c_str(), O_RDONLY));
  ASSERT_TRUE(dirfd.get() != -1);

  std::string full_path;
  ASSERT_TRUE(dir.NewTempFile(&full_path));
  std::string path = GetBaseName(full_path);

  uint64_t size;
  EXPECT_TRUE(GetFileSizeAt(dirfd.get(), path, &size));
  EXPECT_EQ(0u, size);

  std::string content = "Hello World";
  ASSERT_TRUE(WriteFileAt(dirfd.get(), path, content.data(), content.size()));
  EXPECT_TRUE(GetFileSizeAt(dirfd.get(), path, &size));
  EXPECT_EQ(content.size(), size);
}

TEST(File, ReadWriteFileVectorAt) {
  ScopedTempDir dir;
  std::string filename = "bar";
  std::string content = "content";
  fbl::unique_fd dirfd(open(dir.path().c_str(), O_RDONLY));

  EXPECT_TRUE(files::WriteFileAt(dirfd.get(), filename, content.c_str(), content.size()));

  std::vector<uint8_t> read_content;
  EXPECT_TRUE(files::ReadFileToVectorAt(dirfd.get(), filename, &read_content));
  EXPECT_EQ(std::vector<uint8_t>({'c', 'o', 'n', 't', 'e', 'n', 't'}), read_content);
}

}  // namespace
}  // namespace files
