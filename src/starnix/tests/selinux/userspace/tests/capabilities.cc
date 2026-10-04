// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <grp.h>
#include <sys/stat.h>
#include <sys/syscall.h>

#include <gtest/gtest.h>
#include <linux/capability.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

extern std::string DoPrePolicyLoadWork() { return "capabilities_policy"; }

namespace {

/// Returns a header and data struct of the type required by `capget` and `capset`
/// populated with the Linux capability version preferred by Starnix.
/// The caps are properly zeroed out.
std::pair<__user_cap_header_struct, std::array<__user_cap_data_struct, _LINUX_CAPABILITY_U32S_3>>
NewCapStructs() {
  __user_cap_header_struct header;
  memset(&header, 0, sizeof(header));
  header.version = _LINUX_CAPABILITY_VERSION_3;

  std::array<__user_cap_data_struct, _LINUX_CAPABILITY_U32S_3> caps;
  memset(caps.data(), 0, sizeof(caps));

  return {header, caps};
}

/// When the `getcap` process class permission is granted, the `capget` syscall succeeds
/// when the header is valid and the user data argument is non-null.
TEST(CapabilitiesTest, GetCapAllowed) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_allow_getcap_self_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    auto [header, caps] = NewCapStructs();
    EXPECT_THAT(syscall(SYS_capget, &header, caps.data()), SyscallSucceeds());
  }));
}

/// When the `getcap` process class permission is denied, the `capget` syscall fails
/// with `EACCES` when the header is valid and the user data argument is non-null.
TEST(CapabilitiesTest, GetCapDenied) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_deny_getcap_self_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    auto [header, caps] = NewCapStructs();
    EXPECT_THAT(syscall(SYS_capget, &header, caps.data()), SyscallFailsWithErrno(EACCES));
  }));
}

/// When the `getcap` process class permission is denied, the `capget` syscall succeeds
/// when the header is valid and the user data argument is null. The syscall returns
/// without checking the `getcap` permission.
TEST(CapabilitiesTest, GetCapDeniedNullData) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_deny_getcap_self_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    auto [header, _] = NewCapStructs();
    EXPECT_THAT(syscall(SYS_capget, &header, NULL), SyscallSucceeds());
  }));
}

/// When the `getcap` process class permission is denied, the `capget` syscall fails
/// with `EINVAL` when the provided header struct contains an invalid capability version
/// and the user data argument is non-null. The syscall returns without checking the
/// `getcap` permission.
TEST(CapabilitiesTest, GetCapDeniedInvalidVersion) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_deny_getcap_self_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    auto [header, caps] = NewCapStructs();
    header.version = 0;
    EXPECT_THAT(syscall(SYS_capget, &header, caps.data()), SyscallFailsWithErrno(EINVAL));
  }));
}

/// When the `getcap` process class permission is denied, the `capget` syscall fails
/// with `EINVAL` when the provided header struct contains an invalid PID and the
/// arguments are otherwise valid. The syscall returns without checking the `getcap`
/// permission.
TEST(CapabilitiesTest, GetCapDeniedInvalidPid) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_deny_getcap_self_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    auto [header, caps] = NewCapStructs();
    header.pid = -1;
    EXPECT_THAT(syscall(SYS_capget, &header, caps.data()), SyscallFailsWithErrno(EINVAL));
  }));
}

/// When the `setcap` process class permission is granted, the `capset` syscall succeeds.
TEST(CapabilitiesTest, SetCapAllowed) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_allow_setcap_self_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    auto [header, caps] = NewCapStructs();
    // Attempt to drop all capabilities.
    caps.fill({0, 0, 0});
    EXPECT_THAT(syscall(SYS_capset, &header, caps.data()), SyscallSucceeds());
  }));
}

/// When the `setcap` process class permission is denied, the `capset` syscall fails
/// with `EACCES` when the provided arguments are valid.
TEST(CapabilitiesTest, SetCapDenied) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_deny_setcap_self_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    auto [header, caps] = NewCapStructs();
    // Attempt to drop all capabilities.
    EXPECT_THAT(syscall(SYS_capset, &header, caps.data()), SyscallFailsWithErrno(EACCES));
  }));
}

/// When the `setcap` process class permission is denied, the `capset` syscall fails
/// with `EFAULT` when the user data argument is null and the header is valid. The
/// syscall returns without checking the `setcap` permission.
TEST(CapabilitiesTest, SetCapDeniedNullData) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_deny_setcap_self_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    auto [header, _] = NewCapStructs();
    EXPECT_THAT(syscall(SYS_capset, &header, NULL), SyscallFailsWithErrno(EFAULT));
  }));
}

/// When the `setcap` process class permission is denied, the `capset` syscall fails
/// with `EINVAL` when the provided header struct contains an invalid capability version.
/// The syscall returns without checking the `setcap` permission.
TEST(CapabilitiesTest, SetCapDeniedInvalidVersion) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_deny_setcap_self_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    auto [header, caps] = NewCapStructs();
    header.version = 0;
    // Attempt to drop all capabilities.
    EXPECT_THAT(syscall(SYS_capset, &header, caps.data()), SyscallFailsWithErrno(EINVAL));
  }));
}

/// When the `setcap` process class permission is denied, the `capset` syscall fails
/// with EPERM when the target PID is different from the caller's PID. The syscall
/// returns without checking the `setcap` permission.
TEST(CapabilitiesTest, SetCapDeniedDifferentPid) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_deny_setcap_self_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  pid_t test_pid = getpid();

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    // Attempt to drop all capabilities for the parent process.
    auto [header, caps] = NewCapStructs();
    header.pid = test_pid;
    EXPECT_THAT(syscall(SYS_capset, &header, caps.data()), SyscallFailsWithErrno(EPERM));
  }));
}

/// When the `setcap` process class permission is denied, the `capset` syscall fails
/// with `EPERM` when the header is valid and the request attempts to add a capability
/// to the target's permitted set. The syscall returns without checking the `setcap`
/// permission.
TEST(CapabilitiesTest, SetCapDeniedExpandPermittedSet) {
  constexpr char kTestSecurityContext[] = "test_u:test_r:test_deny_setcap_self_t:s0";

  ASSERT_TRUE(RunSubprocessAs(kTestSecurityContext, [&] {
    // Prepare for the test by dropping the `CAP_SYS_ADMIN` capability from the
    // effective and permitted sets while running SELinux in permissive mode.
    auto [header, caps] = NewCapStructs();
    ASSERT_THAT(syscall(SYS_capget, &header, caps.data()), SyscallSucceeds());
    caps[CAP_TO_INDEX(CAP_SYS_ADMIN)].effective &= ~CAP_TO_MASK(CAP_SYS_ADMIN);
    caps[CAP_TO_INDEX(CAP_SYS_ADMIN)].permitted &= ~CAP_TO_MASK(CAP_SYS_ADMIN);
    ASSERT_THAT(syscall(SYS_capset, &header, caps.data()), SyscallSucceeds());

    // Start enforcing SELinux permission checks.
    auto enforce = ScopedEnforcement::SetEnforcing();

    // Attempt to add the `CAP_SYS_ADMIN` capability back to the permitted set.
    caps[CAP_TO_INDEX(CAP_SYS_ADMIN)].effective |= CAP_TO_MASK(CAP_SYS_ADMIN);
    EXPECT_THAT(syscall(SYS_capset, &header, caps.data()), SyscallFailsWithErrno(EPERM));
  }));
}

constexpr char kFsetidFileLabel[] = "test_u:object_r:test_fsetid_file_t:s0";
constexpr char kFownerWithFsetidContext[] = "test_u:test_r:test_fowner_with_fsetid_t:s0";
constexpr char kFownerWithoutFsetidContext[] = "test_u:test_r:test_fowner_without_fsetid_t:s0";

TEST(CapabilitiesTest, ChmodSgidSameOwnerDifferentGroup) {
  auto test_file = ScopedTempFDWithLabel(kFsetidFileLabel);
  ASSERT_THAT(fchown(test_file.fd(), 0, 1), SyscallSucceeds());

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kFownerWithoutFsetidContext, [&] {
    ASSERT_THAT(fchmod(test_file.fd(), S_ISGID | S_IRWXU), SyscallSucceeds());
    struct stat st;
    ASSERT_THAT(fstat(test_file.fd(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & S_ISGID, 0u);
  }));

  ASSERT_TRUE(RunSubprocessAs(kFownerWithFsetidContext, [&] {
    ASSERT_THAT(fchmod(test_file.fd(), S_ISGID | S_IRWXU), SyscallSucceeds());
    struct stat st;
    ASSERT_THAT(fstat(test_file.fd(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & S_ISGID, static_cast<mode_t>(S_ISGID));
  }));
}

TEST(CapabilitiesTest, ChmodSgidDifferentOwnerAndGroup) {
  auto test_file = ScopedTempFDWithLabel(kFsetidFileLabel);
  ASSERT_THAT(fchown(test_file.fd(), 1, 1), SyscallSucceeds());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // Caller (uid 0, gid 0) has CAP_FOWNER to chmod a file owned by uid 1,
  // but lacks CAP_FSETID while file gid is 1. chmod succeeds and clears S_ISGID.
  ASSERT_TRUE(RunSubprocessAs(kFownerWithoutFsetidContext, [&] {
    ASSERT_THAT(fchmod(test_file.fd(), S_ISGID | S_IRWXU), SyscallSucceeds());
    struct stat st;
    ASSERT_THAT(fstat(test_file.fd(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & S_ISGID, 0u);
  }));

  // With both CAP_FOWNER and CAP_FSETID, S_ISGID is preserved.
  ASSERT_TRUE(RunSubprocessAs(kFownerWithFsetidContext, [&] {
    ASSERT_THAT(fchmod(test_file.fd(), S_ISGID | S_IRWXU), SyscallSucceeds());
    struct stat st;
    ASSERT_THAT(fstat(test_file.fd(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & S_ISGID, static_cast<mode_t>(S_ISGID));
  }));
}

TEST(CapabilitiesTest, ChmodSgidChecksFsgid) {
  auto test_file = ScopedTempFDWithLabel(kFsetidFileLabel);
  ASSERT_THAT(fchown(test_file.fd(), 0, 1), SyscallSucceeds());

  auto enforce = ScopedEnforcement::SetEnforcing();

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    // Set fsgid to 1 while egid remains 0 before transitioning to the restricted domain.
    ASSERT_EQ(syscall(SYS_setfsgid, 1), 0);
    ASSERT_EQ(getegid(), 0u);
    ASSERT_EQ(WriteTaskAttr("current", kFownerWithoutFsetidContext), fit::ok());

    ASSERT_THAT(fchmod(test_file.fd(), S_ISGID | S_IRWXU), SyscallSucceeds());
    struct stat st;
    ASSERT_THAT(fstat(test_file.fd(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & S_ISGID, static_cast<mode_t>(S_ISGID));
  });
  EXPECT_TRUE(helper.WaitForChildren());

  // The egid is not considered: with egid matching the file gid, but fsgid not matching it and no
  // supplementary groups, S_ISGID is stripped without CAP_FSETID.
  auto egid_test_file = ScopedTempFDWithLabel(kFsetidFileLabel);
  ASSERT_THAT(fchown(egid_test_file.fd(), 0, 0), SyscallSucceeds());

  helper.RunInForkedProcess([&] {
    ASSERT_THAT(setgroups(0, nullptr), SyscallSucceeds());
    ASSERT_EQ(syscall(SYS_setfsgid, 1), 0);
    ASSERT_EQ(getegid(), 0u);
    ASSERT_EQ(WriteTaskAttr("current", kFownerWithoutFsetidContext), fit::ok());

    // The fchmod call succeeds, but S_ISGID is stripped. The `fsetid` denial is also confirmed by
    // the audit log expectations.
    ASSERT_THAT(fchmod(egid_test_file.fd(), S_ISGID | S_IRWXU), SyscallSucceeds());
    struct stat st;
    ASSERT_THAT(fstat(egid_test_file.fd(), &st), SyscallSucceeds());
    EXPECT_EQ(st.st_mode & S_ISGID, 0u);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(CapabilitiesTest, CreateFileInSgidDirRequiresFsetid) {
  test_helper::ScopedTempDir temp_dir;
  ASSERT_FALSE(temp_dir.path().empty());
  ASSERT_THAT(chown(temp_dir.path().c_str(), 0, 1), SyscallSucceeds());
  ASSERT_THAT(chmod(temp_dir.path().c_str(), S_ISGID | S_IRWXU | S_IRWXG | S_IRWXO),
              SyscallSucceeds());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // Creating a file with S_ISGID but without S_IXGRP (group execute) does not require CAP_FSETID.
  std::string file_without_ixgrp = temp_dir.path() + "/without_ixgrp";
  ASSERT_TRUE(RunSubprocessAs(kFownerWithoutFsetidContext, [&] {
    umask(0);
    int fd = open(file_without_ixgrp.c_str(), O_CREAT | O_RDWR, S_ISGID | S_IRWXU);
    ASSERT_THAT(fd, SyscallSucceeds());
    struct stat st;
    ASSERT_THAT(fstat(fd, &st), SyscallSucceeds());
    EXPECT_EQ(st.st_gid, 1u);
    EXPECT_EQ(st.st_mode & S_ISGID, static_cast<mode_t>(S_ISGID));
    close(fd);
  }));

  std::string file_without_fsetid = temp_dir.path() + "/without_fsetid";
  ASSERT_TRUE(RunSubprocessAs(kFownerWithoutFsetidContext, [&] {
    umask(0);
    int fd = open(file_without_fsetid.c_str(), O_CREAT | O_RDWR, S_ISGID | S_IRWXU | S_IXGRP);
    ASSERT_THAT(fd, SyscallSucceeds());
    struct stat st;
    ASSERT_THAT(fstat(fd, &st), SyscallSucceeds());
    EXPECT_EQ(st.st_gid, 1u);
    EXPECT_EQ(st.st_mode & S_ISGID, 0u);
    close(fd);
  }));

  std::string file_with_fsetid = temp_dir.path() + "/with_fsetid";
  ASSERT_TRUE(RunSubprocessAs(kFownerWithFsetidContext, [&] {
    umask(0);
    int fd = open(file_with_fsetid.c_str(), O_CREAT | O_RDWR, S_ISGID | S_IRWXU | S_IXGRP);
    ASSERT_THAT(fd, SyscallSucceeds());
    struct stat st;
    ASSERT_THAT(fstat(fd, &st), SyscallSucceeds());
    EXPECT_EQ(st.st_gid, 1u);
    EXPECT_EQ(st.st_mode & S_ISGID, static_cast<mode_t>(S_ISGID));
    close(fd);
  }));
}

}  // namespace
