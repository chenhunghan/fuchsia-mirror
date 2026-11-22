// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <dirent.h>
#include <fcntl.h>
#include <grp.h>
#include <lib/fit/defer.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <filesystem>
#include <fstream>
#include <tuple>

#include <gtest/gtest.h>
#include <linux/capability.h>

#include "src/lib/files/file.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

using testing::ElementsAre;
using testing::IsEmpty;

namespace {

struct DirEntry {
  std::string name;
  uint8_t type;
  ino_t inode_num;
  off_t offset;

  static DirEntry Dir(const std::string& name) {
    return DirEntry{
        .name = name,
        .type = DT_DIR,
    };
  }

  static DirEntry CharDev(const std::string& name) {
    return DirEntry{
        .name = name,
        .type = DT_CHR,
    };
  }

  static DirEntry File(const std::string& name) {
    return DirEntry{
        .name = name,
        .type = DT_REG,
    };
  }

  bool operator<(const DirEntry& rhs) const { return name < rhs.name; }
  bool operator==(const DirEntry& rhs) const { return name == rhs.name && type == rhs.type; }
};

std::ostream& operator<<(std::ostream& stream, DirEntry const& value) {
  return stream << "DirEntry(\"" << value.name << "\", type=" << static_cast<int>(value.type)
                << ")";
}

std::vector<DirEntry> ReadDir(const std::string& path) {
  std::vector<DirEntry> result;

  DIR* dir = opendir(path.c_str());
  if (!dir) {
    ADD_FAILURE() << "opendir() failed for " << path << " errno=" << errno;
    return result;
  }
  struct dirent* de;
  errno = 0;

  bool found_dot = false;
  bool found_dot_dot = false;

  while ((de = readdir(dir)) != nullptr) {
    if (std::string(de->d_name) == ".") {
      EXPECT_FALSE(found_dot);
      EXPECT_EQ(de->d_type, DT_DIR);
      found_dot = true;
    } else if (std::string(de->d_name) == "..") {
      EXPECT_FALSE(found_dot_dot);
      EXPECT_EQ(de->d_type, DT_DIR);
      found_dot_dot = true;
    } else {
      result.push_back(DirEntry{
          .name = de->d_name,
          .type = de->d_type,
          .inode_num = de->d_ino,
          .offset = de->d_off,
      });
    }
  }
  closedir(dir);

  if (errno != 0) {
    ADD_FAILURE() << "readdir() failed with errno=" << errno;
    return result;
  }

  EXPECT_TRUE(found_dot);
  EXPECT_TRUE(found_dot_dot);

  std::sort(result.begin(), result.end());

  return result;
}

std::string ReadFileContent(const std::string& path) {
  std::string content;
  EXPECT_TRUE(files::ReadFileToString(path.c_str(), &content));
  return content;
}

bool IsWhiteout(const std::string& path) {
  struct stat s;
  return stat(path.c_str(), &s) == 0 && (s.st_mode & S_IFMT) == S_IFCHR && s.st_rdev == 0;
}

std::string Readlink(const std::string& path) {
  std::string result;
  result.resize(256);
  ssize_t r = readlink(path.c_str(), result.data(), static_cast<int>(result.size()));
  if (r < 0) {
    ADD_FAILURE() << "readlink(" << path << ") failed with errno=" << errno;
    return "";
  }
  result.resize(r);
  return result;
}

class OverlayFsTest : public ::testing::Test {
 protected:
  void SetUp() override {
    // TODO(https://fxbug.dev/317285180) don't skip on baseline
    if (!test_helper::HasSysAdmin()) {
      GTEST_SKIP() << "Not running with sysadmin capabilities, skipping suite.";
    }

    ASSERT_FALSE(temp_dir_.path().empty());

    overlay_ = temp_dir_.path() + "/overlay";
    ASSERT_THAT(mkdir(overlay_.c_str(), 0700), SyscallSucceeds());

    lower_ = temp_dir_.path() + "/lower";
    ASSERT_THAT(mkdir(lower_.c_str(), 0700), SyscallSucceeds());

    upper_base_ = temp_dir_.path() + "/upper_base";
    ASSERT_THAT(mkdir(upper_base_.c_str(), 0700), SyscallSucceeds());

    ASSERT_NO_FATAL_FAILURE(InitUpperDirs());
  }

  void TearDown() override {
    if (!::testing::Test::IsSkipped()) {
      ASSERT_THAT(umount(overlay_.c_str()), SyscallSucceeds());
    }
  }

  void InitUpperDirs() {
    upper_ = upper_base_ + "/upper";
    ASSERT_THAT(mkdir(upper_.c_str(), 0700), SyscallSucceeds());

    work_ = upper_base_ + "/work";
    ASSERT_THAT(mkdir(work_.c_str(), 0700), SyscallSucceeds());
  }

  void Mount() {
    std::string options = fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", lower_.c_str(),
                                            upper_.c_str(), work_.c_str());
    ASSERT_THAT(mount(nullptr, overlay_.c_str(), "overlay", 0, options.c_str()), SyscallSucceeds());
  }

  std::string ReadOverlayFileContent(const std::string& file) {
    return ReadFileContent(overlay_ + file);
  }

  test_helper::ScopedTempDir temp_dir_;
  std::string lower_;
  std::string upper_;
  std::string upper_base_;
  std::string work_;
  std::string overlay_;
};

TEST_F(OverlayFsTest, ListRoot) {
  ASSERT_TRUE(files::WriteFile(lower_ + "/a", "lower/a"));
  ASSERT_TRUE(files::WriteFile(lower_ + "/c", "lower/c"));
  ASSERT_TRUE(files::WriteFile(upper_ + "/b", "upper/b"));
  ASSERT_TRUE(files::WriteFile(upper_ + "/c", "upper/c"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  std::vector<DirEntry> list = ReadDir(overlay_);
  EXPECT_THAT(list, ElementsAre(DirEntry::File("a"), DirEntry::File("b"), DirEntry::File("c")));

  std::vector<DirEntry> lower_list = ReadDir(lower_);
  std::vector<DirEntry> upper_list = ReadDir(upper_);

  // Inode number copied from the source file systems.
  EXPECT_EQ(list[0].inode_num, lower_list[0].inode_num);
  EXPECT_EQ(list[1].inode_num, upper_list[0].inode_num);
  EXPECT_EQ(list[2].inode_num, upper_list[1].inode_num);
}

TEST_F(OverlayFsTest, RootAttributes) {
  // Use distinct modes for lower and upper roots.
  ASSERT_THAT(chmod(lower_.c_str(), 0755), SyscallSucceeds());
  ASSERT_THAT(chmod(upper_.c_str(), 0711), SyscallSucceeds());

  ASSERT_NO_FATAL_FAILURE(Mount());

  struct stat lower_stat, upper_stat, overlay_stat;
  ASSERT_THAT(stat(lower_.c_str(), &lower_stat), SyscallSucceeds());
  ASSERT_THAT(stat(upper_.c_str(), &upper_stat), SyscallSucceeds());
  ASSERT_THAT(stat(overlay_.c_str(), &overlay_stat), SyscallSucceeds());
  ASSERT_NE(upper_stat.st_mode, lower_stat.st_mode);

  // Overlay root should have the same mode as the upper root.
  EXPECT_EQ(overlay_stat.st_mode, upper_stat.st_mode);
}

TEST_F(OverlayFsTest, ListSubdir) {
  ASSERT_THAT(mkdir((lower_ + "/sub").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/sub/a", "a"));

  ASSERT_THAT(mkdir((upper_ + "/sub").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(upper_ + "/sub/b", "b"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  EXPECT_EQ(ReadDir(overlay_ + "/sub"), (std::vector{DirEntry::File("a"), DirEntry::File("b")}));
}

TEST_F(OverlayFsTest, DirAndFile) {
  ASSERT_TRUE(files::WriteFile(lower_ + "/sub", "a"));

  ASSERT_THAT(mkdir((upper_ + "/sub").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(upper_ + "/sub/b", "b"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  EXPECT_EQ(ReadDir(overlay_), (std::vector{DirEntry::Dir("sub")}));
  EXPECT_EQ(ReadDir(overlay_ + "/sub"), (std::vector{DirEntry::File("b")}));

  EXPECT_EQ(ReadOverlayFileContent("/sub/b"), "b");
}

TEST_F(OverlayFsTest, ReadFileLower) {
  ASSERT_TRUE(files::WriteFile(lower_ + "/a", "1"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  EXPECT_EQ(ReadOverlayFileContent("/a"), "1");
}

TEST_F(OverlayFsTest, ReadFileUpper) {
  ASSERT_TRUE(files::WriteFile(upper_ + "/a", "2"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  EXPECT_EQ(ReadOverlayFileContent("/a"), "2");
}

TEST_F(OverlayFsTest, ReadFileBoth) {
  ASSERT_TRUE(files::WriteFile(lower_ + "/a", "1"));
  ASSERT_TRUE(files::WriteFile(upper_ + "/a", "2"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  EXPECT_EQ(ReadOverlayFileContent("/a"), "2");
}

TEST_F(OverlayFsTest, UnmountBase) {
  ASSERT_THAT(mount(nullptr, lower_.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_THAT(mount(nullptr, upper_base_.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_NO_FATAL_FAILURE(InitUpperDirs());

  ASSERT_TRUE(files::WriteFile(lower_ + "/a", "1"));
  ASSERT_TRUE(files::WriteFile(upper_ + "/b", "2"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  // We should be able to unmount base file systems. Overlay should still be accessible.
  ASSERT_THAT(umount(lower_.c_str()), SyscallSucceeds());
  ASSERT_THAT(umount(upper_base_.c_str()), SyscallSucceeds());

  EXPECT_EQ(ReadOverlayFileContent("/a"), "1");
  EXPECT_EQ(ReadOverlayFileContent("/b"), "2");
}

TEST_F(OverlayFsTest, MountSubdir) {
  ASSERT_THAT(mkdir((lower_ + "/sub").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/sub/a", "1"));

  ASSERT_THAT(mkdir((upper_ + "/sub").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(upper_ + "/sub/b", "2"));

  // Mount new FS over the subdirs. That shouldn't affect the files visible to overlayfs.
  ASSERT_THAT(mount(nullptr, (lower_ + "/sub").c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_THAT(mount(nullptr, (upper_ + "/sub").c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(upper_ + "/sub/c", "3"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  EXPECT_EQ(ReadOverlayFileContent("/sub/a"), "1");
  EXPECT_EQ(ReadOverlayFileContent("/sub/b"), "2");

  EXPECT_EQ(ReadDir(overlay_ + "/sub"), (std::vector{DirEntry::File("a"), DirEntry::File("b")}));
}

TEST_F(OverlayFsTest, RemovedFiles) {
  // Char device with `type=0` is a removed file marker. It should not be listed in the
  // overlay FS.
  ASSERT_THAT(mknod((lower_ + "/a").c_str(), S_IFCHR, 0), SyscallSucceeds());
  ASSERT_THAT(mknod((lower_ + "/b").c_str(), S_IFCHR, 1), SyscallSucceeds());
  ASSERT_THAT(mknod((upper_ + "/c").c_str(), S_IFCHR, 0), SyscallSucceeds());
  ASSERT_THAT(mknod((upper_ + "/d").c_str(), S_IFCHR, 1), SyscallSucceeds());

  ASSERT_NO_FATAL_FAILURE(Mount());

  EXPECT_EQ(ReadDir(overlay_), (std::vector{DirEntry::CharDev("b"), DirEntry::CharDev("d")}));
}

TEST_F(OverlayFsTest, MknodZeroDevice) {
  ASSERT_NO_FATAL_FAILURE(Mount());

  // OverlayFS reserves character devices with device ID 0 for whiteouts.
  // Attempts to create one via mknod should fail with EPERM, even if the caller has CAP_MKNOD.
  ASSERT_TRUE(test_helper::HasCapability(CAP_MKNOD));

  EXPECT_THAT(mknod((overlay_ + "/zero_chr_dev").c_str(), S_IFCHR | 0600, 0),
              SyscallFailsWithErrno(EPERM));
  EXPECT_THAT(mknod((overlay_ + "/zero_blk_dev").c_str(), S_IFBLK | 0600, 0), SyscallSucceeds());
}

TEST_F(OverlayFsTest, UnlinkLower) {
  ASSERT_THAT(mkdir((lower_ + "/s").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/s/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  // Remove the file.
  ASSERT_THAT(unlink((overlay_ + "/s/a").c_str()), SyscallSucceeds());
  EXPECT_THAT(ReadDir(overlay_ + "/s"), IsEmpty());
  EXPECT_THAT(open((overlay_ + "/s/a").c_str(), O_RDONLY), SyscallFailsWithErrno(ENOENT));

  // Whiteout should be created in the upper dir.
  EXPECT_TRUE(IsWhiteout(upper_ + "/s/a"));

  // Remove the directory.
  ASSERT_THAT(rmdir((overlay_ + "/s").c_str()), SyscallSucceeds());
  EXPECT_THAT(ReadDir(overlay_), IsEmpty());
  EXPECT_THAT(open((overlay_ + "/s").c_str(), O_RDONLY), SyscallFailsWithErrno(ENOENT));

  // Whiteout should be created in the upper dir.
  EXPECT_TRUE(IsWhiteout(upper_ + "/s"));

  // Lower file should not be touched.
  EXPECT_EQ(ReadFileContent(lower_ + "/s/a"), "a");
}

TEST_F(OverlayFsTest, UnlinkNonEmptyDir) {
  ASSERT_THAT(mkdir((upper_ + "/d").c_str(), 0700), SyscallSucceeds());
  ASSERT_THAT(mkdir((lower_ + "/d").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/d/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  // Try removing non-empty dir. This is expected to fail.
  ASSERT_THAT(rmdir((overlay_ + "/d").c_str()), SyscallFailsWithErrno(ENOTEMPTY));

  // Directory should still exist.
  EXPECT_EQ(ReadDir(overlay_), (std::vector{DirEntry::Dir("d")}));
  EXPECT_EQ(ReadDir(overlay_ + "/d"), (std::vector{DirEntry::File("a")}));

  // No changes to the lower and upper FS.
  EXPECT_THAT(ReadDir(upper_ + "/d"), IsEmpty());
  EXPECT_EQ(ReadDir(lower_ + "/d"), (std::vector{DirEntry::File("a")}));
}

TEST_F(OverlayFsTest, UnlinkUpper) {
  ASSERT_THAT(mkdir((upper_ + "/s").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(upper_ + "/s/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  ASSERT_THAT(unlink((overlay_ + "/s/a").c_str()), SyscallSucceeds());

  EXPECT_THAT(ReadDir(overlay_ + "/s"), IsEmpty());
  EXPECT_THAT(open((overlay_ + "/s/a").c_str(), O_RDONLY), SyscallFailsWithErrno(ENOENT));

  EXPECT_THAT(ReadDir(lower_), IsEmpty());
  EXPECT_THAT(ReadDir(upper_ + "/s"), IsEmpty());

  // Remove the directory.
  ASSERT_THAT(rmdir((overlay_ + "/s").c_str()), SyscallSucceeds());
  EXPECT_THAT(open((overlay_ + "/s").c_str(), O_RDONLY), SyscallFailsWithErrno(ENOENT));

  EXPECT_THAT(ReadDir(overlay_), IsEmpty());
  EXPECT_THAT(ReadDir(lower_), IsEmpty());
  EXPECT_THAT(ReadDir(upper_), IsEmpty());
}

TEST_F(OverlayFsTest, UnlinkBoth) {
  ASSERT_THAT(mkdir((lower_ + "/s").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/s/a", "a"));
  ASSERT_THAT(mkdir((upper_ + "/s").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(upper_ + "/s/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  // Remove the file.
  ASSERT_THAT(unlink((overlay_ + "/s/a").c_str()), SyscallSucceeds());
  EXPECT_THAT(ReadDir(overlay_ + "/s"), IsEmpty());
  EXPECT_THAT(open((overlay_ + "/s/a").c_str(), O_RDONLY), SyscallFailsWithErrno(ENOENT));

  // Whiteout should be created in the upper dir.
  EXPECT_TRUE(IsWhiteout(upper_ + "/s/a"));

  // Remove the directory.
  ASSERT_THAT(rmdir((overlay_ + "/s").c_str()), SyscallSucceeds());
  EXPECT_THAT(ReadDir(overlay_), IsEmpty());
  EXPECT_THAT(open((overlay_ + "/s").c_str(), O_RDONLY), SyscallFailsWithErrno(ENOENT));

  // Whiteout should be created in the upper dir.
  EXPECT_TRUE(IsWhiteout(upper_ + "/s"));

  // Lower file should not be touched.
  EXPECT_EQ(ReadFileContent(lower_ + "/s/a"), "a");
}

TEST_F(OverlayFsTest, RecreateDir) {
  ASSERT_THAT(mkdir((lower_ + "/s").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/s/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  // Remove the file.
  ASSERT_THAT(unlink((overlay_ + "/s/a").c_str()), SyscallSucceeds());
  EXPECT_THAT(ReadDir(overlay_ + "/s"), IsEmpty());

  // Whiteout should be created in the upper dir.
  EXPECT_TRUE(IsWhiteout(upper_ + "/s/a"));

  // Remove the directory.
  ASSERT_THAT(rmdir((overlay_ + "/s").c_str()), SyscallSucceeds());
  EXPECT_THAT(ReadDir(overlay_), IsEmpty());

  // Whiteout should be created in the upper dir.
  EXPECT_TRUE(IsWhiteout(upper_ + "/s"));

  // Create the dir again.
  ASSERT_THAT(mkdir((overlay_ + "/s").c_str(), 0700), SyscallSucceeds());

  // The new dir should be empty.
  EXPECT_THAT(ReadDir(overlay_ + "/s"), IsEmpty());
  EXPECT_THAT(open((overlay_ + "/s/a").c_str(), O_RDONLY), SyscallFailsWithErrno(ENOENT));

  // Lower file should not be touched.
  EXPECT_EQ(ReadFileContent(lower_ + "/s/a"), "a");
}

TEST_F(OverlayFsTest, RecreateFile) {
  ASSERT_THAT(mkdir((lower_ + "/s").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/s/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  // Remove the file.
  ASSERT_THAT(unlink((overlay_ + "/s/a").c_str()), SyscallSucceeds());
  EXPECT_THAT(ReadDir(overlay_ + "/s"), IsEmpty());
  EXPECT_THAT(open((overlay_ + "/s/a").c_str(), O_RDONLY), SyscallFailsWithErrno(ENOENT));

  // Try creating the file again
  ASSERT_TRUE(files::WriteFile(overlay_ + "/s/a", "b"));
  EXPECT_EQ(ReadFileContent(overlay_ + "/s/a"), "b");

  // It should be written to the upper dir.
  EXPECT_EQ(ReadFileContent(upper_ + "/s/a"), "b");

  // Lower file should not be touched.
  EXPECT_EQ(ReadFileContent(lower_ + "/s/a"), "a");
}

TEST_F(OverlayFsTest, CreateDir) {
  ASSERT_THAT(mkdir((lower_ + "/d1").c_str(), 0700), SyscallSucceeds());
  ASSERT_THAT(mkdir((lower_ + "/d2").c_str(), 0700), SyscallSucceeds());

  ASSERT_THAT(mkdir((upper_ + "/d1").c_str(), 0700), SyscallSucceeds());
  ASSERT_THAT(mkdir((upper_ + "/d3").c_str(), 0700), SyscallSucceeds());

  ASSERT_NO_FATAL_FAILURE(Mount());

  // Make new dirs.
  ASSERT_THAT(mkdir((overlay_ + "/d1/1").c_str(), 0700), SyscallSucceeds());
  ASSERT_THAT(mkdir((overlay_ + "/d2/2").c_str(), 0700), SyscallSucceeds());
  ASSERT_THAT(mkdir((overlay_ + "/d3/3").c_str(), 0700), SyscallSucceeds());

  // Verify that the dirs were created.
  EXPECT_EQ(ReadDir(overlay_ + "/d1"), (std::vector{DirEntry::Dir("1")}));
  EXPECT_EQ(ReadDir(overlay_ + "/d2"), (std::vector{DirEntry::Dir("2")}));
  EXPECT_EQ(ReadDir(overlay_ + "/d3"), (std::vector{DirEntry::Dir("3")}));

  // New dirs are empty.
  EXPECT_THAT(ReadDir(overlay_ + "/d1/1"), IsEmpty());
  EXPECT_THAT(ReadDir(overlay_ + "/d2/2"), IsEmpty());
  EXPECT_THAT(ReadDir(overlay_ + "/d3/3"), IsEmpty());

  // Lower FS should not change.
  EXPECT_EQ(ReadDir(lower_), (std::vector{DirEntry::Dir("d1"), DirEntry::Dir("d2")}));
  EXPECT_THAT(ReadDir(lower_ + "/d1"), IsEmpty());
  EXPECT_THAT(ReadDir(lower_ + "/d2"), IsEmpty());

  // New parent dir created in the upper FS.
  EXPECT_EQ(ReadDir(upper_),
            (std::vector{DirEntry::Dir("d1"), DirEntry::Dir("d2"), DirEntry::Dir("d3")}));
}

TEST_F(OverlayFsTest, CreateFile) {
  ASSERT_THAT(mkdir((lower_ + "/d").c_str(), 0700), SyscallSucceeds());

  ASSERT_NO_FATAL_FAILURE(Mount());

  ASSERT_TRUE(files::WriteFile(overlay_ + "/d/a", "foo"));
  EXPECT_EQ(ReadFileContent(overlay_ + "/d/a"), "foo");

  // New file should be created in the upper dir.
  EXPECT_EQ(ReadDir(upper_), (std::vector{DirEntry::Dir("d")}));
  EXPECT_EQ(ReadFileContent(upper_ + "/d/a"), "foo");

  // Lower FS should not change.
  EXPECT_THAT(ReadDir(lower_ + "/d"), IsEmpty());
}

TEST_F(OverlayFsTest, CreateSymlink) {
  ASSERT_THAT(mkdir((lower_ + "/d").c_str(), 0700), SyscallSucceeds());

  ASSERT_NO_FATAL_FAILURE(Mount());

  ASSERT_THAT(symlink("symlink_target", (overlay_ + "/d/a").c_str()), SyscallSucceeds());
  EXPECT_EQ(Readlink(overlay_ + "/d/a"), "symlink_target");

  // New file should be created in the upper dir.
  EXPECT_EQ(ReadDir(upper_), (std::vector{DirEntry::Dir("d")}));
  EXPECT_EQ(Readlink(upper_ + "/d/a"), "symlink_target");

  // Lower FS should not change.
  EXPECT_THAT(ReadDir(lower_ + "/d"), IsEmpty());
}

TEST_F(OverlayFsTest, RewriteFile) {
  ASSERT_THAT(mkdir((lower_ + "/d").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/d/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  ASSERT_TRUE(files::WriteFile(overlay_ + "/d/a", "foo"));
  EXPECT_EQ(ReadFileContent(overlay_ + "/d/a"), "foo");

  // New file should be created in the upper dir.
  EXPECT_EQ(ReadDir(upper_), (std::vector{DirEntry::Dir("d")}));
  EXPECT_EQ(ReadFileContent(upper_ + "/d/a"), "foo");

  // Lower FS should not change.
  EXPECT_EQ(ReadFileContent(lower_ + "/d/a"), "a");
}

TEST_F(OverlayFsTest, AppendFile) {
  ASSERT_THAT(mkdir((lower_ + "/d").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/d/a", "a-"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  {
    std::ofstream stream;
    stream.open(overlay_ + "/d/a", std::ios_base::app);
    stream << "foo";
    ASSERT_FALSE(stream.fail());
  }

  EXPECT_EQ(ReadFileContent(overlay_ + "/d/a"), "a-foo");

  // New file should be created in the upper dir.
  EXPECT_EQ(ReadDir(upper_), (std::vector{DirEntry::Dir("d")}));
  EXPECT_EQ(ReadFileContent(upper_ + "/d/a"), "a-foo");

  // Lower FS should not change.
  EXPECT_EQ(ReadFileContent(lower_ + "/d/a"), "a-");
}

TEST_F(OverlayFsTest, Link) {
  ASSERT_THAT(mkdir((lower_ + "/d").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/d/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  ASSERT_THAT(link((overlay_ + "/d/a").c_str(), (overlay_ + "/d/b").c_str()), SyscallSucceeds());

  // New file should be created in the upper dir.
  auto list = ReadDir(upper_ + "/d");
  EXPECT_EQ(list, (std::vector{DirEntry::File("a"), DirEntry::File("b")}));
  EXPECT_EQ(list[0].inode_num, list[1].inode_num);

  // Write new content. Both files should be updated.
  ASSERT_TRUE(files::WriteFile(overlay_ + "/d/a", "new"));
  EXPECT_EQ(ReadFileContent(upper_ + "/d/a"), "new");
  EXPECT_EQ(ReadFileContent(upper_ + "/d/b"), "new");

  // Lower FS should not change.
  EXPECT_EQ(ReadDir(lower_ + "/d"), (std::vector{DirEntry::File("a")}));
  EXPECT_EQ(ReadFileContent(lower_ + "/d/a"), "a");
}

TEST_F(OverlayFsTest, UpdateAfterOpen) {
  ASSERT_TRUE(files::WriteFile(lower_ + "/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  std::ifstream file(overlay_ + "/a", std::ios::in);

  ASSERT_TRUE(files::WriteFile(overlay_ + "/a", "updated"));

  // Start reading from `file`. We should get new content, written after the file was open.
  std::string data;
  file >> data;
  EXPECT_EQ(data, "updated");
}

TEST_F(OverlayFsTest, RenameFile) {
  ASSERT_THAT(mkdir((lower_ + "/d1").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/d1/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  ASSERT_THAT(mkdir((overlay_ + "/d2").c_str(), 0700), SyscallSucceeds());
  ASSERT_THAT(rename((overlay_ + "/d1/a").c_str(), (overlay_ + "/d2/b").c_str()),
              SyscallSucceeds());

  EXPECT_EQ(ReadDir(overlay_), (std::vector{DirEntry::Dir("d1"), DirEntry::Dir("d2")}));
  EXPECT_EQ(ReadFileContent(overlay_ + "/d2/b"), "a");

  // New file should be created in the upper FS.
  EXPECT_EQ(ReadFileContent(upper_ + "/d2/b"), "a");

  // Lower FS should not change.
  EXPECT_EQ(ReadDir(lower_), (std::vector{DirEntry::Dir("d1")}));
  EXPECT_EQ(ReadFileContent(lower_ + "/d1/a"), "a");
}

TEST_F(OverlayFsTest, RenameNonexistent) {
  ASSERT_THAT(mkdir((lower_ + "/d1").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/d1/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  ASSERT_THAT(rename((overlay_ + "/d1/b").c_str(), (overlay_ + "/d1/c").c_str()),
              SyscallFailsWithErrno(ENOENT));
}

TEST_F(OverlayFsTest, RenameDir) {
  ASSERT_THAT(mkdir((lower_ + "/d1").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_ + "/d1/a", "a"));
  ASSERT_THAT(mkdir((upper_ + "/d1").c_str(), 0700), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(upper_ + "/d1/a", "a"));

  ASSERT_NO_FATAL_FAILURE(Mount());

  ASSERT_THAT(rename((overlay_ + "/d1").c_str(), (overlay_ + "/d2").c_str()),
              SyscallFailsWithErrno(EXDEV));

  // Lower FS should not change.
  EXPECT_EQ(ReadDir(lower_), (std::vector{DirEntry::Dir("d1")}));
  EXPECT_EQ(ReadFileContent(lower_ + "/d1/a"), "a");
}

// During `exec()` the old address-space is torn-down, which can result in locks being taken while
// dropping `mmap()`ed resources. Reproduce an example of this situation and validate that it does
// not trigger a lock-dependency panic.
// This is a regression test for https://fxbug.dev/540127035
TEST_F(OverlayFsTest, DropUncachedDirEntryDuringExecPassesLockDep) {
  ASSERT_NO_FATAL_FAILURE(Mount());

  std::string file_path = overlay_ + "/mapped_file";
  ASSERT_TRUE(files::WriteFile(file_path, "test content"));

  const std::string exit_zero_path = test_helper::GetTestResourcePath("exit_zero");

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    // 1. Open the file on OverlayFS and map it into the process address space.
    int fd = open(file_path.c_str(), O_RDONLY);
    ASSERT_THAT(fd, SyscallSucceeds());

    void* addr = mmap(nullptr, sysconf(_SC_PAGESIZE), PROT_READ, MAP_PRIVATE, fd, 0);
    ASSERT_NE(addr, MAP_FAILED);

    // 2. Close the file descriptor before calling execve().
    // This ensures that the FileMapping in MemoryManager holds the sole
    // remaining strong reference to the underlying DirEntry.
    ASSERT_THAT(close(fd), SyscallSucceeds());

    // 3. Calling execve() replaces the MemoryManager, dropping the FileMapping.
    // On uncached filesystems, this drops the last strong reference to DirEntry
    // and triggers child removal from the parent directory while TaskCredsLock
    // is held by execve().
    char* const argv[] = {const_cast<char*>(exit_zero_path.c_str()), nullptr};
    char* const envp[] = {nullptr};
    EXPECT_THAT(execve(exit_zero_path.c_str(), argv, envp), SyscallSucceeds());
  });

  ASSERT_TRUE(helper.WaitForChildren());
}

TEST_F(OverlayFsTest, FallocateUpdatesTimestamps) {
  ASSERT_THAT(mount(nullptr, lower_.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_THAT(mount(nullptr, upper_base_.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_NO_FATAL_FAILURE(InitUpperDirs());

  ASSERT_TRUE(files::WriteFile(lower_ + "/file", "x"));
  ASSERT_NO_FATAL_FAILURE(Mount());

  std::string path = overlay_ + "/file";
  fbl::unique_fd fd(open(path.c_str(), O_RDWR));
  ASSERT_THAT(fd.get(), SyscallSucceeds());

  const struct timespec old_times[2] = {{.tv_sec = 1, .tv_nsec = 0}, {.tv_sec = 1, .tv_nsec = 0}};
  ASSERT_THAT(futimens(fd.get(), old_times), SyscallSucceeds());

  struct stat before = {};
  ASSERT_THAT(fstat(fd.get(), &before), SyscallSucceeds());
  ASSERT_EQ(before.st_mtim.tv_sec, 1);

  usleep(20'000);
  ASSERT_THAT(fallocate(fd.get(), 0, 0, 4096), SyscallSucceeds());

  struct stat after_fd = {};
  ASSERT_THAT(fstat(fd.get(), &after_fd), SyscallSucceeds());
  EXPECT_EQ(after_fd.st_size, 4096);
  EXPECT_GT(after_fd.st_mtim.tv_sec, 1);
  EXPECT_GT(std::tie(after_fd.st_ctim.tv_sec, after_fd.st_ctim.tv_nsec),
            std::tie(before.st_ctim.tv_sec, before.st_ctim.tv_nsec));

  struct stat after_path = {};
  ASSERT_THAT(stat(path.c_str(), &after_path), SyscallSucceeds());
  EXPECT_EQ(after_path.st_mtim.tv_sec, after_fd.st_mtim.tv_sec);
  EXPECT_EQ(after_path.st_mtim.tv_nsec, after_fd.st_mtim.tv_nsec);
  EXPECT_EQ(after_path.st_ctim.tv_sec, after_fd.st_ctim.tv_sec);
  EXPECT_EQ(after_path.st_ctim.tv_nsec, after_fd.st_ctim.tv_nsec);

  fd.reset();
  ASSERT_THAT(umount(lower_.c_str()), SyscallSucceeds());
  ASSERT_THAT(umount(upper_base_.c_str()), SyscallSucceeds());
}

TEST_F(OverlayFsTest, MaxStackDepth) {
  ASSERT_THAT(mount(nullptr, lower_.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_THAT(mount(nullptr, upper_base_.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_NO_FATAL_FAILURE(InitUpperDirs());

  // Mount a 1-level overlay at `overlay_`.
  ASSERT_NO_FATAL_FAILURE(Mount());

  // Mounting a 2-level overlay over `overlay_` succeeds.
  std::string overlay2 = temp_dir_.path() + "/overlay2";
  ASSERT_THAT(mkdir(overlay2.c_str(), 0700), SyscallSucceeds());
  std::string upper2 = upper_base_ + "/upper2";
  ASSERT_THAT(mkdir(upper2.c_str(), 0700), SyscallSucceeds());
  std::string work2 = upper_base_ + "/work2";
  ASSERT_THAT(mkdir(work2.c_str(), 0700), SyscallSucceeds());
  std::string options2 = fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", overlay_.c_str(),
                                           upper2.c_str(), work2.c_str());
  ASSERT_THAT(mount(nullptr, overlay2.c_str(), "overlay", 0, options2.c_str()), SyscallSucceeds());

  // Mounting a 3-level overlay over `overlay2` exceeds FILESYSTEM_MAX_STACK_DEPTH (2) and fails
  // with EINVAL.
  std::string overlay3 = temp_dir_.path() + "/overlay3";
  ASSERT_THAT(mkdir(overlay3.c_str(), 0700), SyscallSucceeds());
  std::string upper3 = upper_base_ + "/upper3";
  ASSERT_THAT(mkdir(upper3.c_str(), 0700), SyscallSucceeds());
  std::string work3 = upper_base_ + "/work3";
  ASSERT_THAT(mkdir(work3.c_str(), 0700), SyscallSucceeds());
  std::string options3 = fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", overlay2.c_str(),
                                           upper3.c_str(), work3.c_str());
  EXPECT_THAT(mount(nullptr, overlay3.c_str(), "overlay", 0, options3.c_str()),
              SyscallFailsWithErrno(EINVAL));

  ASSERT_THAT(umount(overlay2.c_str()), SyscallSucceeds());
  ASSERT_THAT(umount(lower_.c_str()), SyscallSucceeds());
  ASSERT_THAT(umount(upper_base_.c_str()), SyscallSucceeds());
}

class OverlayFsAccessTest : public OverlayFsTest {
 protected:
  void SetUp() override {
    OverlayFsTest::SetUp();
    if (IsSkipped()) {
      return;
    }

    // Allow users other than the mounter to traverse to the overlay directory, and write to it.
    ASSERT_THAT(chmod(temp_dir_.path().c_str(), 0711), SyscallSucceeds());
    ASSERT_THAT(chmod(overlay_.c_str(), 0777), SyscallSucceeds());
  }

  void MountWithReadableAndWritableFiles() {
    ASSERT_TRUE(files::WriteFile(lower_ + "/readable", "readable"));
    ASSERT_THAT(chmod((lower_ + "/readable").c_str(), 0644), SyscallSucceeds());
    ASSERT_TRUE(files::WriteFile(lower_ + "/lower_readable", "lower_readable"));
    ASSERT_THAT(chmod((lower_ + "/lower_readable").c_str(), 0644), SyscallSucceeds());
    ASSERT_TRUE(files::WriteFile(lower_ + "/upper_readable", "upper_readable"));
    ASSERT_THAT(chmod((lower_ + "/upper_readable").c_str(), 0600), SyscallSucceeds());

    ASSERT_TRUE(files::WriteFile(lower_ + "/writable", "writable"));
    ASSERT_THAT(chmod((lower_ + "/writable").c_str(), 0666), SyscallSucceeds());
    ASSERT_TRUE(files::WriteFile(lower_ + "/lower_writable", "lower_writable"));
    ASSERT_THAT(chmod((lower_ + "/lower_writable").c_str(), 0666), SyscallSucceeds());
    ASSERT_TRUE(files::WriteFile(lower_ + "/upper_writable", "upper_writable"));
    ASSERT_THAT(chmod((lower_ + "/upper_writable").c_str(), 0600), SyscallSucceeds());

    ASSERT_NO_FATAL_FAILURE(Mount());

    // Allow all users to read, write and traverse the root of the overlay.
    ASSERT_THAT(chmod(overlay_.c_str(), 0777), SyscallSucceeds());

    ASSERT_THAT(chmod((overlay_ + "/lower_readable").c_str(), 0600), SyscallSucceeds());
    ASSERT_THAT(chmod((overlay_ + "/upper_readable").c_str(), 0644), SyscallSucceeds());
    ASSERT_THAT(chmod((overlay_ + "/lower_writable").c_str(), 0600), SyscallSucceeds());
    ASSERT_THAT(chmod((overlay_ + "/upper_writable").c_str(), 0666), SyscallSucceeds());
  }

  static void StageExecutable(const std::string& src, const std::string& dst, mode_t mode) {
    std::error_code ec;
    ASSERT_TRUE(std::filesystem::copy_file(src, dst, ec)) << ec.message();
    ASSERT_THAT(chown(dst.c_str(), 0, 0), SyscallSucceeds());
    ASSERT_THAT(chmod(dst.c_str(), mode), SyscallSucceeds());
  }

  static void DropToUser(uid_t uid, gid_t gid) {
    ASSERT_TRUE(test_helper::SwitchToUnprivilegedUser(uid, gid));
  }

  static void DropToOtherUser() { DropToUser(kOtherUid, kOtherGid); }

  static constexpr unsigned int kOtherUid = 65534;
  static constexpr unsigned int kOtherGid = 65534;
};

// A `fallocate()` by a user without `CAP_FSETID` clears `S_ISUID` and `S_ISGID`, both on the
// overlay file and on its upper copy, even though the overlay modifies the upper copy with the
// mounter's credentials.
TEST_F(OverlayFsAccessTest, FallocateClearsSuidAndSgid) {
  ASSERT_THAT(mount(nullptr, lower_.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_THAT(mount(nullptr, upper_base_.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_NO_FATAL_FAILURE(InitUpperDirs());

  ASSERT_TRUE(files::WriteFile(lower_ + "/file", "x"));
  ASSERT_NO_FATAL_FAILURE(Mount());
  ASSERT_THAT(chmod(overlay_.c_str(), 0777), SyscallSucceeds());

  const std::string path = overlay_ + "/file";
  ASSERT_THAT(chmod(path.c_str(), S_ISUID | S_ISGID | 0777), SyscallSucceeds());

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    ASSERT_NO_FATAL_FAILURE(DropToOtherUser());
    fbl::unique_fd fd(open(path.c_str(), O_RDWR));
    ASSERT_THAT(fd.get(), SyscallSucceeds());
    ASSERT_THAT(fallocate(fd.get(), 0, 0, 4096), SyscallSucceeds());
  });
  ASSERT_TRUE(helper.WaitForChildren());

  struct stat overlay_stat = {};
  ASSERT_THAT(stat(path.c_str(), &overlay_stat), SyscallSucceeds());
  EXPECT_EQ(overlay_stat.st_mode & (S_ISUID | S_ISGID), 0u);

  struct stat upper_stat = {};
  ASSERT_THAT(stat((upper_ + "/file").c_str(), &upper_stat), SyscallSucceeds());
  EXPECT_EQ(upper_stat.st_mode & (S_ISUID | S_ISGID), 0u);
  EXPECT_EQ(upper_stat.st_size, 4096);

  ASSERT_THAT(umount(lower_.c_str()), SyscallSucceeds());
  ASSERT_THAT(umount(upper_base_.c_str()), SyscallSucceeds());
}

// Verify that access checks are made against the overlay permissions for the caller, but use the
// mounter credentials to check the underlying filesystems.
TEST_F(OverlayFsAccessTest, MounterAndOtherAccessChecks) {
  ASSERT_NO_FATAL_FAILURE(MountWithReadableAndWritableFiles());

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    ASSERT_NO_FATAL_FAILURE(DropToOtherUser());

    // Other UID can read if it has access in the overlay, even if it could not read directly from
    // `lower_`.
    fbl::unique_fd fd(open((overlay_ + "/readable").c_str(), O_RDONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
    EXPECT_THAT(open((overlay_ + "/lower_readable").c_str(), O_RDONLY),
                SyscallFailsWithErrno(EACCES));
    fd.reset(open((overlay_ + "/upper_readable").c_str(), O_RDONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Other UID can write if it has access in the overlay, even if it could not write directly via
    // `lower_`.
    fd.reset(open((overlay_ + "/writable").c_str(), O_WRONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
    EXPECT_THAT(open((overlay_ + "/lower_writable").c_str(), O_WRONLY),
                SyscallFailsWithErrno(EACCES));
    fd.reset(open((overlay_ + "/upper_writable").c_str(), O_WRONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Other UID can create a new file, which should be owned by them.
    fd.reset(open((overlay_ + "/new_file").c_str(), O_WRONLY | O_CREAT, 0666));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  });
  ASSERT_TRUE(helper.WaitForChildren());

  struct stat stats;
  // The new file should be owned by the other UID.
  ASSERT_THAT(stat((overlay_ + "/new_file").c_str(), &stats), SyscallSucceeds());
  EXPECT_EQ(stats.st_uid, kOtherUid);
  EXPECT_EQ(stats.st_gid, kOtherGid);

  // Existing files that are copied-up should not change ownership.
  ASSERT_THAT(stat((overlay_ + "/writable").c_str(), &stats), SyscallSucceeds());
  EXPECT_EQ(stats.st_uid, 0u);
  EXPECT_EQ(stats.st_gid, 0u);
}

// Verify that the `access()` API correctly reflects caller and mounter access check results.
TEST_F(OverlayFsAccessTest, LockedDownLowerAndWorkdirPermissions) {
  ASSERT_NO_FATAL_FAILURE(MountWithReadableAndWritableFiles());

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    ASSERT_NO_FATAL_FAILURE(DropToOtherUser());

    // Other UID can read if it has access in the overlay, even if it could not read directly from
    // `lower_`.
    EXPECT_THAT(access((overlay_ + "/readable").c_str(), R_OK), SyscallSucceeds());
    EXPECT_THAT(access((overlay_ + "/lower_readable").c_str(), R_OK),
                SyscallFailsWithErrno(EACCES));
    EXPECT_THAT(access((overlay_ + "/upper_readable").c_str(), R_OK), SyscallSucceeds());

    // Other UID can write if it has access in the overlay, even if it could not write directly via
    // `lower_`.
    EXPECT_THAT(access((overlay_ + "/writable").c_str(), W_OK), SyscallSucceeds());
    EXPECT_THAT(access((overlay_ + "/lower_writable").c_str(), W_OK),
                SyscallFailsWithErrno(EACCES));
    EXPECT_THAT(access((overlay_ + "/upper_writable").c_str(), W_OK), SyscallSucceeds());
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

// Verify that `access(X_OK)` and `execve()` evaluate caller execute permissions against the
// active overlay node (upper if copied up, otherwise lower) while using mounter credentials
// to access the underlying lower/upper filesystems.
TEST_F(OverlayFsAccessTest, ExecutableAccessChecks) {
  const std::string exit_zero_path = test_helper::GetTestResourcePath("exit_zero");
  // Use `print_uid_gid_exec_child` (which exits `EXIT_FAILURE` when `kOutputFd` is closed) for
  // files whose execution must fail, so an unexpected `execve()` success fails `WaitForChildren()`.
  const std::string fail_if_execed_path =
      test_helper::GetTestResourcePath("print_uid_gid_exec_child");

  ASSERT_NO_FATAL_FAILURE(StageExecutable(exit_zero_path, lower_ + "/executable", 0755));
  ASSERT_NO_FATAL_FAILURE(StageExecutable(fail_if_execed_path, lower_ + "/lower_executable", 0755));
  ASSERT_NO_FATAL_FAILURE(StageExecutable(exit_zero_path, lower_ + "/upper_executable", 0600));
  ASSERT_NO_FATAL_FAILURE(StageExecutable(fail_if_execed_path, lower_ + "/non_executable", 0644));

  ASSERT_NO_FATAL_FAILURE(Mount());
  ASSERT_THAT(chmod(overlay_.c_str(), 0777), SyscallSucceeds());

  // Revoke non-owner execute permission in upper for `lower_executable` (keeping owner execute so
  // the root mounter check still passes), and grant execute in upper for `upper_executable`.
  ASSERT_THAT(chmod((overlay_ + "/lower_executable").c_str(), 0700), SyscallSucceeds());
  ASSERT_THAT(chmod((overlay_ + "/upper_executable").c_str(), 0755), SyscallSucceeds());

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    ASSERT_NO_FATAL_FAILURE(DropToOtherUser());

    // Direct access to `lower_` fails because `lower_` has mode 0700 owned by root.
    EXPECT_THAT(access((lower_ + "/executable").c_str(), X_OK), SyscallFailsWithErrno(EACCES));

    EXPECT_THAT(access((overlay_ + "/executable").c_str(), X_OK), SyscallSucceeds());
    EXPECT_THAT(access((overlay_ + "/lower_executable").c_str(), X_OK),
                SyscallFailsWithErrno(EACCES));
    EXPECT_THAT(access((overlay_ + "/upper_executable").c_str(), X_OK), SyscallSucceeds());
    EXPECT_THAT(access((overlay_ + "/non_executable").c_str(), X_OK),
                SyscallFailsWithErrno(EACCES));

    std::string lower_exec_overlay = overlay_ + "/lower_executable";
    char* const lower_argv[] = {const_cast<char*>(lower_exec_overlay.c_str()), nullptr};
    EXPECT_THAT(execve(lower_exec_overlay.c_str(), lower_argv, nullptr),
                SyscallFailsWithErrno(EACCES));

    std::string non_exec_overlay = overlay_ + "/non_executable";
    char* const non_exec_argv[] = {const_cast<char*>(non_exec_overlay.c_str()), nullptr};
    EXPECT_THAT(execve(non_exec_overlay.c_str(), non_exec_argv, nullptr),
                SyscallFailsWithErrno(EACCES));
  });
  ASSERT_TRUE(helper.WaitForChildren());

  for (const char* name : {"/executable", "/upper_executable"}) {
    std::string target = overlay_ + name;
    helper.RunInForkedProcess([&] {
      ASSERT_NO_FATAL_FAILURE(DropToOtherUser());

      char* const argv[] = {const_cast<char*>(target.c_str()), nullptr};
      ASSERT_THAT(execve(target.c_str(), argv, nullptr), SyscallSucceeds());
    });
    ASSERT_TRUE(helper.WaitForChildren());
  }
}

// Verify SUID/SGID credential elevation and execute permission checks across lower and upper
// overlay layers.
// Regression test for https://fxbug.dev/508531124.
TEST_F(OverlayFsAccessTest, SuidAndSgidExecOnOverlay) {
  constexpr int kOutputFd = 100;
  const std::string creds_binary = test_helper::GetTestResourcePath("print_uid_gid_exec_child");

  // 1. SUID/SGID binary residing only in `lower_` (which is 0700, inaccessible directly to
  // `kOtherUid`).
  ASSERT_NO_FATAL_FAILURE(
      StageExecutable(creds_binary, lower_ + "/lower_suid", S_ISUID | S_ISGID | 0755));

  // 2. Binary that is 0600 in `lower_`, promoted to `S_ISUID | S_ISGID | 0755` in `upper_`.
  ASSERT_NO_FATAL_FAILURE(StageExecutable(creds_binary, lower_ + "/upper_suid", 0600));

  // 3. Binary that is `S_ISUID | S_ISGID | 0755` in `lower_`, but has non-owner execute permission
  // revoked in `upper_` (`S_ISUID | S_ISGID | 0700`).
  ASSERT_NO_FATAL_FAILURE(
      StageExecutable(creds_binary, lower_ + "/revoked_exec_suid", S_ISUID | S_ISGID | 0755));

  // 4. Binary that is `S_ISUID | S_ISGID | 0755` in `lower_`, but has SUID/SGID cleared in
  // `upper_` (`0755`).
  ASSERT_NO_FATAL_FAILURE(
      StageExecutable(creds_binary, lower_ + "/revoked_suid", S_ISUID | S_ISGID | 0755));

  ASSERT_NO_FATAL_FAILURE(Mount());
  ASSERT_THAT(chmod(overlay_.c_str(), 0777), SyscallSucceeds());

  ASSERT_THAT(chmod((overlay_ + "/upper_suid").c_str(), S_ISUID | S_ISGID | 0755),
              SyscallSucceeds());
  ASSERT_THAT(chmod((overlay_ + "/revoked_exec_suid").c_str(), S_ISUID | S_ISGID | 0700),
              SyscallSucceeds());
  ASSERT_THAT(chmod((overlay_ + "/revoked_suid").c_str(), 0755), SyscallSucceeds());

  auto run_and_expect_creds = [&](const std::string& rel_path, uid_t expected_euid,
                                  gid_t expected_egid) {
    SCOPED_TRACE(rel_path);
    fbl::unique_fd fd(SAFE_SYSCALL(test_helper::MemFdCreate("creds", O_RDWR)));
    ASSERT_TRUE(fd.is_valid());

    std::string target = overlay_ + rel_path;
    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&] {
      ASSERT_THAT(dup2(fd.get(), kOutputFd), SyscallSucceeds());
      ASSERT_NO_FATAL_FAILURE(DropToOtherUser());

      char* const argv[] = {const_cast<char*>(target.c_str()), nullptr};
      ASSERT_THAT(execve(target.c_str(), argv, nullptr), SyscallSucceeds());
    });
    ASSERT_TRUE(helper.WaitForChildren());

    ASSERT_THAT(lseek(fd.get(), 0, SEEK_SET), SyscallSucceeds());
    std::string content;
    ASSERT_TRUE(files::ReadFileDescriptorToString(fd.get(), &content));

    uid_t ruid = 0, euid = 0, suid = 0;
    gid_t rgid = 0, egid = 0, sgid = 0;
    ASSERT_EQ(sscanf(content.c_str(), "ruid: %u euid: %u suid: %u\nrgid: %u egid: %u sgid: %u\n",
                     &ruid, &euid, &suid, &rgid, &egid, &sgid),
              6)
        << "Unexpected credentials output: " << content;
    EXPECT_EQ(ruid, kOtherUid);
    EXPECT_EQ(euid, expected_euid);
    EXPECT_EQ(suid, expected_euid);
    EXPECT_EQ(rgid, kOtherGid);
    EXPECT_EQ(egid, expected_egid);
    EXPECT_EQ(sgid, expected_egid);
  };

  ASSERT_NO_FATAL_FAILURE(run_and_expect_creds("/lower_suid", 0, 0));
  ASSERT_NO_FATAL_FAILURE(run_and_expect_creds("/upper_suid", 0, 0));
  ASSERT_NO_FATAL_FAILURE(run_and_expect_creds("/revoked_suid", kOtherUid, kOtherGid));

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    ASSERT_NO_FATAL_FAILURE(DropToOtherUser());

    std::string revoked_exec = overlay_ + "/revoked_exec_suid";
    char* const argv[] = {const_cast<char*>(revoked_exec.c_str()), nullptr};
    EXPECT_THAT(execve(revoked_exec.c_str(), argv, nullptr), SyscallFailsWithErrno(EACCES));
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

// Verify that creating a setgid file or copying up a setgid regular file preserves `S_ISGID` and
// `st_gid` even when the mounter lacks `CAP_FSETID` and is not a member of the file's group.
// Regression test for https://fxbug.dev/584299369.
TEST_F(OverlayFsAccessTest, CreateAndCopyUpPreserveSgidWithoutMounterCapFsetid) {
  std::string lower_path = lower_ + "/copy_up_sgid";
  ASSERT_TRUE(files::WriteFile(lower_path, "initial"));
  ASSERT_THAT(chown(lower_path.c_str(), 0, kOtherGid), SyscallSucceeds());
  ASSERT_THAT(chmod(lower_path.c_str(), S_ISUID | S_ISGID | 0777), SyscallSucceeds());

  // Mount the overlay without `CAP_FSETID` in the mounter's credentials (whose `fsgid` is 0 and
  // whose supplementary group list does not contain `kOtherGid`).
  test_helper::ForkHelper mount_helper;
  mount_helper.RunInForkedProcess([&] {
    ASSERT_THAT(setgroups(0, nullptr), SyscallSucceeds());
    test_helper::UnsetCapabilityEffective(CAP_FSETID);
    ASSERT_NO_FATAL_FAILURE(Mount());
  });
  ASSERT_TRUE(mount_helper.WaitForChildren());
  ASSERT_THAT(chmod(overlay_.c_str(), 0777), SyscallSucceeds());

  // Trigger regular-file copy-up via `utimensat` and verify `S_ISUID`, `S_ISGID`, and `st_gid`
  // are preserved even though the mounter lacks `CAP_FSETID`.
  const struct timespec kTimes[2] = {{.tv_sec = 100, .tv_nsec = 0}, {.tv_sec = 200, .tv_nsec = 0}};
  ASSERT_THAT(utimensat(AT_FDCWD, (overlay_ + "/copy_up_sgid").c_str(), kTimes, 0),
              SyscallSucceeds());

  for (const std::string& base : {overlay_, upper_}) {
    struct stat st{};
    ASSERT_THAT(stat((base + "/copy_up_sgid").c_str(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & static_cast<mode_t>(S_ISUID | S_ISGID | 0777),
              static_cast<mode_t>(S_ISUID | S_ISGID | 0777));
    EXPECT_EQ(st.st_uid, 0u);
    EXPECT_EQ(st.st_gid, kOtherGid);
  }

  // Create a new executable file with `S_ISGID` as `kOtherUid:kOtherGid` and verify `S_ISGID` is
  // retained because the creating task belongs to `kOtherGid`, even though the mounter does not
  // and lacks `CAP_FSETID`.
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    ASSERT_NO_FATAL_FAILURE(DropToOtherUser());
    umask(0);
    fbl::unique_fd fd(open((overlay_ + "/new_sgid").c_str(), O_CREAT | O_WRONLY, S_ISGID | 0777));
    ASSERT_THAT(fd.get(), SyscallSucceeds());
  });
  ASSERT_TRUE(helper.WaitForChildren());

  for (const std::string& base : {overlay_, upper_}) {
    struct stat st{};
    ASSERT_THAT(stat((base + "/new_sgid").c_str(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & static_cast<mode_t>(S_ISGID | 0777),
              static_cast<mode_t>(S_ISGID | 0777));
    EXPECT_EQ(st.st_uid, kOtherUid);
    EXPECT_EQ(st.st_gid, kOtherGid);
  }
}

// Verify that an `S_ISGID` or non-empty `workdir` is sanitized into a private subdirectory at mount
// time so new nodes and copy-ups do not inherit `workdir` ownership or `S_ISGID`, while still
// honoring `S_ISGID` on overlay directories themselves.
TEST_F(OverlayFsAccessTest, SetgidWorkdirDoesNotPolluteOverlay) {
  constexpr gid_t kWorkGid = 12345;
  constexpr gid_t kDirGid = 23456;

  // Configure `work_` with `S_ISGID` and a stale `work` subdirectory owned by `kWorkGid`.
  std::string stale_work = work_ + "/work";
  std::string stale_dir = stale_work + "/stale_dir";
  ASSERT_THAT(mkdir(stale_work.c_str(), 0755), SyscallSucceeds());
  ASSERT_THAT(mkdir(stale_dir.c_str(), 0755), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(stale_dir + "/stale_file", "stale"));
  ASSERT_THAT(chown(work_.c_str(), 0, kWorkGid), SyscallSucceeds());
  ASSERT_THAT(chmod(work_.c_str(), S_ISGID | 0700), SyscallSucceeds());
  ASSERT_THAT(chown(stale_work.c_str(), 0, kWorkGid), SyscallSucceeds());
  ASSERT_THAT(chmod(stale_work.c_str(), S_ISGID | 0755), SyscallSucceeds());

  // Stage a lower file for copy-up testing.
  std::string lower_dir = lower_ + "/lower_dir";
  std::string lower_file = lower_dir + "/lower_file";
  ASSERT_THAT(mkdir(lower_dir.c_str(), 0777), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_file, "lower"));
  ASSERT_THAT(chown(lower_file.c_str(), kOtherUid, kOtherGid), SyscallSucceeds());
  ASSERT_THAT(chmod(lower_file.c_str(), 0644), SyscallSucceeds());

  ASSERT_NO_FATAL_FAILURE(Mount());
  ASSERT_THAT(chmod(overlay_.c_str(), 0777), SyscallSucceeds());

  // Verify the internal `work` subdirectory was cleared and stripped of `S_ISGID`/permissions.
  struct stat stale_st{};
  EXPECT_THAT(stat(stale_dir.c_str(), &stale_st), SyscallFailsWithErrno(ENOENT));
  struct stat internal_work_st{};
  ASSERT_THAT(stat(stale_work.c_str(), &internal_work_st), SyscallSucceeds());
  EXPECT_EQ(internal_work_st.st_mode & static_cast<mode_t>(S_ISUID | S_ISGID | S_ISVTX | 0777), 0u);

  // Create an `S_ISGID` directory in the overlay with a distinct `kDirGid`.
  std::string sgid_dir = overlay_ + "/sgid_dir";
  ASSERT_THAT(mkdir(sgid_dir.c_str(), 0777), SyscallSucceeds());
  ASSERT_THAT(chown(sgid_dir.c_str(), 0, kDirGid), SyscallSucceeds());
  ASSERT_THAT(chmod(sgid_dir.c_str(), S_ISGID | 0777), SyscallSucceeds());

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    ASSERT_NO_FATAL_FAILURE(DropToOtherUser());
    umask(0);

    // Non-setgid overlay root: nodes keep `kOtherGid` and do not inherit `kWorkGid` or `S_ISGID`.
    fbl::unique_fd fd(open((overlay_ + "/new_file").c_str(), O_CREAT | O_WRONLY, 0644));
    ASSERT_THAT(fd.get(), SyscallSucceeds());
    ASSERT_THAT(mkdir((overlay_ + "/new_dir").c_str(), 0755), SyscallSucceeds());
    ASSERT_THAT(mkfifo((overlay_ + "/new_fifo").c_str(), 0644), SyscallSucceeds());
    ASSERT_THAT(symlink("/target", (overlay_ + "/new_symlink").c_str()), SyscallSucceeds());

    // Setgid overlay directory: children inherit `kDirGid` (and `S_ISGID` for subdirs).
    fbl::unique_fd sgid_fd(open((sgid_dir + "/child_file").c_str(), O_CREAT | O_WRONLY, 0644));
    ASSERT_THAT(sgid_fd.get(), SyscallSucceeds());
    ASSERT_THAT(mkdir((sgid_dir + "/child_dir").c_str(), 0755), SyscallSucceeds());
    ASSERT_THAT(symlink("/target", (sgid_dir + "/child_symlink").c_str()), SyscallSucceeds());

    // Copy-up via write under the setgid `work_`.
    fbl::unique_fd copy_fd(open((overlay_ + "/lower_dir/lower_file").c_str(), O_WRONLY));
    ASSERT_THAT(copy_fd.get(), SyscallSucceeds());
    ASSERT_THAT(write(copy_fd.get(), "up", 2), SyscallSucceedsWithValue(2));
  });
  ASSERT_TRUE(helper.WaitForChildren());

  for (const std::string& base : {overlay_, upper_}) {
    for (const char* rel : {"/new_file", "/new_dir", "/new_fifo", "/new_symlink"}) {
      struct stat st{};
      // Use fstatat(..., AT_SYMLINK_NOFOLLOW) because arch32 does not implement lstat64.
      ASSERT_THAT(fstatat(AT_FDCWD, (base + rel).c_str(), &st, AT_SYMLINK_NOFOLLOW),
                  SyscallSucceeds())
          << base << rel;
      EXPECT_EQ(st.st_uid, kOtherUid) << base << rel;
      EXPECT_EQ(st.st_gid, kOtherGid) << base << rel;
      EXPECT_EQ(st.st_mode & static_cast<mode_t>(S_ISGID), 0u) << base << rel;
    }

    struct stat child_file_st{};
    ASSERT_THAT(stat((base + "/sgid_dir/child_file").c_str(), &child_file_st), SyscallSucceeds());
    EXPECT_EQ(child_file_st.st_uid, kOtherUid);
    EXPECT_EQ(child_file_st.st_gid, kDirGid);
    EXPECT_EQ(child_file_st.st_mode & static_cast<mode_t>(S_ISGID), 0u);

    struct stat child_dir_st{};
    ASSERT_THAT(stat((base + "/sgid_dir/child_dir").c_str(), &child_dir_st), SyscallSucceeds());
    EXPECT_EQ(child_dir_st.st_uid, kOtherUid);
    EXPECT_EQ(child_dir_st.st_gid, kDirGid);
    EXPECT_EQ(child_dir_st.st_mode & static_cast<mode_t>(S_ISGID), static_cast<mode_t>(S_ISGID));

    struct stat child_symlink_st{};
    ASSERT_THAT(fstatat(AT_FDCWD, (base + "/sgid_dir/child_symlink").c_str(), &child_symlink_st,
                        AT_SYMLINK_NOFOLLOW),
                SyscallSucceeds());
    EXPECT_EQ(child_symlink_st.st_uid, kOtherUid);
    EXPECT_EQ(child_symlink_st.st_gid, kDirGid);

    struct stat copy_st{};
    ASSERT_THAT(stat((base + "/lower_dir/lower_file").c_str(), &copy_st), SyscallSucceeds());
    EXPECT_EQ(copy_st.st_uid, kOtherUid);
    EXPECT_EQ(copy_st.st_gid, kOtherGid);
  }

  // Unmount and remount over the existing `0o000` `workdir/work` containing a stale entry.
  ASSERT_THAT(umount(overlay_.c_str()), SyscallSucceeds());
  ASSERT_THAT(mkdir(stale_dir.c_str(), 0755), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(stale_dir + "/stale_file", "remount_stale"));
  ASSERT_NO_FATAL_FAILURE(Mount());
  EXPECT_THAT(stat(stale_dir.c_str(), &stale_st), SyscallFailsWithErrno(ENOENT));
  ASSERT_THAT(stat(stale_work.c_str(), &internal_work_st), SyscallSucceeds());
  EXPECT_EQ(internal_work_st.st_mode & static_cast<mode_t>(S_ISUID | S_ISGID | S_ISVTX | 0777), 0u);
}

// Verify that overlapping `upperdir` and `workdir` fail to mount with `EINVAL` without deleting
// `upperdir/work` or `workdir/work` contents.
TEST_F(OverlayFsAccessTest, OverlappingUpperAndWorkdirRejected) {
  std::string bad_overlay = temp_dir_.path() + "/bad_overlay";
  ASSERT_THAT(mkdir(bad_overlay.c_str(), 0700), SyscallSucceeds());

  std::string shared_dir = upper_base_ + "/shared";
  std::string shared_work_dir = shared_dir + "/work";
  std::string shared_sentinel = shared_work_dir + "/keep";
  ASSERT_THAT(mkdir(shared_dir.c_str(), 0755), SyscallSucceeds());
  ASSERT_THAT(mkdir(shared_work_dir.c_str(), 0755), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(shared_sentinel, "keep"));

  std::string same_opts = fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", lower_.c_str(),
                                            shared_dir.c_str(), shared_dir.c_str());
  EXPECT_THAT(mount(nullptr, bad_overlay.c_str(), "overlay", 0, same_opts.c_str()),
              SyscallFailsWithErrno(EINVAL));
  EXPECT_EQ(ReadFileContent(shared_sentinel), "keep");

  std::string nested_upper = shared_dir + "/nested_upper";
  std::string nested_upper_work = nested_upper + "/work";
  std::string nested_sentinel = nested_upper_work + "/keep";
  ASSERT_THAT(mkdir(nested_upper.c_str(), 0755), SyscallSucceeds());
  ASSERT_THAT(mkdir(nested_upper_work.c_str(), 0755), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(nested_sentinel, "nested"));
  std::string upper_in_work_opts =
      fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", lower_.c_str(), nested_upper.c_str(),
                        shared_dir.c_str());
  EXPECT_THAT(mount(nullptr, bad_overlay.c_str(), "overlay", 0, upper_in_work_opts.c_str()),
              SyscallFailsWithErrno(EINVAL));
  EXPECT_EQ(ReadFileContent(shared_sentinel), "keep");

  std::string work_in_upper_opts =
      fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", lower_.c_str(), shared_dir.c_str(),
                        nested_upper.c_str());
  EXPECT_THAT(mount(nullptr, bad_overlay.c_str(), "overlay", 0, work_in_upper_opts.c_str()),
              SyscallFailsWithErrno(EINVAL));
  EXPECT_EQ(ReadFileContent(nested_sentinel), "nested");

  ASSERT_NO_FATAL_FAILURE(Mount());
}

// Verify that overlapping `lowerdir` and `workdir` fail to mount with `EINVAL` in either direction
// without mutating or clearing `workdir/work` contents.
TEST_F(OverlayFsAccessTest, LowerAndWorkdirOverlapRejected) {
  ASSERT_NO_FATAL_FAILURE(Mount());

  std::string test_root = temp_dir_.path() + "/overlap_lower_work";
  std::string bad_overlay_a = test_root + "/overlay_a";
  std::string bad_overlay_b = test_root + "/overlay_b";
  std::string test_upper = test_root + "/upper";
  std::string test_work = test_root + "/work";
  std::string lower_in_work = test_work + "/lower_in_work";
  std::string stale_work = test_work + "/work";
  std::string stale_keep = stale_work + "/keep";
  std::string test_lower = test_root + "/lower";
  std::string work_in_lower = test_lower + "/work_in_lower";

  for (const std::string& dir : {test_root, bad_overlay_a, bad_overlay_b, test_upper, test_work,
                                 lower_in_work, stale_work, test_lower, work_in_lower}) {
    ASSERT_THAT(mkdir(dir.c_str(), 0755), SyscallSucceeds()) << dir;
  }
  ASSERT_TRUE(files::WriteFile(stale_keep, "keep"));

  auto cleanup = fit::defer([&] {
    umount(bad_overlay_b.c_str());
    umount(bad_overlay_a.c_str());
  });

  std::string lower_in_work_opts =
      fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", lower_in_work.c_str(),
                        test_upper.c_str(), test_work.c_str());
  EXPECT_THAT(mount(nullptr, bad_overlay_a.c_str(), "overlay", 0, lower_in_work_opts.c_str()),
              SyscallFailsWithErrno(EINVAL));
  EXPECT_EQ(ReadFileContent(stale_keep), "keep");

  std::string work_in_lower_opts =
      fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", test_lower.c_str(),
                        test_upper.c_str(), work_in_lower.c_str());
  EXPECT_THAT(mount(nullptr, bad_overlay_b.c_str(), "overlay", 0, work_in_lower_opts.c_str()),
              SyscallFailsWithErrno(EINVAL));
  struct stat st{};
  EXPECT_THAT(stat((work_in_lower + "/work").c_str(), &st), SyscallFailsWithErrno(ENOENT));
}

// Verify that `upperdir` and `workdir` on distinct bind mounts of the same filesystem fail to
// mount with `EINVAL` without clearing `workdir/work` contents.
TEST_F(OverlayFsAccessTest, DifferentUpperAndWorkMountsRejected) {
  ASSERT_NO_FATAL_FAILURE(Mount());

  std::string test_root = temp_dir_.path() + "/diff_upper_work_mounts";
  std::string test_lower = test_root + "/lower";
  std::string mnt1 = test_root + "/mnt1";
  std::string mnt2 = test_root + "/mnt2";
  std::string bad_overlay = test_root + "/overlay";
  for (const std::string& dir : {test_root, test_lower, mnt1, mnt2, bad_overlay}) {
    ASSERT_THAT(mkdir(dir.c_str(), 0755), SyscallSucceeds()) << dir;
  }

  ASSERT_THAT(mount(nullptr, mnt1.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  auto unmount_mnt1 = fit::defer([&] { EXPECT_THAT(umount(mnt1.c_str()), SyscallSucceeds()); });

  std::string mnt1_upper = mnt1 + "/upper";
  std::string mnt1_work = mnt1 + "/work";
  std::string stale_work = mnt1_work + "/work";
  std::string stale_keep = stale_work + "/keep";
  for (const std::string& dir : {mnt1_upper, mnt1_work, stale_work}) {
    ASSERT_THAT(mkdir(dir.c_str(), 0755), SyscallSucceeds()) << dir;
  }
  ASSERT_TRUE(files::WriteFile(stale_keep, "keep"));

  ASSERT_THAT(mount(mnt1.c_str(), mnt2.c_str(), nullptr, MS_BIND, nullptr), SyscallSucceeds());
  auto unmount_mnt2 = fit::defer([&] { EXPECT_THAT(umount(mnt2.c_str()), SyscallSucceeds()); });
  auto cleanup_overlay = fit::defer([&] { umount(bad_overlay.c_str()); });

  std::string opts = fxl::StringPrintf("lowerdir=%s,upperdir=%s/upper,workdir=%s/work",
                                       test_lower.c_str(), mnt1.c_str(), mnt2.c_str());
  EXPECT_THAT(mount(nullptr, bad_overlay.c_str(), "overlay", 0, opts.c_str()),
              SyscallFailsWithErrno(EINVAL));
  EXPECT_EQ(ReadFileContent(stale_keep), "keep");
}

// Verify that mounts (both directory mounts and regular-file bind mounts) anywhere inside a
// stale `workdir/work` reject overlay mounting with `EBUSY` before unlinking sibling entries.
TEST_F(OverlayFsAccessTest, SubmountInWorkdirRejected) {
  ASSERT_NO_FATAL_FAILURE(Mount());

  std::string test_root = temp_dir_.path() + "/submount_in_work";
  std::string test_lower = test_root + "/lower";
  std::string test_upper = test_root + "/upper";
  for (const std::string& dir : {test_root, test_lower, test_upper}) {
    ASSERT_THAT(mkdir(dir.c_str(), 0755), SyscallSucceeds()) << dir;
  }

  {
    std::string test_work = test_root + "/work";
    std::string stale_work = test_work + "/work";
    std::string stale_keep = stale_work + "/keep";
    std::string sub_dir = stale_work + "/sub";
    std::string mounted_dir = sub_dir + "/mounted";
    std::string bad_overlay = test_root + "/overlay_dir";
    for (const std::string& dir : {test_work, stale_work, sub_dir, mounted_dir, bad_overlay}) {
      ASSERT_THAT(mkdir(dir.c_str(), 0755), SyscallSucceeds()) << dir;
    }
    ASSERT_TRUE(files::WriteFile(stale_keep, "keep"));

    ASSERT_THAT(mount(nullptr, mounted_dir.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
    auto unmount_sub =
        fit::defer([&] { EXPECT_THAT(umount(mounted_dir.c_str()), SyscallSucceeds()); });
    auto cleanup_overlay = fit::defer([&] { umount(bad_overlay.c_str()); });

    std::string mounted_file = mounted_dir + "/mounted_file";
    ASSERT_TRUE(files::WriteFile(mounted_file, "mounted"));

    std::string opts = fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", test_lower.c_str(),
                                         test_upper.c_str(), test_work.c_str());
    EXPECT_THAT(mount(nullptr, bad_overlay.c_str(), "overlay", 0, opts.c_str()),
                SyscallFailsWithErrno(EBUSY));
    EXPECT_EQ(ReadFileContent(stale_keep), "keep");
    EXPECT_EQ(ReadFileContent(mounted_file), "mounted");
  }

  // Also verify that bind-mounting a regular file inside `workdir/work` rejects mount with `EBUSY`
  // before unlinking sibling entries.
  {
    std::string work_file_submount = test_root + "/work_file_submount";
    std::string stale_file_work = work_file_submount + "/work";
    std::string file_keep = stale_file_work + "/keep";
    std::string mounted_reg = stale_file_work + "/mounted_reg";
    std::string bind_src = test_root + "/bind_src";
    std::string bad_overlay = test_root + "/overlay_file";
    for (const std::string& dir : {work_file_submount, stale_file_work, bad_overlay}) {
      ASSERT_THAT(mkdir(dir.c_str(), 0755), SyscallSucceeds()) << dir;
    }
    ASSERT_TRUE(files::WriteFile(file_keep, "keep"));
    ASSERT_TRUE(files::WriteFile(mounted_reg, ""));
    ASSERT_TRUE(files::WriteFile(bind_src, "src"));

    ASSERT_THAT(mount(bind_src.c_str(), mounted_reg.c_str(), nullptr, MS_BIND, nullptr),
                SyscallSucceeds());
    auto unmount_reg =
        fit::defer([&] { EXPECT_THAT(umount(mounted_reg.c_str()), SyscallSucceeds()); });
    auto cleanup_overlay = fit::defer([&] { umount(bad_overlay.c_str()); });

    std::string opts = fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", test_lower.c_str(),
                                         test_upper.c_str(), work_file_submount.c_str());
    EXPECT_THAT(mount(nullptr, bad_overlay.c_str(), "overlay", 0, opts.c_str()),
                SyscallFailsWithErrno(EBUSY));
    EXPECT_EQ(ReadFileContent(file_keep), "keep");
  }
}

// Verify that mounting an overlay without `CAP_DAC_OVERRIDE` fails with `EACCES` when preparing
// the `0o000` `workdir/work` directory, and cleans up the partially created `work` directory.
TEST_F(OverlayFsAccessTest, PrepareWorkdirRequiresDacOverride) {
  if (!test_helper::HasCapabilityEffective(CAP_DAC_OVERRIDE)) {
    GTEST_SKIP() << "Caller lacks CAP_DAC_OVERRIDE, skipping test.";
  }

  ASSERT_NO_FATAL_FAILURE(Mount());

  std::string test_root = temp_dir_.path() + "/dac_override_work";
  std::string bad_overlay = test_root + "/overlay";
  std::string test_lower = test_root + "/lower";
  std::string test_upper = test_root + "/upper";
  std::string test_work = test_root + "/work";
  for (const std::string& dir : {test_root, bad_overlay, test_lower, test_upper, test_work}) {
    ASSERT_THAT(mkdir(dir.c_str(), 0700), SyscallSucceeds()) << dir;
  }

  auto cleanup_overlay = fit::defer([&] { umount(bad_overlay.c_str()); });

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    test_helper::UnsetCapabilityEffective(CAP_DAC_OVERRIDE);
    std::string opts = fxl::StringPrintf("lowerdir=%s,upperdir=%s,workdir=%s", test_lower.c_str(),
                                         test_upper.c_str(), test_work.c_str());
    EXPECT_THAT(mount(nullptr, bad_overlay.c_str(), "overlay", 0, opts.c_str()),
                SyscallFailsWithErrno(EACCES));
  });
  ASSERT_TRUE(helper.WaitForChildren());

  struct stat st{};
  EXPECT_THAT(stat((test_work + "/work").c_str(), &st), SyscallFailsWithErrno(ENOENT));
}

// Verify that copying up a non-setgid child directory (staged via `work`) under an `S_ISGID`
// parent directory does not inherit the parent's `gid` or `S_ISGID`.
TEST_F(OverlayFsAccessTest, CopyUpDirUnderSetgidParentDoesNotInheritSgid) {
  ASSERT_THAT(mkdir((lower_ + "/sgid_parent").c_str(), 0777), SyscallSucceeds());
  ASSERT_THAT(mkdir((lower_ + "/sgid_parent/child_dir").c_str(), 0755), SyscallSucceeds());
  ASSERT_THAT(chown((lower_ + "/sgid_parent").c_str(), 0, kOtherGid), SyscallSucceeds());
  ASSERT_THAT(chmod((lower_ + "/sgid_parent").c_str(), S_ISGID | 0777), SyscallSucceeds());

  ASSERT_NO_FATAL_FAILURE(Mount());

  const struct timespec kTimes[2] = {{.tv_sec = 100, .tv_nsec = 0}, {.tv_sec = 200, .tv_nsec = 0}};
  ASSERT_THAT(utimensat(AT_FDCWD, (overlay_ + "/sgid_parent/child_dir").c_str(), kTimes, 0),
              SyscallSucceeds());
  for (const std::string& base : {overlay_, upper_}) {
    struct stat st{};
    ASSERT_THAT(stat((base + "/sgid_parent/child_dir").c_str(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & static_cast<mode_t>(S_ISGID | 0777), 0755u);
    EXPECT_EQ(st.st_gid, 0u);
  }
}

// Verify that copying up a lower symlink preserves its ownership on the upper layer.
TEST_F(OverlayFsAccessTest, SymlinkCopyUpPreservesOwner) {
  // Use fchownat/fstatat with AT_SYMLINK_NOFOLLOW because arch32 does not implement
  // lchown32/lstat64.
  std::string lower_link = lower_ + "/lower_link";
  ASSERT_THAT(symlink("/target/path", lower_link.c_str()), SyscallSucceeds());
  ASSERT_THAT(fchownat(AT_FDCWD, lower_link.c_str(), kOtherUid, kOtherGid, AT_SYMLINK_NOFOLLOW),
              SyscallSucceeds());

  std::string root_gid_link = lower_ + "/root_gid_link";
  ASSERT_THAT(symlink("/other/target", root_gid_link.c_str()), SyscallSucceeds());
  ASSERT_THAT(fchownat(AT_FDCWD, root_gid_link.c_str(), 0, kOtherGid, AT_SYMLINK_NOFOLLOW),
              SyscallSucceeds());

  ASSERT_NO_FATAL_FAILURE(Mount());

  // Trigger metadata-only copy-up of both symlinks without following them.
  const struct timespec kTimes[2] = {{.tv_sec = 100, .tv_nsec = 0}, {.tv_sec = 200, .tv_nsec = 0}};
  ASSERT_THAT(utimensat(AT_FDCWD, (overlay_ + "/lower_link").c_str(), kTimes, AT_SYMLINK_NOFOLLOW),
              SyscallSucceeds());
  ASSERT_THAT(
      utimensat(AT_FDCWD, (overlay_ + "/root_gid_link").c_str(), kTimes, AT_SYMLINK_NOFOLLOW),
      SyscallSucceeds());

  for (const std::string& base : {overlay_, upper_}) {
    struct stat st{};
    ASSERT_THAT(fstatat(AT_FDCWD, (base + "/lower_link").c_str(), &st, AT_SYMLINK_NOFOLLOW),
                SyscallSucceeds());
    EXPECT_TRUE(S_ISLNK(st.st_mode));
    EXPECT_EQ(st.st_uid, kOtherUid);
    EXPECT_EQ(st.st_gid, kOtherGid);
    EXPECT_EQ(Readlink(base + "/lower_link"), "/target/path");

    struct stat root_st{};
    ASSERT_THAT(fstatat(AT_FDCWD, (base + "/root_gid_link").c_str(), &root_st, AT_SYMLINK_NOFOLLOW),
                SyscallSucceeds());
    EXPECT_TRUE(S_ISLNK(root_st.st_mode));
    EXPECT_EQ(root_st.st_uid, 0u);
    EXPECT_EQ(root_st.st_gid, kOtherGid);
    EXPECT_EQ(Readlink(base + "/root_gid_link"), "/other/target");
  }
}

// Verify sticky-directory unlink restrictions and replacing lower-layer whiteouts by another user.
TEST_F(OverlayFsAccessTest, StickyDirWhiteoutAndOwnership) {
  constexpr uid_t kThirdUid = 65533;
  constexpr gid_t kThirdGid = 65533;

  std::string lower_sticky = lower_ + "/sticky";
  ASSERT_THAT(mkdir(lower_sticky.c_str(), 0777), SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_sticky + "/other_file", "other"));
  ASSERT_THAT(chown((lower_sticky + "/other_file").c_str(), kOtherUid, kOtherGid),
              SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_sticky + "/third_file", "third"));
  ASSERT_THAT(chown((lower_sticky + "/third_file").c_str(), kThirdUid, kThirdGid),
              SyscallSucceeds());
  ASSERT_TRUE(files::WriteFile(lower_sticky + "/rename_target", "target"));
  ASSERT_THAT(chown((lower_sticky + "/rename_target").c_str(), kOtherUid, kOtherGid),
              SyscallSucceeds());
  ASSERT_THAT(chmod(lower_sticky.c_str(), S_ISVTX | 0777), SyscallSucceeds());

  // Also test with a sticky `workdir`.
  ASSERT_THAT(chmod(work_.c_str(), S_ISVTX | 0700), SyscallSucceeds());

  // Mount with the mounter's default capabilities (including `CAP_FOWNER`): Linux also requires
  // mounter `CAP_FOWNER` when `kThirdUid` replaces a mounter-owned whiteout in `sticky` because
  // whiteout replacement checks sticky permissions against caller `fsuid`/`fsgid` + mounter caps.
  ASSERT_NO_FATAL_FAILURE(Mount());
  ASSERT_THAT(chmod(overlay_.c_str(), 0777), SyscallSucceeds());

  std::string overlay_sticky = overlay_ + "/sticky";

  // Have `kThirdUid` create a file in the sticky dir (copying `sticky` up).
  {
    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&] {
      ASSERT_NO_FATAL_FAILURE(DropToUser(kThirdUid, kThirdGid));
      fbl::unique_fd fd(open((overlay_sticky + "/user_src").c_str(), O_CREAT | O_WRONLY, 0644));
      ASSERT_THAT(fd.get(), SyscallSucceeds());
    });
    ASSERT_TRUE(helper.WaitForChildren());
  }

  // `kOtherUid` cannot unlink files owned by `kThirdUid`, but can unlink its own lower files
  // (leaving mounter-owned whiteouts in the sticky upper directory).
  {
    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&] {
      ASSERT_NO_FATAL_FAILURE(DropToOtherUser());
      EXPECT_THAT(unlink((overlay_sticky + "/third_file").c_str()), SyscallFailsWithErrno(EPERM));
      EXPECT_THAT(unlink((overlay_sticky + "/user_src").c_str()), SyscallFailsWithErrno(EPERM));
      EXPECT_THAT(unlink((overlay_sticky + "/other_file").c_str()), SyscallSucceeds());
      EXPECT_THAT(unlink((overlay_sticky + "/rename_target").c_str()), SyscallSucceeds());
    });
    ASSERT_TRUE(helper.WaitForChildren());
  }

  // `kThirdUid` can both create a new file and rename over the whiteouts in the sticky dir.
  {
    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&] {
      ASSERT_NO_FATAL_FAILURE(DropToUser(kThirdUid, kThirdGid));
      fbl::unique_fd fd(open((overlay_sticky + "/other_file").c_str(), O_CREAT | O_WRONLY, 0644));
      ASSERT_THAT(fd.get(), SyscallSucceeds());
      EXPECT_THAT(rename((overlay_sticky + "/user_src").c_str(),
                         (overlay_sticky + "/rename_target").c_str()),
                  SyscallSucceeds());
    });
    ASSERT_TRUE(helper.WaitForChildren());
  }

  for (const std::string& base : {overlay_ + "/sticky", upper_ + "/sticky"}) {
    SCOPED_TRACE(base);
    for (const char* name : {"/other_file", "/rename_target"}) {
      struct stat st{};
      ASSERT_THAT(stat((base + name).c_str(), &st), SyscallSucceeds()) << name;
      EXPECT_EQ(st.st_uid, kThirdUid) << name;
      EXPECT_EQ(st.st_gid, kThirdGid) << name;
    }
  }
}

// Verify that modifying files in lower or upper overlay layers clears S_ISUID, S_ISGID, and
// security.capability when the caller lacks CAP_FSETID and CAP_SETFCAP, and that non-modifying
// copy-up preserves S_ISUID, S_ISGID, and security.capability.
// Regression test for https://fxbug.dev/584299369.
TEST_F(OverlayFsAccessTest, ClearPrivilegesOnModifyAcrossLayers) {
  const std::vector<std::string> kFileNames = {
      "/lower_write", "/lower_trunc", "/lower_ftrunc", "/lower_open_trunc", "/lower_falloc",
      "/upper_write", "/upper_trunc", "/upper_ftrunc", "/upper_open_trunc",
  };
  for (const auto& name : kFileNames) {
    std::string lower_path = lower_ + name;
    ASSERT_TRUE(files::WriteFile(lower_path, "initial"));
    ASSERT_THAT(chown(lower_path.c_str(), 0, kOtherGid), SyscallSucceeds());
    ASSERT_THAT(chmod(lower_path.c_str(), S_ISUID | S_ISGID | 0777), SyscallSucceeds());
    ASSERT_THAT(test_helper::SetFileCapabilitiesV2(lower_path, 1ULL << CAP_NET_RAW),
                SyscallSucceeds());
  }

  ASSERT_NO_FATAL_FAILURE(Mount());
  ASSERT_THAT(chmod(overlay_.c_str(), 0777), SyscallSucceeds());

  // Promote upper-target test files via metadata-only update (`utimensat`), and verify that
  // copy-up preserves S_ISUID, S_ISGID, gid, and `security.capability`.
  const struct timespec kTimes[2] = {{.tv_sec = 100, .tv_nsec = 0}, {.tv_sec = 200, .tv_nsec = 0}};
  for (const char* name : {"/upper_write", "/upper_trunc", "/upper_ftrunc", "/upper_open_trunc"}) {
    std::string overlay_path = overlay_ + name;
    ASSERT_THAT(utimensat(AT_FDCWD, overlay_path.c_str(), kTimes, 0), SyscallSucceeds());
    struct stat st{};
    ASSERT_THAT(stat(overlay_path.c_str(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & static_cast<mode_t>(S_ISUID | S_ISGID | 0777),
              static_cast<mode_t>(S_ISUID | S_ISGID | 0777));
    EXPECT_EQ(st.st_gid, kOtherGid);
    EXPECT_THAT(getxattr(overlay_path.c_str(), "security.capability", nullptr, 0),
                SyscallSucceeds());
    EXPECT_THAT(getxattr((upper_ + name).c_str(), "security.capability", nullptr, 0),
                SyscallSucceeds());
  }

  // Modify files as an unprivileged user (lacking CAP_FSETID and CAP_SETFCAP).
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    ASSERT_NO_FATAL_FAILURE(DropToOtherUser());

    for (const char* name : {"/lower_write", "/upper_write"}) {
      fbl::unique_fd fd(open((overlay_ + name).c_str(), O_WRONLY));
      ASSERT_THAT(fd.get(), SyscallSucceeds());
      ASSERT_THAT(write(fd.get(), "x", 1), SyscallSucceedsWithValue(1));
    }

    for (const char* name : {"/lower_trunc", "/upper_trunc"}) {
      ASSERT_THAT(truncate((overlay_ + name).c_str(), 3), SyscallSucceeds());
    }

    for (const char* name : {"/lower_ftrunc", "/upper_ftrunc"}) {
      fbl::unique_fd fd(open((overlay_ + name).c_str(), O_WRONLY));
      ASSERT_THAT(fd.get(), SyscallSucceeds());
      ASSERT_THAT(ftruncate(fd.get(), 3), SyscallSucceeds());
    }

    for (const char* name : {"/lower_open_trunc", "/upper_open_trunc"}) {
      fbl::unique_fd fd(open((overlay_ + name).c_str(), O_WRONLY | O_TRUNC));
      ASSERT_THAT(fd.get(), SyscallSucceeds());
    }

    {
      fbl::unique_fd fd(open((overlay_ + "/lower_falloc").c_str(), O_WRONLY));
      ASSERT_THAT(fd.get(), SyscallSucceeds());
      ASSERT_THAT(fallocate(fd.get(), 0, 0, 16), SyscallSucceeds());
    }
  });
  ASSERT_TRUE(helper.WaitForChildren());

  for (const auto& name : kFileNames) {
    SCOPED_TRACE(name);
    for (const std::string& base : {overlay_, upper_}) {
      std::string path = base + name;
      struct stat st{};
      ASSERT_THAT(stat(path.c_str(), &st), SyscallSucceeds());
      EXPECT_EQ(st.st_mode & static_cast<mode_t>(S_ISUID | S_ISGID), 0u);
      EXPECT_THAT(getxattr(path.c_str(), "security.capability", nullptr, 0),
                  SyscallFailsWithErrno(ENODATA));
    }
  }
}

// Verify that when a caller with CAP_FSETID (and without CAP_SETFCAP) truncates an overlay file,
// S_ISUID and S_ISGID are preserved even if the mounter lacked CAP_FSETID, while
// `security.capability` is still cleared.
// Regression test for https://fxbug.dev/584299369.
TEST_F(OverlayFsAccessTest, TruncateWithCapFsetidPreservesSuidWhenMounterLacksIt) {
  if (!test_helper::HasCapabilityEffective(CAP_FSETID)) {
    GTEST_SKIP() << "Caller lacks CAP_FSETID, skipping test.";
  }

  std::string lower_path = lower_ + "/trunc_fsetid";
  ASSERT_TRUE(files::WriteFile(lower_path, "initial"));
  ASSERT_THAT(chown(lower_path.c_str(), 0, kOtherGid), SyscallSucceeds());
  ASSERT_THAT(chmod(lower_path.c_str(), S_ISUID | S_ISGID | 0777), SyscallSucceeds());
  ASSERT_THAT(test_helper::SetFileCapabilitiesV2(lower_path, 1ULL << CAP_NET_RAW),
              SyscallSucceeds());

  // Mount the overlay without `CAP_FSETID` in the mounter's credentials.
  test_helper::ForkHelper mount_helper;
  mount_helper.RunInForkedProcess([&] {
    test_helper::UnsetCapabilityEffective(CAP_FSETID);
    ASSERT_NO_FATAL_FAILURE(Mount());
  });
  ASSERT_TRUE(mount_helper.WaitForChildren());

  // Caller retains `CAP_FSETID` and drops `CAP_SETFCAP`.
  test_helper::ForkHelper trunc_helper;
  trunc_helper.RunInForkedProcess([&] {
    test_helper::UnsetCapabilityEffective(CAP_SETFCAP);
    ASSERT_THAT(truncate((overlay_ + "/trunc_fsetid").c_str(), 2), SyscallSucceeds());
  });
  ASSERT_TRUE(trunc_helper.WaitForChildren());

  for (const std::string& base : {overlay_, upper_}) {
    std::string path = base + "/trunc_fsetid";
    struct stat st{};
    ASSERT_THAT(stat(path.c_str(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & static_cast<mode_t>(S_ISUID | S_ISGID),
              static_cast<mode_t>(S_ISUID | S_ISGID));
    EXPECT_EQ(st.st_gid, kOtherGid);
    EXPECT_THAT(getxattr(path.c_str(), "security.capability", nullptr, 0),
                SyscallFailsWithErrno(ENODATA));
  }
}

// Verify that `chown()` clears `S_ISUID`, `S_ISGID`, and `security.capability` across both overlay
// layers even when the caller holds `CAP_FSETID`.
// Regression test for https://fxbug.dev/584299369.
TEST_F(OverlayFsAccessTest, ChownClearsPrivilegesWithCapFsetidAcrossLayers) {
  if (!test_helper::HasCapabilityEffective(CAP_FSETID)) {
    GTEST_SKIP() << "Caller lacks CAP_FSETID, skipping test.";
  }

  const std::vector<std::string> kFileNames = {"/lower_chown", "/upper_chown"};
  for (const auto& name : kFileNames) {
    std::string lower_path = lower_ + name;
    ASSERT_TRUE(files::WriteFile(lower_path, "initial"));
    ASSERT_THAT(chown(lower_path.c_str(), 0, kOtherGid), SyscallSucceeds());
    ASSERT_THAT(chmod(lower_path.c_str(), S_ISUID | S_ISGID | 0777), SyscallSucceeds());
    ASSERT_THAT(test_helper::SetFileCapabilitiesV2(lower_path, 1ULL << CAP_NET_RAW),
                SyscallSucceeds());
  }

  ASSERT_NO_FATAL_FAILURE(Mount());

  const struct timespec kTimes[2] = {{.tv_sec = 100, .tv_nsec = 0}, {.tv_sec = 200, .tv_nsec = 0}};
  ASSERT_THAT(utimensat(AT_FDCWD, (overlay_ + "/upper_chown").c_str(), kTimes, 0),
              SyscallSucceeds());

  for (const auto& name : kFileNames) {
    SCOPED_TRACE(name);
    ASSERT_THAT(chown((overlay_ + name).c_str(), kOtherUid, -1), SyscallSucceeds());
    for (const std::string& base : {overlay_, upper_}) {
      std::string path = base + name;
      struct stat st{};
      ASSERT_THAT(stat(path.c_str(), &st), SyscallSucceeds());
      EXPECT_EQ(st.st_uid, kOtherUid);
      EXPECT_EQ(st.st_mode & static_cast<mode_t>(S_ISUID | S_ISGID), 0u);
      EXPECT_THAT(getxattr(path.c_str(), "security.capability", nullptr, 0),
                  SyscallFailsWithErrno(ENODATA));
    }
  }
}

// Verify that modifying an upper file carrying `security.capability` fails with `EPERM` when the
// mounter lacked `CAP_SETFCAP`, even if the caller holds `CAP_SETFCAP`.
TEST_F(OverlayFsAccessTest, ModifyUpperWithCapsFailsWhenMounterLacksCapSetfcap) {
  if (!test_helper::HasCapabilityEffective(CAP_SETFCAP)) {
    GTEST_SKIP() << "Caller lacks CAP_SETFCAP, skipping test.";
  }

  std::string upper_path = upper_ + "/upper_caps";
  ASSERT_TRUE(files::WriteFile(upper_path, "initial"));
  ASSERT_THAT(chmod(upper_path.c_str(), 0755), SyscallSucceeds());
  ASSERT_THAT(test_helper::SetFileCapabilitiesV2(upper_path, 1ULL << CAP_NET_RAW),
              SyscallSucceeds());

  // Mount the overlay without `CAP_SETFCAP` in the mounter's credentials.
  test_helper::ForkHelper mount_helper;
  mount_helper.RunInForkedProcess([&] {
    test_helper::UnsetCapabilityEffective(CAP_SETFCAP);
    ASSERT_NO_FATAL_FAILURE(Mount());
  });
  ASSERT_TRUE(mount_helper.WaitForChildren());

  EXPECT_THAT(truncate((overlay_ + "/upper_caps").c_str(), 2), SyscallFailsWithErrno(EPERM));

  struct stat st{};
  ASSERT_THAT(stat(upper_path.c_str(), &st), SyscallSucceeds());
  EXPECT_EQ(st.st_size, 7);
  EXPECT_THAT(getxattr(upper_path.c_str(), "security.capability", nullptr, 0), SyscallSucceeds());
}

// Verify that `security.capability` is honored on `execve()` for binaries in `lower_` and
// `upper_`, and that modifying a binary through the overlay revokes `security.capability` on
// subsequent `execve()`.
// Regression test for https://fxbug.dev/584299369.
TEST_F(OverlayFsAccessTest, FileCapabilitiesExecOnOverlay) {
  constexpr uint64_t kCapMask = (1ULL << CAP_NET_RAW);
  const std::string helper_binary = test_helper::GetTestResourcePath("print_helper");

  // 1. Binary in `lower_` with `security.capability` set before mount.
  ASSERT_NO_FATAL_FAILURE(StageExecutable(helper_binary, lower_ + "/lower_caps", 0755));
  ASSERT_THAT(test_helper::SetFileCapabilitiesV2(lower_ + "/lower_caps", kCapMask),
              SyscallSucceeds());

  // 2. Binary in `lower_` without capabilities, given `security.capability` via `overlay_` after
  // mount.
  ASSERT_NO_FATAL_FAILURE(StageExecutable(helper_binary, lower_ + "/upper_caps", 0755));

  // 3. Binary in `lower_` with `security.capability` that is modified (via `truncate` to the same
  // size) by an unprivileged user through `overlay_`.
  ASSERT_NO_FATAL_FAILURE(StageExecutable(helper_binary, lower_ + "/modified_caps", 0777));
  ASSERT_THAT(test_helper::SetFileCapabilitiesV2(lower_ + "/modified_caps", kCapMask),
              SyscallSucceeds());

  ASSERT_NO_FATAL_FAILURE(Mount());
  ASSERT_THAT(chmod(overlay_.c_str(), 0777), SyscallSucceeds());

  ASSERT_THAT(test_helper::SetFileCapabilitiesV2(overlay_ + "/upper_caps", kCapMask),
              SyscallSucceeds());

  // Modify `/modified_caps` as an unprivileged user (truncating to its existing size so the ELF
  // binary remains valid while clearing file capabilities).
  test_helper::ForkHelper modify_helper;
  modify_helper.RunInForkedProcess([&] {
    ASSERT_NO_FATAL_FAILURE(DropToOtherUser());
    struct stat st{};
    ASSERT_THAT(stat((overlay_ + "/modified_caps").c_str(), &st), SyscallSucceeds());
    ASSERT_THAT(truncate((overlay_ + "/modified_caps").c_str(), st.st_size), SyscallSucceeds());
  });
  ASSERT_TRUE(modify_helper.WaitForChildren());

  auto run_and_expect_cap_net_raw = [&](const std::string& rel_path, bool expected_has_cap) {
    SCOPED_TRACE(rel_path);
    fbl::unique_fd fd(test_helper::MemFdCreate("caps", 0));
    ASSERT_THAT(fd.get(), SyscallSucceeds());

    std::string target = overlay_ + rel_path;
    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&] {
      ASSERT_THAT(dup2(fd.get(), STDOUT_FILENO), SyscallSucceeds());
      ASSERT_NO_FATAL_FAILURE(DropToOtherUser());

      char* const argv[] = {
          const_cast<char*>(target.c_str()),
          const_cast<char*>("capabilities"),
          nullptr,
      };
      ASSERT_THAT(execve(target.c_str(), argv, nullptr), SyscallSucceeds());
    });
    ASSERT_TRUE(helper.WaitForChildren());

    ASSERT_THAT(lseek(fd.get(), 0, SEEK_SET), SyscallSucceeds());
    std::string content;
    ASSERT_TRUE(files::ReadFileDescriptorToString(fd.get(), &content));

    const int bit = expected_has_cap ? 1 : 0;
    const std::string expected_line = fxl::StringPrintf("\n%d,%d,%d,0,", CAP_NET_RAW, bit, bit);
    EXPECT_NE(content.find(expected_line), std::string::npos)
        << "Expected '" << expected_line << "' in capabilities output:\n"
        << content;
  };

  ASSERT_NO_FATAL_FAILURE(run_and_expect_cap_net_raw("/lower_caps", /* expected_has_cap = */ true));
  ASSERT_NO_FATAL_FAILURE(run_and_expect_cap_net_raw("/upper_caps", /* expected_has_cap = */ true));
  ASSERT_NO_FATAL_FAILURE(
      run_and_expect_cap_net_raw("/modified_caps", /* expected_has_cap = */ false));
}
}  // namespace
