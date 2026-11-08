// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async/cpp/task.h>
#include <lib/driver/runtime/testing/cpp/internal/test_dispatcher_builder.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/zx/time.h>
#include <zircon/assert.h>

#include <cstdio>

[[clang::noinline]] void CrashInDriverTask() { __builtin_trap(); }

int main() {
  fdf_testing::DriverRuntime runtime;

  const void* driver = reinterpret_cast<const void*>(0x1234);
  auto dispatcher_result =
      fdf_internal::TestDispatcherBuilder::CreateUnmanagedSynchronizedDispatcher(
          driver, fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "driver_dispatcher",
          [](fdf_dispatcher_t*) {});
  ZX_ASSERT(dispatcher_result.is_ok());
  const fdf::SynchronizedDispatcher dispatcher = std::move(dispatcher_result.value());

  // Post the outer task which will execute on the dispatcher.
  async::PostTask(dispatcher.async_dispatcher(), [&dispatcher]() {
    // Post a pending task that will remain in callback_queue_ when we fault.
    async::PostTask(dispatcher.async_dispatcher(),
                    []() { std::fprintf(stderr, "Pending task executed unexpectedly\n"); });

    // Post a delayed task that will remain in delayed_tasks_ when we fault.
    async::PostDelayedTask(
        dispatcher.async_dispatcher(),
        []() { std::fprintf(stderr, "Delayed task executed unexpectedly\n"); }, zx::sec(60));

    // Call non-inlined helper that executes __builtin_trap() to stop the inferior.
    CrashInDriverTask();
  });

  runtime.Run();
  return 0;
}
