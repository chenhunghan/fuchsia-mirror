// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <grp.h>
#include <lib/fit/defer.h>
#include <sys/fsuid.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <linux/io_uring.h>

#include "src/lib/files/file.h"
#include "src/starnix/tests/syscalls/cpp/io_uring_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

using io_uring_helper::io_uring_enter;
using io_uring_helper::io_uring_register;
using io_uring_helper::io_uring_setup;

// TODO(b/444216805): Re-enable once debian 12 fix is implemented.
TEST(IoUringTest, IoUringReadWrite) {
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "Test fails on debian 12 Linux, skipping.";
  }
  test_helper::ScopedTempFD temp_fd;
  ASSERT_TRUE(temp_fd.fd() >= 0);

  auto io_uring_res = io_uring_helper::IoUring::Create(2);
  ASSERT_THAT(io_uring_res, SyscallResultIsOk());
  auto ring = std::move(io_uring_res.value());
  const auto& params = ring->params();

  // Write to the file
  char write_data[] = "hello";
  struct iovec write_iov = {.iov_base = write_data, .iov_len = sizeof(write_data)};
  ring->SubmitSqe([&temp_fd, &write_iov](io_uring_helper::Sqe* sqe) {
    sqe->opcode = IORING_OP_WRITEV;
    sqe->fd = temp_fd.fd();
    sqe->addr = (uint64_t)&write_iov;
    sqe->len = 1;
    sqe->off = 0;
    sqe->user_data = 1;
  });

  // Read from the file
  char read_data[sizeof(write_data)];
  struct iovec read_iov = {.iov_base = read_data, .iov_len = sizeof(read_data)};
  ring->SubmitSqe([&temp_fd, &read_iov](io_uring_helper::Sqe* sqe) {
    sqe->opcode = IORING_OP_READV;
    sqe->fd = temp_fd.fd();
    sqe->addr = (uint64_t)&read_iov;
    sqe->len = 1;
    sqe->off = 0;
    sqe->user_data = 2;
  });

  // Submit and wait for both operations
  ASSERT_EQ(io_uring_enter(ring->fd(), 2, 2, IORING_ENTER_GETEVENTS, nullptr), 2);

  // Process completions
  uint32_t head = ring->cq_head_ptr()->load(std::memory_order_acquire);
  ASSERT_EQ(ring->cq_tail_ptr()->load(std::memory_order_acquire) - head, 2U);

  bool write_done = false;
  bool read_done = false;
  for (int i = 0; i < 2; i++) {
    io_uring_cqe* cqe = &ring->cqes()[head & (params.cq_entries - 1)];
    if (cqe->user_data == 1) {
      ASSERT_EQ(cqe->res, static_cast<ssize_t>(sizeof(write_data)));
      write_done = true;
    } else if (cqe->user_data == 2) {
      ASSERT_EQ(cqe->res, static_cast<ssize_t>(sizeof(read_data)));
      read_done = true;
    }
    head++;
  }
  ASSERT_TRUE(write_done);
  ASSERT_TRUE(read_done);
  ring->cq_head_ptr()->store(head, std::memory_order_release);

  // Verify
  ASSERT_STREQ(write_data, read_data);
}

TEST(IoUringTest, IoUringSetupCoopTaskrun) {
  if (!test_helper::IsStarnix() && !test_helper::IsKernelVersionAtLeast(5, 19)) {
    GTEST_SKIP() << "Skip test for unsupported feature";
  }
  struct io_uring_params params = {};
  params.flags = IORING_SETUP_COOP_TASKRUN;
  fbl::unique_fd fd(io_uring_setup(1, &params));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);
}

TEST(IoUringTest, IoUringSetupSingleIssuer) {
  if (!test_helper::IsStarnix() && !test_helper::IsKernelVersionAtLeast(6, 0)) {
    GTEST_SKIP() << "Skip test for unsupported feature";
  }
  struct io_uring_params params = {};
  params.flags = IORING_SETUP_SINGLE_ISSUER;
  fbl::unique_fd fd(io_uring_setup(1, &params));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);
}

TEST(IoUringTest, IoUringSetupDeferTaskrun) {
  if (!test_helper::IsStarnix() && !test_helper::IsKernelVersionAtLeast(6, 1)) {
    GTEST_SKIP() << "Skip test for unsupported feature";
  }
  struct io_uring_params params = {};
  params.flags = IORING_SETUP_DEFER_TASKRUN;
  fbl::unique_fd fd(io_uring_setup(1, &params));
  ASSERT_FALSE(fd.is_valid());
  ASSERT_EQ(EINVAL, errno);

  params.flags = IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_DEFER_TASKRUN;
  fd = fbl::unique_fd(io_uring_setup(1, &params));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);
}

TEST(IoUringTest, RegisterBuffersEmptyError) {
  struct io_uring_params params = {};
  fbl::unique_fd ring_fd(io_uring_setup(1, &params));
  ASSERT_TRUE(ring_fd.is_valid()) << "io_uring_setup failed: " << strerror(errno);

  int ret = io_uring_register(ring_fd.get(), IORING_REGISTER_BUFFERS, nullptr, 0);
  EXPECT_EQ(ret, -1);
  EXPECT_EQ(errno, EFAULT);

  struct iovec iov = {};
  ret = io_uring_register(ring_fd.get(), IORING_REGISTER_BUFFERS, &iov, 0);
  EXPECT_EQ(ret, -1);
  EXPECT_EQ(errno, EINVAL);
}

TEST(IoUringTest, RingMappingOmitsMayExec) {
  if (!test_helper::IsStarnix() && !test_helper::IsKernelVersionAtLeast(6, 16)) {
    // Prior to Linux 6.16, anonymous-inode mappings such as `io_uring` rings reported `me` in
    // `proc_pid_smaps(5)`. Starnix never maps ring memory executable and matches Linux 6.16+.
    GTEST_SKIP() << "io_uring ring mappings report `me` prior to Linux 6.16.";
  }
  struct io_uring_params params = {};
  fbl::unique_fd ring_fd(io_uring_setup(1, &params));
  if (!ring_fd.is_valid() && (errno == EPERM || errno == ENOSYS)) {
    GTEST_SKIP() << "io_uring_setup unavailable: " << strerror(errno);
  }
  ASSERT_TRUE(ring_fd.is_valid()) << "io_uring_setup failed: " << strerror(errno);

  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  auto ring_map = ASSERT_RESULT_SUCCESS_AND_RETURN(test_helper::ScopedMMap::MMap(
      nullptr, page_size, PROT_READ | PROT_WRITE, MAP_SHARED, ring_fd.get(), IORING_OFF_SQ_RING));

  std::string smaps;
  ASSERT_TRUE(files::ReadFileToString("/proc/self/smaps", &smaps));
  auto mapping =
      test_helper::find_memory_mapping_ext(reinterpret_cast<uintptr_t>(ring_map.mapping()), smaps);
  ASSERT_NE(mapping, std::nullopt);
  EXPECT_FALSE(mapping->ContainsFlag("me"));

  EXPECT_THAT(mprotect(ring_map.mapping(), page_size, PROT_READ | PROT_EXEC),
              SyscallFailsWithErrno(EACCES));
}

// With io_uring_disabled set to 1, only members of io_uring_group (or tasks with CAP_SYS_ADMIN)
// can create io_uring instances. Membership is determined by the filesystem GID and the
// supplementary groups, not by the effective GID.
TEST(IoUringTest, IoUringGroupChecksFsGid) {
  if (getuid() != 0 || !test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root, skipping.";
  }
  constexpr char kDisabledPath[] = "/proc/sys/kernel/io_uring_disabled";
  constexpr char kGroupPath[] = "/proc/sys/kernel/io_uring_group";
  std::string old_disabled;
  std::string old_group;
  if (!files::ReadFileToString(kDisabledPath, &old_disabled) ||
      !files::ReadFileToString(kGroupPath, &old_group)) {
    GTEST_SKIP() << "io_uring_disabled and io_uring_group are not supported, skipping.";
  }
  auto restore = fit::defer([&] {
    EXPECT_TRUE(files::WriteFile(kDisabledPath, old_disabled));
    EXPECT_TRUE(files::WriteFile(kGroupPath, old_group));
  });

  constexpr uid_t kUid = 65532;
  constexpr gid_t kIoUringGid = 65532;
  constexpr gid_t kOtherGid = 65531;
  if (!files::WriteFile(kGroupPath, std::to_string(kIoUringGid))) {
    GTEST_SKIP() << "Cannot write io_uring_group, skipping.";
  }
  ASSERT_TRUE(files::WriteFile(kDisabledPath, "1"));

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    ASSERT_THAT(setgroups(0, nullptr), SyscallSucceeds());
    ASSERT_THAT(setresgid(kIoUringGid, kOtherGid, kOtherGid), SyscallSucceeds());
    ASSERT_EQ(setfsgid(kIoUringGid), static_cast<int>(kOtherGid));
    ASSERT_THAT(setresuid(kUid, kUid, kUid), SyscallSucceeds());
    ASSERT_FALSE(test_helper::HasSysAdmin());

    struct io_uring_params params = {};
    fbl::unique_fd fd(io_uring_setup(1, &params));
    EXPECT_TRUE(fd.is_valid()) << strerror(errno);
  });
  ASSERT_TRUE(helper.WaitForChildren());

  helper.RunInForkedProcess([] {
    ASSERT_THAT(setgroups(0, nullptr), SyscallSucceeds());
    ASSERT_THAT(setresgid(kOtherGid, kIoUringGid, kIoUringGid), SyscallSucceeds());
    ASSERT_EQ(setfsgid(kOtherGid), static_cast<int>(kIoUringGid));
    ASSERT_THAT(setresuid(kUid, kUid, kUid), SyscallSucceeds());
    ASSERT_FALSE(test_helper::HasSysAdmin());

    struct io_uring_params params = {};
    EXPECT_THAT(io_uring_setup(1, &params), SyscallFailsWithErrno(EPERM));
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

}  // namespace
