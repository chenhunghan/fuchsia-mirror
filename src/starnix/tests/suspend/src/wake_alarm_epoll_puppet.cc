// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/epoll.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <cstdint>
#include <cstdlib>
#include <iostream>
#include <string>
#include <string_view>
#include <system_error>
#include <thread>

#include "src/lib/files/file.h"
#include "src/lib/files/file_descriptor.h"

namespace {

#define CHECK(cond)                                                       \
  do {                                                                    \
    if (cond) {                                                           \
    } else {                                                              \
      const int err = errno;                                              \
      std::cerr << "CHECK(" #cond ") failed at line " << __LINE__ << ": " \
                << std::generic_category().message(err) << '\n';          \
      std::abort();                                                       \
    }                                                                     \
  } while (0)

void SendMsg(int fd, std::string_view msg) {
  CHECK(fxl::WriteFileDescriptor(fd, msg.data(), static_cast<ssize_t>(msg.size())));
}

void ExpectMsg(int fd, std::string_view expected) {
  std::string buf(expected.size(), '\0');
  CHECK(fxl::ReadFileDescriptor(fd, buf.data(), static_cast<ssize_t>(buf.size())) ==
            static_cast<ssize_t>(buf.size()) &&
        buf == expected);
}

void WaitUntilSleeping(const std::string& stat_path, const std::atomic<bool>* stop = nullptr) {
  std::string s;
  while (stop == nullptr || !stop->load(std::memory_order_acquire)) {
    if (!files::ReadFileToString(stat_path, &s)) {
      CHECK(stop != nullptr && stop->load(std::memory_order_acquire));
      return;
    }
    auto p = s.rfind(')');
    if (p != std::string::npos && p + 2 < s.size() && s[p + 2] == 'S') {
      return;
    }
    usleep(1000);
  }
}

struct WakeupWatcher {
  std::atomic<bool> unblocked{false};
  std::atomic<pid_t> tid{0};
  std::thread thread;

  WakeupWatcher() {
    int fd = open("/sys/power/wakeup_count", O_RDONLY | O_CLOEXEC);
    CHECK(fd >= 0);
    thread = std::thread([this, fd] {
      char buf[64] = {};
      tid.store(static_cast<pid_t>(syscall(SYS_gettid)), std::memory_order_release);
      tid.notify_one();
      ssize_t n;
      do {
        n = read(fd, buf, sizeof(buf));
      } while (n < 0 && errno == EINTR);
      CHECK(n > 0);
      close(fd);
      unblocked.store(true, std::memory_order_release);
    });
    tid.wait(0, std::memory_order_acquire);
    WaitUntilSleeping(
        "/proc/self/task/" + std::to_string(tid.load(std::memory_order_relaxed)) + "/stat",
        &unblocked);
  }

  ~WakeupWatcher() {
    if (thread.joinable()) {
      thread.join();
    }
  }
};

void SetCgroupFrozen(bool freeze) {
  CHECK(files::WriteFile("/tmp/cgroup/w/cgroup.freeze", freeze ? "1" : "0"));
  std::string target = freeze ? "frozen 1" : "frozen 0";
  std::string events;
  do {
    usleep(1000);
    CHECK(files::ReadFileToString("/tmp/cgroup/w/cgroup.events", &events));
  } while (!events.contains(target));
}

void ArmAlarm1Ns(int tfd) {
  struct itimerspec its = {};
  its.it_value.tv_nsec = 1;
  CHECK(timerfd_settime(tfd, 0, &its, nullptr) == 0);
}

}  // namespace

int main() {
  CHECK(mkdir("/tmp/cgroup", 0755) == 0);
  CHECK(mount(nullptr, "/tmp/cgroup", "cgroup2", 0, nullptr) == 0);
  CHECK(mkdir("/tmp/cgroup/w", 0755) == 0);

  int epfd = epoll_create1(EPOLL_CLOEXEC);
  int tfd = timerfd_create(CLOCK_BOOTTIME_ALARM, TFD_NONBLOCK | TFD_CLOEXEC);
  CHECK(epfd >= 0 && tfd >= 0);

  struct epoll_event ev = {.events = EPOLLIN | EPOLLWAKEUP, .data = {.u64 = 42}};
  CHECK(epoll_ctl(epfd, EPOLL_CTL_ADD, tfd, &ev) == 0);
  ArmAlarm1Ns(tfd);

  int c2p[2], p2c[2];
  CHECK(pipe2(c2p, O_CLOEXEC) == 0 && pipe2(p2c, O_CLOEXEC) == 0);

  pid_t child = fork();
  CHECK(child >= 0);
  if (child == 0) {
    close(STDIN_FILENO);
    close(STDOUT_FILENO);
    close(c2p[0]);
    close(p2c[1]);
    struct epoll_event out = {};
    int ret;
    do {
      ret = epoll_wait(epfd, &out, 1, -1);
    } while (ret < 0 && errno == EINTR);
    CHECK(ret == 1 && (out.events & EPOLLIN) != 0);
    SendMsg(c2p[1], "epoll_woke\n");

    ExpectMsg(p2c[0], "recheck\n");
    CHECK(epoll_wait(epfd, &out, 1, 0) == 0);
    _exit(0);
  }
  close(c2p[1]);
  close(p2c[0]);

  CHECK(files::WriteFile("/tmp/cgroup/w/cgroup.procs", std::to_string(child)));
  WaitUntilSleeping("/proc/" + std::to_string(child) + "/stat");
  SetCgroupFrozen(true);

  // Initial suspend; blocks until the test runner fires the mock WakeAlarms timer.
  CHECK(files::WriteFile("/sys/power/state", "mem"));

  // Wait for the test runner to confirm `HrTimerManager` dropped its `WakeAlarms` lease token.
  ExpectMsg(STDIN_FILENO, "lease_dropped\n");

  // 1. `epoll_wait` caller is still frozen (has not awakened yet).
  struct pollfd pfd = {.fd = c2p[0], .events = POLLIN};
  CHECK(poll(&pfd, 1, 0) == 0);
  // TODO(https://fxbug.dev/581114664): `EPOLLWAKEUP` is not activated on wake, leaving a suspend
  // gap where `/sys/power/wakeup_count` fails to block before `epoll_wait` runs.
  {
    WakeupWatcher gap_watcher;
    CHECK(gap_watcher.unblocked.load(std::memory_order_acquire));
  }

  // 2. Thaw `epoll_wait` caller so `epoll_wait` returns `EPOLLIN`, before `read(tfd)`.
  SetCgroupFrozen(false);
  ExpectMsg(c2p[0], "epoll_woke\n");
  WakeupWatcher watcher;
  usleep(10'000);
  CHECK(!watcher.unblocked.load(std::memory_order_acquire));
  CHECK(!files::WriteFile("/sys/power/state", "mem") && errno == EINVAL);

  // 3. Drain `timerfd` via `read(tfd)`, before the next `epoll_wait`.
  uint64_t expirations = 0;
  CHECK(read(tfd, &expirations, sizeof(expirations)) == sizeof(expirations) && expirations == 1);
  usleep(10'000);
  CHECK(!watcher.unblocked.load(std::memory_order_acquire));
  CHECK(!files::WriteFile("/sys/power/state", "mem") && errno == EINVAL);

  // 4. Next `epoll_wait` deactivates `EPOLLWAKEUP`, unblocking `watcher`.
  SendMsg(p2c[1], "recheck\n");
  int status = 0;
  CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
  watcher.thread.join();

  CHECK(rmdir("/tmp/cgroup/w") == 0 && umount("/tmp/cgroup") == 0 && rmdir("/tmp/cgroup") == 0);

  // Signal end of the guarded window, wait for the test runner to exit its guard check, and verify
  // container re-suspend succeeds.
  ArmAlarm1Ns(tfd);
  SendMsg(STDOUT_FILENO, "wakeup_cleared\n");
  ExpectMsg(STDIN_FILENO, "resuspend\n");
  CHECK(files::WriteFile("/sys/power/state", "mem"));
  SendMsg(STDOUT_FILENO, "resuspended\n");
  return 0;
}
