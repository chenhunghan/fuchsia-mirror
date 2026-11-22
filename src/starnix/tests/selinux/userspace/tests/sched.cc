// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sched.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <string>
#include <tuple>

#include <gtest/gtest.h>
#include <linux/capability.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string GetTestPolicyName() { return "sched_policy"; }

namespace {

class ScopedTargetProcess {
 public:
  ScopedTargetProcess(std::string label) {
    test_helper::Rendezvous transitioned = test_helper::MakeRendezvous();
    test_helper::Rendezvous exit_event = test_helper::MakeRendezvous();

    exit_poker_ = std::move(exit_event.poker);

    child_ = fork_helper_.RunInForkedProcess([label, poker = std::move(transitioned.poker),
                                              holder = std::move(exit_event.holder)]() mutable {
      fit::result<int> success = WriteTaskAttr("current", label);
      poker.poke();
      holder.hold();
      _exit(success.is_error());
    });

    transitioned.holder.hold();
  }

  ~ScopedTargetProcess() { exit_poker_.poke(); }

  pid_t pid() const { return child_; }

 private:
  test_helper::Poker exit_poker_;
  test_helper::ForkHelper fork_helper_;
  pid_t child_;
};

TEST(SchedTest, SetPrioritySucceedsWithSetsched) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_sched_child_t:s0", [&] {
    EXPECT_THAT(syscall(SYS_setpriority, PRIO_PROCESS, target.pid(), 10), SyscallSucceeds());
  }));
}

TEST(SchedTest, SetPriorityDeniedWithoutSetsched) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_sched_child_no_setsched_t:s0", [&] {
    EXPECT_THAT(syscall(SYS_setpriority, PRIO_PROCESS, target.pid(), 10),
                SyscallFailsWithErrno(EACCES));
  }));
}

TEST(SchedTest, SchedSetschedulerSucceedsWithSetsched) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_sched_child_t:s0", [&] {
    struct sched_param param = {.sched_priority = 0};
    EXPECT_THAT(syscall(SYS_sched_setscheduler, target.pid(), SCHED_OTHER, &param),
                SyscallSucceeds());
  }));
}

TEST(SchedTest, SchedSetschedulerDeniedWithoutSetsched) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_sched_child_no_setsched_t:s0", [&] {
    struct sched_param param = {.sched_priority = 0};
    EXPECT_THAT(syscall(SYS_sched_setscheduler, target.pid(), SCHED_OTHER, &param),
                SyscallFailsWithErrno(EACCES));
  }));
}

TEST(SchedTest, SchedGetschedulerSucceedsWithGetsched) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_sched_child_t:s0", [&] {
    EXPECT_THAT(syscall(SYS_sched_getscheduler, target.pid()), SyscallSucceeds());
  }));
}

TEST(SchedTest, SchedGetschedulerDeniedWithoutGetsched) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_sched_child_no_getsched_t:s0", [&] {
    EXPECT_THAT(syscall(SYS_sched_getscheduler, target.pid()), SyscallFailsWithErrno(EACCES));
  }));
}

TEST(SchedTest, SchedGetaffinitySucceedsWithGetsched) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_sched_child_t:s0", [&] {
    cpu_set_t mask;
    CPU_ZERO(&mask);
    EXPECT_THAT(syscall(SYS_sched_getaffinity, target.pid(), sizeof(mask), &mask),
                SyscallSucceeds());
  }));
}

TEST(SchedTest, SchedGetaffinityDeniedWithoutGetsched) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_sched_child_no_getsched_t:s0", [&] {
    cpu_set_t mask;
    CPU_ZERO(&mask);
    EXPECT_THAT(syscall(SYS_sched_getaffinity, target.pid(), sizeof(mask), &mask),
                SyscallFailsWithErrno(EACCES));
  }));
}

TEST(SchedTest, SchedSetaffinitySucceedsWithSetsched) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_sched_child_t:s0", [&] {
    cpu_set_t mask;
    CPU_ZERO(&mask);
    CPU_SET(0, &mask);
    EXPECT_THAT(syscall(SYS_sched_setaffinity, target.pid(), sizeof(mask), &mask),
                SyscallSucceeds());
  }));
}

TEST(SchedTest, SchedSetaffinityDeniedWithoutSetsched) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_sched_child_no_setsched_t:s0", [&] {
    cpu_set_t mask;
    CPU_ZERO(&mask);
    CPU_SET(0, &mask);
    EXPECT_THAT(syscall(SYS_sched_setaffinity, target.pid(), sizeof(mask), &mask),
                SyscallFailsWithErrno(EACCES));
  }));
}

struct SchedSysNiceOp {
  const char* name;
  long (*fn)(pid_t);
};

long SetPriorityNice1(pid_t pid) { return setpriority(PRIO_PROCESS, pid, 1); }

long SetAffinityCpu0(pid_t pid) {
  cpu_set_t mask;
  CPU_ZERO(&mask);
  CPU_SET(0, &mask);
  return syscall(SYS_sched_setaffinity, pid, sizeof(mask), &mask);
}

class SchedSysNiceTest : public ::testing::TestWithParam<std::tuple<SchedSysNiceOp, bool>> {};

/// When `capability { sys_nice }` is granted, scheduling modifications on a task with an extra
/// permitted capability succeed; when denied, they fail with `EPERM`.
TEST_P(SchedSysNiceTest, MissingTargetCapRequiresSysNice) {
  const auto& [op, allowed] = GetParam();
  const char* domain = allowed ? "test_u:test_r:test_sched_child_sys_nice_t:s0"
                               : "test_u:test_r:test_sched_child_no_sys_nice_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();
  ScopedTargetProcess target("test_u:test_r:test_sched_target_t:s0");

  EXPECT_TRUE(RunSubprocessAs(domain, [&] {
    // Drop a permitted capability that `target` retains so `CAP_SYS_NICE` is required even when
    // both tasks share the same UID.
    test_helper::UnsetCapabilityEffective(CAP_NET_RAW);
    test_helper::UnsetCapabilityPermitted(CAP_NET_RAW);
    EXPECT_THAT(op.fn(target.pid()), SyscallSucceedsOrFailsWithErrno(allowed, EPERM));
  }));
}

INSTANTIATE_TEST_SUITE_P(
    SchedTest, SchedSysNiceTest,
    ::testing::Combine(::testing::Values(SchedSysNiceOp{"Setpriority", SetPriorityNice1},
                                         SchedSysNiceOp{"SchedSetaffinity", SetAffinityCpu0}),
                       ::testing::Bool()),
    [](const ::testing::TestParamInfo<std::tuple<SchedSysNiceOp, bool>>& info) {
      return std::string(std::get<0>(info.param).name) +
             (std::get<1>(info.param) ? "Allowed" : "Denied");
    });

}  // namespace
