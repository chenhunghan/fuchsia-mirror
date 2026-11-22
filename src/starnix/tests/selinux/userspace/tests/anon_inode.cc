// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <unistd.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/perf_event.h>
#include <linux/userfaultfd.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

namespace {

#ifndef SYS_memfd_secret
#define SYS_memfd_secret 447
#endif

TEST(AnonInodeTest, EventFdIsUnlabeled) {
  fbl::unique_fd fd(eventfd(0, 0));
  ASSERT_TRUE(fd.is_valid());

  EXPECT_THAT(GetLabel(fd.get()), SyscallResultIsErrno(ENOTSUP));
}

TEST(AnonInodeTest, PrivateFdIsUnchecked) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  // Create an eventfd within a test domain, then validate whether the FD is usable from a set
  // of test domains with differing levels of access.
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:anon_inode_test_t:s0", [] {
    fbl::unique_fd fd(eventfd(0, 0));
    ASSERT_TRUE(fd.is_valid());
    auto fd_label = GetLabel(fd.get());

    // Ensure that `fd` is of an un-labeled, aka "private", kind.
    ASSERT_THAT(fd_label, SyscallResultIsErrno(ENOTSUP));

    uint64_t event_buf = 1;

    ASSERT_TRUE(RunSubprocessAs("test_u:test_r:anon_inode_use_fd_and_perms_t:s0", [&] {
      EXPECT_THAT(write(fd.get(), &event_buf, sizeof(event_buf)), SyscallSucceeds())
          << "Domain granted FD-use and permissions should have access";
    }));
    ASSERT_TRUE(RunSubprocessAs("test_u:test_r:anon_inode_use_fd_no_perms_t:s0", [&] {
      EXPECT_THAT(write(fd.get(), &event_buf, sizeof(event_buf)), SyscallSucceeds())
          << "Domain granted FD-use but no file node permissions should have access";
    }));
    ASSERT_TRUE(RunSubprocessAs("test_u:test_r:anon_inode_no_use_fd_t:s0", [&] {
      EXPECT_THAT(write(fd.get(), &event_buf, sizeof(event_buf)), SyscallFailsWithErrno(EACCES))
          << "Domain not granted FD-use should not have access";
    }));
  }));
}

TEST(AnonInodeTest, TmpFileHasLabel) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:anon_inode_test_t:s0", [] {
    constexpr char kTmpPath[] = "/tmp";

    fbl::unique_fd fd(open(kTmpPath, O_RDWR | O_TMPFILE, 0o600));
    ASSERT_TRUE(fd.is_valid());

    EXPECT_THAT(GetLabel(fd.get()), SyscallResultIsOk());
  }));
}

TEST(AnonInodeTest, UserfaultFdHasLabel) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:anon_inode_test_t:s0", [] {
    fbl::unique_fd fd(static_cast<int>(syscall(SYS_userfaultfd, O_CLOEXEC)));
    ASSERT_TRUE(fd.is_valid());

    EXPECT_THAT(GetLabel(fd.get()),
                SyscallResultIsOk("test_u:object_r:anon_inode_test_userfaultfd_t:s0"));
  }));
}

TEST(AnonInodeTest, UserfaultFdRequiresCreate) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:anon_inode_test_no_create_t:s0", [] {
    EXPECT_THAT(syscall(SYS_userfaultfd, O_CLOEXEC), SyscallFailsWithErrno(EACCES));
  }));
}

TEST(AnonInodeTest, EpollIsUnlabeled) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  fbl::unique_fd fd(epoll_create1(0));
  ASSERT_TRUE(fd.is_valid());

  EXPECT_THAT(GetLabel(fd.get()), SyscallResultIsErrno(ENOTSUP));
}

TEST(AnonInodeTest, InotifyIsUnlabeled) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  fbl::unique_fd fd(inotify_init());
  ASSERT_TRUE(fd.is_valid());

  EXPECT_THAT(GetLabel(fd.get()), SyscallResultIsErrno(ENOTSUP));
}

TEST(AnonInodeTest, PidFdIsUnlabeled) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  fbl::unique_fd fd(static_cast<int>(syscall(SYS_pidfd_open, getpid(), 0)));
  ASSERT_TRUE(fd.is_valid());

  EXPECT_THAT(GetLabel(fd.get()), SyscallResultIsErrno(ENOTSUP));
}

TEST(AnonInodeTest, TimerFdIsUnlabeled) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  fbl::unique_fd fd(timerfd_create(CLOCK_MONOTONIC, 0));
  ASSERT_TRUE(fd.is_valid());

  EXPECT_THAT(GetLabel(fd.get()), SyscallResultIsErrno(ENOTSUP));
}

TEST(AnonInodeTest, SignalFdIsUnlabeled) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  sigset_t signals;
  sigemptyset(&signals);
  fbl::unique_fd fd(signalfd(-1, &signals, SFD_CLOEXEC));
  ASSERT_TRUE(fd.is_valid());

  EXPECT_THAT(GetLabel(fd.get()), SyscallResultIsErrno(ENOTSUP));
}

TEST(AnonInodeTest, PerfEventFdIsUnlabeled) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  perf_event_attr attr{};
  attr.type = PERF_TYPE_SOFTWARE;
  attr.size = sizeof(attr);
  attr.config = PERF_COUNT_SW_CPU_CLOCK;
  attr.sample_type = PERF_SAMPLE_IP;
  attr.disabled = true;
  attr.exclude_kernel = true;
  attr.exclude_hv = true;
  attr.exclude_idle = true;

  fbl::unique_fd fd(static_cast<int>(
      syscall(SYS_perf_event_open, &attr, /*pid=*/0, /*cpu=*/-1, /*group_fd=*/-1, /*flags=*/0)));
  ASSERT_TRUE(fd.is_valid());

  EXPECT_THAT(GetLabel(fd.get()), SyscallResultIsErrno(ENOTSUP));
}

// See https://fxbug.dev/398698182: Starnix doesn't support `memfd_secret(2)`. Must pass if/when
// implemented.
TEST(AnonInodeTest, MemfdSecretNotSupported) {
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "memfd_secret(2) is supported on Linux";
  }
  EXPECT_THAT(syscall(SYS_memfd_secret, /* flags = */ 0), SyscallFailsWithErrno(ENOSYS));
}

// See https://fxbug.dev/398707804: Starnix doesn't support `userfaultfd(2)` without
// `UFFD_FEATURE_SIGBUS` (i.e. event/read mode). Must pass if/when implemented.
TEST(AnonInodeTest, UserfaultFdEventModeNotSupported) {
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "userfaultfd(2) event mode is supported on Linux";
  }

  fbl::unique_fd uffd(static_cast<int>(syscall(SYS_userfaultfd, O_CLOEXEC)));
  ASSERT_TRUE(uffd.is_valid()) << strerror(errno);

  uffdio_api api = {.api = UFFD_API, .features = 0, .ioctls = 0};
  ASSERT_THAT(ioctl(uffd.get(), UFFDIO_API, &api), SyscallSucceeds());

  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  auto mapping = ASSERT_RESULT_SUCCESS_AND_RETURN(test_helper::ScopedMMap::MMap(
      nullptr, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));

  uffdio_register reg = {
      .range = {.start = reinterpret_cast<uintptr_t>(mapping.mapping()), .len = page_size},
      .mode = UFFDIO_REGISTER_MODE_MISSING,
      .ioctls = 0,
  };
  EXPECT_THAT(ioctl(uffd.get(), UFFDIO_REGISTER, &reg), SyscallFailsWithErrno(ENOTSUP));
}

}  // namespace

extern std::string GetTestPolicyName() { return "anon_inode_policy"; }
