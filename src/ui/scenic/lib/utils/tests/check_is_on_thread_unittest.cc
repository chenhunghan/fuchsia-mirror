// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/utils/check_is_on_thread.h"

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>

#include <future>
#include <thread>

#include <gtest/gtest.h>

namespace utils::test {
namespace {

struct ProbeResult {
  bool on_main;
  bool on_input;
};

ProbeResult ProbeOnNewStdThread() {
  std::promise<ProbeResult> promise;
  std::thread thread(
      [&] { promise.set_value({.on_main = IsOnMainThread(), .on_input = IsOnInputThread()}); });
  thread.join();
  return promise.get_future().get();
}

ProbeResult ProbeOnDispatcher(async_dispatcher_t* dispatcher) {
  std::promise<ProbeResult> promise;
  async::PostTask(dispatcher, [&] {
    promise.set_value({.on_main = IsOnMainThread(), .on_input = IsOnInputThread()});
  });
  return promise.get_future().get();
}

TEST(CheckIsOnThreadTest, ReturnsFalseWhenUnsetAndCheckIsLenient) {
  async::Loop main_loop(&kAsyncLoopConfigAttachToCurrentThread);

  EXPECT_FALSE(IsOnMainThread());
  EXPECT_FALSE(IsOnInputThread());

  std::thread worker([&] {
    EXPECT_FALSE(IsOnMainThread());
    EXPECT_FALSE(IsOnInputThread());
    // Pins documented leniency when both global and thread default dispatcher are nullptr.
    CheckIsOnMainThread();
    CheckIsOnInputThread();
  });
  worker.join();
}

TEST(CheckIsOnThreadTest, DistinguishesMainInputOtherAndWorkerThreads) {
  async::Loop main_loop(&kAsyncLoopConfigAttachToCurrentThread);
  async::Loop input_loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  async::Loop other_loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_EQ(input_loop.StartThread("input-thread"), ZX_OK);
  ASSERT_EQ(other_loop.StartThread("other-thread"), ZX_OK);

  ScopedThreadDispatcherSetter setter(main_loop.dispatcher(), input_loop.dispatcher());

  EXPECT_TRUE(IsOnMainThread());
  EXPECT_FALSE(IsOnInputThread());
  CheckIsOnMainThread();

  std::promise<void> input_done;
  async::PostTask(input_loop.dispatcher(), [&] {
    EXPECT_FALSE(IsOnMainThread());
    EXPECT_TRUE(IsOnInputThread());
    CheckIsOnInputThread();
    input_done.set_value();
  });
  input_done.get_future().wait();

  const ProbeResult other_result = ProbeOnDispatcher(other_loop.dispatcher());
  EXPECT_FALSE(other_result.on_main);
  EXPECT_FALSE(other_result.on_input);

  const ProbeResult worker_result = ProbeOnNewStdThread();
  EXPECT_FALSE(worker_result.on_main);
  EXPECT_FALSE(worker_result.on_input);

  other_loop.Quit();
  other_loop.JoinThreads();
  input_loop.Quit();
  input_loop.JoinThreads();
}

TEST(CheckIsOnThreadTest, ResetsMainAndInputDispatchersOnDestruction) {
  async::Loop main_loop(&kAsyncLoopConfigAttachToCurrentThread);
  async::Loop input_loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_EQ(input_loop.StartThread("input-thread"), ZX_OK);

  {
    ScopedThreadDispatcherSetter setter(main_loop.dispatcher(), input_loop.dispatcher());
    EXPECT_TRUE(IsOnMainThread());
  }

  EXPECT_FALSE(IsOnMainThread());
  EXPECT_FALSE(IsOnInputThread());

  const ProbeResult input_result = ProbeOnDispatcher(input_loop.dispatcher());
  EXPECT_FALSE(input_result.on_main);
  EXPECT_FALSE(input_result.on_input);

  input_loop.Quit();
  input_loop.JoinThreads();
}

TEST(CheckIsOnThreadTest, SameDispatcherForMainAndInput) {
  async::Loop main_loop(&kAsyncLoopConfigAttachToCurrentThread);

  ScopedThreadDispatcherSetter setter(main_loop.dispatcher(), main_loop.dispatcher());

  EXPECT_TRUE(IsOnMainThread());
  EXPECT_TRUE(IsOnInputThread());

  const ProbeResult worker_result = ProbeOnNewStdThread();
  EXPECT_FALSE(worker_result.on_main);
  EXPECT_FALSE(worker_result.on_input);
}

}  // namespace
}  // namespace utils::test
