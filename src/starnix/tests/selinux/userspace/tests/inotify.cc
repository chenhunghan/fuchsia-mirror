// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/inotify.h>
#include <sys/socket.h>
#include <unistd.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string DoPrePolicyLoadWork() { return "inotify_policy"; }

namespace {

constexpr char kFileLabel[] = "test_u:object_r:test_inotify_file_t:s0";

// Verify that setting a watch is denied if the "watch" permission is not granted.
TEST(InotifyTest, WatchDenied) {
  auto test_file = ScopedTempFDWithLabel(kFileLabel);
  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_inotify_no_watch_t:s0", [&]() {
    fbl::unique_fd fd(inotify_init1(IN_CLOEXEC));
    ASSERT_TRUE(fd.is_valid()) << "errno: " << errno;
    EXPECT_THAT(inotify_add_watch(fd.get(), test_file.name().c_str(), IN_MODIFY),
                SyscallFailsWithErrno(EACCES));
  }));
}

// Verify that setting a watch is allowed if the "watch" permission is granted.
TEST(InotifyTest, WatchAllowed) {
  auto test_file = ScopedTempFDWithLabel(kFileLabel);
  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_inotify_watch_t:s0", [&]() {
    fbl::unique_fd fd(inotify_init1(IN_CLOEXEC));
    ASSERT_TRUE(fd.is_valid()) << "errno: " << errno;
    EXPECT_THAT(inotify_add_watch(fd.get(), test_file.name().c_str(), IN_MODIFY),
                SyscallSucceeds());
  }));
}

// Verify that watching `IN_ACCESS` events is denied without the "watch_reads" permission.
TEST(InotifyTest, WatchAccessDenied) {
  auto test_file = ScopedTempFDWithLabel(kFileLabel);
  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_inotify_watch_t:s0", [&]() {
    fbl::unique_fd fd(inotify_init1(IN_CLOEXEC));
    ASSERT_TRUE(fd.is_valid()) << "errno: " << errno;
    EXPECT_THAT(inotify_add_watch(fd.get(), test_file.name().c_str(), IN_MODIFY | IN_ACCESS),
                SyscallFailsWithErrno(EACCES));
  }));
}

// Verify that watching `IN_CLOSE_NOWRITE` events is denied without the "watch_reads" permission.
TEST(InotifyTest, WatchCloseNoWriteDenied) {
  auto test_file = ScopedTempFDWithLabel(kFileLabel);
  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_inotify_watch_t:s0", [&]() {
    fbl::unique_fd fd(inotify_init1(IN_CLOEXEC));
    ASSERT_TRUE(fd.is_valid()) << "errno: " << errno;
    EXPECT_THAT(inotify_add_watch(fd.get(), test_file.name().c_str(), IN_CLOSE_NOWRITE),
                SyscallFailsWithErrno(EACCES));
  }));
}

// Verify that watching `IN_OPEN` events does not require the "watch_reads" permission, since
// opening a file is not a read-exclusive event.
TEST(InotifyTest, WatchOpenAllowedWithoutWatchReads) {
  auto test_file = ScopedTempFDWithLabel(kFileLabel);
  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_inotify_watch_t:s0", [&]() {
    fbl::unique_fd fd(inotify_init1(IN_CLOEXEC));
    ASSERT_TRUE(fd.is_valid()) << "errno: " << errno;
    EXPECT_THAT(inotify_add_watch(fd.get(), test_file.name().c_str(), IN_OPEN), SyscallSucceeds());
  }));
}

// Verify that the "watch" permission is required even if only read-like events are watched.
TEST(InotifyTest, WatchReadsOnlyDenied) {
  auto test_file = ScopedTempFDWithLabel(kFileLabel);
  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_inotify_watch_reads_only_t:s0", [&]() {
    fbl::unique_fd fd(inotify_init1(IN_CLOEXEC));
    ASSERT_TRUE(fd.is_valid()) << "errno: " << errno;
    EXPECT_THAT(inotify_add_watch(fd.get(), test_file.name().c_str(), IN_ACCESS),
                SyscallFailsWithErrno(EACCES));
  }));
}

// Verify that setting a watch on a socket node, reached via "/proc/self/fd/", is denied. The
// "watch" permissions are not defined for socket-like classes, so they cannot be granted.
TEST(InotifyTest, WatchSocketDenied) {
  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_inotify_no_watch_t:s0", [&]() {
    fbl::unique_fd sock(socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0));
    ASSERT_TRUE(sock.is_valid()) << "errno: " << errno;
    std::string sock_path = "/proc/self/fd/" + std::to_string(sock.get());

    fbl::unique_fd fd(inotify_init1(IN_CLOEXEC));
    ASSERT_TRUE(fd.is_valid()) << "errno: " << errno;
    EXPECT_THAT(inotify_add_watch(fd.get(), sock_path.c_str(), IN_MODIFY),
                SyscallFailsWithErrno(EACCES));
  }));
}

// Verify that watching read-like events is allowed with the "watch_reads" permission.
TEST(InotifyTest, WatchReadsAllowed) {
  auto test_file = ScopedTempFDWithLabel(kFileLabel);
  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_inotify_watch_reads_t:s0", [&]() {
    fbl::unique_fd fd(inotify_init1(IN_CLOEXEC));
    ASSERT_TRUE(fd.is_valid()) << "errno: " << errno;
    EXPECT_THAT(inotify_add_watch(fd.get(), test_file.name().c_str(),
                                  IN_MODIFY | IN_ACCESS | IN_CLOSE_NOWRITE),
                SyscallSucceeds());
  }));
}

}  // namespace
