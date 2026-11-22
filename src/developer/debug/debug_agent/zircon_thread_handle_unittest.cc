// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/debug_agent/zircon_thread_handle.h"

#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fit/defer.h>
#include <lib/zx/channel.h>
#include <lib/zx/event.h>
#include <lib/zx/exception.h>
#include <lib/zx/process.h>
#include <lib/zx/thread.h>
#include <zircon/syscalls/exception.h>

#include <chrono>
#include <condition_variable>
#include <memory>
#include <mutex>
#include <thread>

#include <gtest/gtest.h>

#include "src/developer/debug/debug_agent/debugged_thread.h"
#include "src/developer/debug/debug_agent/process_handle.h"
#include "src/developer/debug/debug_agent/suspend_handle.h"
#include "src/developer/debug/debug_agent/test_utils.h"
#include "src/developer/debug/debug_agent/time.h"
#include "src/developer/debug/debug_agent/zircon_binary_launcher.h"

namespace debug_agent {
namespace {

TEST(ZirconThreadHandleTest, WaitForSuspensionSignalReturnsFalseOnTimeoutAndTrueWhenSuspended) {
  zx::event stop_event;
  ASSERT_ZX_EQ(zx::event::create(0, &stop_event), ZX_OK);

  std::mutex mutex;
  std::condition_variable ready_cv;
  bool ready = false;
  zx_status_t dup_status = ZX_ERR_INTERNAL;
  zx::thread worker_thread;

  std::thread worker([&] {
    zx::thread self_dup;
    zx_status_t status = zx::thread::self()->duplicate(
        ZX_RIGHT_SAME_RIGHTS, &self_dup);  // NOLINT(bugprone-signed-bitwise)
    {
      std::scoped_lock lock(mutex);
      dup_status = status;
      worker_thread = std::move(self_dup);
      ready = true;
    }
    ready_cv.notify_one();
    if (status == ZX_OK) {
      stop_event.wait_one(ZX_USER_SIGNAL_0,  // NOLINT(bugprone-signed-bitwise)
                          zx::time::infinite(), nullptr);
    }
  });
  auto cleanup = fit::defer([&] {
    stop_event.signal(0, ZX_USER_SIGNAL_0);  // NOLINT(bugprone-signed-bitwise)
    if (worker.joinable()) {
      worker.join();
    }
  });

  {
    std::unique_lock<std::mutex> lock(mutex);
    ready_cv.wait(lock, [&] { return ready; });
  }
  ASSERT_ZX_EQ(dup_status, ZX_OK);
  ASSERT_TRUE(worker_thread.is_valid());

  // Declare the thread handle and suspend token after cleanup so the thread resumes first.
  ZirconThreadHandle thread_handle(std::move(worker_thread));
  EXPECT_FALSE(thread_handle.WaitForSuspensionSignal(TickTimePoint{}));
  EXPECT_FALSE(thread_handle.WaitForSuspensionSignal(std::chrono::steady_clock::now()));

  std::unique_ptr<SuspendHandle> suspend = thread_handle.Suspend();
  ASSERT_TRUE(suspend);
  EXPECT_TRUE(thread_handle.WaitForSuspensionSignal(DebuggedThread::DefaultSuspendDeadline()));
  EXPECT_TRUE(thread_handle.WaitForSuspensionSignal(TickTimePoint{}));
}

TEST(ZirconThreadHandleTest, WaitForSuspensionSignalReturnsFalseWhenBlockedOnException) {
  auto svc_dir = component::OpenServiceRoot();
  ASSERT_TRUE(svc_dir.is_ok()) << svc_dir.status_string();

  ZirconBinaryLauncher launcher(*svc_dir);
  ASSERT_TRUE(
      launcher.Setup({"/pkg/test/debug_agent_unit_tests", "--gtest_filter=NonExistent"}).ok());

  std::unique_ptr<ProcessHandle> process_handle = launcher.GetProcess();
  ASSERT_TRUE(process_handle);

  zx::process child_process;
  const zx_status_t dup_status = process_handle->GetNativeHandle().duplicate(
      ZX_RIGHT_SAME_RIGHTS, &child_process);  // NOLINT(bugprone-signed-bitwise)
  ASSERT_ZX_EQ(dup_status, ZX_OK);
  auto kill_child = fit::defer([&child_process] {
    if (child_process.is_valid()) {
      child_process.kill();
    }
  });

  zx::channel exc_channel;
  ASSERT_ZX_EQ(child_process.create_exception_channel(ZX_EXCEPTION_CHANNEL_DEBUGGER, &exc_channel),
               ZX_OK);
  ASSERT_TRUE(launcher.Start().ok());

  zx_signals_t observed = 0;
  ASSERT_ZX_EQ(exc_channel.wait_one(ZX_CHANNEL_READABLE,  // NOLINT(bugprone-signed-bitwise)
                                    zx::deadline_after(zx::sec(10)), &observed),
               ZX_OK);
  ASSERT_TRUE(observed & ZX_CHANNEL_READABLE);  // NOLINT(bugprone-signed-bitwise)

  zx_exception_info_t info = {};
  zx::exception exception;
  uint32_t actual_bytes = 0;
  uint32_t actual_handles = 0;
  ASSERT_ZX_EQ(exc_channel.read(0, &info, exception.reset_and_get_address(), sizeof(info), 1,
                                &actual_bytes, &actual_handles),
               ZX_OK);
  ASSERT_EQ(actual_bytes, sizeof(info));
  ASSERT_EQ(actual_handles, 1u);
  ASSERT_EQ(info.type, ZX_EXCP_THREAD_STARTING);

  zx::thread thread;
  ASSERT_ZX_EQ(exception.get_thread(&thread), ZX_OK);
  ZirconThreadHandle thread_handle(std::move(thread));

  ASSERT_TRUE(thread_handle.GetState().is_blocked_on_exception());
  EXPECT_FALSE(thread_handle.WaitForSuspensionSignal(TickTimePoint{}));
  EXPECT_FALSE(thread_handle.WaitForSuspensionSignal(std::chrono::steady_clock::now()));
}

TEST(ZirconThreadHandleTest, WaitForSuspensionSignalReturnsFalseWhenThreadTerminated) {
  zx::thread worker_thread;
  zx_status_t dup_status = ZX_ERR_INTERNAL;
  std::thread worker([&] {
    // NOLINTNEXTLINE(bugprone-signed-bitwise)
    dup_status = zx::thread::self()->duplicate(ZX_RIGHT_SAME_RIGHTS, &worker_thread);
  });
  worker.join();

  ASSERT_ZX_EQ(dup_status, ZX_OK);
  ASSERT_TRUE(worker_thread.is_valid());
  ASSERT_ZX_EQ(worker_thread.wait_one(ZX_THREAD_TERMINATED,  // NOLINT(bugprone-signed-bitwise)
                                      zx::deadline_after(zx::sec(10)), nullptr),
               ZX_OK);

  ZirconThreadHandle thread_handle(std::move(worker_thread));
  EXPECT_FALSE(thread_handle.WaitForSuspensionSignal(DebuggedThread::DefaultSuspendDeadline()));
}

}  // namespace
}  // namespace debug_agent
