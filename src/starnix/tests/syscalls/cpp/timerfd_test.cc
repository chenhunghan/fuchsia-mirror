// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <stdint.h>
#include <sys/poll.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

#include <chrono>
#include <ctime>
#include <thread>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/capability.h>

#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {
constexpr int64_t kNanosPerSec = 1'000'000'000;
}  // namespace

void run_timerfd_test(clockid_t clock_id) {
  timespec begin = {};
  ASSERT_EQ(0, clock_gettime(CLOCK_REALTIME, &begin));

  int fd = timerfd_create(clock_id, 0);
  ASSERT_NE(-1, fd) << strerror(errno);

  // Test timer 1 second in the future.
  struct itimerspec its = {};
  its.it_value = begin;
  its.it_value.tv_sec += 1;
  EXPECT_EQ(0, timerfd_settime(fd, TFD_TIMER_ABSTIME, &its, nullptr)) << strerror(errno);

  // Test polling on the timer expiration signal.
  pollfd pfd = {.fd = fd, .events = POLLIN};
  ASSERT_EQ(poll(&pfd, 1, -1), 1) << strerror(errno);
  EXPECT_EQ(pfd.revents, POLLIN);

  uint64_t val = 0;
  EXPECT_EQ(ssize_t(sizeof(val)), read(fd, &val, sizeof(val))) << strerror(errno);
  EXPECT_EQ(1u, val);  // Timer went off one time.

  // Elapsed time should be at least one second in the future.
  timespec end = {};
  ASSERT_EQ(0, clock_gettime(CLOCK_REALTIME, &end));
  EXPECT_LT(begin.tv_sec, end.tv_sec);

  // Set the timer in the past.
  its.it_value.tv_sec = begin.tv_sec - 10;
  EXPECT_EQ(0, timerfd_settime(fd, TFD_TIMER_ABSTIME, &its, nullptr));

  // Should be called immediately. That's hard to check in a non-flaky way so this just checks that
  // it went off at all.
  val = 0;
  EXPECT_EQ(ssize_t(sizeof(val)), read(fd, &val, sizeof(val))) << errno;
  EXPECT_EQ(1u, val);  // Timer went off one time.

  // Update the time should clear the previous signal.
  ASSERT_EQ(0, clock_gettime(CLOCK_REALTIME, &begin));
  its.it_value = begin;
  its.it_value.tv_sec += 1;
  EXPECT_EQ(0, timerfd_settime(fd, TFD_TIMER_ABSTIME, &its, nullptr));
  std::this_thread::sleep_for(std::chrono::seconds(2));
  its.it_value.tv_sec += 2;
  EXPECT_EQ(0, timerfd_settime(fd, TFD_TIMER_ABSTIME, &its, nullptr));
  ASSERT_EQ(poll(&pfd, 1, 0), 0);
  ASSERT_EQ(poll(&pfd, 1, -1), 1);

  // Stop the timer should clear the previous signal.
  ASSERT_EQ(0, clock_gettime(CLOCK_REALTIME, &begin));
  its.it_value = begin;
  its.it_value.tv_sec += 1;
  EXPECT_EQ(0, timerfd_settime(fd, TFD_TIMER_ABSTIME, &its, nullptr));
  std::this_thread::sleep_for(std::chrono::seconds(2));
  struct itimerspec itempty = {};
  EXPECT_EQ(0, timerfd_settime(fd, TFD_TIMER_ABSTIME, &itempty, nullptr));
  ASSERT_EQ(poll(&pfd, 1, 0), 0);

  close(fd);
}

TEST(TimerFD, RealtimeAbsolute) { run_timerfd_test(CLOCK_REALTIME); }

TEST(TimerFD, RealtimeAlarm) {
  // If the test process does not have the capability, the timerfd_create call should fail.
  if (!test_helper::HasCapability(CAP_WAKE_ALARM)) {
    GTEST_SKIP()
        << "The CAP_WAKE_ALARM capability is required to create a CLOCK_REALTIME_ALARM timer.";
  }
  run_timerfd_test(CLOCK_REALTIME_ALARM);
}

TEST(TimerFD, NoWakeAlarmCap) {
  // If the test process does not have the capability, the timerfd_create call should fail.
  test_helper::UnsetCapability(CAP_WAKE_ALARM);
  int fd = timerfd_create(CLOCK_REALTIME_ALARM, 0);
  ASSERT_EQ(-1, fd) << errno;
}

TEST(TimerFD, DeleteTimerThatDoesNotExist) { ASSERT_EQ(syscall(__NR_timer_delete, 0), -1); }

TEST(TimerFD, NoWakeAlarmCapSetTime) {
  if (!test_helper::HasCapability(CAP_WAKE_ALARM)) {
    GTEST_SKIP() << "The CAP_WAKE_ALARM capability is required to run this test.";
  }

  // Create the alarm timer while we have the capability.
  int fd = timerfd_create(CLOCK_REALTIME_ALARM, 0);
  ASSERT_NE(-1, fd) << strerror(errno);

  // Run the rest in a forked process where we drop the capability.
  test_helper::ForkHelper fork_helper;
  fork_helper.RunInForkedProcess([&]() {
    test_helper::UnsetCapability(CAP_WAKE_ALARM);

    struct itimerspec its = {};
    its.it_value.tv_sec = 1;
    int ret = timerfd_settime(fd, 0, &its, nullptr);
    EXPECT_EQ(-1, ret);
    EXPECT_EQ(EPERM, errno);
  });

  close(fd);
}

TEST(TimerFD, SetTimeOldValueUnarmed) {
  fbl::unique_fd fd(timerfd_create(CLOCK_MONOTONIC, 0));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);

  struct itimerspec new_val = {};
  new_val.it_value.tv_sec = 5;
  struct itimerspec old_val = {};
  // Pre-fill with sentinel values to ensure timerfd_settime writes over them.
  old_val.it_value.tv_sec = -1;
  old_val.it_value.tv_nsec = -1;
  old_val.it_interval.tv_sec = -1;
  old_val.it_interval.tv_nsec = -1;

  SAFE_SYSCALL(timerfd_settime(fd.get(), 0, &new_val, &old_val));
  EXPECT_EQ(0, old_val.it_value.tv_sec);
  EXPECT_EQ(0, old_val.it_value.tv_nsec);
  EXPECT_EQ(0, old_val.it_interval.tv_sec);
  EXPECT_EQ(0, old_val.it_interval.tv_nsec);
}

TEST(TimerFD, SetTimeOldValueRelative) {
  fbl::unique_fd fd(timerfd_create(CLOCK_MONOTONIC, 0));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);

  struct timespec before = {};
  SAFE_SYSCALL(clock_gettime(CLOCK_MONOTONIC, &before));

  struct itimerspec new_val = {};
  new_val.it_value.tv_sec = 60;
  new_val.it_interval.tv_sec = 2;
  SAFE_SYSCALL(timerfd_settime(fd.get(), 0, &new_val, nullptr));

  struct itimerspec disarm_val = {};
  struct itimerspec old_val = {};
  SAFE_SYSCALL(timerfd_settime(fd.get(), 0, &disarm_val, &old_val));

  struct timespec after = {};
  SAFE_SYSCALL(clock_gettime(CLOCK_MONOTONIC, &after));

  EXPECT_EQ(2, old_val.it_interval.tv_sec);
  EXPECT_EQ(0, old_val.it_interval.tv_nsec);
  // Remaining time should be a normalized positive relative duration <= 60s.
  EXPECT_GE(old_val.it_value.tv_sec, 0);
  EXPECT_GE(old_val.it_value.tv_nsec, 0);
  EXPECT_LT(old_val.it_value.tv_nsec, kNanosPerSec);
  int64_t old_ns = old_val.it_value.tv_sec * kNanosPerSec + old_val.it_value.tv_nsec;
  int64_t elapsed_ns =
      (after.tv_sec - before.tv_sec) * kNanosPerSec + (after.tv_nsec - before.tv_nsec);
  EXPECT_GT(old_ns, 0);
  EXPECT_LE(old_ns, 60 * kNanosPerSec);
  if (elapsed_ns < 50 * kNanosPerSec) {
    EXPECT_GE(old_ns, 60 * kNanosPerSec - elapsed_ns - 10'000'000);
  }
}

TEST(TimerFD, SetTimeOldValueAbsolute) {
  fbl::unique_fd fd(timerfd_create(CLOCK_REALTIME, 0));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);

  struct timespec now = {};
  SAFE_SYSCALL(clock_gettime(CLOCK_REALTIME, &now));

  // Set an absolute timer 60 seconds in the future.
  struct itimerspec new_val = {};
  new_val.it_value.tv_sec = now.tv_sec + 60;
  new_val.it_value.tv_nsec = now.tv_nsec;
  new_val.it_interval.tv_sec = 5;
  SAFE_SYSCALL(timerfd_settime(fd.get(), TFD_TIMER_ABSTIME, &new_val, nullptr));

  // Read old_value while resetting or disarming.
  // Per man 2 timerfd_settime / timerfd_gettime:
  // "The it_value field returns the amount of time until the timer will next expire...
  //  This field always contains a relative value, regardless of whether the
  //  TFD_TIMER_ABSTIME flag was specified when setting the timer."
  struct itimerspec disarm_val = {};
  struct itimerspec old_val = {};
  SAFE_SYSCALL(timerfd_settime(fd.get(), 0, &disarm_val, &old_val));

  EXPECT_EQ(5, old_val.it_interval.tv_sec);
  EXPECT_EQ(0, old_val.it_interval.tv_nsec);
  // old_val.it_value MUST be relative (<= 60 seconds), NOT the absolute epoch (~1.7 billion).
  EXPECT_GE(old_val.it_value.tv_sec, 0);
  EXPECT_GE(old_val.it_value.tv_nsec, 0);
  EXPECT_LT(old_val.it_value.tv_nsec, kNanosPerSec);
  int64_t old_ns = old_val.it_value.tv_sec * kNanosPerSec + old_val.it_value.tv_nsec;
  EXPECT_GT(old_ns, 0);
  EXPECT_LE(old_ns, 60 * kNanosPerSec);
}

TEST(TimerFD, SetTimeOldValueExpiredSingleShot) {
  fbl::unique_fd fd(timerfd_create(CLOCK_MONOTONIC, 0));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);

  // Set a single-shot timer for 20ms.
  struct itimerspec new_val = {};
  new_val.it_value.tv_nsec = 20'000'000;
  ASSERT_EQ(0, SAFE_SYSCALL(timerfd_settime(fd.get(), 0, &new_val, nullptr)));

  // Wait for the timer to expire.
  pollfd pfd = {.fd = fd.get(), .events = POLLIN};
  ASSERT_EQ(1, SAFE_SYSCALL(poll(&pfd, 1, -1)));
  EXPECT_EQ(POLLIN, pfd.revents);

  // Disarm and query old_value.
  // Since the timer was single-shot and has expired, it_value should be {0, 0}.
  struct itimerspec disarm_val = {};
  struct itimerspec old_val = {};
  SAFE_SYSCALL(timerfd_settime(fd.get(), 0, &disarm_val, &old_val));

  EXPECT_EQ(0, old_val.it_value.tv_sec);
  EXPECT_EQ(0, old_val.it_value.tv_nsec);
  EXPECT_EQ(0, old_val.it_interval.tv_sec);
  EXPECT_EQ(0, old_val.it_interval.tv_nsec);
}

TEST(TimerFD, SetTimeOldValueExpiredPeriodic) {
  fbl::unique_fd fd(timerfd_create(CLOCK_MONOTONIC, 0));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);

  // Set a periodic timer: initial expiration 20ms, interval 500ms.
  struct itimerspec new_val = {};
  new_val.it_value.tv_nsec = 20'000'000;
  new_val.it_interval.tv_nsec = 500'000'000;
  ASSERT_EQ(0, SAFE_SYSCALL(timerfd_settime(fd.get(), 0, &new_val, nullptr)));

  // Wait for the first expiration.
  pollfd pfd = {.fd = fd.get(), .events = POLLIN};
  ASSERT_EQ(1, SAFE_SYSCALL(poll(&pfd, 1, -1)));
  EXPECT_EQ(POLLIN, pfd.revents);

  // Both timerfd_gettime and timerfd_settime old_value must return the positive time
  // remaining until the NEXT expiration (bounded by the 500ms interval).
  struct itimerspec get_val = {};
  SAFE_SYSCALL(timerfd_gettime(fd.get(), &get_val));
  EXPECT_EQ(0, get_val.it_interval.tv_sec);
  EXPECT_EQ(500'000'000, get_val.it_interval.tv_nsec);
  EXPECT_EQ(0, get_val.it_value.tv_sec);
  EXPECT_GT(get_val.it_value.tv_nsec, 0);
  EXPECT_LE(get_val.it_value.tv_nsec, 500'000'000);

  struct itimerspec disarm_val = {};
  struct itimerspec old_val = {};
  SAFE_SYSCALL(timerfd_settime(fd.get(), 0, &disarm_val, &old_val));

  EXPECT_EQ(0, old_val.it_interval.tv_sec);
  EXPECT_EQ(500'000'000, old_val.it_interval.tv_nsec);
  EXPECT_EQ(0, old_val.it_value.tv_sec);
  EXPECT_GT(old_val.it_value.tv_nsec, 0);
  EXPECT_LE(old_val.it_value.tv_nsec, 500'000'000);
}

TEST(TimerFD, SetTimeOldValueMatchesGetTime) {
  fbl::unique_fd fd(timerfd_create(CLOCK_MONOTONIC, 0));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);

  struct timespec before = {};
  SAFE_SYSCALL(clock_gettime(CLOCK_MONOTONIC, &before));

  struct itimerspec its = {};
  its.it_value.tv_sec = 60;
  its.it_interval.tv_sec = 3;
  SAFE_SYSCALL(timerfd_settime(fd.get(), 0, &its, nullptr));

  struct itimerspec get_val = {};
  SAFE_SYSCALL(timerfd_gettime(fd.get(), &get_val));

  struct itimerspec disarm_val = {};
  struct itimerspec set_old_val = {};
  SAFE_SYSCALL(timerfd_settime(fd.get(), 0, &disarm_val, &set_old_val));

  struct timespec after = {};
  SAFE_SYSCALL(clock_gettime(CLOCK_MONOTONIC, &after));

  EXPECT_EQ(get_val.it_interval.tv_sec, set_old_val.it_interval.tv_sec);
  EXPECT_EQ(get_val.it_interval.tv_nsec, set_old_val.it_interval.tv_nsec);

  EXPECT_GE(get_val.it_value.tv_sec, 0);
  EXPECT_GE(get_val.it_value.tv_nsec, 0);
  EXPECT_LT(get_val.it_value.tv_nsec, kNanosPerSec);
  EXPECT_GE(set_old_val.it_value.tv_sec, 0);
  EXPECT_GE(set_old_val.it_value.tv_nsec, 0);
  EXPECT_LT(set_old_val.it_value.tv_nsec, kNanosPerSec);

  int64_t get_ns = get_val.it_value.tv_sec * kNanosPerSec + get_val.it_value.tv_nsec;
  int64_t set_old_ns = set_old_val.it_value.tv_sec * kNanosPerSec + set_old_val.it_value.tv_nsec;
  int64_t elapsed_ns =
      (after.tv_sec - before.tv_sec) * kNanosPerSec + (after.tv_nsec - before.tv_nsec);
  EXPECT_GT(set_old_ns, 0);
  EXPECT_LE(set_old_ns, get_ns);
  EXPECT_LE(get_ns, 60 * kNanosPerSec);
  if (elapsed_ns < 50 * kNanosPerSec) {
    EXPECT_GE(set_old_ns, 60 * kNanosPerSec - elapsed_ns - 10'000'000);
  }
}
