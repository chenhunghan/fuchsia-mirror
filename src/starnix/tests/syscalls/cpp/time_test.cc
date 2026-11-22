// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#include <future>
#include <thread>

#include <linux/capability.h>

#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

// Returns the ID of the CPU-time clock of the thread with the given `tid`, encoded like Linux's
// `MAKE_THREAD_CPUCLOCK(tid, CPUCLOCK_SCHED)`.
clockid_t ThreadCpuClock(pid_t tid) {
  constexpr clockid_t kCpuClockPerThreadMask = 4;
  constexpr clockid_t kCpuClockSched = 2;
  return static_cast<clockid_t>(~static_cast<unsigned>(tid) << 3) | kCpuClockPerThreadMask |
         kCpuClockSched;
}

pid_t GetTid() { return static_cast<pid_t>(syscall(SYS_gettid)); }

}  // namespace

TEST(TimeTest, ClockGetResMonotonic) {
  clockid_t clockid = CLOCK_MONOTONIC;
  struct timespec tp;
  ASSERT_THAT(clock_getres(clockid, &tp), SyscallSucceeds());
  ASSERT_EQ(tp.tv_sec, 0);
  ASSERT_EQ(tp.tv_nsec, 1);
}

TEST(TimeTest, ClockGetResRealTime) {
  clockid_t clockid = CLOCK_REALTIME;
  struct timespec tp;
  ASSERT_THAT(clock_getres(clockid, &tp), SyscallSucceeds());
  ASSERT_EQ(tp.tv_sec, 0);
  ASSERT_EQ(tp.tv_nsec, 1);
}

TEST(TimeTest, ClockGetResSyscallFail) {
  // Setting clockid to 15 as it will cause an error to be thrown.
  // The vDSO will check if the clockid is valid using the function is_valid_cpu_clock.
  // Since it isn't valid, the vDSO throws an error.
  clockid_t clockid = 15;
  struct timespec tp;
  tp.tv_nsec = 0;
  ASSERT_THAT(clock_getres(clockid, &tp), SyscallFails());
}

// A thread CPU clock may refer to any thread in the caller's thread group.
TEST(TimeTest, ThreadCpuClockInThreadGroupIsValid) {
  struct timespec tp;
  EXPECT_THAT(clock_gettime(ThreadCpuClock(GetTid()), &tp), SyscallSucceeds());
  EXPECT_THAT(clock_getres(ThreadCpuClock(GetTid()), &tp), SyscallSucceeds());

  // Retrieve both futures before starting the sibling so that no member function of either promise
  // is called concurrently from both threads.
  std::promise<pid_t> sibling_tid_promise;
  std::future<pid_t> sibling_tid_future = sibling_tid_promise.get_future();
  std::promise<void> sibling_exit_promise;
  std::future<void> sibling_exit_future = sibling_exit_promise.get_future();
  std::thread sibling([&] {
    sibling_tid_promise.set_value(GetTid());
    sibling_exit_future.wait();
  });
  pid_t sibling_tid = sibling_tid_future.get();

  EXPECT_THAT(clock_gettime(ThreadCpuClock(sibling_tid), &tp), SyscallSucceeds());
  EXPECT_THAT(clock_getres(ThreadCpuClock(sibling_tid), &tp), SyscallSucceeds());

  sibling_exit_promise.set_value();
  sibling.join();
}

// A thread CPU clock may not refer to a thread outside the caller's thread group.
TEST(TimeTest, ThreadCpuClockOutsideThreadGroupIsInvalid) {
  test_helper::Rendezvous exit_child = test_helper::MakeRendezvous();

  test_helper::ForkHelper helper;
  helper.KillChildrenOnDestroy();
  helper.OnlyWaitForForkedChildren();
  pid_t child_pid = helper.RunInForkedProcess([&] { exit_child.holder.hold(); });

  struct timespec tp;
  EXPECT_THAT(clock_gettime(ThreadCpuClock(child_pid), &tp), SyscallFailsWithErrno(EINVAL));
  EXPECT_THAT(clock_getres(ThreadCpuClock(child_pid), &tp), SyscallFailsWithErrno(EINVAL));

  exit_child.poker.poke();
  EXPECT_TRUE(helper.WaitForChildren());
}

// TODO(https://fxbug.dev/350763590) remove no_sanitize attribute and diagnostic suppression
TEST(TimeTest, GetTimeOfDayNullTvSomeTz) __attribute__((no_sanitize("nonnull-attribute"))) {
  struct timezone tz;
// glibc adds nonnull attribute to the tv argument in getttimeofday.
// gettimeofday, however, does allow the tv argument to be NULL.
// To test that the vdso gettimeofday function allows tv to be NULL, the nonnull warning is
// temporarily disabled.
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wnonnull"
  ASSERT_THAT(gettimeofday(nullptr, &tz), SyscallSucceeds());
#pragma GCC diagnostic pop
}

TEST(TimeTest, GetTimeOfDaySyscallVsVDSO) {
  struct timeval tv1, tv2, tv3;
  ASSERT_THAT(gettimeofday(&tv1, nullptr), SyscallSucceeds());
  ASSERT_THAT(syscall(SYS_gettimeofday, &tv2, nullptr), SyscallSucceeds());
  ASSERT_THAT(gettimeofday(&tv3, nullptr), SyscallSucceeds());
  EXPECT_LE(std::make_pair(tv1.tv_sec, tv1.tv_usec), std::make_pair(tv2.tv_sec, tv2.tv_usec));
  EXPECT_LE(std::make_pair(tv2.tv_sec, tv2.tv_usec), std::make_pair(tv3.tv_sec, tv3.tv_usec));
}

TEST(TimeTest, GetTimeOfDaySomeTvNullTz) {
  struct timeval tv;
  ASSERT_THAT(gettimeofday(&tv, nullptr), SyscallSucceeds());
}

TEST(TimeTest, GetTimeOfDaySomeTvSomeTz) {
  struct timeval tv;
  struct timezone tz;
  ASSERT_THAT(gettimeofday(&tv, &tz), SyscallSucceeds());
}

// TODO(https://fxbug.dev/350763590) remove no_sanitize attribute and diagnostic suppression
TEST(TimeTest, GetTimeOfDayNullTvNullTz) __attribute__((no_sanitize("nonnull-attribute"))) {
// glibc adds nonnull attribute to the tv argument in getttimeofday.
// gettimeofday, however, does allow the tv argument to be NULL.
// To test that the vdso gettimeofday function allows tv to be NULL, the nonnull warning is
// temporarily disabled.
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wnonnull"
  ASSERT_THAT(gettimeofday(nullptr, nullptr), SyscallSucceeds());
#pragma GCC diagnostic pop
}

TEST(TimeTest, AdjustedTimeReflectedInVdso) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "AdjustedTimeReflectedInVdso test requires CAP_SYS_ADMIN";
  }

  // Get time via vDSO (standard gettimeofday)
  struct timeval tv_vdso1;
  ASSERT_EQ(0, gettimeofday(&tv_vdso1, nullptr));

  // Get time via syscall
  struct timeval tv_sys1;
  ASSERT_EQ(0, syscall(SYS_gettimeofday, &tv_sys1, nullptr));

  // Adjust time by winding it forward.
  struct timeval tv_adjust = tv_sys1;
  tv_adjust.tv_sec += 10000;

  // Use syscall to set time.
  ASSERT_EQ(0, syscall(SYS_settimeofday, &tv_adjust, nullptr)) << strerror(errno);

  // Get time via vDSO again.
  struct timeval tv_vdso2;
  ASSERT_EQ(0, gettimeofday(&tv_vdso2, nullptr));

  // Get time via syscall again.
  struct timeval tv_sys2;
  ASSERT_EQ(0, syscall(SYS_gettimeofday, &tv_sys2, nullptr));

  // Verify that the vDSO time has jumped forward.
  EXPECT_GE(tv_vdso2.tv_sec, tv_adjust.tv_sec);

  // Verify that vDSO and syscall agree (roughly).
  long diff = tv_sys2.tv_sec - tv_vdso2.tv_sec;
  if (diff < 0) {
    diff = -diff;
  }
  EXPECT_LE(diff, 1);
}

namespace {

struct SetTimeOfDayCase {
  const char* name;
  bool require_cap_sys_time;
  bool pass_tv;
  bool use_current_tv_sec;
  struct timeval tv;
  bool pass_tz;
  struct timezone tz;
  int expected_errno;
};

class SetTimeOfDayValidationTest : public ::testing::TestWithParam<SetTimeOfDayCase> {};

TEST_P(SetTimeOfDayValidationTest, FailsWithExpectedErrno) {
  const SetTimeOfDayCase& param = GetParam();
  if (param.require_cap_sys_time && !test_helper::HasCapabilityPermitted(CAP_SYS_TIME)) {
    GTEST_SKIP() << "CAP_SYS_TIME not permitted, skipping privileged validation case.";
  }

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&param] {
    if (param.require_cap_sys_time) {
      test_helper::SetCapabilityEffective(CAP_SYS_TIME);
    } else {
      test_helper::UnsetCapabilityEffective(CAP_SYS_TIME);
    }

    struct timeval tv = param.tv;
    if (param.use_current_tv_sec) {
      struct timeval now{};
      ASSERT_THAT(gettimeofday(&now, nullptr), SyscallSucceeds());
      tv.tv_sec = now.tv_sec;
    }
    struct timezone tz = param.tz;

    EXPECT_THAT(
        syscall(SYS_settimeofday, param.pass_tv ? &tv : nullptr, param.pass_tz ? &tz : nullptr),
        SyscallFailsWithErrno(param.expected_errno));
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

constexpr SetTimeOfDayCase kSetTimeOfDayCases[] = {
    {"CurrentTvWithoutCap", false, true, true, {}, false, {}, EPERM},
    {"NullArgsWithoutCap", false, false, false, {}, false, {}, EPERM},
    {"UsecTooLargeWithoutCap", false, true, true, {0, 1000000}, false, {}, EINVAL},
    {"NegativeUsecWithoutCap", false, true, true, {0, -1}, false, {}, EINVAL},
    {"NegativeSecWithoutCap", false, true, false, {-1, 0}, false, {}, EINVAL},
    {"TzHighWithoutCap", false, false, false, {}, true, {15 * 60 + 1, 0}, EPERM},
    {"TzLowWithoutCap", false, false, false, {}, true, {-15 * 60 - 1, 0}, EPERM},
    {"TzHighWithCap", true, false, false, {}, true, {15 * 60 + 1, 0}, EINVAL},
    {"TzLowWithCap", true, false, false, {}, true, {-15 * 60 - 1, 0}, EINVAL},
};

INSTANTIATE_TEST_SUITE_P(TimeTest, SetTimeOfDayValidationTest,
                         ::testing::ValuesIn(kSetTimeOfDayCases),
                         [](const ::testing::TestParamInfo<SetTimeOfDayCase>& info) {
                           return info.param.name;
                         });

}  // namespace
