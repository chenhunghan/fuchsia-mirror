// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <unistd.h>

#include <atomic>
#include <cerrno>
#include <iterator>
#include <thread>
#include <vector>

#include <gtest/gtest.h>
#include <linux/capability.h>

#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"

namespace {

TEST(SetPriorityTest, ZeroRLimit) {
  struct rlimit limit = {
      .rlim_cur = 0,
      .rlim_max = 0,
  };

  ASSERT_EQ(setrlimit(RLIMIT_NICE, &limit), 0)
      << "setrlimit failed" << std::strerror(errno) << '\n';

  ASSERT_EQ(setpriority(PRIO_PROCESS, 0, 19), 0)
      << "setpriority failed" << std::strerror(errno) << '\n';
}

// Threads that keep changing their scheduler role while other threads are created must never see
// a failure, and each thread must keep the nice value that it set last.
TEST(SetPriorityTest, ConcurrentNiceChangesAndThreadCreation) {
  if (!test_helper::HasCapabilityEffective(CAP_SYS_NICE)) {
    GTEST_SKIP() << "Lowering nice values needs CAP_SYS_NICE.";
  }

  // These nice values map to different scheduler roles on Starnix.
  static constexpr int kNiceValues[] = {0, 2, 4};
  static constexpr int kFinalNice = 4;
  constexpr int kChangingThreads = 2;
  constexpr int kCreatedThreads = 200;

  std::atomic<bool> stop = false;
  std::atomic<int> failures = 0;
  std::vector<std::thread> changing_threads;
  changing_threads.reserve(kChangingThreads);
  for (int t = 0; t < kChangingThreads; ++t) {
    changing_threads.emplace_back([&stop, &failures, t] {
      const auto tid = static_cast<id_t>(syscall(SYS_gettid));
      size_t i = t;
      while (!stop.load()) {
        if (setpriority(PRIO_PROCESS, tid, kNiceValues[i++ % std::size(kNiceValues)]) != 0) {
          failures++;
        }
      }
      if (setpriority(PRIO_PROCESS, tid, kFinalNice) != 0) {
        failures++;
      }
      errno = 0;
      if (getpriority(PRIO_PROCESS, tid) != kFinalNice || errno != 0) {
        failures++;
      }
    });
  }

  for (int i = 0; i < kCreatedThreads; ++i) {
    std::thread([] {}).join();
  }
  stop = true;
  for (std::thread& thread : changing_threads) {
    thread.join();
  }
  EXPECT_EQ(failures.load(), 0);
}

}  //  namespace
