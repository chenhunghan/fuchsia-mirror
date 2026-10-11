// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

TEST(AppendTest, DontRequireGetattrForAppend) {
  fbl::unique_fd fd;
  {
    auto fscreate =
        ScopedTaskAttrResetter::SetTaskAttr("fscreate", "test_u:object_r:test_append_file_t:s0");
    fd = fbl::unique_fd(open("/tmp", O_APPEND | O_WRONLY | O_TMPFILE, 0644));
    ASSERT_TRUE(fd.is_valid()) << errno;
  }

  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_append_no_write_t:s0", [&] {
    char message[] = "abcdef";
    EXPECT_THAT(write(fd.get(), message, sizeof(message)),
                SyscallSucceedsWithValue(sizeof(message)));
  }));
}

TEST(AppendTest, DontRequireGetattrForSeek) {
  fbl::unique_fd fd;
  {
    auto fscreate =
        ScopedTaskAttrResetter::SetTaskAttr("fscreate", "test_u:object_r:test_append_file_t:s0");
    fd = fbl::unique_fd(open("/tmp", O_WRONLY | O_TMPFILE, 0644));
    ASSERT_TRUE(fd.is_valid()) << errno;
  }

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_append_no_append_t:s0", [&] {
    EXPECT_THAT(lseek(fd.get(), 0, SEEK_END), SyscallSucceeds());
  }));
}

// Verifies that a task with only 'append' rights to a file can still write to it
// after the O_APPEND flag has been cleared by a peer, provided the restricted task
// was the one that originally opened the file descriptor.
//
// This behavior is a consequence of SELinux bypassing repeat file permission
// checks on an established file descriptor when the calling task's security
// context, the file's security context, and the loaded policy have not changed
// since open().
TEST(AppendTest, WriteAllowedAfterOAppendClearedByPeer) {
  auto temp_file = ScopedTempFDWithLabel("test_u:object_r:test_append_file_t:s0");
  ASSERT_TRUE(temp_file.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_append_no_write_t:s0", [&] {
    // Open the file with O_APPEND as required by our restricted rights.
    fbl::unique_fd fd(open(temp_file.name().c_str(), O_APPEND | O_WRONLY));
    ASSERT_THAT(fd.get(), SyscallSucceeds());

    // Initial write with O_APPEND should succeed.
    char message1[] = "append1";
    EXPECT_THAT(write(fd.get(), message1, sizeof(message1)),
                SyscallSucceedsWithValue(sizeof(message1)));

    // Fork Task B to clear O_APPEND.
    test_helper::ForkHelper fork_helper;
    RunInForkedProcessWithLabel(fork_helper, "test_u:test_r:test_append_t:s0", [&] {
      int flags = fcntl(fd.get(), F_GETFL);
      ASSERT_THAT(flags, SyscallSucceeds());
      EXPECT_THAT(fcntl(fd.get(), F_SETFL, flags & ~O_APPEND), SyscallSucceeds());
    });

    EXPECT_TRUE(fork_helper.WaitForChildren());

    // The write still succeeds because the calling task is the opener of the FD,
    // and neither the task context, file label, nor policy has changed.
    char message2[] = "write2";
    EXPECT_THAT(write(fd.get(), message2, sizeof(message2)),
                SyscallSucceedsWithValue(sizeof(message2)));
  }));
}

// Verifies that a task with 'write' (but not 'append') rights that opens a
// file O_WRONLY can set O_APPEND via fcntl(F_SETFL) and continue writing,
// because neither fcntl(F_SETFL, O_APPEND) nor subsequent writes by the opener
// re-check 'append' permission once open() has succeeded.
TEST(AppendTest, WriteAllowedAfterOAppendSetByOpenerWithoutAppend) {
  auto temp_file = ScopedTempFDWithLabel("test_u:object_r:test_append_file_t:s0");
  ASSERT_TRUE(temp_file.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_append_no_append_t:s0", [&] {
    fbl::unique_fd fd(open(temp_file.name().c_str(), O_WRONLY));
    ASSERT_THAT(fd.get(), SyscallSucceeds());

    int flags = fcntl(fd.get(), F_GETFL);
    ASSERT_THAT(flags, SyscallSucceeds());
    EXPECT_THAT(fcntl(fd.get(), F_SETFL, flags | O_APPEND), SyscallSucceeds());

    char message[] = "append_via_write_opener";
    EXPECT_THAT(write(fd.get(), message, sizeof(message)),
                SyscallSucceedsWithValue(sizeof(message)));
  }));
}

// Verifies that when a file is opened in permissive mode without the required
// permission for its open flags, subsequent writes by the opening task after
// switching to enforcing mode still succeed because setenforce(1) does not
// invalidate the file's cached open state.
TEST(AppendTest, WriteAllowedAfterPermissiveOpenAndSetEnforce) {
  auto temp_file = ScopedTempFDWithLabel("test_u:object_r:test_append_file_t:s0");
  ASSERT_TRUE(temp_file.is_valid());

  auto permissive = ScopedEnforcement::SetPermissive();

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_append_no_write_t:s0", [&] {
    // Opening O_WRONLY (without O_APPEND) requires 'write', which this domain
    // lacks, but succeeds in permissive mode.
    fbl::unique_fd fd(open(temp_file.name().c_str(), O_WRONLY));
    ASSERT_THAT(fd.get(), SyscallSucceeds());

    auto enforce = ScopedEnforcement::SetEnforcing();

    char message[] = "write_after_setenforce";
    EXPECT_THAT(write(fd.get(), message, sizeof(message)),
                SyscallSucceedsWithValue(sizeof(message)));
  }));
}

}  // namespace

extern std::string DoPrePolicyLoadWork() { return "append_policy"; }
