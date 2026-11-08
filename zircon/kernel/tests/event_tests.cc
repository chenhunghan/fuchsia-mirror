// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/fit/defer.h>
#include <lib/unittest/unittest.h>
#include <lib/zircon-internal/macros.h>

#include <kernel/event.h>
#include <kernel/thread.h>

// Ensure that Event::Signal while holding a spinlock is safe.
//
// This is a regression test for https://fxbug.dev/42157402.
static bool event_signal_spinlock_test() {
  BEGIN_TEST;

  struct Args {
    RelaxedAtomic<bool> about_to_wait{false};
    Event event;
  };

  thread_start_routine Waiter = [](void* args_) -> int {
    auto* args = reinterpret_cast<Args*>(args_);
    args->about_to_wait.store(true);
    args->event.Wait();
    return 0;
  };

  // Pin the current thread to its CPU.
  Thread* const current_thread = Thread::Current::Get();
  const cpu_mask_t original_affinity_mask = current_thread->GetCpuAffinity();
  const auto restore_affinity = fit::defer([original_affinity_mask, current_thread]() {
    current_thread->SetCpuAffinity(original_affinity_mask);
  });
  cpu_num_t target_cpu = arch_curr_cpu_num();
  current_thread->SetCpuAffinity(cpu_num_to_mask(target_cpu));

  // Create a thread that can only run on this same CPU.
  Args args;
  Thread* t = Thread::Create("event_signal_spinlock_test", Waiter, &args, DEFAULT_PRIORITY);
  t->SetCpuAffinity(cpu_num_to_mask(target_cpu));

  // Give the thread deadline parameters with 100% utilization to increase the likelihood that it
  // reaches its Event::Wait before the current thread reaches its Event::Signal.
  t->SetBaseProfile(SchedulerState::BaseProfile{
      SchedDeadlineParams{SchedDuration{ZX_USEC(150)}, SchedDuration{ZX_USEC(150)}}});
  t->Resume();

  // Spin until we know the Waiter has started running.
  while (!args.about_to_wait.load()) {
    Thread::Current::Yield();
  }

  DECLARE_SINGLETON_SPINLOCK_WITH_TYPE(SpinlockForEventSignalTest, MonitoredSpinLock);
  {
    Guard<MonitoredSpinLock, IrqSave> guard{SpinlockForEventSignalTest::Get(), SOURCE_TAG};
    args.event.Signal();
    // Now that we have signaled, we should see that a preemption is pending on this CPU.
    ASSERT_NE(
        0u, (Thread::Current::preemption_state().preempts_pending() & cpu_num_to_mask(target_cpu)));
  }

  t->Join(nullptr, ZX_TIME_INFINITE);

  END_TEST;
}

UNITTEST_START_TESTCASE(event_tests)
UNITTEST("test signaling event while holding spinlock", event_signal_spinlock_test)
UNITTEST_END_TESTCASE(event_tests, "event", "Tests for events")
