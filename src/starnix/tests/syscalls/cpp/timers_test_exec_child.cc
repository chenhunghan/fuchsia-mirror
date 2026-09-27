// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

#include <cerrno>
#include <csignal>
#include <cstdlib>
#include <ctime>

#include <gtest/gtest.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

int main(int argc, char** argv) {
  if (argc != 2) {
    return EXIT_FAILURE;
  }
  int kernel_timer_id = std::atoi(argv[1]);

  // POSIX timers created via timer_create(2) are not preserved across execve(2).
  struct itimerspec its = {};
  EXPECT_THAT(syscall(SYS_timer_gettime, kernel_timer_id, &its), SyscallFailsWithErrno(EINVAL));
  EXPECT_THAT(syscall(SYS_timer_delete, kernel_timer_id), SyscallFailsWithErrno(EINVAL));

  // Interval timers (setitimer(2)) are preserved across execve(2).
  struct itimerval itv = {};
  EXPECT_THAT(getitimer(ITIMER_REAL, &itv), SyscallSucceeds());
  EXPECT_GT(itv.it_value.tv_sec, 0);

  struct itimerval zero_itv = {};
  EXPECT_THAT(setitimer(ITIMER_REAL, &zero_itv, nullptr), SyscallSucceeds());

  // Verify that new POSIX timers can be created after execve(2). Linux is observed to not reset
  // the per-process timer ID counter across execve(2), so the new timer receives the next
  // sequential ID after the pre-exec timer.
  int new_timer_id = -1;
  struct sigevent sev = {};
  sev.sigev_notify = SIGEV_NONE;
  EXPECT_THAT(syscall(SYS_timer_create, CLOCK_MONOTONIC, &sev, &new_timer_id), SyscallSucceeds());
  EXPECT_EQ(new_timer_id, kernel_timer_id + 1);
  EXPECT_THAT(syscall(SYS_timer_delete, new_timer_id), SyscallSucceeds());

  return ::testing::Test::HasFailure() ? EXIT_FAILURE : EXIT_SUCCESS;
}
