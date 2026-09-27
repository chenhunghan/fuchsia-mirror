// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/mount.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

#include <cerrno>
#include <csignal>
#include <cstdlib>
#include <ctime>
#include <string>

#include <gtest/gtest.h>
#include <linux/capability.h>

#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

TEST(Timers, NoWakeAlarmCap) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([]() {
    test_helper::UnsetCapability(CAP_WAKE_ALARM);
    timer_t timer_id;
    struct sigevent sev = {};
    sev.sigev_notify = SIGEV_NONE;

    EXPECT_THAT(timer_create(CLOCK_BOOTTIME_ALARM, &sev, &timer_id), SyscallFailsWithErrno(EPERM));
    EXPECT_THAT(timer_create(CLOCK_REALTIME_ALARM, &sev, &timer_id), SyscallFailsWithErrno(EPERM));
  });
}

TEST(Timers, RealtimeAlarm) {
  if (!test_helper::HasCapability(CAP_WAKE_ALARM)) {
    GTEST_SKIP()
        << "The CAP_WAKE_ALARM capability is required to create a CLOCK_REALTIME_ALARM timer.";
  }
  timespec begin = {};
  ASSERT_THAT(clock_gettime(CLOCK_REALTIME, &begin), SyscallSucceeds());

  timer_t timer_id;
  struct sigevent sev = {};
  sev.sigev_notify = SIGEV_NONE;
  ASSERT_THAT(timer_create(CLOCK_REALTIME_ALARM, &sev, &timer_id), SyscallSucceeds());

  // Test timer 1 second in the future.
  struct itimerspec its = {};
  its.it_value = begin;
  its.it_value.tv_sec += 1;
  ASSERT_THAT(timer_settime(timer_id, TIMER_ABSTIME, &its, nullptr), SyscallSucceeds());

  struct timespec sleep_t = {
      .tv_sec = 1,
      .tv_nsec = 5000,
  };
  nanosleep(&sleep_t, nullptr);

  // The timer should count down to 0 and stop since the interval is zero. No overruns should be
  // counted.
  EXPECT_THAT(timer_gettime(timer_id, &its), SyscallSucceeds());
  EXPECT_EQ(0, its.it_value.tv_sec);
  EXPECT_EQ(0, its.it_value.tv_nsec);
  EXPECT_THAT(timer_getoverrun(timer_id), SyscallSucceedsWithValue(0));

  EXPECT_THAT(timer_delete(timer_id), SyscallSucceeds());
}

TEST(Timers, PosixTimersNotPreservedAcrossExec) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    int kernel_timer_id = -1;
    struct sigevent sev = {};
    sev.sigev_notify = SIGEV_NONE;
    ASSERT_THAT(syscall(SYS_timer_create, CLOCK_MONOTONIC, &sev, &kernel_timer_id),
                SyscallSucceeds());

    struct itimerspec its = {};
    its.it_value.tv_sec = 60;
    its.it_interval.tv_sec = 60;
    ASSERT_THAT(syscall(SYS_timer_settime, kernel_timer_id, 0, &its, nullptr), SyscallSucceeds());

    struct itimerval itv = {};
    itv.it_value.tv_sec = 60;
    ASSERT_THAT(setitimer(ITIMER_REAL, &itv, nullptr), SyscallSucceeds());

    std::string child_path = test_helper::GetTestResourcePath("timers_test_exec_child");
    std::string timer_id_str = std::to_string(kernel_timer_id);
    char* const argv[] = {
        const_cast<char*>(child_path.c_str()),
        const_cast<char*>(timer_id_str.c_str()),
        nullptr,
    };
    execve(child_path.c_str(), argv, nullptr);
    perror("execve");
    _exit(EXIT_FAILURE);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}
