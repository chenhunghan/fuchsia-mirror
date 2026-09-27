// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async/cpp/task.h>
#include <lib/driver/runtime/testing/cpp/internal/test_dispatcher_builder.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fdf/cpp/env.h>
#include <lib/fdf/testing.h>
#include <lib/sync/cpp/completion.h>

#include "src/devices/bin/driver_runtime/dispatcher.h"
#include "src/devices/bin/driver_runtime/dispatcher_coordinator.h"
#include "src/devices/bin/driver_runtime/runtime_test_case.h"

class DispatcherDumpTest : public RuntimeTestCase {
 public:
  void SetUp() override;
  void TearDown() override;

 protected:
  static constexpr std::string_view kDispatcherName = "synchronized";
  static constexpr std::string_view kAdditionalDispatcherName = "additional";

  fdf_testing::internal::DriverRuntimeEnv runtime_env;

  fdf::SynchronizedDispatcher dispatcher_;
  libsync::Completion shutdown_completion_;

  fdf::SynchronizedDispatcher dispatcher2_;
  libsync::Completion shutdown_completion2_;

  const void* fake_driver_ = nullptr;
  const void* fake_driver2_ = nullptr;
};

void DispatcherDumpTest::SetUp() {
  driver_runtime::GetDispatcherCoordinator().Reset();

  fake_driver_ = CreateFakeDriver();
  auto shutdown_handler = [&](fdf_dispatcher_t* shutdown_dispatcher) {
    shutdown_completion_.Signal();
  };
  auto dispatcher = fdf_internal::TestDispatcherBuilder::CreateUnmanagedSynchronizedDispatcher(
      fake_driver_, {}, kDispatcherName, shutdown_handler);
  ASSERT_OK(dispatcher.status_value());
  dispatcher_ = std::move(*dispatcher);

  fake_driver2_ = CreateFakeDriver();
  auto shutdown_handler2 = [&](fdf_dispatcher_t* shutdown_dispatcher) {
    shutdown_completion2_.Signal();
  };
  auto dispatcher2 = fdf_internal::TestDispatcherBuilder::CreateUnmanagedSynchronizedDispatcher(
      fake_driver2_, {}, kAdditionalDispatcherName, shutdown_handler2);
  ASSERT_OK(dispatcher2.status_value());
  dispatcher2_ = std::move(*dispatcher2);
}

void DispatcherDumpTest::TearDown() {
  dispatcher_.ShutdownAsync();
  dispatcher2_.ShutdownAsync();
  ASSERT_OK(fdf_testing_run_until_idle());
  shutdown_completion_.Wait();
  shutdown_completion2_.Wait();
}

TEST_F(DispatcherDumpTest, DumpNoTasks) {
  {
    driver_runtime::DumpState state;
    driver_runtime::Dispatcher* runtime_dispatcher = dispatcher_.get()->GetDispatcher();
    runtime_dispatcher->Dump(&state);

    ASSERT_EQ(state.running_dispatcher, nullptr);
    ASSERT_EQ(state.running_driver, nullptr);
    ASSERT_EQ(state.dispatcher_to_dump, dispatcher_.get()->GetDispatcher());
    ASSERT_EQ(state.driver_owner, fake_driver_);
    ASSERT_EQ(state.state, driver_runtime::DispatcherState::kRunning);
    ASSERT_EQ(state.name.data(), kDispatcherName);
    ASSERT_TRUE(state.synchronized);
    ASSERT_FALSE(state.allow_sync_calls);
    ASSERT_EQ(state.queued_tasks.size(), 0);
  }
}

TEST_F(DispatcherDumpTest, DumpFromTask) {
  // Queue 2 tasks, and dump the dispatcher state during each task.
  libsync::Completion task_completion;
  ASSERT_OK(async::PostTask(dispatcher_.async_dispatcher(), [&] {
    {
      driver_runtime::DumpState state;
      driver_runtime::Dispatcher* runtime_dispatcher = dispatcher_.get()->GetDispatcher();
      runtime_dispatcher->Dump(&state);

      ASSERT_EQ(state.running_dispatcher, dispatcher_.get()->GetDispatcher());
      ASSERT_EQ(state.running_driver, fake_driver_);
      ASSERT_EQ(state.dispatcher_to_dump, dispatcher_.get()->GetDispatcher());
      ASSERT_EQ(state.driver_owner, fake_driver_);
      ASSERT_EQ(state.state, driver_runtime::DispatcherState::kRunning);
      ASSERT_EQ(state.name.data(), kDispatcherName);
      ASSERT_EQ(state.queued_tasks.size(),
                1);  // We will post a second task before running the runtime thread.
    }
    task_completion.Signal();
  }));

  libsync::Completion task_completion2;
  ASSERT_OK(async::PostTask(dispatcher_.async_dispatcher(), [&] {
    {
      driver_runtime::DumpState state;
      driver_runtime::Dispatcher* runtime_dispatcher = dispatcher_.get()->GetDispatcher();
      runtime_dispatcher->Dump(&state);

      ASSERT_EQ(state.running_dispatcher, dispatcher_.get()->GetDispatcher());
      ASSERT_EQ(state.running_driver, fake_driver_);
      ASSERT_EQ(state.dispatcher_to_dump, dispatcher_.get()->GetDispatcher());
      ASSERT_EQ(state.driver_owner, fake_driver_);
      ASSERT_EQ(state.state, driver_runtime::DispatcherState::kRunning);
      ASSERT_EQ(state.name.data(), kDispatcherName);
      ASSERT_EQ(state.queued_tasks.size(), 0);  // This is the last task that was queued.
    }
    task_completion2.Signal();
  }));

  ASSERT_OK(fdf_testing_run_until_idle());
  task_completion.Wait();
  task_completion2.Wait();
}

TEST_F(DispatcherDumpTest, DumpFromAnotherDispatcher) {
  constexpr std::string_view kAdditionalDispatcherName = "additional_dispatcher";

  auto fake_driver2 = CreateFakeDriver();
  libsync::Completion shutdown_completion;
  auto shutdown_handler = [&](fdf_dispatcher_t* shutdown_dispatcher) {
    shutdown_completion.Signal();
  };
  auto dispatcher2 = fdf_internal::TestDispatcherBuilder::CreateUnmanagedSynchronizedDispatcher(
      fake_driver2, {}, kAdditionalDispatcherName, shutdown_handler);
  ASSERT_OK(dispatcher2.status_value());

  libsync::Completion task_completion;
  ASSERT_OK(async::PostTask(dispatcher_.async_dispatcher(), [&] {
    {
      driver_runtime::DumpState state;
      driver_runtime::Dispatcher* runtime_dispatcher = dispatcher2->get()->GetDispatcher();
      runtime_dispatcher->Dump(&state);

      // The dispatcher being dumped is different from the dispatcher running on the current
      // thread.
      ASSERT_EQ(state.running_dispatcher, dispatcher_.get()->GetDispatcher());
      ASSERT_EQ(state.running_driver, fake_driver_);
      ASSERT_EQ(state.dispatcher_to_dump, dispatcher2->get()->GetDispatcher());
      ASSERT_EQ(state.driver_owner, fake_driver2);
      ASSERT_EQ(state.state, driver_runtime::DispatcherState::kRunning);
      ASSERT_EQ(state.name.data(), kAdditionalDispatcherName);
      ASSERT_EQ(state.queued_tasks.size(), 0);
    }
    task_completion.Signal();
  }));

  ASSERT_OK(fdf_testing_run_until_idle());
  task_completion.Wait();

  dispatcher2->ShutdownAsync();
  ASSERT_OK(fdf_testing_run_until_idle());
  shutdown_completion.Wait();
}

TEST_F(DispatcherDumpTest, QueueTaskFromAnotherDispatcher) {
  constexpr std::string_view kAdditionalDispatcherName = "additional_dispatcher";

  auto fake_driver2 = CreateFakeDriver();
  libsync::Completion shutdown_completion;
  auto shutdown_handler = [&](fdf_dispatcher_t* shutdown_dispatcher) {
    shutdown_completion.Signal();
  };
  auto dispatcher2 = fdf_internal::TestDispatcherBuilder::CreateUnmanagedSynchronizedDispatcher(
      fake_driver2, {}, kAdditionalDispatcherName, shutdown_handler);
  ASSERT_OK(dispatcher2.status_value());

  libsync::Completion task_completion;
  libsync::Completion task_completion2;
  ASSERT_OK(async::PostTask(dispatcher_.async_dispatcher(), [&] {
    // Queue a task on the other dispatcher and print dump the details.
    ASSERT_OK(async::PostTask(dispatcher2->async_dispatcher(), [&] { task_completion2.Signal(); }));

    {
      driver_runtime::DumpState state;
      driver_runtime::Dispatcher* runtime_dispatcher2 = dispatcher2->get()->GetDispatcher();
      runtime_dispatcher2->Dump(&state);

      // The dispatcher being dumped is different from the dispatcher running on the current
      // thread.
      ASSERT_EQ(state.running_dispatcher, dispatcher_.get()->GetDispatcher());
      ASSERT_EQ(state.running_driver, fake_driver_);
      ASSERT_EQ(state.dispatcher_to_dump, dispatcher2->get()->GetDispatcher());
      ASSERT_EQ(state.driver_owner, fake_driver2);
      ASSERT_EQ(state.state, driver_runtime::DispatcherState::kRunning);
      ASSERT_EQ(state.name.data(), kAdditionalDispatcherName);
      ASSERT_EQ(state.queued_tasks.size(), 1);
      ASSERT_EQ(state.queued_tasks[0].initiating_dispatcher, dispatcher_.get()->GetDispatcher());
      ASSERT_EQ(state.queued_tasks[0].initiating_driver, fake_driver_);
    }
    task_completion.Signal();
  }));

  ASSERT_OK(fdf_testing_run_until_idle());
  task_completion.Wait();
  task_completion2.Wait();

  dispatcher2->ShutdownAsync();
  ASSERT_OK(fdf_testing_run_until_idle());
  shutdown_completion.Wait();
}

// We will use the C API for the shutdown test, as async_task_t is not automatically dropped
// on cancellation like async::Task is.
struct Task {
  async_task_t task;
  libsync::Completion completion;
};

void TaskHandler(async_dispatcher_t* dispatcher, async_task_t* task, zx_status_t result) {
  auto* typed_task = reinterpret_cast<Task*>(task);
  typed_task->completion.Signal();
}

TEST_F(DispatcherDumpTest, DumpDuringShutdown) {
  Task task;
  task.task.state = ASYNC_STATE_INIT;
  task.task.handler = &TaskHandler;

  EXPECT_OK(async_post_task(dispatcher_.async_dispatcher(), &task.task));

  dispatcher_.ShutdownAsync();

  {
    driver_runtime::DumpState state;
    driver_runtime::Dispatcher* runtime_dispatcher = dispatcher_.get()->GetDispatcher();
    runtime_dispatcher->Dump(&state);

    ASSERT_EQ(state.running_dispatcher, nullptr);
    ASSERT_EQ(state.running_driver, nullptr);
    ASSERT_EQ(state.dispatcher_to_dump, dispatcher_.get()->GetDispatcher());
    ASSERT_EQ(state.driver_owner, fake_driver_);
    ASSERT_EQ(state.state, driver_runtime::DispatcherState::kShuttingDown);
    ASSERT_EQ(state.name.data(), kDispatcherName);
    ASSERT_EQ(state.queued_tasks.size(), 1);
    ASSERT_EQ(state.queued_tasks[0].ptr, &task.task);
    ASSERT_EQ(state.queued_tasks[0].initiating_dispatcher, nullptr);
    ASSERT_EQ(state.queued_tasks[0].initiating_driver, nullptr);
  }

  ASSERT_OK(fdf_testing_run_until_idle());
  task.completion.Wait();
  shutdown_completion_.Wait();
}

TEST_F(DispatcherDumpTest, DumpUnsynchronizedDispatcher) {
  constexpr std::string_view kDispatcherName = "unsynchronized";

  auto fake_driver2 = CreateFakeDriver();
  libsync::Completion completion;
  auto shutdown_handler = [&](fdf_dispatcher_t* shutdown_dispatcher) { completion.Signal(); };
  auto dispatcher = fdf_internal::TestDispatcherBuilder::CreateUnmanagedUnsynchronizedDispatcher(
      fake_driver2, {}, kDispatcherName, shutdown_handler);

  libsync::Completion task_completion;
  ASSERT_OK(async::PostTask(dispatcher_.async_dispatcher(), [&] {
    {
      driver_runtime::DumpState state;
      driver_runtime::Dispatcher* runtime_dispatcher = dispatcher->get()->GetDispatcher();

      runtime_dispatcher->Dump(&state);
      ASSERT_EQ(state.running_dispatcher, dispatcher_.get()->GetDispatcher());
      ASSERT_EQ(state.running_driver, fake_driver_);
      ASSERT_EQ(state.dispatcher_to_dump, dispatcher->get()->GetDispatcher());
      ASSERT_EQ(state.driver_owner, fake_driver2);
      ASSERT_EQ(state.state, driver_runtime::DispatcherState::kRunning);
      ASSERT_EQ(state.name.data(), kDispatcherName);
      ASSERT_FALSE(state.synchronized);
      ASSERT_FALSE(state.allow_sync_calls);
      ASSERT_EQ(state.queued_tasks.size(), 0);
    }
    task_completion.Signal();
  }));

  ASSERT_OK(fdf_testing_run_until_idle());
  task_completion.Wait();

  dispatcher->ShutdownAsync();
  ASSERT_OK(fdf_testing_run_until_idle());
  completion.Wait();
}

TEST_F(DispatcherDumpTest, DumpRequestsCount) {
  // Post a non-inlined task.
  ASSERT_OK(async::PostTask(dispatcher_.async_dispatcher(), [&] {}));
  ASSERT_OK(fdf_testing_run_until_idle());
  {
    driver_runtime::DumpState state;
    driver_runtime::Dispatcher* runtime_dispatcher = dispatcher_.get()->GetDispatcher();
    runtime_dispatcher->Dump(&state);
    ASSERT_EQ(state.debug_stats.num_inlined_requests, 0);
    ASSERT_EQ(state.debug_stats.num_total_requests, 1);
  }

  // Send an inlined request from end1 to end0 of the channel, which will be serviced by
  // |dispatcher_|.
  auto channels = fdf::ChannelPair::Create(0);
  ASSERT_OK(channels.status_value());

  auto channel_read = std::make_unique<fdf::ChannelRead>(
      channels->end0.get(), 0,
      [&](fdf_dispatcher_t* dispatcher, fdf::ChannelRead* channel_read, zx_status_t status) {});
  ASSERT_OK(channel_read->Begin(dispatcher_.get()));

  auto fake_driver2 = CreateFakeDriver();
  auto shutdown_handler = [&](fdf_dispatcher_t* shutdown_dispatcher) {};
  auto dispatcher2 = fdf_internal::TestDispatcherBuilder::CreateUnmanagedSynchronizedDispatcher(
      fake_driver2, {}, "additional_dispatcher", shutdown_handler);
  ASSERT_OK(dispatcher2.status_value());

  fdf::Arena arena('TEST');
  ASSERT_OK(async::PostTask(dispatcher2->async_dispatcher(), [&] {
    ASSERT_OK(
        channels->end1.Write(0, arena, nullptr, 0, cpp20::span<zx_handle_t>()).status_value());
  }));
  ASSERT_OK(fdf_testing_run_until_idle());

  // We should see 1 inlined request in the dump.
  {
    driver_runtime::DumpState state;
    driver_runtime::Dispatcher* runtime_dispatcher = dispatcher_.get()->GetDispatcher();
    runtime_dispatcher->Dump(&state);
    ASSERT_EQ(state.debug_stats.num_inlined_requests, 1);
    ASSERT_EQ(state.debug_stats.num_total_requests, 2);
  }

  dispatcher2->ShutdownAsync();
  ASSERT_OK(fdf_testing_run_until_idle());
}

TEST_F(DispatcherDumpTest, DumpChannelWaitNotYetRegistered) {
  auto channels = fdf::ChannelPair::Create(0);
  ASSERT_OK(channels.status_value());

  // Write to one end of the channel before registering a read on the other end.
  fdf::Arena arena('TEST');
  ASSERT_OK(async::PostTask(dispatcher_.async_dispatcher(), [&] {
    ASSERT_OK(
        channels->end0.Write(0, arena, nullptr, 0, cpp20::span<zx_handle_t>()).status_value());
  }));
  ASSERT_OK(fdf_testing_run_until_idle());

  auto channel_read = std::make_unique<fdf::ChannelRead>(
      channels->end1.get(), 0,
      [&](fdf_dispatcher_t* dispatcher, fdf::ChannelRead* channel_read, zx_status_t status) {});
  ASSERT_OK(async::PostTask(dispatcher2_.async_dispatcher(),
                            [&] { ASSERT_OK(channel_read->Begin(dispatcher2_.get())); }));
  ASSERT_OK(fdf_testing_run_until_idle());

  {
    driver_runtime::DumpState state;
    driver_runtime::Dispatcher* runtime_dispatcher = dispatcher2_.get()->GetDispatcher();
    runtime_dispatcher->Dump(&state);
    ASSERT_EQ(state.debug_stats.num_inlined_requests, 0);
    ASSERT_EQ(state.debug_stats.num_total_requests, 2);
    ASSERT_EQ(state.debug_stats.non_inlined.task, 1);
    ASSERT_EQ(state.debug_stats.non_inlined.channel_wait_not_yet_registered, 1);
  }
}

TEST_F(DispatcherDumpTest, DumpChannelWaitRegistered) {
  auto channels = fdf::ChannelPair::Create(0);
  ASSERT_OK(channels.status_value());

  auto channel_read = std::make_unique<fdf::ChannelRead>(
      channels->end1.get(), 0,
      [&](fdf_dispatcher_t* dispatcher, fdf::ChannelRead* channel_read, zx_status_t status) {});
  ASSERT_OK(async::PostTask(dispatcher2_.async_dispatcher(),
                            [&] { ASSERT_OK(channel_read->Begin(dispatcher2_.get())); }));
  ASSERT_OK(fdf_testing_run_until_idle());

  // Write to one end of the channel after the read has been registered on the other end.
  fdf::Arena arena('TEST');
  ASSERT_OK(async::PostTask(dispatcher_.async_dispatcher(), [&] {
    ASSERT_OK(
        channels->end0.Write(0, arena, nullptr, 0, cpp20::span<zx_handle_t>()).status_value());
  }));
  ASSERT_OK(fdf_testing_run_until_idle());

  {
    driver_runtime::DumpState state;
    driver_runtime::Dispatcher* runtime_dispatcher = dispatcher2_.get()->GetDispatcher();
    runtime_dispatcher->Dump(&state);
    ASSERT_EQ(state.debug_stats.num_inlined_requests, 1);
    ASSERT_EQ(state.debug_stats.num_total_requests, 2);
    ASSERT_EQ(state.debug_stats.non_inlined.task, 1);
  }
}

TEST_F(DispatcherDumpTest, DumpAllDispatchersCAbi) {
  ASSERT_OK(async::PostTask(dispatcher_.async_dispatcher(), [&] {}));
  ASSERT_OK(fdf_testing_run_until_idle());

  ASSERT_OK(async::PostTask(dispatcher2_.async_dispatcher(), [&] {}));

  fdf_dispatcher_dump_entry_t* entries = nullptr;
  size_t count = 0;
  fdf_env_get_all_dispatchers_dump(&entries, &count);
  ASSERT_NE(entries, nullptr);
  ASSERT_GE(count, 2u);

  bool found_dispatcher1 = false;
  bool found_dispatcher2 = false;
  for (size_t i = 0; i < count; i++) {
    std::string_view name(entries[i].name);
    if (name == kDispatcherName) {
      found_dispatcher1 = true;
      EXPECT_EQ(entries[i].driver, fake_driver_);
      EXPECT_TRUE(entries[i].synchronized);
      EXPECT_FALSE(entries[i].allow_sync_calls);
      EXPECT_EQ(entries[i].state, FDF_DISPATCHER_STATE_RUNNING);
      EXPECT_EQ(entries[i].debug_stats.num_total_requests, 1u);
      EXPECT_EQ(entries[i].debug_stats.non_inlined.task, 1u);
      EXPECT_EQ(entries[i].num_queued_tasks, 0u);
    } else if (name == kAdditionalDispatcherName) {
      found_dispatcher2 = true;
      EXPECT_EQ(entries[i].driver, fake_driver2_);
      EXPECT_EQ(entries[i].num_queued_tasks, 1u);
      ASSERT_NE(entries[i].queued_tasks, nullptr);
      EXPECT_NE(entries[i].queued_tasks[0].ptr, 0u);
    }
  }
  EXPECT_TRUE(found_dispatcher1);
  EXPECT_TRUE(found_dispatcher2);

  fdf_env_free_all_dispatchers_dump(entries, count);

  fdf_thread_dump_entry_t* thread_entries = nullptr;
  size_t thread_count = 0;
  fdf_env_get_all_threads_dump(&thread_entries, &thread_count);
  EXPECT_EQ(thread_entries, nullptr);
  EXPECT_EQ(thread_count, 0u);
  fdf_env_free_all_threads_dump(thread_entries, thread_count);

  ASSERT_OK(driver_runtime::GetDispatcherCoordinator().default_thread_pool()->AddThread());
  zx::result role_thread_pool =
      driver_runtime::GetDispatcherCoordinator().GetOrCreateThreadPool("fuchsia.test.role");
  ASSERT_OK(role_thread_pool.status_value());

  fdf_env_get_all_threads_dump(&thread_entries, &thread_count);
  ASSERT_NE(thread_entries, nullptr);
  ASSERT_EQ(thread_count, 2u);
  bool found_default_thread = false;
  bool found_role_thread = false;
  for (size_t i = 0; i < thread_count; ++i) {
    EXPECT_NE(thread_entries[i].koid, ZX_KOID_INVALID);
    if (std::string_view(thread_entries[i].scheduler_role).empty()) {
      found_default_thread = true;
      EXPECT_EQ(std::string_view(thread_entries[i].name), "fdf-dispatcher-thread-0");
    } else if (std::string_view(thread_entries[i].scheduler_role) == "fuchsia.test.role") {
      found_role_thread = true;
      EXPECT_EQ(std::string_view(thread_entries[i].name),
                "fdf-dispatcher-thread-0:fuchsia.test.role");
    }
  }
  EXPECT_TRUE(found_default_thread);
  EXPECT_TRUE(found_role_thread);
  fdf_env_free_all_threads_dump(thread_entries, thread_count);

  ASSERT_OK(fdf_testing_run_until_idle());
}
