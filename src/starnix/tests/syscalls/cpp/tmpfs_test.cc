// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <sched.h>
#include <stdlib.h>
#include <string.h>
#include <sys/fsuid.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <linux/capability.h>

#include "fault_test.h"
#include "fault_test_suite.h"
#include "src/lib/fxl/strings/split_string.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

int CreateTempFile() {
  char tmpl[] = "/tmp/tmpfile.XXXXXX";
  return mkstemp(tmpl);
}

INSTANTIATE_TEST_SUITE_P(TmpfsFaultTest, FaultFileTest, ::testing::Values(CreateTempFile));

TEST(TmpfsTest, DefaultStickyBit) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "need CAP_SYS_ADMIN to mount tmpfs";
  }

  test_helper::ScopedTempDir temp_dir;

  auto mount_result = test_helper::ScopedMount::Mount("none", temp_dir.path(), "tmpfs", 0, nullptr);
  ASSERT_THAT(mount_result, SyscallResultIsOk()) << "mount(tmpfs) failed";

  struct stat stat_buf;
  ASSERT_EQ(stat(temp_dir.path().c_str(), &stat_buf), 0);
  EXPECT_EQ(stat_buf.st_mode & 07777, 01777u) << "Sticky bit should be set on tmpfs root";
}

TEST(TmpfsTest, CasefoldDisabledByDefault) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "need CAP_SYS_ADMIN to mount tmpfs";
  }

  test_helper::ScopedTempDir temp_dir;

  ASSERT_THAT(test_helper::ScopedMount::Mount("none", temp_dir.path(), "tmpfs", 0, nullptr),
              SyscallResultIsOk());

  std::string dir_path = temp_dir.path() + "/dir";
  ASSERT_THAT(mkdir(dir_path.c_str(), 0777), SyscallSucceeds());

  EXPECT_THAT(test_helper::SetCasefold(dir_path, true), SyscallResultIsErrno(EOPNOTSUPP));
}

TEST(TmpfsTest, DefaultUidAndGidUseCallerFsCred) {
  if (!test_helper::HasSysAdmin() || !test_helper::HasCapability(CAP_SETUID) ||
      !test_helper::HasCapability(CAP_SETGID)) {
    GTEST_SKIP() << "need CAP_SYS_ADMIN, CAP_SETUID, and CAP_SETGID";
  }

  test_helper::ScopedTempDir temp_dir;

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    ASSERT_THAT(unshare(CLONE_NEWNS), SyscallSucceeds());
    ASSERT_THAT(mount(nullptr, "/", nullptr, MS_PRIVATE | MS_REC, nullptr), SyscallSucceeds());

    constexpr uid_t kTestUid = 1082;
    constexpr gid_t kTestGid = 1082;
    ASSERT_THAT(prctl(PR_SET_KEEPCAPS, 1), SyscallSucceeds());
    ASSERT_THAT(setresgid(kTestGid, kTestGid, kTestGid), SyscallSucceeds());
    ASSERT_THAT(setresuid(kTestUid, kTestUid, kTestUid), SyscallSucceeds());

    test_helper::SetCapabilityEffective(CAP_SYS_ADMIN);
    test_helper::UnsetCapabilityEffective(CAP_DAC_OVERRIDE);

    ASSERT_THAT(mount("tmpfs", temp_dir.path().c_str(), "tmpfs", 0, "mode=0755"),
                SyscallSucceeds());

    struct stat stat_buf;
    ASSERT_THAT(stat(temp_dir.path().c_str(), &stat_buf), SyscallSucceeds());
    EXPECT_EQ(stat_buf.st_uid, kTestUid);
    EXPECT_EQ(stat_buf.st_gid, kTestGid);
    EXPECT_EQ(stat_buf.st_mode & 07777, 0755u);

    std::string subdir_path = temp_dir.path() + "/subdir";
    EXPECT_THAT(mkdir(subdir_path.c_str(), 0755), SyscallSucceeds());
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

TEST(TmpfsTest, NoswapMountAndFileOperations) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "need CAP_SYS_ADMIN to mount tmpfs with noswap";
  }
  if (!test_helper::IsStarnix() && !test_helper::IsKernelVersionAtLeast(6, 4)) {
    GTEST_SKIP() << "tmpfs noswap mount option requires Linux 6.4+";
  }

  test_helper::ScopedTempDir temp_dir;

  auto mount_result =
      test_helper::ScopedMount::Mount("none", temp_dir.path(), "tmpfs", 0, "noswap");
  ASSERT_THAT(mount_result, SyscallResultIsOk()) << "mount(tmpfs, noswap) failed";

  auto mount_info = test_helper::ReadMountInfoLine(temp_dir.path());
  ASSERT_TRUE(mount_info.has_value());
  auto super_options =
      fxl::SplitString(mount_info->super_options, ",", fxl::kKeepWhitespace, fxl::kSplitWantAll);
  EXPECT_THAT(super_options, ::testing::Contains("noswap")) << mount_info->super_options;

  std::string file_path = temp_dir.path() + "/pinned_file";
  fbl::unique_fd fd(open(file_path.c_str(), O_CREAT | O_RDWR, 0600));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);

  constexpr char kWriteData[] = "hello noswap";
  ASSERT_THAT(write(fd.get(), kWriteData, sizeof(kWriteData)),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(kWriteData))));

  char read_buf[sizeof(kWriteData)] = {};
  ASSERT_THAT(pread(fd.get(), read_buf, sizeof(read_buf), 0),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(read_buf))));
  EXPECT_STREQ(read_buf, kWriteData);

  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  ASSERT_THAT(ftruncate(fd.get(), static_cast<off_t>(page_size)), SyscallSucceeds());

  auto mapping = test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_READ | PROT_WRITE,
                                               MAP_SHARED, fd.get(), 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());
  static_cast<char*>(mapping->mapping())[0] = 'H';

  ASSERT_THAT(pread(fd.get(), read_buf, sizeof(read_buf), 0),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(read_buf))));
  EXPECT_STREQ(read_buf, "Hello noswap");

  mapping->Unmap();
  fd.reset();
  EXPECT_THAT(unlink(file_path.c_str()), SyscallSucceeds());

  fbl::unique_fd tmpfile_fd(open(temp_dir.path().c_str(), O_TMPFILE | O_RDWR, 0600));
  ASSERT_TRUE(tmpfile_fd.is_valid()) << strerror(errno);
  ASSERT_THAT(write(tmpfile_fd.get(), kWriteData, sizeof(kWriteData)),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(kWriteData))));
  memset(read_buf, 0, sizeof(read_buf));
  ASSERT_THAT(pread(tmpfile_fd.get(), read_buf, sizeof(read_buf), 0),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(read_buf))));
  EXPECT_STREQ(read_buf, kWriteData);
}

TEST(TmpfsTest, NoswapRejectsValueArgument) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "need CAP_SYS_ADMIN to mount tmpfs with noswap";
  }
  if (!test_helper::IsStarnix() && !test_helper::IsKernelVersionAtLeast(6, 4)) {
    GTEST_SKIP() << "tmpfs noswap mount option requires Linux 6.4+";
  }

  test_helper::ScopedTempDir temp_dir;

  EXPECT_THAT(mount("none", temp_dir.path().c_str(), "tmpfs", 0, "noswap=1"),
              SyscallFailsWithErrno(EINVAL));
  EXPECT_THAT(mount("none", temp_dir.path().c_str(), "tmpfs", 0, "noswap=0"),
              SyscallFailsWithErrno(EINVAL));
}

TEST(TmpfsTest, NoswapDoesNotRequireCapIpcLock) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "need CAP_SYS_ADMIN to mount tmpfs with noswap";
  }
  if (!test_helper::IsStarnix() && !test_helper::IsKernelVersionAtLeast(6, 4)) {
    GTEST_SKIP() << "tmpfs noswap mount option requires Linux 6.4+";
  }

  test_helper::ScopedTempDir temp_dir;

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    test_helper::UnsetCapabilityEffective(CAP_IPC_LOCK);
    ASSERT_TRUE(test_helper::HasCapabilityEffective(CAP_SYS_ADMIN));
    ASSERT_FALSE(test_helper::HasCapabilityEffective(CAP_IPC_LOCK));

    auto mount_result =
        test_helper::ScopedMount::Mount("none", temp_dir.path(), "tmpfs", 0, "noswap");
    ASSERT_THAT(mount_result, SyscallResultIsOk());

    std::string file_path = temp_dir.path() + "/pinned_file";
    fbl::unique_fd fd(open(file_path.c_str(), O_CREAT | O_RDWR, 0600));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);

    constexpr char kWriteData[] = "hello noswap";
    ASSERT_THAT(write(fd.get(), kWriteData, sizeof(kWriteData)),
                SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(kWriteData))));
    fd.reset();
    EXPECT_THAT(unlink(file_path.c_str()), SyscallSucceeds());
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

}  // namespace
