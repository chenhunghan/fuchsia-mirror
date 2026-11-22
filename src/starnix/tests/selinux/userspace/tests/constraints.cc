// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <lib/fit/defer.h>
#include <stdlib.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string>
#include <string_view>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/lib/fxl/strings/string_printf.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string GetTestPolicyName() { return "constraints_policy"; }

namespace {

std::string MakeContext(std::string_view user, std::string_view role, std::string_view type,
                        std::string_view level = "s0") {
  return fxl::StringPrintf("%s:%s:%s:%s", std::string(user).c_str(), std::string(role).c_str(),
                           std::string(type).c_str(), std::string(level).c_str());
}

// `constrain` with boolean operators (`not`, `and`, `or`) and nested parenthesized expressions:
// `( ( u1 == u2 and r1 == r2 ) or ( t1 == test_constraints_bool_override_s and not ( u2 ==
// test_constraints_u3 ) ) )`.
TEST(ConstraintsTest, BooleanOperatorsAndNestedExpressions) {
  auto file_u1_r1 = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u1", "test_constraints_r1", "test_constraints_bool_file_t"));
  ASSERT_TRUE(file_u1_r1.is_valid());
  auto file_u2_r1 = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u2", "test_constraints_r1", "test_constraints_bool_file_t"));
  ASSERT_TRUE(file_u2_r1.is_valid());
  auto file_u3_r2 = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u3", "test_constraints_r2", "test_constraints_bool_file_t"));
  ASSERT_TRUE(file_u3_r2.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // Left branch `(u1 == u2 and r1 == r2)` satisfied.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_constraints_r1", "test_constraints_bool_regular_s"),
      [&] {
        fbl::unique_fd fd(open(file_u1_r1.name().c_str(), O_RDONLY));
        EXPECT_THAT(fd.get(), SyscallSucceeds());
      }));

  // Neither branch satisfied (`r1 != r2` for `regular_s`).
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_constraints_r2", "test_constraints_bool_regular_s"),
      [&] {
        EXPECT_THAT(open(file_u1_r1.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
      }));

  // Right branch `(t1 == override_s and not (u2 == u3))` satisfied despite `u1 != u2` and
  // `r1 != r2`.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_constraints_r2", "test_constraints_bool_override_s"),
      [&] {
        fbl::unique_fd fd(open(file_u2_r1.name().c_str(), O_RDONLY));
        EXPECT_THAT(fd.get(), SyscallSucceeds());
      }));

  // Right branch negated (`u2 == test_constraints_u3`) and left branch false (`u1 != u2`).
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_constraints_r1", "test_constraints_bool_override_s"),
      [&] {
        EXPECT_THAT(open(file_u3_r2.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
      }));
}

// `constrain` on inherited `common file` permissions (`dir { read }`) vs class-specific
// permissions (`dir { rmdir }`), while unconstrained permissions on the same class
// (`dir { getattr search }`) remain allowed.
TEST(ConstraintsTest, CommonVsClassSpecificAndUnconstrainedPermissions) {
  std::string dir_template = "/tmp/test_constraints_dir_XXXXXX";
  {
    auto fscreate = ScopedTaskAttrResetter::SetTaskAttr(
        "fscreate",
        MakeContext("test_constraints_u1", "test_constraints_r1", "test_constraints_dir_t"));
    ASSERT_NE(mkdtemp(dir_template.data()), nullptr) << strerror(errno);
  }
  auto cleanup_dir = fit::defer([&] { rmdir(dir_template.c_str()); });

  auto enforce = ScopedEnforcement::SetEnforcing();

  // When `u1 != u2` and `r1 != r2`: unconstrained `getattr` (inherited) and `search`
  // (class-specific) succeed, while constrained `read` (inherited) and `rmdir` (class-specific)
  // fail with `EACCES` and emit AVC denials.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u2", "test_constraints_r2", "test_constraints_dir_s"), [&] {
        struct stat st;
        EXPECT_THAT(stat(dir_template.c_str(), &st), SyscallSucceeds());
        EXPECT_THAT(access(dir_template.c_str(), X_OK), SyscallSucceeds());
        EXPECT_THAT(open(dir_template.c_str(), O_RDONLY | O_DIRECTORY),
                    SyscallFailsWithErrno(EACCES));
        EXPECT_THAT(rmdir(dir_template.c_str()), SyscallFailsWithErrno(EACCES));
      }));

  // When `u1 == u2` and `r1 != r2`: inherited `dir { read }` succeeds, while class-specific
  // `dir { rmdir }` is still denied.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_constraints_r2", "test_constraints_dir_s"), [&] {
        fbl::unique_fd fd(open(dir_template.c_str(), O_RDONLY | O_DIRECTORY));
        EXPECT_THAT(fd.get(), SyscallSucceeds());
        EXPECT_THAT(rmdir(dir_template.c_str()), SyscallFailsWithErrno(EACCES));
      }));

  // When `r1 == r2`: class-specific `dir { rmdir }` succeeds even though `u1 != u2`.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u2", "test_constraints_r1", "test_constraints_dir_s"),
      [&] { EXPECT_THAT(rmdir(dir_template.c_str()), SyscallSucceeds()); }));
}

// `mlsconstrain` with `dom` and `domby` across `l1`, `h1`, `l2`, `h2`.
TEST(ConstraintsTest, MlsConstrainDominance) {
  auto file_s1c0 = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u1", "object_r", "test_constraints_mls_dom_file_t", "s1:c0"));
  ASSERT_TRUE(file_s1c0.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // `s1:c0.c1` dominates `s1:c0` (`read` allowed via `l1 dom h2 and h1 dom h2`), but is not
  // dominated by `s1:c0` (`write` denied via `l1 domby l2`).
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_mls_dom_s", "s1:c0.c1"), [&] {
        fbl::unique_fd fd(open(file_s1c0.name().c_str(), O_RDONLY));
        EXPECT_THAT(fd.get(), SyscallSucceeds());
        EXPECT_THAT(open(file_s1c0.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));
      }));

  // Range `s0:c0-s2:c0.c1`: `l1` (`s0:c0`) does not dominate `h2` (`s1:c0`) so `read` is denied,
  // while `l1 domby l2` (`s0:c0 domby s1:c0`), `h1 dom l2` (`s2:c0.c1 dom s1:c0`), and
  // `l1 domby h2` are all true so `write` succeeds.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_mls_dom_s", "s0:c0-s2:c0.c1"),
      [&] {
        EXPECT_THAT(open(file_s1c0.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
        fbl::unique_fd fd(open(file_s1c0.name().c_str(), O_WRONLY));
        EXPECT_THAT(fd.get(), SyscallSucceeds());
      }));

  // Range `s0:c0-s0:c0.c1`: `l1 domby l2` is true, but `h1` (`s0:c0.c1`) does not dominate `l2`
  // (`s1:c0`), so `write` is denied.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_mls_dom_s", "s0:c0-s0:c0.c1"),
      [&] {
        EXPECT_THAT(open(file_s1c0.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));
      }));
}

// `mlsconstrain` with `eq` (`==`) and `!=` across `l1`, `h1`, `l2`, `h2`.
TEST(ConstraintsTest, MlsConstrainEqualityAndInequality) {
  auto single_level_file = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u1", "object_r", "test_constraints_mls_eq_file_t", "s0:c0"));
  ASSERT_TRUE(single_level_file.is_valid());
  auto ranged_file = ScopedTempFDWithLabel(MakeContext(
      "test_constraints_u1", "object_r", "test_constraints_mls_eq_file_t", "s0:c0-s1:c0"));
  ASSERT_TRUE(ranged_file.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // Single-level subject `s0:c0`: `l1 eq l2` succeeds for `read`, but `l1 != h1` fails for
  // `write`.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_mls_eq_s", "s0:c0"), [&] {
        fbl::unique_fd fd(open(single_level_file.name().c_str(), O_RDONLY));
        EXPECT_THAT(fd.get(), SyscallSucceeds());
        EXPECT_THAT(open(single_level_file.name().c_str(), O_WRONLY),
                    SyscallFailsWithErrno(EACCES));
      }));

  // Ranged subject `s0:c0-s1:c0` (`l1 != h1`): `write` succeeds on single-level `single_level_file`
  // (`l2 == h2`), but fails on `ranged_file` (`l2 != h2`).
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_mls_eq_s", "s0:c0-s1:c0"),
      [&] {
        fbl::unique_fd fd(open(single_level_file.name().c_str(), O_WRONLY));
        EXPECT_THAT(fd.get(), SyscallSucceeds());
        EXPECT_THAT(open(ranged_file.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));
      }));

  // Subject at `s0`: `read` fails `l1 eq l2`.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_mls_eq_s", "s0"), [&] {
        EXPECT_THAT(open(single_level_file.name().c_str(), O_RDONLY),
                    SyscallFailsWithErrno(EACCES));
      }));
}

// `mlsconstrain` with `incomp` across incomparable category sets (`s0:c0` vs `s0:c1`) and
// sensitivity/category trade-offs (`s1:c1` vs `s0:c0`).
TEST(ConstraintsTest, MlsConstrainIncomparableLevels) {
  auto file_s0c0 = ScopedTempFDWithLabel(MakeContext(
      "test_constraints_u1", "object_r", "test_constraints_mls_incomp_file_t", "s0:c0"));
  ASSERT_TRUE(file_s0c0.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // `s0:c1` is incomparable with `s0:c0`: `read` (`not (l1 incomp l2)`) is denied, while `write`
  // (`l1 incomp l2 and h1 incomp h2`) succeeds.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_mls_incomp_s", "s0:c1"), [&] {
        EXPECT_THAT(open(file_s0c0.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
        fbl::unique_fd fd(open(file_s0c0.name().c_str(), O_WRONLY));
        EXPECT_THAT(fd.get(), SyscallSucceeds());
      }));

  // `s0:c0.c1` dominates (is comparable with) `s0:c0`: `read` succeeds, while `write` is denied.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_mls_incomp_s", "s0:c0.c1"),
      [&] {
        fbl::unique_fd fd(open(file_s0c0.name().c_str(), O_RDONLY));
        EXPECT_THAT(fd.get(), SyscallSucceeds());
        EXPECT_THAT(open(file_s0c0.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));
      }));

  // `s1:c1` vs `s0:c0` (higher sensitivity `s1 > s0`, but disjoint category `c1` vs `c0`) is also
  // incomparable: `read` is denied and `write` succeeds.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_mls_incomp_s", "s1:c1"), [&] {
        EXPECT_THAT(open(file_s0c0.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
        fbl::unique_fd fd(open(file_s0c0.name().c_str(), O_WRONLY));
        EXPECT_THAT(fd.get(), SyscallSucceeds());
      }));
}

// Multiple `constrain` statements on the same class and permission (`file { link }`) are all
// evaluated and must all be satisfied, while `dontaudit` suppresses the AVC audit record on a
// constraint denial.
TEST(ConstraintsTest, MultipleConstraintsOnSamePermission) {
  auto target_file = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u1", "object_r", "test_constraints_multi_file_t"));
  ASSERT_TRUE(target_file.is_valid());
  std::string link_path = target_file.name() + ".link";
  auto cleanup_link = fit::defer([&] { unlink(link_path.c_str()); });

  auto enforce = ScopedEnforcement::SetEnforcing();

  // Both constraints satisfied (`u1 == u2` and `t1 != test_constraints_multi_bad_type_s`).
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_multi_ok_s"), [&] {
        ASSERT_THAT(link(target_file.name().c_str(), link_path.c_str()), SyscallSucceeds());
        EXPECT_THAT(unlink(link_path.c_str()), SyscallSucceeds());
      }));

  // First constraint unsatisfied (`u1 != u2`), second satisfied (`t1 == multi_ok_s`).
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u2", "test_r", "test_constraints_multi_ok_s"), [&] {
        EXPECT_THAT(link(target_file.name().c_str(), link_path.c_str()),
                    SyscallFailsWithErrno(EACCES));
      }));

  // First constraint satisfied (`u1 == u2`), second unsatisfied (`t1 == multi_bad_type_s`).
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_multi_bad_type_s"), [&] {
        EXPECT_THAT(link(target_file.name().c_str(), link_path.c_str()),
                    SyscallFailsWithErrno(EACCES));
      }));

  // `dontaudit` on `(test_constraints_multi_dontaudit_s, test_constraints_multi_file_t, file)`
  // suppresses the AVC audit record when the `u1 == u2` constraint denies `link`.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u2", "test_r", "test_constraints_multi_dontaudit_s"), [&] {
        EXPECT_THAT(link(target_file.name().c_str(), link_path.c_str()),
                    SyscallFailsWithErrno(EACCES));
      }));
}

// `constrain` with `r1 == r2` (`file { setattr }`) and `r1 != r2` (`file { read }`).
TEST(ConstraintsTest, RoleAttributeEqualityAndInequality) {
  auto eq_file = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u1", "test_constraints_r1", "test_constraints_role_eq_file_t"));
  ASSERT_TRUE(eq_file.is_valid());
  auto ne_file = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u1", "test_constraints_r1", "test_constraints_role_ne_file_t"));
  ASSERT_TRUE(ne_file.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // Same role (`r1 == r2`): `setattr` on `eq_file` succeeds, while `read` on `ne_file` fails.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_constraints_r1", "test_constraints_role_s"), [&] {
        EXPECT_THAT(chmod(eq_file.name().c_str(), 0640), SyscallSucceeds());
        EXPECT_THAT(open(ne_file.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
      }));

  // Different role (`r1 != r2`): `setattr` on `eq_file` fails, while `read` on `ne_file` succeeds.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_constraints_r2", "test_constraints_role_s"), [&] {
        EXPECT_THAT(chmod(eq_file.name().c_str(), 0644), SyscallFailsWithErrno(EACCES));
        fbl::unique_fd ne_fd(open(ne_file.name().c_str(), O_RDONLY));
        EXPECT_THAT(ne_fd.get(), SyscallSucceeds());
      }));
}

// `constrain` with source and target role sets (`r1 == { ... }`, `r1 != ...`, `r2 == { ... }`,
// `r2 != ...`).
TEST(ConstraintsTest, RoleNameSets) {
  auto file_r1 = ScopedTempFDWithLabel(MakeContext("test_constraints_u1", "test_constraints_r1",
                                                   "test_constraints_role_set_file_t"));
  ASSERT_TRUE(file_r1.is_valid());
  auto file_r2 = ScopedTempFDWithLabel(MakeContext("test_constraints_u1", "test_constraints_r2",
                                                   "test_constraints_role_set_file_t"));
  ASSERT_TRUE(file_r2.is_valid());
  auto file_r3 = ScopedTempFDWithLabel(MakeContext("test_constraints_u1", "test_constraints_r3",
                                                   "test_constraints_role_set_file_t"));
  ASSERT_TRUE(file_r3.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // `r1 = test_constraints_r2` is in `{ r1 r2 r3 }` and `!= r3`, so `read` succeeds; `write`
  // succeeds on `file_r1` (`r2 = r1`), fails `r2 != test_constraints_r2` on `file_r2`, and fails
  // `r2 == { object_r r1 r2 }` on `file_r3`.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_constraints_r2", "test_constraints_role_set_s"),
      [&] {
        fbl::unique_fd r_fd(open(file_r1.name().c_str(), O_RDONLY));
        EXPECT_THAT(r_fd.get(), SyscallSucceeds());
        fbl::unique_fd w_fd(open(file_r1.name().c_str(), O_WRONLY));
        EXPECT_THAT(w_fd.get(), SyscallSucceeds());
        EXPECT_THAT(open(file_r2.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));
        EXPECT_THAT(open(file_r3.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));
      }));

  // `r1 = test_constraints_r3` is in `{ r1 r2 r3 }` but rejected by `r1 != test_constraints_r3`.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_constraints_r3", "test_constraints_role_set_s"),
      [&] { EXPECT_THAT(open(file_r1.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES)); }));

  // `r1 = test_r` satisfies `r1 != test_constraints_r3` but is rejected by `r1 == { r1 r2 r3 }`.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u1", "test_r", "test_constraints_role_set_s"),
      [&] { EXPECT_THAT(open(file_r1.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES)); }));
}

// `constrain` with type and attribute name sets (`t1 == { attr type }`, `t2 != attr`).
TEST(ConstraintsTest, TypeAndAttributeNameSets) {
  auto ok_file =
      ScopedTempFDWithLabel(MakeContext("test_u", "object_r", "test_constraints_type_ok_file_t"));
  ASSERT_TRUE(ok_file.is_valid());
  auto blocked_file = ScopedTempFDWithLabel(
      MakeContext("test_u", "object_r", "test_constraints_type_blocked_file_t"));
  ASSERT_TRUE(blocked_file.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // `test_constraints_type_attr_s` matches `t1` via attribute `test_constraints_allowed_src_a`,
  // succeeding on `ok_file` and failing `t2 != test_constraints_denied_tgt_a` on `blocked_file`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_constraints_type_attr_s"), [&] {
    fbl::unique_fd fd(open(ok_file.name().c_str(), O_RDONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
    EXPECT_THAT(open(blocked_file.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
  }));

  // `test_constraints_type_listed_s` matches `t1` via concrete type in `{ ... }`.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_constraints_type_listed_s"), [&] {
    fbl::unique_fd fd(open(ok_file.name().c_str(), O_RDONLY));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));

  // `test_constraints_type_unlisted_s` is in neither the attribute nor the concrete type list.
  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_constraints_type_unlisted_s"), [&] {
    EXPECT_THAT(open(ok_file.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
  }));
}

// `constrain` with `t1 == t2` (`file { append }`) and `t1 != t2` (`file { write }`).
TEST(ConstraintsTest, TypeAttributeEqualityAndInequality) {
  auto same_type_file =
      ScopedTempFDWithLabel(MakeContext("test_u", "object_r", "test_constraints_same_type_t"));
  ASSERT_TRUE(same_type_file.is_valid());
  auto diff_type_file =
      ScopedTempFDWithLabel(MakeContext("test_u", "object_r", "test_constraints_diff_type_file_t"));
  ASSERT_TRUE(diff_type_file.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs(MakeTestSecurityContext("test_constraints_same_type_t"), [&] {
    // `t1 == t2`: `append` succeeds, `write` fails.
    fbl::unique_fd append_fd(open(same_type_file.name().c_str(), O_WRONLY | O_APPEND));
    EXPECT_THAT(append_fd.get(), SyscallSucceeds());
    EXPECT_THAT(open(same_type_file.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));

    // `t1 != t2`: `write` succeeds, `append` fails.
    fbl::unique_fd write_fd(open(diff_type_file.name().c_str(), O_WRONLY));
    EXPECT_THAT(write_fd.get(), SyscallSucceeds());
    EXPECT_THAT(open(diff_type_file.name().c_str(), O_WRONLY | O_APPEND),
                SyscallFailsWithErrno(EACCES));
  }));
}

// `constrain` with `u1 == u2` (`file { lock }`) and `u1 != u2` (`file { read }`).
TEST(ConstraintsTest, UserAttributeEqualityAndInequality) {
  auto eq_file = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u1", "object_r", "test_constraints_user_eq_file_t"));
  ASSERT_TRUE(eq_file.is_valid());
  auto ne_file = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u1", "object_r", "test_constraints_user_ne_file_t"));
  ASSERT_TRUE(ne_file.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // Same user (`u1 == u2`): `lock` on `eq_file` succeeds, while `read` on `ne_file` fails.
  EXPECT_TRUE(
      RunSubprocessAs(MakeContext("test_constraints_u1", "test_r", "test_constraints_user_s"), [&] {
        fbl::unique_fd eq_fd(open(eq_file.name().c_str(), O_RDONLY));
        ASSERT_THAT(eq_fd.get(), SyscallSucceeds());
        EXPECT_THAT(flock(eq_fd.get(), LOCK_SH | LOCK_NB), SyscallSucceeds());
        EXPECT_THAT(flock(eq_fd.get(), LOCK_UN), SyscallSucceeds());

        EXPECT_THAT(open(ne_file.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
      }));

  // Different user (`u1 != u2`): `lock` on `eq_file` fails, while `read` on `ne_file` succeeds.
  EXPECT_TRUE(
      RunSubprocessAs(MakeContext("test_constraints_u2", "test_r", "test_constraints_user_s"), [&] {
        fbl::unique_fd eq_fd(open(eq_file.name().c_str(), O_RDONLY));
        ASSERT_THAT(eq_fd.get(), SyscallSucceeds());
        EXPECT_THAT(flock(eq_fd.get(), LOCK_SH | LOCK_NB), SyscallFailsWithErrno(EACCES));

        fbl::unique_fd ne_fd(open(ne_file.name().c_str(), O_RDONLY));
        EXPECT_THAT(ne_fd.get(), SyscallSucceeds());
      }));
}

// `constrain` with source and target user sets (`u1 == { ... }`, `u1 != ...`, `u2 == { ... }`,
// `u2 != ...`).
TEST(ConstraintsTest, UserNameSets) {
  auto file_u1 = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u1", "object_r", "test_constraints_user_set_file_t"));
  ASSERT_TRUE(file_u1.is_valid());
  auto file_u3 = ScopedTempFDWithLabel(
      MakeContext("test_constraints_u3", "object_r", "test_constraints_user_set_file_t"));
  ASSERT_TRUE(file_u3.is_valid());
  auto file_test_u =
      ScopedTempFDWithLabel(MakeContext("test_u", "object_r", "test_constraints_user_set_file_t"));
  ASSERT_TRUE(file_test_u.is_valid());

  auto enforce = ScopedEnforcement::SetEnforcing();

  // `u1 = test_constraints_u2` is in `{ u1 u2 u3 }` and `!= u3`: `read` succeeds, `write` succeeds
  // on `file_u1` (`u2 = u1`), fails `u2 != test_constraints_u3` on `file_u3`, and fails
  // `u2 == { u1 u2 u3 }` on `file_test_u`.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u2", "test_r", "test_constraints_user_set_s"), [&] {
        fbl::unique_fd r_fd(open(file_u1.name().c_str(), O_RDONLY));
        EXPECT_THAT(r_fd.get(), SyscallSucceeds());
        fbl::unique_fd w_fd(open(file_u1.name().c_str(), O_WRONLY));
        EXPECT_THAT(w_fd.get(), SyscallSucceeds());
        EXPECT_THAT(open(file_u3.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));
        EXPECT_THAT(open(file_test_u.name().c_str(), O_WRONLY), SyscallFailsWithErrno(EACCES));
      }));

  // `u1 = test_constraints_u3` is in `{ u1 u2 u3 }` but rejected by `u1 != test_constraints_u3`.
  EXPECT_TRUE(RunSubprocessAs(
      MakeContext("test_constraints_u3", "test_r", "test_constraints_user_set_s"),
      [&] { EXPECT_THAT(open(file_u1.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES)); }));

  // `u1 = test_u` satisfies `u1 != test_constraints_u3` but is rejected by `u1 == { u1 u2 u3 }`.
  EXPECT_TRUE(RunSubprocessAs(MakeContext("test_u", "test_r", "test_constraints_user_set_s"), [&] {
    EXPECT_THAT(open(file_u1.name().c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
  }));
}

}  // namespace
