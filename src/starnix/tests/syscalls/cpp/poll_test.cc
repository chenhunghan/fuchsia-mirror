// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <gtest/gtest.h>

#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

TEST(PollTest, REventsIsCleared) {
  int pipefd[2];
  SAFE_SYSCALL(pipe2(pipefd, 0));

  struct pollfd fds[] = {{
                             .fd = pipefd[0],
                             .events = POLLIN,
                             .revents = 42,
                         },
                         {
                             .fd = pipefd[1],
                             .events = POLLOUT,
                             .revents = 42,
                         }};

  ASSERT_EQ(1, poll(fds, 2, 0));
  ASSERT_EQ(0, fds[0].revents);
  ASSERT_EQ(POLLOUT, fds[1].revents);
}

TEST(PollTest, UnconnectedSocket) {
  int fd = socket(PF_INET, SOCK_STREAM, 0);
  ASSERT_GT(fd, 0);

  struct pollfd p;
  p.fd = fd;
  p.events = 0x7FFF;

  EXPECT_EQ(poll(&p, 1, 0), 1);
  EXPECT_EQ(p.revents, POLLHUP | POLLWRNORM | POLLOUT);

  close(fd);
}

// Waits until the forked child reports that it is about to block, then gives it a moment to get
// there.
void WaitForChildToBlock(test_helper::ScopedPipe &ready) {
  char token;
  ASSERT_EQ(read(ready.ReadSide().get(), &token, 1), ssize_t{1});
  usleep(100000);
}

// An interrupted `poll` that runs no signal handler must resume instead of reporting the
// interruption. Stopping and continuing the process interrupts the wait without running a handler.
TEST(PollTest, ResumesAfterStopAndContinue) {
  test_helper::ScopedPipe pipe;
  test_helper::ScopedPipe ready;

  test_helper::ForkHelper helper;
  pid_t child = helper.RunInForkedProcess([&] {
    SAFE_SYSCALL(write(ready.WriteSide().get(), "r", 1));

    struct pollfd p = {.fd = pipe.ReadSide().get(), .events = POLLIN, .revents = 0};
    int result = poll(&p, 1, -1);
    EXPECT_EQ(result, 1) << "poll reported an interruption, errno " << errno;
    EXPECT_EQ(p.revents & POLLIN, POLLIN);
  });

  WaitForChildToBlock(ready);
  ASSERT_EQ(kill(child, SIGSTOP), 0);
  usleep(50000);
  ASSERT_EQ(kill(child, SIGCONT), 0);
  usleep(50000);

  EXPECT_EQ(write(pipe.WriteSide().get(), "x", 1), ssize_t{1});
  ASSERT_TRUE(helper.WaitForChildren());
}

// `ppoll` with a null timeout takes a separate path from finite timeouts and must also resume when
// interrupted without running a handler.
TEST(PollTest, PpollResumesAfterStopAndContinue) {
  test_helper::ScopedPipe pipe;
  test_helper::ScopedPipe ready;

  test_helper::ForkHelper helper;
  pid_t child = helper.RunInForkedProcess([&] {
    SAFE_SYSCALL(write(ready.WriteSide().get(), "r", 1));

    struct pollfd p = {.fd = pipe.ReadSide().get(), .events = POLLIN, .revents = 0};
    int result = ppoll(&p, 1, nullptr, nullptr);
    EXPECT_EQ(result, 1) << "ppoll reported an interruption, errno " << errno;
    EXPECT_EQ(p.revents & POLLIN, POLLIN);
  });

  WaitForChildToBlock(ready);
  ASSERT_EQ(kill(child, SIGSTOP), 0);
  usleep(50000);
  ASSERT_EQ(kill(child, SIGCONT), 0);
  usleep(50000);

  EXPECT_EQ(write(pipe.WriteSide().get(), "x", 1), ssize_t{1});
  ASSERT_TRUE(helper.WaitForChildren());
}

// A restarted `poll` must keep its original deadline rather than starting its relative timeout
// over on every interruption.
TEST(PollTest, PreservesTimeoutOnRestart) {
  test_helper::ScopedPipe pipe;
  test_helper::ScopedPipe ready;

  test_helper::ForkHelper helper;
  pid_t child = helper.RunInForkedProcess([&] {
    SAFE_SYSCALL(write(ready.WriteSide().get(), "r", 1));

    struct pollfd p = {.fd = pipe.ReadSide().get(), .events = POLLIN, .revents = 0};
    EXPECT_EQ(poll(&p, 1, 200), 0) << "poll failed instead of timing out, errno " << errno;
  });

  WaitForChildToBlock(ready);
  ASSERT_EQ(kill(child, SIGSTOP), 0);
  // Let the original deadline expire while the child is stopped so the restarted wait finishes
  // immediately once continued.
  usleep(200000);
  ASSERT_EQ(kill(child, SIGCONT), 0);

  ASSERT_TRUE(helper.WaitForChildren());
}

// Conversely, `poll` is never resumed once a handler has run, even a handler installed with
// SA_RESTART.
TEST(PollTest, ReportsEintrWhenHandlerRuns) {
  test_helper::ScopedPipe pipe;

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    struct sigaction sa = {};
    sa.sa_handler = [](int) {};
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = SA_RESTART;
    SAFE_SYSCALL(sigaction(SIGALRM, &sa, nullptr));

    // Arming the timer just before the call keeps the signal from arriving too early.
    struct itimerval timer = {};
    timer.it_value.tv_usec = 100000;
    SAFE_SYSCALL(setitimer(ITIMER_REAL, &timer, nullptr));

    // Nothing is ever written to the pipe, so only the timer can end this wait. The timeout is a
    // backstop against hanging.
    struct pollfd p = {.fd = pipe.ReadSide().get(), .events = POLLIN, .revents = 0};
    EXPECT_EQ(poll(&p, 1, 30000), -1);
    EXPECT_EQ(errno, EINTR);
  });

  ASSERT_TRUE(helper.WaitForChildren());
}

}  // namespace
