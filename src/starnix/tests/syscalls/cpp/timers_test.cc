// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <poll.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

#include <atomic>
#include <cerrno>
#include <chrono>
#include <climits>
#include <csignal>
#include <cstdlib>
#include <ctime>
#include <initializer_list>
#include <string>
#include <utility>

#include <fbl/unique_fd.h>
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

namespace {

using namespace std::chrono_literals;

struct timespec ToTimespec(std::chrono::nanoseconds duration) {
  auto secs = std::chrono::duration_cast<std::chrono::seconds>(duration);
  auto nsecs = duration - secs;
  return {.tv_sec = static_cast<time_t>(secs.count()), .tv_nsec = static_cast<long>(nsecs.count())};
}

sigset_t MakeSignalSet(std::initializer_list<int> signos) {
  sigset_t mask;
  sigemptyset(&mask);
  for (int signo : signos) {
    sigaddset(&mask, signo);
  }
  return mask;
}

class ScopedSignalMask {
 public:
  explicit ScopedSignalMask(sigset_t mask, int how = SIG_BLOCK) : mask_(mask), how_(how) {
    EXPECT_THAT(sigprocmask(how_, &mask_, &old_mask_), SyscallSucceeds());
  }
  explicit ScopedSignalMask(int signo, int how = SIG_BLOCK)
      : ScopedSignalMask(MakeSignalSet({signo}), how) {}
  ScopedSignalMask(const ScopedSignalMask&) = delete;
  ScopedSignalMask& operator=(const ScopedSignalMask&) = delete;
  ~ScopedSignalMask() {
    if (how_ == SIG_BLOCK) {
      struct timespec zero = {};
      while (sigtimedwait(&mask_, nullptr, &zero) > 0) {
      }
    }
    sigprocmask(SIG_SETMASK, &old_mask_, nullptr);
  }

  const sigset_t& mask() const { return mask_; }

 private:
  sigset_t mask_ = {};
  sigset_t old_mask_ = {};
  int how_ = SIG_BLOCK;
};

class ScopedTimer {
 public:
  ScopedTimer() = default;
  ScopedTimer(const ScopedTimer&) = delete;
  ScopedTimer& operator=(const ScopedTimer&) = delete;
  ~ScopedTimer() { Reset(); }

  void Init(int signo = SIGRTMIN, int notify = SIGEV_SIGNAL, clockid_t clock_id = CLOCK_MONOTONIC) {
    Reset();
    struct sigevent sev = {};
    sev.sigev_notify = notify;
    sev.sigev_signo = signo;
    if (notify == SIGEV_THREAD_ID) {
      sev._sigev_un._tid = static_cast<pid_t>(syscall(SYS_gettid));
    }
    ASSERT_THAT(timer_create(clock_id, &sev, &timer_id_), SyscallSucceeds());
    owner_pid_ = getpid();
    active_ = true;
  }

  void Arm(std::chrono::nanoseconds value, std::chrono::nanoseconds interval = 0ns,
           int flags = 0) const {
    struct itimerspec its = {
        .it_interval = ToTimespec(interval),
        .it_value = ToTimespec(value),
    };
    ASSERT_THAT(timer_settime(timer_id_, flags, &its, nullptr), SyscallSucceeds());
  }

  void Disarm() const { Arm(0ns, 0ns); }

  void Delete() {
    if (std::exchange(active_, false) && owner_pid_ == getpid()) {
      ASSERT_THAT(timer_delete(timer_id_), SyscallSucceeds());
    }
  }

  timer_t get() const { return timer_id_; }

 private:
  void Reset() {
    if (std::exchange(active_, false) && owner_pid_ == getpid()) {
      EXPECT_THAT(timer_delete(timer_id_), SyscallSucceeds());
    }
  }

  timer_t timer_id_ = {};
  pid_t owner_pid_ = 0;
  bool active_ = false;
};

void WaitForSignalfdPollin(int sfd) {
  struct pollfd pfd = {.fd = sfd, .events = POLLIN};
  int ret;
  do {
    ret = poll(&pfd, 1, 5000);
  } while (ret < 0 && errno == EINTR);
  ASSERT_EQ(ret, 1) << strerror(errno);
  ASSERT_TRUE((pfd.revents & POLLIN) != 0);
}

void WaitForPendingSignal(int signo = SIGRTMIN) {
  sigset_t mask = MakeSignalSet({signo});
  fbl::unique_fd sfd(signalfd(-1, &mask, SFD_NONBLOCK | SFD_CLOEXEC));
  ASSERT_TRUE(sfd.is_valid()) << strerror(errno);
  WaitForSignalfdPollin(sfd.get());
}

void ExpectNoPendingSignal(const sigset_t& mask) {
  siginfo_t info = {};
  struct timespec zero_timeout = {};
  EXPECT_THAT(sigtimedwait(&mask, &info, &zero_timeout), SyscallFailsWithErrno(EAGAIN));
}

void Sleep(std::chrono::nanoseconds duration) {
  struct timespec ts = ToTimespec(duration);
  ASSERT_THAT(nanosleep(&ts, nullptr), SyscallSucceeds());
}

class ScopedSigaction {
 public:
  ScopedSigaction() = default;
  ScopedSigaction(const ScopedSigaction&) = delete;
  ScopedSigaction& operator=(const ScopedSigaction&) = delete;
  ~ScopedSigaction() {
    if (active_) {
      sigaction(signo_, &old_sa_, nullptr);
    }
  }

  void Install(int signo, void (*action)(int, siginfo_t*, void*)) {
    struct sigaction sa = {};
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = SA_SIGINFO;
    sa.sa_sigaction = action;
    signo_ = signo;
    ASSERT_THAT(sigaction(signo_, &sa, &old_sa_), SyscallSucceeds());
    active_ = true;
  }

 private:
  int signo_ = 0;
  struct sigaction old_sa_ = {};
  bool active_ = false;
};

class PosixTimerTest : public ::testing::Test {
 protected:
  void SetUp() override { ASSERT_NO_FATAL_FAILURE(timer_.Init()); }

  void ExpectNoPendingSignal() const { ::ExpectNoPendingSignal(rt_mask_.mask()); }

  void DequeueAndVerifyOverruns(int* out_overruns, int signo = SIGRTMIN) const {
    sigset_t set = MakeSignalSet({signo});
    siginfo_t info = {};
    struct timespec timeout = ToTimespec(5s);
    ASSERT_THAT(sigtimedwait(&set, &info, &timeout), SyscallSucceedsWithValue(signo));
    EXPECT_EQ(info.si_code, SI_TIMER);
    EXPECT_THAT(timer_getoverrun(timer_.get()), SyscallSucceedsWithValue(info.si_overrun));
    *out_overruns = info.si_overrun;
  }

  ScopedTimer timer_;

 private:
  ScopedSignalMask rt_mask_{SIGRTMIN};
};

enum class TimerDeliveryMode { kSigwaitinfo, kSignalfd, kSignalHandler };

struct TimerOverrunTestCase {
  TimerDeliveryMode delivery;
  int notify;
  int signo;
  const char* name;
};

class PosixTimerOverrunTest : public PosixTimerTest,
                              public ::testing::WithParamInterface<TimerOverrunTestCase> {};

}  // namespace

TEST_P(PosixTimerOverrunTest, AccountsOverrunsWithoutDuplication) {
  const TimerOverrunTestCase& test_case = GetParam();
  ScopedSignalMask sig_mask(test_case.signo);

  fbl::unique_fd sfd;
  if (test_case.delivery == TimerDeliveryMode::kSignalfd) {
    sfd.reset(signalfd(-1, &sig_mask.mask(), SFD_NONBLOCK | SFD_CLOEXEC));
    ASSERT_TRUE(sfd.is_valid()) << strerror(errno);
  }

  static std::atomic<int> handler_calls{0};
  static std::atomic<int> handler_si_overrun{-1};
  static std::atomic<int> handler_getoverrun{-1};
  static std::atomic<timer_t> active_timer{};
  static_assert(std::atomic<timer_t>::is_always_lock_free);

  ASSERT_NO_FATAL_FAILURE(timer_.Init(test_case.signo, test_case.notify));

  ScopedSigaction sigaction_guard;
  if (test_case.delivery == TimerDeliveryMode::kSignalHandler) {
    handler_calls.store(0, std::memory_order_relaxed);
    handler_si_overrun.store(-1, std::memory_order_relaxed);
    handler_getoverrun.store(-1, std::memory_order_relaxed);
    active_timer.store(timer_.get(), std::memory_order_release);

    ASSERT_NO_FATAL_FAILURE(
        sigaction_guard.Install(test_case.signo, [](int, siginfo_t* info, void*) {
          handler_si_overrun.store(info->si_overrun, std::memory_order_relaxed);
          handler_getoverrun.store(timer_getoverrun(active_timer.load(std::memory_order_acquire)),
                                   std::memory_order_relaxed);
          handler_calls.fetch_add(1, std::memory_order_release);
        }));
  }

  auto dequeue_and_check_overruns = [&](int* out_overruns) {
    switch (test_case.delivery) {
      case TimerDeliveryMode::kSigwaitinfo: {
        ASSERT_NO_FATAL_FAILURE(DequeueAndVerifyOverruns(out_overruns, test_case.signo));
        break;
      }
      case TimerDeliveryMode::kSignalfd: {
        struct signalfd_siginfo ssi = {};
        ASSERT_THAT(read(sfd.get(), &ssi, sizeof(ssi)), SyscallSucceedsWithValue(sizeof(ssi)));
        EXPECT_EQ(ssi.ssi_signo, static_cast<uint32_t>(test_case.signo));
        EXPECT_EQ(ssi.ssi_code, static_cast<int32_t>(SI_TIMER));
        *out_overruns = static_cast<int>(ssi.ssi_overrun);
        EXPECT_THAT(timer_getoverrun(timer_.get()), SyscallSucceedsWithValue(*out_overruns));
        break;
      }
      case TimerDeliveryMode::kSignalHandler: {
        int prev_calls = handler_calls.load(std::memory_order_relaxed);
        sigset_t empty_mask;
        sigemptyset(&empty_mask);
        ASSERT_THAT(sigsuspend(&empty_mask), SyscallFailsWithErrno(EINTR));
        ASSERT_EQ(handler_calls.load(std::memory_order_acquire), prev_calls + 1);
        *out_overruns = handler_si_overrun.load(std::memory_order_relaxed);
        EXPECT_EQ(handler_getoverrun.load(std::memory_order_relaxed), *out_overruns);
        break;
      }
    }
    EXPECT_GE(*out_overruns, 1);
  };

  timer_.Arm(5ms, 5ms);
  ASSERT_NO_FATAL_FAILURE(WaitForPendingSignal(test_case.signo));
  Sleep(50ms);

  // Before a signal is delivered or accepted, timer_getoverrun reports 0.
  EXPECT_THAT(timer_getoverrun(timer_.get()), SyscallSucceedsWithValue(0));

  int first_overruns = -1;
  ASSERT_NO_FATAL_FAILURE(dequeue_and_check_overruns(&first_overruns));

  // Wait for the next expiration and accumulate overruns on it. Subsequent expirations for the
  // next pending signal must not clobber the latched overrun count of the last delivered signal
  // until that next signal is delivered.
  ASSERT_NO_FATAL_FAILURE(WaitForPendingSignal(test_case.signo));
  Sleep(30ms);
  EXPECT_THAT(timer_getoverrun(timer_.get()), SyscallSucceedsWithValue(first_overruns));

  // Dequeue the second pending signal and verify that it carries the newly accumulated overruns
  // (rather than a stale zero-overrun duplicate from the first batch).
  int second_overruns = -1;
  ASSERT_NO_FATAL_FAILURE(dequeue_and_check_overruns(&second_overruns));

  // Disarming resets the overrun count, and no duplicate signals from the elapsed intervals
  // remain queued.
  timer_.Disarm();
  EXPECT_THAT(timer_getoverrun(timer_.get()), SyscallSucceedsWithValue(0));
  if (test_helper::IsStarnix() || test_helper::IsKernelVersionAtLeast(6, 12)) {
    ::ExpectNoPendingSignal(sig_mask.mask());
  }
}

INSTANTIATE_TEST_SUITE_P(
    PosixTimerTests, PosixTimerOverrunTest,
    ::testing::Values(TimerOverrunTestCase{TimerDeliveryMode::kSigwaitinfo, SIGEV_SIGNAL, SIGRTMIN,
                                           "SigwaitinfoSignal"},
                      TimerOverrunTestCase{TimerDeliveryMode::kSigwaitinfo, SIGEV_THREAD_ID,
                                           SIGRTMIN, "SigwaitinfoThreadId"},
                      TimerOverrunTestCase{TimerDeliveryMode::kSignalfd, SIGEV_SIGNAL, SIGRTMIN,
                                           "SignalfdSignal"},
                      // Use a standard signal (<= 32) so arch32 rt_sigreturn preserves the signal
                      // mask across sigsuspend (https://fxbug.dev/486225910); this also covers
                      // periodic overrun latching for standard signals.
                      TimerOverrunTestCase{TimerDeliveryMode::kSignalHandler, SIGEV_SIGNAL, SIGUSR1,
                                           "SignalHandlerSignal"}),
    [](const ::testing::TestParamInfo<TimerOverrunTestCase>& info) { return info.param.name; });

class PosixTimerCancelTest : public PosixTimerTest, public ::testing::WithParamInterface<bool> {};

TEST_P(PosixTimerCancelTest, DiscardsPendingSignal) {
  if (!test_helper::IsStarnix() && !test_helper::IsKernelVersionAtLeast(6, 12)) {
    GTEST_SKIP()
        << "Dropping pending POSIX timer signals on disarm or delete requires Linux >= 6.12";
  }

  timer_.Arm(10ms, 10ms);
  ASSERT_NO_FATAL_FAILURE(WaitForPendingSignal());

  if (GetParam()) {
    timer_.Delete();
  } else {
    timer_.Disarm();
  }

  sigset_t pending = {};
  ASSERT_THAT(sigpending(&pending), SyscallSucceeds());
  EXPECT_EQ(sigismember(&pending, SIGRTMIN), 0);
  ExpectNoPendingSignal();
}

INSTANTIATE_TEST_SUITE_P(PosixTimerTests, PosixTimerCancelTest, ::testing::Bool(),
                         [](const ::testing::TestParamInfo<bool>& info) {
                           return info.param ? "Delete" : "Disarm";
                         });

TEST_F(PosixTimerTest, OverrunSaturatesAtIntMax) {
  ASSERT_NO_FATAL_FAILURE(timer_.Init(SIGRTMIN, SIGEV_SIGNAL, CLOCK_REALTIME));

  // Arm with an absolute deadline near the 1970 epoch and a 1us interval so that the elapsed
  // interval count far exceeds INT_MAX (regression test for CVE-2018-12896 / b/582006129).
  timer_.Arm(1s, 1us, TIMER_ABSTIME);
  ASSERT_NO_FATAL_FAILURE(WaitForPendingSignal());

  int overruns = -1;
  ASSERT_NO_FATAL_FAILURE(DequeueAndVerifyOverruns(&overruns));
  timer_.Disarm();
  EXPECT_EQ(overruns, INT_MAX);
}

TEST_F(PosixTimerTest, RearmAcrossGenerationsAccountsCurrentOverruns) {
  ASSERT_NO_FATAL_FAILURE(timer_.Init(SIGRTMIN, SIGEV_SIGNAL, CLOCK_REALTIME));

  struct timespec now_ts = {};
  ASSERT_THAT(clock_gettime(CLOCK_REALTIME, &now_ts), SyscallSucceeds());
  auto now = std::chrono::seconds(now_ts.tv_sec) + std::chrono::nanoseconds(now_ts.tv_nsec);

  // Generation 1: arm in the past with a long interval so overruns accumulate immediately while
  // the next periodic tick stays seconds away (preventing a stray generation-1 signal from
  // queueing before re-arm on Linux < 6.12).
  timer_.Arm(now - 25s, 10s, TIMER_ABSTIME);
  ASSERT_NO_FATAL_FAILURE(WaitForPendingSignal());

  int overruns = -1;
  ASSERT_NO_FATAL_FAILURE(DequeueAndVerifyOverruns(&overruns));
  EXPECT_GE(overruns, 2);

  if (test_helper::IsStarnix() || test_helper::IsKernelVersionAtLeast(6, 12)) {
    // Queue a pending signal with accumulated overruns before re-arming, exercising in-place
    // replacement of a pending timer signal (Linux >= 6.12).
    timer_.Arm(now - 25s, 10s, TIMER_ABSTIME);
    ASSERT_NO_FATAL_FAILURE(WaitForPendingSignal());
  }

  // Generation 2: re-arm as a one-shot timer; prior generation overruns must be reset.
  timer_.Arm(5ms);
  EXPECT_THAT(timer_getoverrun(timer_.get()), SyscallSucceedsWithValue(0));
  ASSERT_NO_FATAL_FAILURE(WaitForPendingSignal());

  ASSERT_NO_FATAL_FAILURE(DequeueAndVerifyOverruns(&overruns));
  EXPECT_EQ(overruns, 0);
  ExpectNoPendingSignal();
}

TEST_F(PosixTimerTest, StandardSignalTimerQueuesAlongsideKill) {
  ScopedSignalMask usr1_mask(SIGUSR1);

  ASSERT_THAT(kill(getpid(), SIGUSR1), SyscallSucceeds());

  ASSERT_NO_FATAL_FAILURE(timer_.Init(SIGUSR1));
  timer_.Arm(5ms, 5ms);
  Sleep(50ms);

  siginfo_t first = {};
  struct timespec timeout = ToTimespec(5s);
  ASSERT_THAT(sigtimedwait(&usr1_mask.mask(), &first, &timeout), SyscallSucceedsWithValue(SIGUSR1));
  EXPECT_EQ(first.si_code, SI_USER);

  int overruns = -1;
  ASSERT_NO_FATAL_FAILURE(DequeueAndVerifyOverruns(&overruns, SIGUSR1));
  EXPECT_GE(overruns, 1);

  timer_.Disarm();
  if (test_helper::IsStarnix() || test_helper::IsKernelVersionAtLeast(6, 12)) {
    ::ExpectNoPendingSignal(usr1_mask.mask());
  }
}

TEST_F(PosixTimerTest, RlimitSigpendingDeduplicatesTimerOverruns) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([this]() {
    ASSERT_NO_FATAL_FAILURE(timer_.Init(SIGRTMIN, SIGEV_THREAD_ID));

    // Arm the timer and wait until its initial signal is queued before zeroing RLIMIT_SIGPENDING.
    timer_.Arm(5ms, 5ms);
    ASSERT_NO_FATAL_FAILURE(WaitForPendingSignal());

    struct rlimit rl = {.rlim_cur = 0, .rlim_max = 0};
    ASSERT_THAT(setrlimit(RLIMIT_SIGPENDING, &rl), SyscallSucceeds());

    // Let additional expirations accumulate while RLIMIT_SIGPENDING is saturated.
    Sleep(50ms);

    int overruns = -1;
    ASSERT_NO_FATAL_FAILURE(DequeueAndVerifyOverruns(&overruns));
    timer_.Disarm();
    EXPECT_GE(overruns, 1);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST_F(PosixTimerTest, IgnoredSignalDoesNotAccumulateOverruns) {
  ScopedSignalMask unblocked_chld(SIGCHLD, SIG_UNBLOCK);

  ASSERT_NO_FATAL_FAILURE(timer_.Init(SIGCHLD, SIGEV_SIGNAL, CLOCK_REALTIME));

  struct timespec now_ts = {};
  ASSERT_THAT(clock_gettime(CLOCK_REALTIME, &now_ts), SyscallSucceeds());
  auto now = std::chrono::seconds(now_ts.tv_sec) + std::chrono::nanoseconds(now_ts.tv_nsec);

  timer_.Arm(now - 25s, 100ms, TIMER_ABSTIME);
  Sleep(20ms);
  EXPECT_THAT(timer_getoverrun(timer_.get()), SyscallSucceedsWithValue(0));

  sigset_t pending = {};
  ASSERT_THAT(sigpending(&pending), SyscallSucceeds());
  EXPECT_EQ(sigismember(&pending, SIGCHLD), 0);

  if (test_helper::IsStarnix()) {
    // Block SIGCHLD while the periodic timer remains armed; prior ignored expirations must not
    // contribute to the overrun count of the next queued signal.
    ScopedSignalMask blocked_chld(SIGCHLD, SIG_BLOCK);

    int overruns = -1;
    ASSERT_NO_FATAL_FAILURE(DequeueAndVerifyOverruns(&overruns, SIGCHLD));
    timer_.Disarm();
    EXPECT_EQ(overruns, 0);
  } else if (test_helper::IsKernelVersionAtLeast(6, 12)) {
    // Linux >= 6.12 only re-arms periodic POSIX timers when a queued signal is dequeued, so once
    // an expiration is dropped as ignored the timer does not fire again until re-armed.
    ScopedSignalMask blocked_chld(SIGCHLD, SIG_BLOCK);
    siginfo_t info = {};
    struct timespec timeout = ToTimespec(150ms);
    EXPECT_THAT(sigtimedwait(&blocked_chld.mask(), &info, &timeout), SyscallFailsWithErrno(EAGAIN));
    timer_.Disarm();
  }
}
