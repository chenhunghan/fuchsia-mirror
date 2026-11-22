// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <lib/fit/defer.h>
#include <lib/fit/result.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <condition_variable>
#include <mutex>
#include <string>
#include <string_view>
#include <thread>
#include <utility>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/files/file.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

extern std::string GetTestPolicyName() { return "bounds_policy"; }

namespace {

std::string MakeFileContext(std::string_view type, std::string_view level = "s0") {
  return fxl::StringPrintf("test_u:object_r:%s:%s", std::string(type).c_str(),
                           std::string(level).c_str());
}

fit::result<int, std::string> PrepareExecBinary(const test_helper::ScopedTempDir& dir,
                                                std::string_view file_type) {
  if (dir.path().empty()) {
    return fit::error(EINVAL);
  }
  const std::string src_path = "data/bin/is_current_domain_bin";
  std::string content;
  if (!files::ReadFileToString(src_path, &content)) {
    return fit::error(errno ? errno : EIO);
  }
  auto fscreate = ScopedTaskAttrResetter::SetTaskAttr("fscreate", MakeFileContext(file_type));
  std::string dst_path = dir.path() + "/is_current_domain_bin";
  if (!files::WriteFile(dst_path, content)) {
    return fit::error(errno ? errno : EIO);
  }
  if (chmod(dst_path.c_str(), 0755) != 0) {
    return fit::error(errno);
  }
  return fit::ok(std::move(dst_path));
}

void ExecExpectingDomain(const std::string& bin_path, const std::string& target_ctx) {
  ASSERT_THAT(WriteTaskAttr("exec", target_ctx), SyscallResultIsOk());
  char* const argv[] = {const_cast<char*>(bin_path.c_str()), const_cast<char*>(target_ctx.c_str()),
                        nullptr};
  char* const envp[] = {nullptr};
  SAFE_SYSCALL(execve(bin_path.c_str(), argv, envp));
}

template <typename Fn>
void RunWhileMultithreaded(Fn&& fn) {
  std::mutex mu;
  std::condition_variable cv;
  bool thread_ready = false;
  bool stop_thread = false;
  std::thread worker([&] {
    std::unique_lock lock(mu);
    thread_ready = true;
    cv.notify_one();
    cv.wait(lock, [&] { return stop_thread; });
  });
  auto join_worker = fit::defer([&] {
    {
      std::lock_guard lock(mu);
      stop_thread = true;
    }
    cv.notify_one();
    worker.join();
  });
  {
    std::unique_lock lock(mu);
    cv.wait(lock, [&] { return thread_ready; });
  }
  fn();
}

// Paired `(child_s, child_t)` is bounded against `(parent_s, parent_t)`, not `(parent_s, child_t)`.
TEST(BoundsTest, PairedSourceAndTargetBounds) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto child_file = ScopedTempFDWithLabel(MakeFileContext("test_bounds_child_file_t"));
  ASSERT_TRUE(child_file.is_valid());

  // Parent domain has no direct rule on `child_file_t`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_parent_s"), [&] {
    EXPECT_THAT(open(child_file.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
  }));

  // Child domain is allowed on `child_file_t` because `(parent_s, parent_file_t)` allows it.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_child_s"), [&] {
    fbl::unique_fd fd(open(child_file.name().c_str(), O_RDONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

// Standalone `(unbounded_s, child_t)` is not bounded against `(unbounded_s, parent_t)`.
TEST(BoundsTest, StandaloneTargetBoundsNotEnforced) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto parent_file = ScopedTempFDWithLabel(MakeFileContext("test_bounds_parent_file_t"));
  ASSERT_TRUE(parent_file.is_valid());
  auto child_file = ScopedTempFDWithLabel(MakeFileContext("test_bounds_child_file_t"));
  ASSERT_TRUE(child_file.is_valid());

  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_unbounded_s"), [&] {
    fbl::unique_fd child_fd(open(child_file.name().c_str(), O_RDONLY));
    EXPECT_THAT(child_fd.get(), SyscallSucceeds());
    EXPECT_THAT(open(parent_file.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
  }));
}

// Unequal `typebounds` chain depths pair parent types step-by-step and stop once source is
// unbounded.
TEST(BoundsTest, UnequalDepthPairedBounds) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto shallow_child_file =
      ScopedTempFDWithLabel(MakeFileContext("test_bounds_shallow_child_file_t"));
  ASSERT_TRUE(shallow_child_file.is_valid());
  auto deep_child_file = ScopedTempFDWithLabel(MakeFileContext("test_bounds_deep_child_file_t"));
  ASSERT_TRUE(deep_child_file.is_valid());

  // Deeper source (`gp_s -> mid_s -> deep_child_s`), shallower target
  // (`mid_file_t -> shallow_child_file_t`).
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_deep_child_s"), [&] {
    fbl::unique_fd fd(open(shallow_child_file.name().c_str(), O_RDONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));

  // Shallower source (`shallow_parent_s -> shallow_child_s`), deeper target
  // (`gp_file_t -> mid2_file_t -> deep_child_file_t`).
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_shallow_child_s"), [&] {
    fbl::unique_fd fd(open(deep_child_file.name().c_str(), O_RDONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

// Transitive `typebounds` permit multi-threaded `dyntransition` across multiple hierarchy levels,
// while upward or unrelated transitions remain denied.
TEST(BoundsTest, MultiLevelMultithreadedDyntransition) {
  auto enforce = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_chain_0_t"), [&] {
    RunWhileMultithreaded([&] {
      // Multi-hop downward transition succeeds (`chain_0 -> chain_1 -> chain_2 -> chain_3`).
      ASSERT_THAT(WriteTaskAttr("current", MakeTestSecurityContext("test_bounds_chain_3_t")),
                  SyscallResultIsOk());
      // Upward transition fails (`chain_3` does not bound `chain_0`).
      EXPECT_THAT(WriteTaskAttr("current", MakeTestSecurityContext("test_bounds_chain_0_t")),
                  SyscallResultIsErrno(EPERM));
      // Unrelated transition fails.
      EXPECT_THAT(WriteTaskAttr("current", MakeTestSecurityContext("test_bounds_unbounded_s")),
                  SyscallResultIsErrno(EPERM));
    });
  }));
}

// Transitive `typebounds` permit `execve` domain transitions under `PR_SET_NO_NEW_PRIVS`, while
// unbounded transitions are rejected.
TEST(BoundsTest, MultiLevelNoNewPrivsExecveTransition) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  test_helper::ScopedTempDir temp_dir;
  auto bin_path = PrepareExecBinary(temp_dir, "test_bounds_exec_file_t");
  ASSERT_THAT(bin_path, SyscallResultIsOk());

  // Multi-hop bounded transition (`chain_0 -> chain_3`) succeeds under `PR_SET_NO_NEW_PRIVS`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_chain_0_t"), [&] {
    ASSERT_THAT(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), SyscallSucceeds());
    ExecExpectingDomain(bin_path.value(), MakeTestSecurityContext("test_bounds_chain_3_t"));
  }));

  // Unbounded transition succeeds when NNP is not set.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_chain_0_t"), [&] {
    ExecExpectingDomain(bin_path.value(), MakeTestSecurityContext("test_bounds_unbounded_s"));
  }));

  // Unbounded transition fails with `EPERM` under `PR_SET_NO_NEW_PRIVS`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_chain_0_t"), [&] {
    ASSERT_THAT(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), SyscallSucceeds());
    ASSERT_THAT(WriteTaskAttr("exec", MakeTestSecurityContext("test_bounds_unbounded_s")),
                SyscallResultIsOk());
    char* const argv[] = {const_cast<char*>(bin_path.value().c_str()),
                          const_cast<char*>("unexpected_exec"), nullptr};
    char* const envp[] = {nullptr};
    EXPECT_THAT(execve(bin_path.value().c_str(), argv, envp), SyscallFailsWithErrno(EPERM));
  }));
}

// Transitive `typebounds` permit `execve` domain transitions on `MS_NOSUID` mounts, while
// unbounded transitions fail with `EACCES` when `nnp_nosuid_transition` is disabled.
TEST(BoundsTest, MultiLevelNosuidMountExecveTransition) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  auto enforce = ScopedEnforcement::SetEnforcing();
  test_helper::ScopedTempDir mount_dir;
  ASSERT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", MS_NOSUID, nullptr),
              SyscallSucceeds());
  auto unmount =
      fit::defer([&] { EXPECT_THAT(umount(mount_dir.path().c_str()), SyscallSucceeds()); });

  auto bin_path = PrepareExecBinary(mount_dir, "test_bounds_exec_file_t");
  ASSERT_THAT(bin_path, SyscallResultIsOk());

  // Multi-hop bounded transition (`chain_0 -> chain_3`) succeeds from an `MS_NOSUID` mount.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_chain_0_t"), [&] {
    ExecExpectingDomain(bin_path.value(), MakeTestSecurityContext("test_bounds_chain_3_t"));
  }));

  // Unbounded transition from an `MS_NOSUID` mount fails with `EACCES`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_chain_0_t"), [&] {
    ASSERT_THAT(WriteTaskAttr("exec", MakeTestSecurityContext("test_bounds_unbounded_s")),
                SyscallResultIsOk());
    char* const argv[] = {const_cast<char*>(bin_path.value().c_str()),
                          const_cast<char*>("unexpected_exec"), nullptr};
    char* const envp[] = {nullptr};
    EXPECT_THAT(execve(bin_path.value().c_str(), argv, envp), SyscallFailsWithErrno(EACCES));
  }));
}

// Source `t1` constraints that deny a parent or grandparent domain also deny the bounded child.
TEST(BoundsTest, SourceConstrainAppliedAcrossBounds) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto constrain_file = ScopedTempFDWithLabel(MakeFileContext("test_bounds_constrain_file_t"));
  ASSERT_TRUE(constrain_file.is_valid());
  auto deep_constrain_file =
      ScopedTempFDWithLabel(MakeFileContext("test_bounds_constrain_deep_file_t"));
  ASSERT_TRUE(deep_constrain_file.is_valid());

  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_constrain_child_s"), [&] {
    struct stat st;
    EXPECT_THAT(stat(constrain_file.name().c_str(), &st), SyscallSucceeds());
    // Denied because parent fails `constrain file { read } ( t1 != constrain_parent_s )`.
    EXPECT_THAT(open(constrain_file.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
    // Denied because parent lacks `constrain_child_only_a` for `constrain file { append }`.
    EXPECT_THAT(open(constrain_file.name().c_str(), O_WRONLY | O_APPEND),
                SyscallFailsWithErrno(EACCES));
  }));

  // Multi-level (`gp_s -> mid_s -> deep_s`): denied because grandparent fails `t1 != gp_s`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_constrain_deep_s"), [&] {
    struct stat st;
    EXPECT_THAT(stat(deep_constrain_file.name().c_str(), &st), SyscallSucceeds());
    EXPECT_THAT(open(deep_constrain_file.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
  }));
}

// Target `t2` constraints that deny a paired parent target also deny a bounded source accessing
// the bounded target, without restricting an unbounded source.
TEST(BoundsTest, TargetConstrainAppliedAcrossBounds) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto constrain_file = ScopedTempFDWithLabel(MakeFileContext("test_bounds_constrain_file_t"));
  ASSERT_TRUE(constrain_file.is_valid());
  auto target_child_file =
      ScopedTempFDWithLabel(MakeFileContext("test_bounds_constrain_target_child_t"));
  ASSERT_TRUE(target_child_file.is_valid());

  // `t2_child_s` can read `constrain_file_t`, but is denied on `target_child_t` because paired
  // `target_parent_t` fails `t2 != test_bounds_constrain_target_parent_t`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_constrain_t2_child_s"), [&] {
    fbl::unique_fd allowed_fd(open(constrain_file.name().c_str(), O_RDONLY));
    EXPECT_THAT(allowed_fd.get(), SyscallSucceeds());

    struct stat st;
    EXPECT_THAT(stat(target_child_file.name().c_str(), &st), SyscallSucceeds());
    EXPECT_THAT(open(target_child_file.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
  }));

  // Standalone unbounded source is not subject to `target_parent_t` constraint when accessing
  // `target_child_t`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_unbounded_s"), [&] {
    fbl::unique_fd fd(open(target_child_file.name().c_str(), O_RDONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

// `mlsconstrain` expressions are evaluated against the parent domain (preserving the child's MLS
// range) when `source` is bounded.
TEST(BoundsTest, MlsConstrainAppliedAcrossBounds) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto mls_file = ScopedTempFDWithLabel(MakeFileContext("test_bounds_mls_file_t", "s1:c0"));
  ASSERT_TRUE(mls_file.is_valid());

  // Unbounded `mls_trusted_a` domain at `s0` writes `s1:c0` file via `t1 ==
  // test_bounds_mls_trusted_a`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_mls_unbounded_trusted_s"), [&] {
    fbl::unique_fd fd(open(mls_file.name().c_str(), O_WRONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));

  // Bounded `mls_child_s` at `s0` fails because `mls_parent_s` lacks `test_bounds_mls_trusted_a`
  // and `s0` does not dominate `s1:c0`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_mls_child_s"), [&] {
    struct stat st;
    EXPECT_THAT(stat(mls_file.name().c_str(), &st), SyscallSucceeds());
    EXPECT_THAT(open(mls_file.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));
  }));

  // Bounded `mls_child_s` at `s1:c0` succeeds (`mls_range()` preserved during parent synthesis).
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_bounds_mls_child_s:s1:c0", [&] {
    fbl::unique_fd fd(open(mls_file.name().c_str(), O_WRONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

// Extended permission bitmaps (`allowxperm`) are checked per-domain and not masked across
// `typebounds`, even though the base `ioctl` permission is bounded.
TEST(BoundsTest, ExtendedPermissionsPerDomainNotBounded) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto xperm_file = ScopedTempFDWithLabel(MakeFileContext("test_bounds_xperm_file_t"));
  ASSERT_TRUE(xperm_file.is_valid());

  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_xperm_parent_s"), [&] {
    fbl::unique_fd fd(open(xperm_file.name().c_str(), O_RDONLY));
    ASSERT_THAT(fd.get(), SyscallSucceeds());
    size_t val = 0;
    EXPECT_THAT(ioctl(fd.get(), 0xabcd, &val), ::testing::Not(SyscallFailsWithErrno(EACCES)));
    EXPECT_THAT(ioctl(fd.get(), 0xabce, &val), SyscallFailsWithErrno(EACCES));
  }));

  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_xperm_child_s"), [&] {
    fbl::unique_fd fd(open(xperm_file.name().c_str(), O_RDONLY));
    ASSERT_THAT(fd.get(), SyscallSucceeds());
    size_t val = 0;
    EXPECT_THAT(ioctl(fd.get(), 0xabce, &val), ::testing::Not(SyscallFailsWithErrno(EACCES)));
    EXPECT_THAT(ioctl(fd.get(), 0xabcd, &val), SyscallFailsWithErrno(EACCES));
  }));
}

// `dontaudit` and `permissive` remain per-domain and are neither inherited nor masked across
// `typebounds`.
TEST(BoundsTest, AuditAndPermissiveFlagsNotBounded) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto audit_file = ScopedTempFDWithLabel(MakeFileContext("test_bounds_audit_file_t"));
  ASSERT_TRUE(audit_file.is_valid());

  // `audit_parent_s` is `permissive` and has `dontaudit { write }`, so `O_WRONLY` succeeds
  // without emitting an audit record.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_audit_parent_s"), [&] {
    fbl::unique_fd fd(open(audit_file.name().c_str(), O_WRONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));

  // Bounded `audit_child_s` inherits neither `permissive` nor `dontaudit { write }` from its
  // parent (`O_WRONLY` fails and is audited), while its own `dontaudit { append }` is not masked
  // by its parent (`O_WRONLY | O_APPEND` fails without emitting an audit record).
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_audit_child_s"), [&] {
    EXPECT_THAT(open(audit_file.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));
    EXPECT_THAT(open(audit_file.name().c_str(), O_WRONLY | O_APPEND),
                SyscallFailsWithErrno(EACCES));
  }));

  // Conversely, `permissive` on `permissive_child_s` is not masked by its enforcing parent
  // `permissive_parent_s`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_permissive_parent_s"), [&] {
    EXPECT_THAT(open(audit_file.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
  }));
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_bounds_permissive_child_s"), [&] {
    fbl::unique_fd fd(open(audit_file.name().c_str(), O_RDONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

}  // namespace
