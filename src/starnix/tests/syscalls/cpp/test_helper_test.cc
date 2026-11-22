// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/starnix/tests/syscalls/cpp/test_helper.h"

#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <lib/fit/defer.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cstring>
#include <thread>
#include <utility>

#include <gmock/gmock.h>
#include <gtest/gtest-spi.h>
#include <gtest/gtest.h>
#include <linux/capability.h>

#include "src/lib/files/file.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

namespace {

TEST(TestHelperTest, DetectFailingChildren) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([] { FAIL() << "Expected failure"; });

  EXPECT_FALSE(helper.WaitForChildren());
}

TEST(ScopedTestDirTest, DoesntLeakFileDescriptors) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([] {
    struct rlimit limit;
    ASSERT_EQ(getrlimit(RLIMIT_NOFILE, &limit), 0) << "getrlimit: " << std::strerror(errno);
    auto cleanup = fit::defer([limit]() {
      EXPECT_EQ(setrlimit(RLIMIT_NOFILE, &limit), 0) << "setrlimit: " << std::strerror(errno);
    });

    struct rlimit new_limit = {.rlim_cur = 100, .rlim_max = limit.rlim_max};
    ASSERT_EQ(setrlimit(RLIMIT_NOFILE, &new_limit), 0) << "setrlimit: " << std::strerror(errno);

    for (size_t i = 0; i <= new_limit.rlim_cur; i++) {
      test_helper::ScopedTempDir scoped_dir;
      fbl::unique_fd mem_fd(test_helper::MemFdCreate("try_create", O_WRONLY));
      EXPECT_TRUE(mem_fd.is_valid()) << "memfd_create: " << std::strerror(errno);
    }
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(EventFdSemTest, WaitFromThread) {
  test_helper::EventFdSem sem(0);
  std::thread t1([&sem]() { SAFE_SYSCALL(sem.Wait()); });

  SAFE_SYSCALL(sem.Notify(1));
  t1.join();
}

TEST(EventFdSemTest, WaitFromFork) {
  test_helper::EventFdSem sem(0);
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&sem]() { SAFE_SYSCALL(sem.Wait()); });

  SAFE_SYSCALL(sem.Notify(1));
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(EventFdSemTest, CheckWaitThreaded) {
  test_helper::EventFdSem sem(0);
  std::atomic<bool> flag = false;

  std::thread t1([&sem, &flag]() {
    SAFE_SYSCALL(sem.Wait());
    flag = true;
  });

  EXPECT_EQ(flag, false);
  SAFE_SYSCALL(sem.Notify(1));
  t1.join();
  EXPECT_EQ(flag, true);
}

TEST(EventFdSemTest, NotifyFromThread) {
  test_helper::EventFdSem sem(0);
  std::thread t1([&sem]() { SAFE_SYSCALL(sem.Notify(1)); });

  t1.join();
  SAFE_SYSCALL(sem.Wait());
}

TEST(EventFdSemTest, NotifyFromFork) {
  test_helper::EventFdSem sem(0);
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&sem]() { SAFE_SYSCALL(sem.Notify(1)); });

  EXPECT_TRUE(helper.WaitForChildren());
  SAFE_SYSCALL(sem.Wait());
}

TEST(TestHelperTest, WaitForChildrenWithResults) {
  test_helper::ForkHelper helper;

  pid_t failing_child = helper.RunInForkedProcess([] { exit(1); });
  pid_t succeeding_child = helper.RunInForkedProcess([] { exit(0); });

  helper.ExpectExitValue(0);
  auto results = helper.WaitForChildrenWithResults();

  ASSERT_EQ(results.size(), 2u);

  bool found_failing = false;
  bool found_succeeding = false;
  for (const auto& result : results) {
    if (result.subprocess_id == failing_child) {
      EXPECT_FALSE(result.determined_result);
      EXPECT_EQ(result.subprocess_exit_status, 1);
      found_failing = true;
    } else if (result.subprocess_id == succeeding_child) {
      EXPECT_TRUE(result.determined_result);
      EXPECT_EQ(result.subprocess_exit_status, 0);
      found_succeeding = true;
    }
  }
  EXPECT_TRUE(found_failing);
  EXPECT_TRUE(found_succeeding);
}

TEST(TestHelperTest, WaitForChild) {
  test_helper::ForkHelper helper;

  pid_t failing_child = helper.RunInForkedProcess([] { exit(1); });
  pid_t succeeding_child = helper.RunInForkedProcess([] { exit(0); });
  pid_t custom_exit_child = helper.RunInForkedProcess([] { exit(42); });
  pid_t signaled_child = helper.RunInForkedProcess([] { raise(SIGUSR1); });

  helper.ExpectExitValue(0);
  auto failing_result = helper.WaitForChild(failing_child);
  EXPECT_FALSE(failing_result.determined_result);
  EXPECT_EQ(failing_result.subprocess_exit_status, 1);

  auto succeeding_result = helper.WaitForChild(succeeding_child);
  EXPECT_TRUE(succeeding_result.determined_result);
  EXPECT_EQ(succeeding_result.subprocess_exit_status, 0);

  auto custom_exit_result = helper.WaitForChild(custom_exit_child, /*expected_exit_status=*/42);
  EXPECT_TRUE(custom_exit_result.determined_result);
  EXPECT_EQ(custom_exit_result.subprocess_exit_status, 42);

  auto signaled_result =
      helper.WaitForChild(signaled_child, /*expected_exit_status=*/0, /*death_signum=*/SIGUSR1);
  EXPECT_TRUE(signaled_result.determined_result);
}

TEST(TestHelperTest, WaitForChildMultipleTimes) {
  test_helper::ForkHelper helper;

  pid_t child1 = helper.RunInForkedProcess([] { exit(1); });
  pid_t child2 = helper.RunInForkedProcess([] { exit(0); });
  pid_t child3 = helper.RunInForkedProcess([] { exit(2); });

  helper.ExpectExitValue(0);

  test_helper::ForkResult result1 = helper.WaitForChild(child1);
  EXPECT_FALSE(result1.determined_result);
  EXPECT_EQ(result1.subprocess_exit_status, 1);

  test_helper::ForkResult result2 = helper.WaitForChild(child2);
  EXPECT_TRUE(result2.determined_result);
  EXPECT_EQ(result2.subprocess_exit_status, 0);

  test_helper::ForkResult result3 = helper.WaitForChild(child3);
  EXPECT_FALSE(result3.determined_result);
  EXPECT_EQ(result3.subprocess_exit_status, 2);
}

TEST(TestHelperTest, WaitForChildThenWaitForChildren) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([] { exit(0); });
  pid_t second_child = helper.RunInForkedProcess([] { exit(20); });
  helper.RunInForkedProcess([] { exit(0); });

  helper.ExpectExitValue(0);

  // Wait for the second child specifically.
  test_helper::ForkResult second_result = helper.WaitForChild(second_child);
  EXPECT_FALSE(second_result.determined_result);
  EXPECT_EQ(second_result.subprocess_exit_status, 20);
  EXPECT_EQ(second_result.subprocess_id, second_child);

  // Wait for the remaining children. The children not explicitly waited for will be reaped here.
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(TestHelperTest, MultipleChildrenPassing) {
  test_helper::ForkHelper helper;

  for (int i = 0; i < 5; i++) {
    helper.RunInForkedProcess([]() { exit(0); });
  }

  helper.ExpectExitValue(0);
  auto results = helper.WaitForChildrenWithResults();
  EXPECT_EQ(results.size(), 5u);
  for (const auto& result : results) {
    EXPECT_TRUE(result.determined_result);
  }
}

TEST(TestHelperTest, MultipleChildrenFailing) {
  test_helper::ForkHelper helper;
  helper.ExpectExitValue(0);

  for (int i = 0; i < 5; i++) {
    helper.RunInForkedProcess([i]() { exit(i + 1); });
  }

  auto results = helper.WaitForChildrenWithResults();
  EXPECT_EQ(results.size(), 5u);
  for (const auto& result : results) {
    EXPECT_FALSE(result.determined_result);
  }
}

TEST(TestHelperTest, KillChildrenOnDestroyKillsRunningAndStoppedChildren) {
  pid_t paused_child = -1;
  pid_t stopped_child = -1;
  {
    test_helper::ForkHelper helper;
    helper.OnlyWaitForForkedChildren();
    helper.KillChildrenOnDestroy();

    stopped_child = helper.RunInForkedProcess([] {
      SAFE_SYSCALL(ptrace(PTRACE_TRACEME, 0, nullptr, nullptr));
      SAFE_SYSCALL(raise(SIGSTOP));
      pause();
    });
    paused_child = helper.RunInForkedProcess([] { pause(); });

    // WaitForChildren observes the ptrace stop on stopped_child, reports failure, and leaves both
    // children in child_pids_ to be killed and reaped by ~ForkHelper().
    EXPECT_FALSE(helper.WaitForChildren());
  }

  int status = 0;
  EXPECT_THAT(waitpid(paused_child, &status, WNOHANG), SyscallFailsWithErrno(ECHILD));
  EXPECT_THAT(waitpid(stopped_child, &status, WNOHANG), SyscallFailsWithErrno(ECHILD));
}

TEST(TestHelperTest, KillChildrenOnDestroyHandlesExitedAndReapedChildren) {
  pid_t reaped_child = -1;
  pid_t zombie_child = -1;
  pid_t running_child = -1;
  {
    test_helper::ForkHelper helper;
    helper.OnlyWaitForForkedChildren();
    helper.KillChildrenOnDestroy();

    reaped_child = helper.RunInForkedProcess([] { _exit(0); });
    int status = 0;
    ASSERT_EQ(HANDLE_EINTR(waitpid(reaped_child, &status, 0)), reaped_child);

    zombie_child = helper.RunInForkedProcess([] { _exit(0); });
    siginfo_t info = {};
    ASSERT_THAT(HANDLE_EINTR(waitid(P_PID, zombie_child, &info, WEXITED | WNOWAIT)),
                SyscallSucceeds());

    running_child = helper.RunInForkedProcess([] { pause(); });
  }

  int status = 0;
  EXPECT_THAT(waitpid(reaped_child, &status, WNOHANG), SyscallFailsWithErrno(ECHILD));
  EXPECT_THAT(waitpid(zombie_child, &status, WNOHANG), SyscallFailsWithErrno(ECHILD));
  EXPECT_THAT(waitpid(running_child, &status, WNOHANG), SyscallFailsWithErrno(ECHILD));
}

TEST(TestHelperTest, KillChildrenExplicitlyKillsAndReapsChildren) {
  test_helper::ForkHelper helper;
  helper.OnlyWaitForForkedChildren();

  pid_t exited_child = helper.RunInForkedProcess([] { _exit(0); });
  siginfo_t info = {};
  ASSERT_THAT(HANDLE_EINTR(waitid(P_PID, exited_child, &info, WEXITED | WNOWAIT)),
              SyscallSucceeds());

  pid_t running_child = helper.RunInForkedProcess([] { pause(); });

  EXPECT_TRUE(helper.KillChildren());

  int status = 0;
  EXPECT_THAT(waitpid(exited_child, &status, WNOHANG), SyscallFailsWithErrno(ECHILD));
  EXPECT_THAT(waitpid(running_child, &status, WNOHANG), SyscallFailsWithErrno(ECHILD));
}

TEST(SyscallResultMatchersTest, SyscallResultIsOk) {
  fit::result<int> ok_result = fit::ok();
  EXPECT_THAT(ok_result, SyscallResultIsOk());
  EXPECT_THAT(ok_result, ::testing::Not(SyscallResultIsOk("hello")));
  EXPECT_THAT(ok_result, ::testing::Not(SyscallResultIsOkWithValue("hello")));

  fit::result<int, std::string> ok_val_result = fit::ok("hello");
  EXPECT_THAT(ok_val_result, SyscallResultIsOk());
  EXPECT_THAT(ok_val_result, SyscallResultIsOk("hello"));
  EXPECT_THAT(ok_val_result, SyscallResultIsOk(::testing::StartsWith("hell")));
  using ResultType = fit::result<int, std::string>;
  EXPECT_EQ(
      "is fit::ok with value starts with \"hell\"",
      ::testing::DescribeMatcher<ResultType>(SyscallResultIsOk(::testing::StartsWith("hell"))));
  EXPECT_THAT(ok_val_result, ::testing::Not(SyscallResultIsOk("world")));
  EXPECT_THAT(ok_val_result, SyscallResultIsOkWithValue("hello"));

  fit::result<int> err_result = fit::error(ENOENT);
  EXPECT_THAT(err_result, ::testing::Not(SyscallResultIsOk()));

  fit::result<int, std::string> err_val_result = fit::error(EACCES);
  EXPECT_THAT(err_val_result, ::testing::Not(SyscallResultIsOk()));
  EXPECT_THAT(err_val_result, ::testing::Not(SyscallResultIsOk("hello")));
  EXPECT_THAT(err_val_result, ::testing::Not(SyscallResultIsOkWithValue("hello")));
}

TEST(SyscallResultMatchersTest, SyscallResultIsErrno) {
  fit::result<int> err_result = fit::error(ENOENT);
  EXPECT_THAT(err_result, SyscallResultIsErrno(ENOENT));
  EXPECT_THAT(err_result, ::testing::Not(SyscallResultIsErrno(EACCES)));

  fit::result<int, std::string> err_val_result = fit::error(EEXIST);
  EXPECT_THAT(err_val_result, SyscallResultIsErrno(EEXIST));
  EXPECT_THAT(err_val_result, ::testing::Not(SyscallResultIsErrno(ENOENT)));

  fit::result<int> ok_result = fit::ok();
  EXPECT_THAT(ok_result, ::testing::Not(SyscallResultIsErrno(ENOENT)));
}

TEST(SyscallMatchersTest, SyscallSucceedsRejectsMinusOneEvenWhenErrnoIsZero) {
  errno = 0;
  EXPECT_THAT(-1, ::testing::Not(SyscallSucceeds()));
  EXPECT_THAT(0, SyscallSucceeds());
}

TEST(SyscallMatchersTest, SyscallMatchesResultAndSucceedsOrFailsWithErrno) {
  EXPECT_THAT(0, SyscallMatchesResult(fit::ok()));
  EXPECT_THAT(42, SyscallMatchesResult(fit::ok()));
  EXPECT_THAT(intptr_t{-4096}, SyscallMatchesResult(fit::ok()));
  EXPECT_THAT(0, SyscallSucceedsOrFailsWithErrno(/* expect_success = */ true, EACCES));

  errno = EACCES;
  EXPECT_THAT(-1, ::testing::Not(SyscallMatchesResult(fit::ok())));
  EXPECT_THAT(-1,
              ::testing::Not(SyscallSucceedsOrFailsWithErrno(/* expect_success = */ true, EACCES)));
  EXPECT_THAT(-1, SyscallMatchesResult(fit::error(EACCES)));
  EXPECT_THAT(-1, SyscallSucceedsOrFailsWithErrno(/* expect_success = */ false, EACCES));

  EXPECT_THAT(0, ::testing::Not(SyscallMatchesResult(fit::error(EACCES))));
  EXPECT_THAT(
      0, ::testing::Not(SyscallSucceedsOrFailsWithErrno(/* expect_success = */ false, EACCES)));

  errno = ENOENT;
  EXPECT_THAT(-1, ::testing::Not(SyscallMatchesResult(fit::error(EACCES))));
  EXPECT_THAT(
      -1, ::testing::Not(SyscallSucceedsOrFailsWithErrno(/* expect_success = */ false, EACCES)));
}

TEST(TestHelperTest, SignalMaskHelperBlocksMultipleSignalsAndRestoresOnDestroy) {
  sigset_t initial_mask;
  ASSERT_THAT(sigprocmask(SIG_SETMASK, nullptr, &initial_mask), SyscallSucceeds());
  ASSERT_EQ(sigismember(&initial_mask, SIGUSR1), 0);
  ASSERT_EQ(sigismember(&initial_mask, SIGUSR2), 0);

  {
    test_helper::SignalMaskHelper mask_helper;
    mask_helper.blockSignal(SIGUSR1);
    mask_helper.blockSignal(SIGUSR2);

    sigset_t current_mask;
    ASSERT_THAT(sigprocmask(SIG_SETMASK, nullptr, &current_mask), SyscallSucceeds());
    EXPECT_EQ(sigismember(&current_mask, SIGUSR1), 1);
    EXPECT_EQ(sigismember(&current_mask, SIGUSR2), 1);
  }

  sigset_t restored_mask;
  ASSERT_THAT(sigprocmask(SIG_SETMASK, nullptr, &restored_mask), SyscallSucceeds());
  EXPECT_EQ(sigismember(&restored_mask, SIGUSR1), 0);
  EXPECT_EQ(sigismember(&restored_mask, SIGUSR2), 0);
}

TEST(TestHelperTest, SignalMaskHelperWaitsForSpecificPendingSignal) {
  test_helper::SignalMaskHelper mask_helper;
  mask_helper.blockSignal(SIGUSR1);
  mask_helper.blockSignal(SIGUSR2);

  ASSERT_THAT(raise(SIGUSR1), SyscallSucceeds());
  ASSERT_THAT(raise(SIGUSR2), SyscallSucceeds());

  // Wait for SIGUSR2 even though SIGUSR1 is also pending.
  ASSERT_NO_FATAL_FAILURE(mask_helper.waitForSignal(SIGUSR2));

  sigset_t pending;
  ASSERT_THAT(sigpending(&pending), SyscallSucceeds());
  EXPECT_EQ(sigismember(&pending, SIGUSR1), 1);

  EXPECT_EQ(mask_helper.timedWaitForSignal(SIGUSR1, 100), SIGUSR1);
  EXPECT_THAT(mask_helper.timedWaitForSignal(SIGUSR1, 1), SyscallFailsWithErrno(EAGAIN));
}

TEST(TestHelperTest, ScopedSocketPairLifecycleAndAccessors) {
  int closed_client_fd = -1;
  int closed_server_fd = -1;
  {
    test_helper::ScopedSocketPair pair;
    ASSERT_TRUE(pair.is_valid());
    ASSERT_TRUE(static_cast<bool>(pair));
    EXPECT_EQ(pair[0].get(), pair.client_fd());
    EXPECT_EQ(pair[1].get(), pair.server_fd());

    const char kMessage[] = "ping";
    ASSERT_EQ(write(pair.client_fd(), kMessage, sizeof(kMessage)),
              static_cast<ssize_t>(sizeof(kMessage)));
    char buf[sizeof(kMessage)] = {};
    ASSERT_EQ(read(pair.server_fd(), buf, sizeof(buf)), static_cast<ssize_t>(sizeof(buf)));
    EXPECT_STREQ(buf, kMessage);

    test_helper::ScopedSocketPair moved(std::move(pair));
    EXPECT_TRUE(moved.is_valid());
    EXPECT_FALSE(pair.is_valid());
    closed_client_fd = moved.client_fd();
    closed_server_fd = moved.server_fd();
  }
  EXPECT_THAT(fcntl(closed_client_fd, F_GETFD), SyscallFailsWithErrno(EBADF));
  EXPECT_THAT(fcntl(closed_server_fd, F_GETFD), SyscallFailsWithErrno(EBADF));

  auto dgram =
      ASSERT_RESULT_SUCCESS_AND_RETURN(test_helper::ScopedSocketPair::Create(AF_UNIX, SOCK_DGRAM));
  EXPECT_TRUE(dgram.is_valid());
  EXPECT_THAT(test_helper::ScopedSocketPair::Create(-1, SOCK_STREAM),
              SyscallResultIsErrno(EAFNOSUPPORT));
}

TEST(TestHelperTest, ScopedMMapDefaultAndSelfMove) {
  test_helper::ScopedMMap empty;
  EXPECT_FALSE(empty.is_valid());
  EXPECT_FALSE(static_cast<bool>(empty));

  const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
  auto mapped = ASSERT_RESULT_SUCCESS_AND_RETURN(test_helper::ScopedMMap::MMap(
      nullptr, 2 * page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
  EXPECT_TRUE(mapped.is_valid());

  test_helper::ScopedMMap* self = &mapped;
  mapped = std::move(*self);
  EXPECT_TRUE(mapped.is_valid());
}

TEST(CapabilitiesHelperTest, GetLastCap) {
  EXPECT_THAT(test_helper::GetLastCap(), SyscallResultIsOk(::testing::Gt(0)));
}

TEST(CapabilitiesHelperTest, RejectsInvalidCapabilityIndex) {
  EXPECT_THAT(test_helper::TrySetCapabilityEffective(-1), SyscallResultIsErrno(EINVAL));
  EXPECT_THAT(test_helper::TrySetCapabilityEffective(64), SyscallResultIsErrno(EINVAL));
  EXPECT_THAT(test_helper::TrySetCapabilityPermitted(64), SyscallResultIsErrno(EINVAL));
  EXPECT_THAT(test_helper::TryUnsetCapabilityPermitted(-1), SyscallResultIsErrno(EINVAL));
  EXPECT_THAT(test_helper::TrySetCapabilityAmbient(-1), SyscallResultIsErrno(EINVAL));

  EXPECT_NONFATAL_FAILURE(EXPECT_FALSE(test_helper::HasCapabilityEffective(-1)),
                          "Invalid capability");
  EXPECT_NONFATAL_FAILURE(EXPECT_FALSE(test_helper::HasCapabilityEffective(64)),
                          "Invalid capability");
}

TEST(CapabilitiesHelperTest, SwitchToUnprivilegedUser) {
  if (!test_helper::HasCapability(CAP_SETUID) || !test_helper::HasCapability(CAP_SETGID)) {
    GTEST_SKIP() << "Requires CAP_SETUID and CAP_SETGID";
  }

  test_helper::ForkHelper fork_helper;
  fork_helper.RunInForkedProcess([] {
    const gid_t groups[] = {1000};
    ASSERT_THAT(setgroups(1, groups), SyscallSucceeds());
    ASSERT_THAT(prctl(PR_SET_KEEPCAPS, 1UL, 0UL, 0UL, 0UL), SyscallSucceeds());
    ASSERT_TRUE(test_helper::SwitchToUnprivilegedUser(65534, 65534, /*keep_caps=*/false));

    uid_t ruid = 0, euid = 0, suid = 0;
    gid_t rgid = 0, egid = 0, sgid = 0;
    ASSERT_THAT(getresuid(&ruid, &euid, &suid), SyscallSucceeds());
    ASSERT_THAT(getresgid(&rgid, &egid, &sgid), SyscallSucceeds());
    EXPECT_EQ(ruid, 65534u);
    EXPECT_EQ(euid, 65534u);
    EXPECT_EQ(suid, 65534u);
    EXPECT_EQ(rgid, 65534u);
    EXPECT_EQ(egid, 65534u);
    EXPECT_EQ(sgid, 65534u);
    EXPECT_EQ(getgroups(0, nullptr), 0);
    EXPECT_EQ(prctl(PR_GET_KEEPCAPS, 0UL, 0UL, 0UL, 0UL), 0);
    EXPECT_FALSE(test_helper::HasCapabilityPermitted(CAP_SETUID));
    EXPECT_FALSE(test_helper::SwitchToUnprivilegedUser(0, 0));
  });

  fork_helper.RunInForkedProcess([] {
    const gid_t groups[] = {1000};
    ASSERT_THAT(setgroups(1, groups), SyscallSucceeds());
    test_helper::UnsetCapabilityEffective(CAP_SETGID);
    EXPECT_FALSE(test_helper::SwitchToUnprivilegedUser(getuid(), getgid()));
  });

  fork_helper.RunInForkedProcess([] {
    ASSERT_TRUE(test_helper::SwitchToUnprivilegedUser(65534, 65534, /*keep_caps=*/true));

    uid_t ruid = 0, euid = 0, suid = 0;
    gid_t rgid = 0, egid = 0, sgid = 0;
    ASSERT_THAT(getresuid(&ruid, &euid, &suid), SyscallSucceeds());
    ASSERT_THAT(getresgid(&rgid, &egid, &sgid), SyscallSucceeds());
    EXPECT_EQ(ruid, 65534u);
    EXPECT_EQ(euid, 65534u);
    EXPECT_EQ(suid, 65534u);
    EXPECT_EQ(rgid, 65534u);
    EXPECT_EQ(egid, 65534u);
    EXPECT_EQ(sgid, 65534u);
    EXPECT_EQ(getgroups(0, nullptr), 0);
    EXPECT_EQ(prctl(PR_GET_KEEPCAPS, 0UL, 0UL, 0UL, 0UL), 1);
    EXPECT_TRUE(test_helper::HasCapabilityPermitted(CAP_SETUID));
  });
  EXPECT_TRUE(fork_helper.WaitForChildren());
}

TEST(TestHelperTest, GetTaskState) {
  EXPECT_EQ(test_helper::GetTaskState("123 (test_binary) S 1 123 123 0 -1"), "S");
  EXPECT_EQ(test_helper::GetTaskState("456 (name (with) parens) R 1 456 456"), "R");
  EXPECT_EQ(test_helper::GetTaskState("789 (Z) T 1 789 789"), "T");
  EXPECT_EQ(test_helper::GetTaskState("invalid stat"), std::nullopt);
}

TEST(TestHelperTest, GetStatusFieldInKb) {
  test_helper::ScopedTempFD status_file;
  ASSERT_TRUE(status_file.is_valid());
  ASSERT_TRUE(files::WriteFile(status_file.name(),
                               "Name:\ttest\nVmSize:\t  1234 kB\nVmLck:\t0 kB\nBad:\tnot_kb\n"));

  EXPECT_EQ(test_helper::GetStatusFieldInKb(status_file.name(), "VmSize"), 1234u);
  EXPECT_EQ(test_helper::GetStatusFieldInKb(status_file.name(), "VmLck"), 0u);
  EXPECT_EQ(test_helper::GetStatusFieldInKb(status_file.name(), "Bad"), std::nullopt);
  EXPECT_EQ(test_helper::GetStatusFieldInKb(status_file.name(), "Missing"), std::nullopt);
  EXPECT_EQ(test_helper::GetStatusFieldInKb("/proc/nonexistent_test_helper_status", "VmSize"),
            std::nullopt);

  auto self_vmsize = test_helper::GetStatusFieldInKb("/proc/self/status", "VmSize");
  ASSERT_TRUE(self_vmsize.has_value());
  EXPECT_GT(*self_vmsize, 0u);
}

TEST(TestHelperTest, ScopedSysctlCreateRestoresAndReturnsErrno) {
  test_helper::ScopedSysctl empty;
  EXPECT_FALSE(empty.is_valid());
  EXPECT_FALSE(static_cast<bool>(empty));
  empty.Restore();
  EXPECT_FALSE(empty.is_valid());

  test_helper::ScopedTempFD temp_file;
  ASSERT_TRUE(temp_file.is_valid());
  ASSERT_TRUE(files::WriteFile(temp_file.name(), "initial"));

  {
    auto sysctl = test_helper::ScopedSysctl::Create(temp_file.name(), "updated");
    ASSERT_THAT(sysctl, SyscallResultIsOk());
    EXPECT_TRUE(sysctl.value().is_valid());
    EXPECT_TRUE(static_cast<bool>(sysctl.value()));
    std::string content;
    ASSERT_TRUE(files::ReadFileToString(temp_file.name(), &content));
    EXPECT_EQ(content, "updated");
  }

  std::string restored;
  ASSERT_TRUE(files::ReadFileToString(temp_file.name(), &restored));
  EXPECT_EQ(restored, "initial");

  {
    auto save_only = test_helper::ScopedSysctl::Create(temp_file.name());
    ASSERT_THAT(save_only, SyscallResultIsOk());
    EXPECT_TRUE(save_only.value().is_valid());
    ASSERT_TRUE(files::WriteFile(temp_file.name(), "modified_later"));
  }

  ASSERT_TRUE(files::ReadFileToString(temp_file.name(), &restored));
  EXPECT_EQ(restored, "initial");

  test_helper::ScopedTempFD second_file;
  ASSERT_TRUE(second_file.is_valid());
  ASSERT_TRUE(files::WriteFile(second_file.name(), "second_initial"));
  {
    auto first = test_helper::ScopedSysctl::Create(temp_file.name(), "first_updated");
    ASSERT_THAT(first, SyscallResultIsOk());
    test_helper::ScopedSysctl moved(std::move(first.value()));
    EXPECT_FALSE(first.value().is_valid());
    EXPECT_TRUE(moved.is_valid());
    ASSERT_TRUE(files::WriteFile(temp_file.name(), "clobbered_before_restore"));

    auto second = test_helper::ScopedSysctl::Create(second_file.name(), "second_updated");
    ASSERT_THAT(second, SyscallResultIsOk());
    moved = std::move(second.value());
    EXPECT_FALSE(second.value().is_valid());
    EXPECT_TRUE(moved.is_valid());

    // Move-assigning over `moved` must have restored `temp_file` to "initial".
    ASSERT_TRUE(files::ReadFileToString(temp_file.name(), &restored));
    EXPECT_EQ(restored, "initial");
    ASSERT_TRUE(files::WriteFile(temp_file.name(), "stays_after_moved_from_destroyed"));

    // Self-move-assignment must remain valid without prematurely restoring `second_file`.
    test_helper::ScopedSysctl* self = &moved;
    moved = std::move(*self);
    EXPECT_TRUE(moved.is_valid());
    ASSERT_TRUE(files::ReadFileToString(second_file.name(), &restored));
    EXPECT_EQ(restored, "second_updated");

    // Explicit `Restore()` restores immediately and is idempotent.
    moved.Restore();
    EXPECT_FALSE(moved.is_valid());
    ASSERT_TRUE(files::ReadFileToString(second_file.name(), &restored));
    EXPECT_EQ(restored, "second_initial");
    ASSERT_TRUE(files::WriteFile(second_file.name(), "stays_after_explicit_restore"));
    moved.Restore();
  }

  ASSERT_TRUE(files::ReadFileToString(temp_file.name(), &restored));
  EXPECT_EQ(restored, "stays_after_moved_from_destroyed");
  ASSERT_TRUE(files::ReadFileToString(second_file.name(), &restored));
  EXPECT_EQ(restored, "stays_after_explicit_restore");

  {
    ASSERT_TRUE(files::WriteFile(temp_file.name(), "before_assign_empty"));
    auto early_result = test_helper::ScopedSysctl::Create(temp_file.name(), "during_assign_empty");
    ASSERT_THAT(early_result, SyscallResultIsOk());
    test_helper::ScopedSysctl early = std::move(early_result.value());
    early = {};
    EXPECT_FALSE(early.is_valid());
    ASSERT_TRUE(files::ReadFileToString(temp_file.name(), &restored));
    EXPECT_EQ(restored, "before_assign_empty");
    ASSERT_TRUE(files::WriteFile(temp_file.name(), "stays_after_assign_empty"));
  }

  ASSERT_TRUE(files::ReadFileToString(temp_file.name(), &restored));
  EXPECT_EQ(restored, "stays_after_assign_empty");

  auto missing = test_helper::ScopedSysctl::Create("/proc/sys/nonexistent_test_helper_sysctl", "1");
  EXPECT_THAT(missing, SyscallResultIsErrno(ENOENT));

  test_helper::ScopedTempFD read_only_file;
  ASSERT_TRUE(read_only_file.is_valid());
  ASSERT_TRUE(files::WriteFile(read_only_file.name(), "read_only_initial"));
  ASSERT_THAT(fchmod(read_only_file.fd(), 0444), SyscallSucceeds());

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&read_only_file] {
    test_helper::DropAllCapabilities();
    EXPECT_THAT(test_helper::ScopedSysctl::Create(read_only_file.name(), "updated"),
                SyscallResultIsErrno(EACCES));
  });
  EXPECT_TRUE(helper.WaitForChildren());
  ASSERT_TRUE(files::ReadFileToString(read_only_file.name(), &restored));
  EXPECT_EQ(restored, "read_only_initial");
}

void DummySignalHandler(int) {}
void DummySigInfoHandler(int, siginfo_t*, void*) {}

TEST(TestHelperTest, ScopedSigActionRestoresPreviousHandler) {
  struct sigaction original = {};
  ASSERT_THAT(sigaction(SIGUSR1, nullptr, &original), SyscallSucceeds());
  {
    test_helper::ScopedSigAction scoped(SIGUSR1, DummySignalHandler, SA_RESTART);
    struct sigaction active = {};
    ASSERT_THAT(sigaction(SIGUSR1, nullptr, &active), SyscallSucceeds());
    EXPECT_EQ(active.sa_handler, &DummySignalHandler);
    EXPECT_EQ(active.sa_flags & SA_RESTART, SA_RESTART);

    {
      test_helper::ScopedSigAction nested(SIGUSR1, DummySigInfoHandler, SA_RESTART);
      ASSERT_THAT(sigaction(SIGUSR1, nullptr, &active), SyscallSucceeds());
      EXPECT_EQ(active.sa_sigaction, &DummySigInfoHandler);
      EXPECT_EQ(active.sa_flags & (SA_SIGINFO | SA_RESTART), SA_SIGINFO | SA_RESTART);
    }

    ASSERT_THAT(sigaction(SIGUSR1, nullptr, &active), SyscallSucceeds());
    EXPECT_EQ(active.sa_handler, &DummySignalHandler);
    EXPECT_EQ(active.sa_flags & (SA_SIGINFO | SA_RESTART), SA_RESTART);
  }
  struct sigaction restored = {};
  ASSERT_THAT(sigaction(SIGUSR1, nullptr, &restored), SyscallSucceeds());
  EXPECT_EQ(restored.sa_handler, original.sa_handler);
}

}  // namespace
