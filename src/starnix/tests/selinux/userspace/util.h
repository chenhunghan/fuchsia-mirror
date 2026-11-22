// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STARNIX_TESTS_SELINUX_USERSPACE_UTIL_H_
#define SRC_STARNIX_TESTS_SELINUX_USERSPACE_UTIL_H_

#include <lib/fit/function.h>
#include <lib/fit/result.h>
#include <sched.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>

#include <string>

#include <gmock/gmock.h>
#include <linux/capability.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace test_helper {
class ForkHelper;
}  // namespace test_helper

#include <array>
#include <utility>

/// Returns the name of the policy file under `data/policies/` to load for the test suite.
extern std::string GetTestPolicyName();

/// Optional hook invoked once prior to loading the initial policy.
///
/// Only test suites that verify pre-policy-load kernel state and have few enough tests to run in a
/// single runner invocation (<= 50 tests) may define this hook.
[[gnu::weak]] extern void DoPrePolicyLoadWork();

/// Loads the binary policy file `name` from `data/policies/` into `/sys/fs/selinux/load`.
void LoadPolicy(const std::string& name);

/// Returns true if the kernel supports the specified policy capability.
bool IsPolicyCapSupported(const char* capability);

/// Returns true if the capability is enabled in the loaded policy, via `policycap`.
bool IsPolicyCapEnabled(const char* capability);

/// Writes `data` to the file at `path`, returning the `errno` if any part of that process fails.
fit::result<int> WriteExistingFile(const std::string& path, std::string_view data);

/// Ensures that a device node exists at `path` with the given `mode` (including file type bits such
/// as `S_IFCHR` or `S_IFBLK` and permissions) and device number `dev`, creating any missing parent
/// directories.
fit::result<int> EnsureDeviceNode(std::string_view path, mode_t mode, dev_t dev);

/// Reads the contents of the file at `path`.
fit::result<int, std::string> ReadFile(const std::string& path);

/// Reads the specified security attribute (e.g. "current", "exec", etc) for the current task.
fit::result<int, std::string> ReadTaskAttr(std::string_view attr_name);

/// Writes the specified security attribute (e.g. "current", "exec", etc) for the current task.
fit::result<int> WriteTaskAttr(std::string_view attr_name, std::string_view context);

/// Verifies that the `in`put string is empty, or ends with a trailing NUL, which is removed.
/// If no trailing NUL is found then `EOVERFLOW` is returned, and a gTest failure added.
fit::result<int, std::string> RemoveTrailingNul(std::string in);

/// Reads the security label of the specified `fd`, returning the `errno` on failure.
/// The trailing NUL, if any, will be stripped before the label is returned.
fit::result<int, std::string> GetLabel(int fd);

/// Reads the security label of the symbolic link `fd` rather than that of the file it points to,
/// returning the `errno` on failure.
/// The trailing NUL, if any, will be stripped before the label is returned.
fit::result<int, std::string> GetLinkLabel(int fd);

/// Reads the security label of the specified `path`, returning the `errno` on failure.
/// The trailing NUL, if any, will be stripped before the label is returned.
fit::result<int, std::string> GetLabel(const std::string& path);

/// Reads the peer security label of the specified socket `fd`, returning the `errno` on failure.
/// The trailing NUL, if any, will be stripped before the label is returned.
fit::result<int, std::string> GetPeerSec(int fd);

/// Sets the security `label` of the specified `path`.
fit::result<int> SetLabel(const std::string& path, std::string_view label);

/// Returns a full SELinux security context constructed from the specified `test_domain`.
std::string MakeTestSecurityContext(std::string_view test_domain);

/// Checks whether two file descriptors map to the same inode.
/// Returns an `errno` on failure.
fit::result<int, bool> IsSameInode(int fd_1, int fd_2);

/// Returns the `/proc/mounts` mount options string for `mount_path`, or an empty string if not
/// found.
std::string MountOptionsFor(std::string_view mount_path);

/// Runs in a child process the given `action` after transitioning to `label`.
/// The process belongs to `fork_helper`.
pid_t RunInForkedProcessWithLabel(test_helper::ForkHelper& fork_helper, std::string_view label,
                                  fit::function<void()> action);

/// Runs the given action in a forked process after transitioning to `label`. This requires some
/// rules to be set-up. For transitions from unconfined_t (the starting label for tests), giving
/// them the `test_a` attribute from `test_policy.conf` is sufficient.
template <typename T>
::testing::AssertionResult RunSubprocessAs(std::string_view label, T action) {
  test_helper::ForkHelper fork_helper;
  fork_helper.OnlyWaitForForkedChildren();
  pid_t pid = RunInForkedProcessWithLabel(fork_helper, label, std::move(action));
  if (pid < 0) {
    return ::testing::AssertionFailure() << "fork failed: " << strerror(errno);
  }
  if (auto wait_result = fork_helper.WaitForChildren(); !wait_result) {
    return ::testing::AssertionFailure()
           << "Subprocess with label \"" << label << "\" failed: " << wait_result.message();
  }
  return ::testing::AssertionSuccess();
}

/// Enables (or disables) enforcement while in scope, then restores enforcement to the previous
/// state.
class ScopedEnforcement {
 public:
  static ScopedEnforcement SetEnforcing();
  static ScopedEnforcement SetPermissive();
  ScopedEnforcement(const ScopedEnforcement&) = delete;
  ScopedEnforcement& operator=(const ScopedEnforcement&) = delete;
  ~ScopedEnforcement();

 private:
  explicit ScopedEnforcement(bool enforcing);
  std::string previous_state_;
};

/// Restores a task security attribute (e.g. `current`, `fscreate`, `sockcreate`) to its
/// previous value upon destruction.
class ScopedTaskAttrResetter {
 public:
  /// Sets the specified security attribute for the current task while in scope, and restores its
  /// previous value when deleted.
  static ScopedTaskAttrResetter SetTaskAttr(std::string_view attr_name, std::string_view new_value);
  ScopedTaskAttrResetter(const ScopedTaskAttrResetter&) = delete;
  ScopedTaskAttrResetter& operator=(const ScopedTaskAttrResetter&) = delete;
  ~ScopedTaskAttrResetter();

 private:
  explicit ScopedTaskAttrResetter(std::string_view attr_name, std::string_view old_value);
  std::string attr_name_;
  std::string old_value_;
};

namespace fit {

/// Kludge to tell gTest how to stringify `fit::result<>` values.
template <typename E, typename T>
void PrintTo(const fit::result<E, T>& result, std::ostream* os) {
  if (result.is_error()) {
    *os << "fit::failed( " << result.error_value() << " )";
  } else {
    *os << "fit::ok( ";
    ::testing::internal::UniversalPrinter<T>::Print(result.value(), os);
    *os << " )";
  }
}

}  // namespace fit

/// Returns a ScopedTempFD labeled with the given SELinux `label`.
test_helper::ScopedTempFD ScopedTempFDWithLabel(std::string_view label);

/// Base test class that isolates the mount namespace for the running test process.
class IsolatedMountNamespaceTest : public ::testing::Test {
 protected:
  static void SetUpTestSuite() {
    // The unshare() call will isolate the mount namespaces for the running
    // test process. This allows the Linux-based tests to execute syscalls with
    // root permissions, without fear of messing the environment up. While the
    // Starnix tests don't strictly need to unshare, it's beneficial to run the
    // same test binaries on Linux and on Starnix so we can be sure the semantics
    // match. As a side effect, this means that the mounted directories will not
    // be viewable in traditional ways, e.g. ffx component explore.
    ASSERT_THAT(unshare(CLONE_NEWNS), SyscallSucceeds());
  }
};

#endif  // SRC_STARNIX_TESTS_SELINUX_USERSPACE_UTIL_H_
