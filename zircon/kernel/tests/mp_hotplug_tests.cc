// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/fit/defer.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <lib/unittest/unittest.h>
#include <platform.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/interrupt.h>
#include <dev/power.h>
#include <fbl/alloc_checker.h>
#include <kernel/cpu.h>
#include <kernel/dpc.h>
#include <kernel/event.h>
#include <kernel/mp.h>
#include <kernel/mutex.h>
#include <kernel/scheduler.h>
#include <kernel/thread.h>
#include <ktl/atomic.h>
#include <ktl/iterator.h>
#include <ktl/unique_ptr.h>

#include "tests.h"

#include <ktl/enforce.h>

static int resume_cpu_test_thread(void* arg) {
  *reinterpret_cast<cpu_num_t*>(arg) = arch_curr_cpu_num();
  return 0;
}

// "Unplug" online secondary (non-BOOT) cores
static zx_status_t unplug_all_cores() {
  cpu_mask_t cpumask = mp_get_online_mask() & ~cpu_num_to_mask(BOOT_CPU_ID);
  return mp_unplug_cpu_mask(cpumask, ZX_TIME_INFINITE);
}

static zx_status_t hotplug_core(cpu_num_t i) {
  cpu_mask_t cpumask = cpu_num_to_mask(i);
  return mp_hotplug_cpu_mask(cpumask);
}

static unsigned get_num_cpus_online() {
  unsigned count = 0;
  cpu_mask_t online = mp_get_online_mask();
  while (online) {
    online >>= 1;
    ++count;
  }
  return count;
}

static zx_status_t wait_for_cpu_offline(cpu_num_t i) {
  zx_instant_mono_t print_time = zx_time_add_duration(current_mono_time(), ZX_SEC(5));
  while (true) {
    zx::result<power_cpu_state> res = platform_get_cpu_state(i);
    if (res.is_error()) {
      if (res.error_value() == ZX_ERR_NOT_SUPPORTED) {
        // x86 does not implement platform_get_cpu_state, so return OK if the call returns
        // ZX_ERR_NOT_SUPPORTED.
        return ZX_OK;
      }
      return res.error_value();
    } else if (res.value() == power_cpu_state::OFF || res.value() == power_cpu_state::STOPPED) {
      return ZX_OK;
    }
    if (current_mono_time() > print_time) {
      print_time = zx_time_add_duration(current_mono_time(), ZX_SEC(5));
      printf("Still waiting for CPU %u to go offline, waiting 5 more seconds\n", i);
    }
    Thread::Current::SleepRelative(ZX_USEC(200));
  }
}

static void wait_for_cpu_active(cpu_num_t i) {
  zx_instant_mono_t print_time = zx_time_add_duration(current_mono_time(), ZX_SEC(5));
  while (true) {
    if (Scheduler::PeekIsActive(i)) {
      return;
    }
    if (current_mono_time() > print_time) {
      print_time = zx_time_add_duration(current_mono_time(), ZX_SEC(5));
      printf("Still waiting for CPU %u to become active, waiting 5 more seconds\n", i);
    }
    Thread::Current::SleepRelative(ZX_USEC(200));
  }
}

// Wait until |i|'s DpcRunner is running, by queuing a Dpc on each of |i|'s Dpc queues and waiting
// for them to execute.
//
// A hotplugged core becomes active (see wait_for_cpu_active) before it has created its Dpc threads,
// and a core must not be unplugged before then (mp_unplug_cpu_mask shuts down its Dpc threads).
// The general queue's thread is created (and may start executing Dpcs) before the low latency
// queue's thread, so both queues must be checked.
static zx_status_t wait_for_cpu_dpc_runner(cpu_num_t i) {
  Thread* t = Thread::Create(
      "wait-for-dpc-runner",
      [](void*) -> int {
        constexpr DpcRunner::QueueType kQueueTypes[] = {DpcRunner::QueueType::General,
                                                        DpcRunner::QueueType::LowLatency};
        for (const DpcRunner::QueueType type : kQueueTypes) {
          Event event;
          Dpc dpc([](Dpc* d) { d->arg<Event>()->Signal(); }, &event);
          const zx_status_t status = DpcRunner::Enqueue(dpc, type);
          if (status != ZX_OK) {
            return status;
          }
          // Wait non-interruptibly, since |dpc| and |event| live on this thread's stack.
          event.Wait();
        }
        return ZX_OK;
      },
      nullptr, DEFAULT_PRIORITY);
  if (t == nullptr) {
    return ZX_ERR_NO_MEMORY;
  }
  t->SetCpuAffinity(cpu_num_to_mask(i));
  t->Resume();
  int retcode;
  const zx_status_t status = t->Join(&retcode, ZX_TIME_INFINITE);
  return status != ZX_OK ? status : retcode;
}

// Unplug all cores (except for Boot core), then hotplug
// the cores one by one and make sure that we can schedule
// tasks on that core.
[[maybe_unused]] static bool mp_hotplug_test() {
  BEGIN_TEST;

// TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
#if defined(__riscv)
  printf("skipping test mp_hotplug, hotplug only suported on x64 and arm64\n");
  END_TEST;
#endif

  uint num_cores = get_num_cpus_online();
  if (num_cores < 2) {
    printf("skipping test mp_hotplug, not enough online cpus\n");
    END_TEST;
  }
  Thread::Current::MigrateToCpu(BOOT_CPU_ID);
  // "Unplug" online secondary (non-BOOT) cores
  ASSERT_OK(unplug_all_cores(), "unplugging all cores failed");
  for (cpu_num_t i = 0; i < num_cores; i++) {
    if (i == BOOT_CPU_ID) {
      continue;
    }
    // Wait until this core is fully offline.
    ASSERT_OK(wait_for_cpu_offline(i), "waiting for core to go offline failed");
    // Hotplug this core.
    ASSERT_OK(hotplug_core(i), "hotplugging core failed");
    // Wait until the core is active.
    wait_for_cpu_active(i);
    // Create a thread, affine it to the core just hotplugged
    // and make sure the thread does get scheduled there.
    cpu_num_t running_core{INVALID_CPU};
    Thread* nt = Thread::Create("resume-test-thread", resume_cpu_test_thread, &running_core,
                                DEFAULT_PRIORITY);
    ASSERT_NE(nullptr, nt, "Thread create failed");
    nt->SetCpuAffinity(cpu_num_to_mask(i));
    nt->SetMigrateFn([](auto...) {});
    nt->Resume();
    ASSERT_OK(nt->Join(nullptr, ZX_TIME_INFINITE), "thread join failed");
    ASSERT_EQ(i, running_core, "Thread not running on hotplugged core");
  }

  END_TEST;
}

// Repeatedly unplug and hotplug a core while another thread repeatedly calls mp_sync_exec targeting
// it, and make sure that every mp_sync_exec call returns.
//
// A task may be queued for the core after it has disabled interrupts to go offline, in which case
// the task is drained when the core is marked offline rather than executed.  Otherwise, the task is
// executed by the core before it goes offline.  Either way, mp_sync_exec must not wait
// indefinitely for the core, and must not return while its task is still queued (which
// mp_sync_exec checks with a DEBUG_ASSERT).
[[maybe_unused]] static bool mp_hotplug_sync_exec_test() {
  BEGIN_TEST;

// TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
#if defined(__riscv)
  printf("skipping test mp_hotplug_sync_exec, hotplug only supported on x64 and arm64\n");
  END_TEST;
#endif

  const cpu_mask_t online_secondaries = mp_get_online_mask() & ~cpu_num_to_mask(BOOT_CPU_ID);
  if (online_secondaries == 0) {
    printf("skipping test mp_hotplug_sync_exec, not enough online cpus\n");
    END_TEST;
  }
  const cpu_num_t target_cpu = highest_cpu_set(online_secondaries);

  auto restore_affinity = fit::defer(
      [previous_affinity = Thread::Current::SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID))] {
        Thread::Current::SetCpuAffinity(previous_affinity);
      });

  // If an assertion below fails while |target_cpu| is unplugged, hotplug it again so that
  // subsequent tests run with the same set of cores.
  auto restore_target_cpu = fit::defer([target_cpu] {
    if (!mp_is_cpu_online(target_cpu)) {
      const zx_status_t status = hotplug_core(target_cpu);
      ASSERT_MSG(status == ZX_OK, "hotplugging core %u failed: %d", target_cpu, status);
      wait_for_cpu_active(target_cpu);
      const zx_status_t dpc_status = wait_for_cpu_dpc_runner(target_cpu);
      ASSERT_MSG(dpc_status == ZX_OK, "waiting for core %u's dpc runner failed: %d", target_cpu,
                 dpc_status);
    }
  });

  struct State {
    const cpu_num_t target_cpu;
    ktl::atomic<bool> stop{false};
    ktl::atomic<uint64_t> calls{0};
  } state{.target_cpu = target_cpu};

  // The sender may run on any online core other than the one being unplugged and hotplugged.
  Thread* sender = Thread::Create(
      "mp-hotplug-sync-exec-sender",
      [](void* arg) -> int {
        auto* s = static_cast<State*>(arg);
        while (!s->stop.load(ktl::memory_order_relaxed)) {
          // Alternate between targeting just the core being unplugged and hotplugged, and all
          // other cores.
          const uint64_t calls = s->calls.load(ktl::memory_order_relaxed);
          if (calls % 2 == 0) {
            mp_sync_exec(
                mp_ipi_target::MASK, cpu_num_to_mask(s->target_cpu), [](void*) {}, nullptr);
          } else {
            // Targeting all but the local core requires interrupts to be disabled.
            InterruptDisableGuard irqd;
            mp_sync_exec(mp_ipi_target::ALL_BUT_LOCAL, 0, [](void*) {}, nullptr);
          }
          s->calls.store(calls + 1, ktl::memory_order_relaxed);
          // While the target core is offline, every other call returns immediately, so yield to
          // avoid monopolizing this core.
          Thread::Current::Yield();
        }
        return 0;
      },
      &state, DEFAULT_PRIORITY);
  ASSERT_NONNULL(sender, "Thread create failed");
  sender->SetCpuAffinity(mp_get_online_mask() & ~cpu_num_to_mask(target_cpu));
  sender->Resume();

  // Stop the sender, and wait for it to finish, before |state| goes out of scope, even if an
  // assertion below fails.  If any mp_sync_exec call fails to return, the sender never observes
  // |stop|, and this test hangs.
  auto stop_sender = fit::defer([&state, sender] {
    state.stop.store(true, ktl::memory_order_relaxed);
    sender->Join(nullptr, ZX_TIME_INFINITE);
  });

  constexpr int kIterations = 20;
  for (int i = 0; i < kIterations; ++i) {
    ASSERT_OK(mp_unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
              "unplugging core failed");
    ASSERT_OK(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
    ASSERT_OK(hotplug_core(target_cpu), "hotplugging core failed");
    wait_for_cpu_active(target_cpu);
    ASSERT_OK(wait_for_cpu_dpc_runner(target_cpu), "waiting for core's dpc runner failed");
  }

  stop_sender.call();
  EXPECT_GT(state.calls.load(ktl::memory_order_relaxed), 0u);

  END_TEST;
}

static void wait_for_thread_state(const Thread& thread, thread_state expected_state) {
  zx_instant_mono_t print_time = zx_time_add_duration(current_mono_time(), ZX_SEC(5));
  while (true) {
    {
      SingleChainLockGuard guard{IrqSaveOption, thread.get_lock(),
                                 CLT_TAG("mp_hotplug_wait_for_thread_state")};
      if (thread.state() == expected_state) {
        return;
      }
    }
    if (current_mono_time() > print_time) {
      print_time = zx_time_add_duration(current_mono_time(), ZX_SEC(5));
      printf("Still waiting for thread to enter state %d, waiting 5 more seconds\n",
             static_cast<int>(expected_state));
    }
    Thread::Current::SleepRelative(ZX_USEC(100));
  }
}

// Regression test for https://fxbug.dev/578020457:
// Verify that threads hard-pinned to a single secondary CPU can be woken (both from an
// OwnedWaitQueue via Mutex release and from a WaitQueue via Event::Signal) while their pinned CPU
// is offline or remain THREAD_READY across unplug (running MigrateStage::Save before power-off),
// stay queued on that inactive CPU's scheduler without panicking or dereferencing INVALID_CPU, and
// execute on that CPU once it is hotplugged back online.
[[maybe_unused]] static bool mp_hotplug_unblock_pinned_while_offline_test() {
  BEGIN_TEST;

// TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
#if defined(__riscv)
  printf(
      "skipping test mp_hotplug_unblock_pinned_while_offline, hotplug only supported on x64 and "
      "arm64\n");
  END_TEST;
#endif

  const cpu_mask_t online_secondaries = mp_get_online_mask() & ~cpu_num_to_mask(BOOT_CPU_ID);
  if (online_secondaries == 0) {
    printf("skipping test mp_hotplug_unblock_pinned_while_offline, not enough online cpus\n");
    END_TEST;
  }
  const cpu_num_t target_cpu = highest_cpu_set(online_secondaries);

  auto restore_affinity = fit::defer(
      [previous_affinity = Thread::Current::SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID))] {
        Thread::Current::SetCpuAffinity(previous_affinity);
      });

  // Relaxed atomic loads/stores on |State| are synchronized by Event::Wait(),
  // wait_for_thread_state() / wait_for_cpu_offline() lock acquisitions, and Thread::Join().
  // Heap-allocate |State| to keep this function's kernel stack frame small.
  struct State {
    DECLARE_MUTEX(State) lock;
    Event holder_locked_event;
    Event release_lock_event;
    Event holder_released_event;
    Event wake_event;
    Event ready_pinned_started_event;
    ktl::atomic<bool> observe_post_hotplug_cpu{false};
    ktl::atomic<bool> stop_ready_pinned{false};
    ktl::atomic<cpu_num_t> mutex_waiter_before_cpu{INVALID_CPU};
    ktl::atomic<cpu_num_t> mutex_waiter_after_cpu{INVALID_CPU};
    ktl::atomic<cpu_num_t> event_waiter_before_cpu{INVALID_CPU};
    ktl::atomic<cpu_num_t> event_waiter_after_cpu{INVALID_CPU};
    ktl::atomic<cpu_num_t> ready_pinned_before_cpu{INVALID_CPU};
    ktl::atomic<cpu_num_t> ready_pinned_after_cpu{INVALID_CPU};
    ktl::atomic<uint32_t> migrate_save_calls{0};
    ktl::atomic<uint32_t> migrate_restore_calls{0};
    ktl::atomic<uint32_t> ready_migrate_save_calls{0};
    ktl::atomic<uint32_t> ready_migrate_restore_calls{0};
  };

  fbl::AllocChecker ac;
  auto state = ktl::make_unique<State>(&ac);
  ASSERT_TRUE(ac.check());

  Thread* mutex_holder = nullptr;
  Thread* mutex_waiter = nullptr;
  Thread* event_waiter = nullptr;
  Thread* ready_pinned = nullptr;

  auto cleanup_threads =
      fit::defer([state = state.get(), &mutex_holder, &mutex_waiter, &event_waiter, &ready_pinned] {
        state->stop_ready_pinned.store(true, ktl::memory_order_relaxed);
        state->release_lock_event.Signal();
        state->wake_event.Signal();
        if (mutex_holder != nullptr) {
          mutex_holder->Join(nullptr, ZX_TIME_INFINITE);
        }
        if (mutex_waiter != nullptr) {
          mutex_waiter->Join(nullptr, ZX_TIME_INFINITE);
        }
        if (event_waiter != nullptr) {
          event_waiter->Join(nullptr, ZX_TIME_INFINITE);
        }
        if (ready_pinned != nullptr) {
          ready_pinned->Join(nullptr, ZX_TIME_INFINITE);
        }
      });

  // Declared after |cleanup_threads| so |target_cpu| is brought back online first during unwinding,
  // allowing any threads pinned to |target_cpu| to run and exit when |cleanup_threads| joins them.
  auto restore_target_cpu = fit::defer([target_cpu] {
    if (!mp_is_cpu_online(target_cpu)) {
      const zx_status_t status = hotplug_core(target_cpu);
      ASSERT_MSG(status == ZX_OK, "hotplugging core %u failed: %d", target_cpu, status);
      wait_for_cpu_active(target_cpu);
      const zx_status_t dpc_status = wait_for_cpu_dpc_runner(target_cpu);
      ASSERT_MSG(dpc_status == ZX_OK, "waiting for core %u's dpc runner failed: %d", target_cpu,
                 dpc_status);
    }
  });

  mutex_holder = Thread::Create(
      "pinned-offline-mutex-holder",
      [](void* arg) -> int {
        auto* s = static_cast<State*>(arg);
        {
          Guard<Mutex> guard{&s->lock};
          s->holder_locked_event.Signal();
          s->release_lock_event.Wait();
        }
        s->holder_released_event.Signal();
        return 0;
      },
      state.get(), DEFAULT_PRIORITY);
  ASSERT_NONNULL(mutex_holder, "mutex_holder create failed");
  mutex_holder->SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID));
  mutex_holder->Resume();
  ASSERT_OK(state->holder_locked_event.Wait(), "waiting for holder_locked_event failed");

  mutex_waiter = Thread::Create(
      "pinned-offline-mutex-waiter",
      [](void* arg) -> int {
        auto* s = static_cast<State*>(arg);
        s->mutex_waiter_before_cpu.store(arch_curr_cpu_num(), ktl::memory_order_relaxed);
        Guard<Mutex> guard{&s->lock};
        s->mutex_waiter_after_cpu.store(arch_curr_cpu_num(), ktl::memory_order_relaxed);
        return 0;
      },
      state.get(), DEFAULT_PRIORITY);
  ASSERT_NONNULL(mutex_waiter, "mutex_waiter create failed");
  mutex_waiter->SetCpuAffinity(cpu_num_to_mask(target_cpu));
  mutex_waiter->Resume();

  event_waiter = Thread::Create(
      "pinned-offline-event-waiter",
      [](void* arg) -> int {
        auto* s = static_cast<State*>(arg);
        s->event_waiter_before_cpu.store(arch_curr_cpu_num(), ktl::memory_order_relaxed);
        s->wake_event.Wait();
        s->event_waiter_after_cpu.store(arch_curr_cpu_num(), ktl::memory_order_relaxed);
        return 0;
      },
      state.get(), DEFAULT_PRIORITY);
  ASSERT_NONNULL(event_waiter, "event_waiter create failed");
  event_waiter->SetCpuAffinity(cpu_num_to_mask(target_cpu));
  event_waiter->SetMigrateFn([state = state.get()](Thread*, Thread::MigrateStage stage) {
    if (stage == Thread::MigrateStage::Save) {
      state->migrate_save_calls.fetch_add(1, ktl::memory_order_relaxed);
    } else if (stage == Thread::MigrateStage::Restore) {
      state->migrate_restore_calls.fetch_add(1, ktl::memory_order_relaxed);
    }
  });
  event_waiter->Resume();

  ready_pinned = Thread::Create(
      "pinned-offline-ready-worker",
      [](void* arg) -> int {
        auto* s = static_cast<State*>(arg);
        s->ready_pinned_before_cpu.store(arch_curr_cpu_num(), ktl::memory_order_relaxed);
        s->ready_pinned_started_event.Signal();
        while (!s->stop_ready_pinned.load(ktl::memory_order_relaxed)) {
          if (s->observe_post_hotplug_cpu.load(ktl::memory_order_relaxed)) {
            s->ready_pinned_after_cpu.store(arch_curr_cpu_num(), ktl::memory_order_relaxed);
            break;
          }
          Thread::Current::Yield();
        }
        return 0;
      },
      state.get(), LOW_PRIORITY);
  ASSERT_NONNULL(ready_pinned, "ready_pinned create failed");
  ready_pinned->SetCpuAffinity(cpu_num_to_mask(target_cpu));
  ready_pinned->SetMigrateFn([state = state.get()](Thread*, Thread::MigrateStage stage) {
    if (stage == Thread::MigrateStage::Save) {
      state->ready_migrate_save_calls.fetch_add(1, ktl::memory_order_relaxed);
    } else if (stage == Thread::MigrateStage::Restore) {
      state->ready_migrate_restore_calls.fetch_add(1, ktl::memory_order_relaxed);
    }
  });
  ready_pinned->Resume();

  // Wait until both waiters have run on |target_cpu| and entered THREAD_BLOCKED, and
  // |ready_pinned| has started executing on |target_cpu|.
  wait_for_thread_state(*mutex_waiter, THREAD_BLOCKED);
  wait_for_thread_state(*event_waiter, THREAD_BLOCKED);
  ASSERT_OK(state->ready_pinned_started_event.Wait(),
            "waiting for ready_pinned_started_event failed");
  EXPECT_EQ(target_cpu, state->mutex_waiter_before_cpu.load(ktl::memory_order_relaxed));
  EXPECT_EQ(target_cpu, state->event_waiter_before_cpu.load(ktl::memory_order_relaxed));
  EXPECT_EQ(target_cpu, state->ready_pinned_before_cpu.load(ktl::memory_order_relaxed));

  // Take |target_cpu| offline while the waiters are blocked and |ready_pinned| is runnable on
  // |target_cpu| (preempted into THREAD_READY during unplug).
  ASSERT_OK(mp_unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
            "unplugging core failed");
  ASSERT_OK(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
  ASSERT_FALSE(Scheduler::PeekIsActive(target_cpu));
  wait_for_thread_state(*ready_pinned, THREAD_READY);
  EXPECT_EQ(1u, state->ready_migrate_save_calls.load(ktl::memory_order_relaxed));
  EXPECT_EQ(0u, state->ready_migrate_restore_calls.load(ktl::memory_order_relaxed));
  state->observe_post_hotplug_cpu.store(true, ktl::memory_order_relaxed);

  // Unblock both pinned waiters while |target_cpu| is offline:
  // 1. Releasing |state->lock| wakes |mutex_waiter| via OwnedWaitQueue::WakeThreadsLocked.
  // 2. Signaling |state->wake_event| wakes |event_waiter| via WaitQueue::WakeOne.
  state->release_lock_event.Signal();
  ASSERT_OK(state->holder_released_event.Wait(), "waiting for holder_released_event failed");
  state->wake_event.Signal();

  // All three pinned threads should now be queued in THREAD_READY on inactive |target_cpu|, and
  // must not have run on any other CPU while |target_cpu| is offline.
  wait_for_thread_state(*mutex_waiter, THREAD_READY);
  wait_for_thread_state(*event_waiter, THREAD_READY);
  EXPECT_EQ(INVALID_CPU, state->mutex_waiter_after_cpu.load(ktl::memory_order_relaxed));
  EXPECT_EQ(INVALID_CPU, state->event_waiter_after_cpu.load(ktl::memory_order_relaxed));
  EXPECT_EQ(INVALID_CPU, state->ready_pinned_after_cpu.load(ktl::memory_order_relaxed));

  // Hotplug |target_cpu| back online and verify that all queued pinned threads execute on it.
  ASSERT_OK(hotplug_core(target_cpu), "hotplugging core failed");
  wait_for_cpu_active(target_cpu);
  ASSERT_OK(wait_for_cpu_dpc_runner(target_cpu), "waiting for core's dpc runner failed");

  ASSERT_OK(mutex_holder->Join(nullptr, ZX_TIME_INFINITE), "joining mutex_holder failed");
  mutex_holder = nullptr;
  ASSERT_OK(mutex_waiter->Join(nullptr, ZX_TIME_INFINITE), "joining mutex_waiter failed");
  mutex_waiter = nullptr;
  ASSERT_OK(event_waiter->Join(nullptr, ZX_TIME_INFINITE), "joining event_waiter failed");
  event_waiter = nullptr;
  ASSERT_OK(ready_pinned->Join(nullptr, ZX_TIME_INFINITE), "joining ready_pinned failed");
  ready_pinned = nullptr;

  EXPECT_EQ(target_cpu, state->mutex_waiter_after_cpu.load(ktl::memory_order_relaxed));
  EXPECT_EQ(target_cpu, state->event_waiter_after_cpu.load(ktl::memory_order_relaxed));
  EXPECT_EQ(target_cpu, state->ready_pinned_after_cpu.load(ktl::memory_order_relaxed));
  EXPECT_EQ(1u, state->migrate_save_calls.load(ktl::memory_order_relaxed));
  EXPECT_EQ(1u, state->migrate_restore_calls.load(ktl::memory_order_relaxed));
  EXPECT_EQ(1u, state->ready_migrate_save_calls.load(ktl::memory_order_relaxed));
  EXPECT_EQ(1u, state->ready_migrate_restore_calls.load(ktl::memory_order_relaxed));

  END_TEST;
}

// Regression test for https://fxbug.dev/578020457:
// Verify that unpinned threads with migration functions neither livelock nor spin-deadlock when
// woken (or unpinned via SetCpuAffinity) while their last_cpu is offline or deactivating:
// 1. When last_cpu is already offline (!is_active && !cpu_deactivating) and a migration function is
//    installed after CallMigrateFnForCpu has already completed, waking a blocked thread or widening
//    an offline-enqueued thread's affinity places it directly onto an active CPU without hanging or
//    running MigrateStage::Save on the wrong CPU.
// 2. When last_cpu is deactivating in Scheduler::MigrateUnpinnedThreads, concurrent wakeups from
//    other CPUs queue onto last_cpu's save_state_list_ without deadlocking against
//    Thread::CallMigrateFnForCpu and preserve strict Save-on-last_cpu / Restore ordering.
[[maybe_unused]] static bool mp_hotplug_unblock_unpinned_migrate_while_offline_test() {
  BEGIN_TEST;

// TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
#if defined(__riscv)
  printf(
      "skipping test mp_hotplug_unblock_unpinned_migrate_while_offline, hotplug only supported on "
      "x64 and arm64\n");
  END_TEST;
#endif

  const cpu_mask_t online_secondaries = mp_get_online_mask() & ~cpu_num_to_mask(BOOT_CPU_ID);
  if (online_secondaries == 0) {
    printf(
        "skipping test mp_hotplug_unblock_unpinned_migrate_while_offline, not enough online "
        "cpus\n");
    END_TEST;
  }
  const cpu_num_t target_cpu = highest_cpu_set(online_secondaries);

  auto restore_affinity = fit::defer(
      [previous_affinity = Thread::Current::SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID))] {
        Thread::Current::SetCpuAffinity(previous_affinity);
      });

  // Relaxed atomic loads/stores on |State| are synchronized by Event::Wait(),
  // wait_for_thread_state() / wait_for_cpu_offline() lock acquisitions, and Thread::Join();
  // per-worker |save_pending| and |exited| fields are accessed exclusively under each worker
  // thread's ChainLock inside CallMigrateFnLocked. Heap-allocate |State| to keep this function's
  // kernel stack frame small.
  struct State;
  struct WorkerSlot {
    Thread* thread{nullptr};
    State* state{nullptr};
    Event started_event;
    AutounsignalEvent wake_event;
    ktl::atomic<uint64_t> wakeups{0};
    ktl::atomic<uint64_t> save_calls{0};
    ktl::atomic<uint64_t> restore_calls{0};
    ktl::atomic<bool> migrate_order_ok{true};
    bool save_pending{false};
    bool exited{false};
  };

  static constexpr size_t kNumWorkers = 4;
  struct State {
    Event offline_waiter_started_event;
    Event offline_waiter_wake_event;
    Event pinned_enqueued_started_event;
    ktl::atomic<bool> unpinned_observe_cpu{false};
    ktl::atomic<bool> stop_pinned_enqueued{false};
    ktl::atomic<bool> stop_workers{false};
    ktl::atomic<cpu_num_t> offline_waiter_before_cpu{INVALID_CPU};
    ktl::atomic<cpu_num_t> offline_waiter_after_cpu{INVALID_CPU};
    ktl::atomic<cpu_num_t> pinned_enqueued_before_cpu{INVALID_CPU};
    ktl::atomic<cpu_num_t> pinned_enqueued_after_cpu{INVALID_CPU};
    ktl::atomic<uint32_t> offline_waiter_save_calls{0};
    ktl::atomic<uint32_t> offline_waiter_restore_calls{0};
    ktl::atomic<uint32_t> offline_waiter_exiting_calls{0};
    ktl::atomic<bool> offline_waiter_migrate_ok{true};
    ktl::atomic<uint32_t> pinned_enqueued_save_calls{0};
    ktl::atomic<uint32_t> pinned_enqueued_restore_calls{0};
    ktl::atomic<uint32_t> pinned_enqueued_exiting_calls{0};
    ktl::atomic<bool> pinned_enqueued_migrate_ok{true};
    WorkerSlot workers[kNumWorkers];
  };

  fbl::AllocChecker ac;
  auto state = ktl::make_unique<State>(&ac);
  ASSERT_TRUE(ac.check());

  Thread* offline_waiter = nullptr;
  Thread* pinned_enqueued = nullptr;
  Thread* waker = nullptr;

  auto cleanup_threads =
      fit::defer([state = state.get(), &offline_waiter, &pinned_enqueued, &waker] {
        state->stop_pinned_enqueued.store(true, ktl::memory_order_relaxed);
        state->stop_workers.store(true, ktl::memory_order_relaxed);
        state->offline_waiter_wake_event.Signal();
        for (WorkerSlot& slot : state->workers) {
          slot.wake_event.Signal();
        }
        if (waker != nullptr) {
          waker->Join(nullptr, ZX_TIME_INFINITE);
          waker = nullptr;
        }
        for (WorkerSlot& slot : state->workers) {
          if (slot.thread != nullptr) {
            slot.wake_event.Signal();
            slot.thread->Join(nullptr, ZX_TIME_INFINITE);
            slot.thread = nullptr;
          }
        }
        if (offline_waiter != nullptr) {
          offline_waiter->Join(nullptr, ZX_TIME_INFINITE);
          offline_waiter = nullptr;
        }
        if (pinned_enqueued != nullptr) {
          pinned_enqueued->Join(nullptr, ZX_TIME_INFINITE);
          pinned_enqueued = nullptr;
        }
      });

  // Declared after |cleanup_threads| so |target_cpu| is brought back online first during unwinding.
  auto restore_target_cpu = fit::defer([target_cpu] {
    if (!mp_is_cpu_online(target_cpu)) {
      const zx_status_t status = hotplug_core(target_cpu);
      ASSERT_MSG(status == ZX_OK, "hotplugging core %u failed: %d", target_cpu, status);
      wait_for_cpu_active(target_cpu);
      const zx_status_t dpc_status = wait_for_cpu_dpc_runner(target_cpu);
      ASSERT_MSG(dpc_status == ZX_OK, "waiting for core %u's dpc runner failed: %d", target_cpu,
                 dpc_status);
    }
  });

  // Phase 1: Wake an unpinned thread (and unpin an offline-enqueued thread) whose |last_cpu| is
  // already offline and had a migration function installed after |CallMigrateFnForCpu| ran.
  offline_waiter = Thread::Create(
      "unpinned-offline-migrate-waiter",
      [](void* arg) -> int {
        auto* s = static_cast<State*>(arg);
        s->offline_waiter_before_cpu.store(arch_curr_cpu_num(), ktl::memory_order_relaxed);
        s->offline_waiter_started_event.Signal();
        s->offline_waiter_wake_event.Wait();
        s->offline_waiter_after_cpu.store(arch_curr_cpu_num(), ktl::memory_order_relaxed);
        return 0;
      },
      state.get(), DEFAULT_PRIORITY);
  ASSERT_NONNULL(offline_waiter, "offline_waiter create failed");
  offline_waiter->SetSoftCpuAffinity(cpu_num_to_mask(target_cpu));
  offline_waiter->Resume();

  pinned_enqueued = Thread::Create(
      "unpin-while-offline-migrate-worker",
      [](void* arg) -> int {
        auto* s = static_cast<State*>(arg);
        s->pinned_enqueued_before_cpu.store(arch_curr_cpu_num(), ktl::memory_order_relaxed);
        s->pinned_enqueued_started_event.Signal();
        while (!s->stop_pinned_enqueued.load(ktl::memory_order_relaxed)) {
          if (s->unpinned_observe_cpu.load(ktl::memory_order_relaxed)) {
            s->pinned_enqueued_after_cpu.store(arch_curr_cpu_num(), ktl::memory_order_relaxed);
            break;
          }
          Thread::Current::Yield();
        }
        return 0;
      },
      state.get(), LOW_PRIORITY);
  ASSERT_NONNULL(pinned_enqueued, "pinned_enqueued create failed");
  pinned_enqueued->SetCpuAffinity(cpu_num_to_mask(target_cpu));
  pinned_enqueued->Resume();

  ASSERT_OK(state->offline_waiter_started_event.Wait(),
            "waiting for offline_waiter_started_event failed");
  wait_for_thread_state(*offline_waiter, THREAD_BLOCKED);
  ASSERT_OK(state->pinned_enqueued_started_event.Wait(),
            "waiting for pinned_enqueued_started_event failed");
  EXPECT_EQ(target_cpu, state->offline_waiter_before_cpu.load(ktl::memory_order_relaxed));
  EXPECT_EQ(target_cpu, state->pinned_enqueued_before_cpu.load(ktl::memory_order_relaxed));

  ASSERT_OK(mp_unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
            "unplugging core failed");
  ASSERT_OK(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
  ASSERT_FALSE(Scheduler::PeekIsActive(target_cpu));
  wait_for_thread_state(*pinned_enqueued, THREAD_READY);

  // Install migration functions while |target_cpu| is offline so |last_cpu == target_cpu| with
  // |!migrate_pending|.
  offline_waiter->SetMigrateFn([state = state.get()](Thread* t, Thread::MigrateStage stage) {
    ChainLockTransaction::AssertActive();
    t->get_lock().AssertHeld();
    switch (stage) {
      case Thread::MigrateStage::Save:
        if (arch_curr_cpu_num() != t->LastCpuLocked()) {
          state->offline_waiter_migrate_ok.store(false, ktl::memory_order_relaxed);
        }
        state->offline_waiter_save_calls.fetch_add(1, ktl::memory_order_relaxed);
        break;
      case Thread::MigrateStage::Restore:
        state->offline_waiter_restore_calls.fetch_add(1, ktl::memory_order_relaxed);
        state->offline_waiter_migrate_ok.store(false, ktl::memory_order_relaxed);
        break;
      case Thread::MigrateStage::Exiting:
        state->offline_waiter_exiting_calls.fetch_add(1, ktl::memory_order_relaxed);
        break;
    }
  });

  state->offline_waiter_wake_event.Signal();
  ASSERT_OK(offline_waiter->Join(nullptr, ZX_TIME_INFINITE), "joining offline_waiter failed");
  offline_waiter = nullptr;

  const cpu_num_t waiter_after_cpu =
      state->offline_waiter_after_cpu.load(ktl::memory_order_relaxed);
  EXPECT_NE(INVALID_CPU, waiter_after_cpu);
  EXPECT_NE(target_cpu, waiter_after_cpu);
  EXPECT_EQ(0u, state->offline_waiter_save_calls.load(ktl::memory_order_relaxed));
  EXPECT_EQ(0u, state->offline_waiter_restore_calls.load(ktl::memory_order_relaxed));
  EXPECT_EQ(1u, state->offline_waiter_exiting_calls.load(ktl::memory_order_relaxed));
  EXPECT_TRUE(state->offline_waiter_migrate_ok.load(ktl::memory_order_relaxed));

  pinned_enqueued->SetMigrateFn([state = state.get()](Thread* t, Thread::MigrateStage stage) {
    ChainLockTransaction::AssertActive();
    t->get_lock().AssertHeld();
    switch (stage) {
      case Thread::MigrateStage::Save:
        if (arch_curr_cpu_num() != t->LastCpuLocked()) {
          state->pinned_enqueued_migrate_ok.store(false, ktl::memory_order_relaxed);
        }
        state->pinned_enqueued_save_calls.fetch_add(1, ktl::memory_order_relaxed);
        break;
      case Thread::MigrateStage::Restore:
        state->pinned_enqueued_restore_calls.fetch_add(1, ktl::memory_order_relaxed);
        state->pinned_enqueued_migrate_ok.store(false, ktl::memory_order_relaxed);
        break;
      case Thread::MigrateStage::Exiting:
        state->pinned_enqueued_exiting_calls.fetch_add(1, ktl::memory_order_relaxed);
        break;
    }
  });

  state->unpinned_observe_cpu.store(true, ktl::memory_order_relaxed);
  pinned_enqueued->SetCpuAffinity(CPU_MASK_ALL);
  ASSERT_OK(pinned_enqueued->Join(nullptr, ZX_TIME_INFINITE), "joining pinned_enqueued failed");
  pinned_enqueued = nullptr;

  const cpu_num_t enqueued_after_cpu =
      state->pinned_enqueued_after_cpu.load(ktl::memory_order_relaxed);
  EXPECT_NE(INVALID_CPU, enqueued_after_cpu);
  EXPECT_NE(target_cpu, enqueued_after_cpu);
  EXPECT_EQ(0u, state->pinned_enqueued_save_calls.load(ktl::memory_order_relaxed));
  EXPECT_EQ(0u, state->pinned_enqueued_restore_calls.load(ktl::memory_order_relaxed));
  EXPECT_EQ(1u, state->pinned_enqueued_exiting_calls.load(ktl::memory_order_relaxed));
  EXPECT_TRUE(state->pinned_enqueued_migrate_ok.load(ktl::memory_order_relaxed));

  ASSERT_OK(hotplug_core(target_cpu), "hotplugging core failed");
  wait_for_cpu_active(target_cpu);
  ASSERT_OK(wait_for_cpu_dpc_runner(target_cpu), "waiting for core's dpc runner failed");

  // Phase 2: Repeatedly unplug and hotplug |target_cpu| while unpinned workers with migration
  // functions block on |target_cpu| and are woken concurrently from |BOOT_CPU_ID|.
  for (size_t i = 0; i < kNumWorkers; ++i) {
    WorkerSlot& slot = state->workers[i];
    slot.state = state.get();
    slot.thread = Thread::Create(
        "unpinned-deactivating-migrate-worker",
        [](void* arg) -> int {
          auto* w = static_cast<WorkerSlot*>(arg);
          State* s = w->state;
          w->started_event.Signal();
          uint64_t iter = 0;
          while (!s->stop_workers.load(ktl::memory_order_relaxed)) {
            if ((++iter & 1u) == 0u) {
              w->wake_event.Wait(Deadline::after_mono(ZX_USEC(150)));
            } else {
              Thread::Current::SleepRelative(ZX_USEC(50));
            }
            w->wakeups.fetch_add(1, ktl::memory_order_relaxed);
          }
          return 0;
        },
        &slot, DEFAULT_PRIORITY);
    ASSERT_NONNULL(slot.thread, "worker create failed");
    slot.thread->SetCpuAffinity(cpu_num_to_mask(target_cpu));
    slot.thread->SetMigrateFn([&slot](Thread* t, Thread::MigrateStage stage) {
      ChainLockTransaction::AssertActive();
      t->get_lock().AssertHeld();
      switch (stage) {
        case Thread::MigrateStage::Save:
          if (slot.exited || slot.save_pending || arch_curr_cpu_num() != t->LastCpuLocked()) {
            slot.migrate_order_ok.store(false, ktl::memory_order_relaxed);
          }
          slot.save_pending = true;
          slot.save_calls.fetch_add(1, ktl::memory_order_relaxed);
          break;
        case Thread::MigrateStage::Restore:
          if (slot.exited || !slot.save_pending) {
            slot.migrate_order_ok.store(false, ktl::memory_order_relaxed);
          }
          slot.save_pending = false;
          slot.restore_calls.fetch_add(1, ktl::memory_order_relaxed);
          break;
        case Thread::MigrateStage::Exiting:
          if (slot.exited || slot.save_pending) {
            slot.migrate_order_ok.store(false, ktl::memory_order_relaxed);
          }
          slot.exited = true;
          break;
      }
    });
    slot.thread->Resume();
  }

  for (WorkerSlot& slot : state->workers) {
    ASSERT_OK(slot.started_event.Wait(), "waiting for worker to start on target_cpu failed");
    slot.thread->SetCpuAffinity(CPU_MASK_ALL);
    slot.thread->SetSoftCpuAffinity(cpu_num_to_mask(target_cpu));
  }

  waker = Thread::Create(
      "unpinned-deactivating-migrate-waker",
      [](void* arg) -> int {
        auto* s = static_cast<State*>(arg);
        while (!s->stop_workers.load(ktl::memory_order_relaxed)) {
          for (WorkerSlot& slot : s->workers) {
            slot.wake_event.Signal();
          }
          Thread::Current::SleepRelative(ZX_USEC(25));
        }
        return 0;
      },
      state.get(), DEFAULT_PRIORITY);
  ASSERT_NONNULL(waker, "waker create failed");
  waker->SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID));
  waker->Resume();

  constexpr int kUnplugCycles = 20;
  for (int i = 0; i < kUnplugCycles; ++i) {
    Thread::Current::SleepRelative(ZX_USEC(250));
    ASSERT_OK(mp_unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
              "unplugging core failed");
    ASSERT_OK(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
    ASSERT_OK(hotplug_core(target_cpu), "hotplugging core failed");
    wait_for_cpu_active(target_cpu);
    ASSERT_OK(wait_for_cpu_dpc_runner(target_cpu), "waiting for core's dpc runner failed");
  }

  cleanup_threads.call();

  uint64_t total_wakeups = 0;
  uint64_t total_save_calls = 0;
  uint64_t total_restore_calls = 0;
  for (const WorkerSlot& slot : state->workers) {
    total_wakeups += slot.wakeups.load(ktl::memory_order_relaxed);
    total_save_calls += slot.save_calls.load(ktl::memory_order_relaxed);
    total_restore_calls += slot.restore_calls.load(ktl::memory_order_relaxed);
    EXPECT_TRUE(slot.migrate_order_ok.load(ktl::memory_order_relaxed));
  }
  EXPECT_GE(total_wakeups, kNumWorkers);
  EXPECT_GE(total_save_calls, kNumWorkers);
  EXPECT_EQ(total_save_calls, total_restore_calls);

  END_TEST;
}

// Regression test for https://fxbug.dev/578020457 and https://fxbug.dev/448121736:
// Repeatedly unplug and hotplug a secondary CPU while unpinned fair threads of widely differing
// weights (driving fair_affine_transform.slope < 1.0) actively run on and migrate off that CPU.
// Verifies that Scheduler::MigrateUnpinnedThreads converts fair start/finish times from variable to
// monotonic time only once rather than corrupting the fair timeline across unplug cycles.
[[maybe_unused]] static bool mp_hotplug_migrate_fair_timeline_test() {
  BEGIN_TEST;

// TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
#if defined(__riscv)
  printf(
      "skipping test mp_hotplug_migrate_fair_timeline, hotplug only supported on x64 and arm64\n");
  END_TEST;
#endif

  const cpu_mask_t online_secondaries = mp_get_online_mask() & ~cpu_num_to_mask(BOOT_CPU_ID);
  if (online_secondaries == 0) {
    printf("skipping test mp_hotplug_migrate_fair_timeline, not enough online cpus\n");
    END_TEST;
  }
  const cpu_num_t target_cpu = highest_cpu_set(online_secondaries);

  auto restore_affinity = fit::defer(
      [previous_affinity = Thread::Current::SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID))] {
        Thread::Current::SetCpuAffinity(previous_affinity);
      });

  // |stop| is a monotonic cancellation flag and |iterations| is read after Thread::Join().
  struct State {
    ktl::atomic<bool> stop{false};
    ktl::atomic<uint64_t> iterations{0};
  } state;

  // Combining LOWEST_PRIORITY and HIGHEST_PRIORITY fair threads with soft affinity preferring
  // |target_cpu| guarantees min_weight < weight_total * kMinFairUtilization on |target_cpu|,
  // forcing fair_affine_transform.slope well below 1.0 while keeping hard_affinity unpinned so
  // MigrateUnpinnedThreads migrates them on each unplug cycle.
  constexpr int kPriorities[] = {
      LOWEST_PRIORITY, LOWEST_PRIORITY, DEFAULT_PRIORITY, HIGHEST_PRIORITY, HIGHEST_PRIORITY,
  };
  Thread* workers[ktl::size(kPriorities)] = {};

  auto stop_workers = fit::defer([&] {
    state.stop.store(true, ktl::memory_order_relaxed);
    for (Thread*& worker : workers) {
      if (worker != nullptr) {
        worker->Join(nullptr, ZX_TIME_INFINITE);
        worker = nullptr;
      }
    }
  });

  auto restore_target_cpu = fit::defer([target_cpu] {
    if (!mp_is_cpu_online(target_cpu)) {
      const zx_status_t status = hotplug_core(target_cpu);
      ASSERT_MSG(status == ZX_OK, "hotplugging core %u failed: %d", target_cpu, status);
      wait_for_cpu_active(target_cpu);
      const zx_status_t dpc_status = wait_for_cpu_dpc_runner(target_cpu);
      ASSERT_MSG(dpc_status == ZX_OK, "waiting for core %u's dpc runner failed: %d", target_cpu,
                 dpc_status);
    }
  });

  for (size_t i = 0; i < ktl::size(kPriorities); ++i) {
    workers[i] = Thread::Create(
        "mp-hotplug-fair-timeline-worker",
        [](void* arg) -> int {
          auto* s = static_cast<State*>(arg);
          while (!s->stop.load(ktl::memory_order_relaxed)) {
            s->iterations.fetch_add(1, ktl::memory_order_relaxed);
            Thread::Current::Yield();
          }
          return 0;
        },
        &state, kPriorities[i]);
    ASSERT_NONNULL(workers[i], "worker create failed");
    workers[i]->SetSoftCpuAffinity(cpu_num_to_mask(target_cpu));
    workers[i]->Resume();
  }

  constexpr int kUnplugCycles = 25;
  for (int i = 0; i < kUnplugCycles; ++i) {
    Thread::Current::SleepRelative(ZX_MSEC(1));
    ASSERT_OK(mp_unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
              "unplugging core failed");
    ASSERT_OK(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
    ASSERT_OK(hotplug_core(target_cpu), "hotplugging core failed");
    wait_for_cpu_active(target_cpu);
    ASSERT_OK(wait_for_cpu_dpc_runner(target_cpu), "waiting for core's dpc runner failed");
  }

  stop_workers.call();
  EXPECT_GT(state.iterations.load(ktl::memory_order_relaxed), 0u);

  END_TEST;
}

// Regression test for https://fxbug.dev/578020457:
// Repeatedly unplug and hotplug |target_cpu| while worker threads on other active CPUs transition
// their own soft affinity to |target_cpu| and yield. If |target_cpu| deactivates after
// Scheduler::NeedsMigrationLocal initiates migration away from the current CPU, target selection
// falls back to hard affinity and may re-select the current CPU; verifies that RescheduleCommon
// does not assert, invoke a spurious MigrateStage::Save without leaving the CPU, or lose the local
// reschedule.
[[maybe_unused]] static bool mp_hotplug_soft_affinity_migration_fallback_test() {
  BEGIN_TEST;

// TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
#if defined(__riscv)
  printf(
      "skipping test mp_hotplug_soft_affinity_migration_fallback, hotplug only supported on x64 "
      "and arm64\n");
  END_TEST;
#endif

  const cpu_mask_t online_secondaries = mp_get_online_mask() & ~cpu_num_to_mask(BOOT_CPU_ID);
  if (online_secondaries == 0) {
    printf("skipping test mp_hotplug_soft_affinity_migration_fallback, not enough online cpus\n");
    END_TEST;
  }
  const cpu_num_t target_cpu = highest_cpu_set(online_secondaries);

  auto restore_affinity = fit::defer(
      [previous_affinity = Thread::Current::SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID))] {
        Thread::Current::SetCpuAffinity(previous_affinity);
      });

  // |stop| is a monotonic cancellation flag, |probe_target| is an advisory toggle carrying no
  // dependent state, and |iterations|, |migrate_save_calls|, and |migrate_order_ok| are read after
  // Thread::Join(). Heap-allocate |State| to keep this function's kernel stack frame small.
  struct State;
  struct WorkerSlot {
    Thread* thread{nullptr};
    State* state{nullptr};
    cpu_num_t home_cpu{INVALID_CPU};
    ktl::atomic<uint64_t> migrate_save_calls{0};
    ktl::atomic<bool> migrate_order_ok{true};
    bool save_pending{false};
    bool exited{false};
  };

  struct State {
    ktl::atomic<bool> stop{false};
    ktl::atomic<bool> probe_target{false};
    ktl::atomic<uint64_t> iterations{0};
    WorkerSlot workers[SMP_MAX_CPUS]{};
    cpu_num_t target_cpu{INVALID_CPU};
  };

  fbl::AllocChecker ac;
  auto state = ktl::make_unique<State>(&ac);
  ASSERT_TRUE(ac.check(), "failed to allocate test state");
  state->target_cpu = target_cpu;

  auto stop_workers = fit::defer([state = state.get()] {
    state->stop.store(true, ktl::memory_order_relaxed);
    for (WorkerSlot& slot : state->workers) {
      if (slot.thread != nullptr) {
        slot.thread->Join(nullptr, ZX_TIME_INFINITE);
        slot.thread = nullptr;
      }
    }
  });

  auto restore_target_cpu = fit::defer([target_cpu] {
    if (!mp_is_cpu_online(target_cpu)) {
      const zx_status_t status = hotplug_core(target_cpu);
      ASSERT_MSG(status == ZX_OK, "hotplugging core %u failed: %d", target_cpu, status);
      wait_for_cpu_active(target_cpu);
      const zx_status_t dpc_status = wait_for_cpu_dpc_runner(target_cpu);
      ASSERT_MSG(dpc_status == ZX_OK, "waiting for core %u's dpc runner failed: %d", target_cpu,
                 dpc_status);
    }
  });

  cpu_mask_t remaining_cpus = mp_get_online_mask() & ~cpu_num_to_mask(target_cpu);
  size_t worker_index = 0;
  while (remaining_cpus != 0) {
    const cpu_num_t home_cpu = lowest_cpu_set(remaining_cpus);
    remaining_cpus &= ~cpu_num_to_mask(home_cpu);

    WorkerSlot& slot = state->workers[worker_index];
    slot.state = state.get();
    slot.home_cpu = home_cpu;
    slot.thread = Thread::Create(
        "mp-hotplug-soft-affinity-worker",
        [](void* arg) -> int {
          auto* w = static_cast<WorkerSlot*>(arg);
          State* s = w->state;
          const cpu_mask_t home_mask = cpu_num_to_mask(w->home_cpu);
          const cpu_mask_t target_mask = cpu_num_to_mask(s->target_cpu);
          Thread* self = Thread::Current::Get();
          uint64_t step = 0;
          while (!s->stop.load(ktl::memory_order_relaxed)) {
            if (s->probe_target.load(ktl::memory_order_relaxed) && ((++step & 1u) == 0u)) {
              self->SetSoftCpuAffinity(target_mask);
            } else {
              self->SetSoftCpuAffinity(home_mask);
            }
            s->iterations.fetch_add(1, ktl::memory_order_relaxed);
            Thread::Current::Yield();
          }
          return 0;
        },
        &slot, DEFAULT_PRIORITY);
    ASSERT_NONNULL(slot.thread, "worker create failed");
    slot.thread->SetCpuAffinity(cpu_num_to_mask(home_cpu) | cpu_num_to_mask(target_cpu));
    slot.thread->SetSoftCpuAffinity(cpu_num_to_mask(home_cpu));
    if ((worker_index % 2) == 0) {
      slot.thread->SetMigrateFn([&slot](Thread*, Thread::MigrateStage stage) {
        switch (stage) {
          case Thread::MigrateStage::Save:
            if (slot.exited || slot.save_pending) {
              slot.migrate_order_ok.store(false, ktl::memory_order_relaxed);
            }
            slot.save_pending = true;
            slot.migrate_save_calls.fetch_add(1, ktl::memory_order_relaxed);
            break;
          case Thread::MigrateStage::Restore:
            if (slot.exited || !slot.save_pending) {
              slot.migrate_order_ok.store(false, ktl::memory_order_relaxed);
            }
            slot.save_pending = false;
            break;
          case Thread::MigrateStage::Exiting:
            if (slot.exited) {
              slot.migrate_order_ok.store(false, ktl::memory_order_relaxed);
            }
            slot.exited = true;
            break;
        }
      });
    }
    slot.thread->Resume();
    ++worker_index;
  }

  constexpr int kUnplugCycles = 30;
  for (int i = 0; i < kUnplugCycles; ++i) {
    state->probe_target.store(false, ktl::memory_order_relaxed);
    Thread::Current::SleepRelative(ZX_USEC(250));
    state->probe_target.store(true, ktl::memory_order_relaxed);
    ASSERT_OK(mp_unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
              "unplugging core failed");
    state->probe_target.store(false, ktl::memory_order_relaxed);
    ASSERT_OK(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
    ASSERT_OK(hotplug_core(target_cpu), "hotplugging core failed");
    wait_for_cpu_active(target_cpu);
    ASSERT_OK(wait_for_cpu_dpc_runner(target_cpu), "waiting for core's dpc runner failed");
  }

  stop_workers.call();
  EXPECT_GT(state->iterations.load(ktl::memory_order_relaxed), 0u);
  uint64_t total_migrate_save_calls = 0;
  for (const WorkerSlot& slot : state->workers) {
    total_migrate_save_calls += slot.migrate_save_calls.load(ktl::memory_order_relaxed);
    EXPECT_TRUE(slot.migrate_order_ok.load(ktl::memory_order_relaxed));
  }
  EXPECT_GT(total_migrate_save_calls, 0u);

  END_TEST;
}

// Regression test for https://fxbug.dev/578020457:
// Concurrently mutate soft and hard CPU affinity on runnable worker threads queued on |target_cpu|
// while repeatedly unplugging and hotplugging |target_cpu|. Verifies that
// Scheduler::MigrateUnpinnedThreads and Scheduler::SetCpuAffinity do not race on run-queue removal,
// unblock_list_node_, or thread migration state when queue_lock_ is released.
[[maybe_unused]] static bool mp_hotplug_set_affinity_during_unplug_test() {
  BEGIN_TEST;

// TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
#if defined(__riscv)
  printf(
      "skipping test mp_hotplug_set_affinity_during_unplug, hotplug only supported on x64 and "
      "arm64\n");
  END_TEST;
#endif

  const cpu_mask_t online_secondaries = mp_get_online_mask() & ~cpu_num_to_mask(BOOT_CPU_ID);
  if (online_secondaries == 0) {
    printf("skipping test mp_hotplug_set_affinity_during_unplug, not enough online cpus\n");
    END_TEST;
  }
  const cpu_num_t target_cpu = highest_cpu_set(online_secondaries);

  auto restore_affinity = fit::defer(
      [previous_affinity = Thread::Current::SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID))] {
        Thread::Current::SetCpuAffinity(previous_affinity);
      });

  // |stop| is a monotonic cancellation flag; counters are checked after Thread::Join().
  struct State {
    ktl::atomic<bool> stop{false};
    ktl::atomic<uint64_t> worker_iterations{0};
    ktl::atomic<uint64_t> affinity_updates{0};
    Thread* workers[4]{};
    cpu_num_t target_cpu{INVALID_CPU};
  } state;
  state.target_cpu = target_cpu;

  Thread* mutator = nullptr;
  auto cleanup_threads = fit::defer([&] {
    state.stop.store(true, ktl::memory_order_relaxed);
    if (mutator != nullptr) {
      mutator->Join(nullptr, ZX_TIME_INFINITE);
      mutator = nullptr;
    }
    for (Thread*& worker : state.workers) {
      if (worker != nullptr) {
        worker->Join(nullptr, ZX_TIME_INFINITE);
        worker = nullptr;
      }
    }
  });

  auto restore_target_cpu = fit::defer([target_cpu] {
    if (!mp_is_cpu_online(target_cpu)) {
      const zx_status_t status = hotplug_core(target_cpu);
      ASSERT_MSG(status == ZX_OK, "hotplugging core %u failed: %d", target_cpu, status);
      wait_for_cpu_active(target_cpu);
      const zx_status_t dpc_status = wait_for_cpu_dpc_runner(target_cpu);
      ASSERT_MSG(dpc_status == ZX_OK, "waiting for core %u's dpc runner failed: %d", target_cpu,
                 dpc_status);
    }
  });

  for (Thread*& worker : state.workers) {
    worker = Thread::Create(
        "mp-hotplug-affinity-worker",
        [](void* arg) -> int {
          auto* s = static_cast<State*>(arg);
          while (!s->stop.load(ktl::memory_order_relaxed)) {
            s->worker_iterations.fetch_add(1, ktl::memory_order_relaxed);
            Thread::Current::Yield();
          }
          return 0;
        },
        &state, DEFAULT_PRIORITY);
    ASSERT_NONNULL(worker, "worker create failed");
    worker->SetSoftCpuAffinity(cpu_num_to_mask(target_cpu));
    worker->Resume();
  }

  mutator = Thread::Create(
      "mp-hotplug-affinity-mutator",
      [](void* arg) -> int {
        auto* s = static_cast<State*>(arg);
        const cpu_mask_t target_mask = cpu_num_to_mask(s->target_cpu);
        const cpu_mask_t boot_or_target = cpu_num_to_mask(BOOT_CPU_ID) | target_mask;
        uint64_t step = 0;
        while (!s->stop.load(ktl::memory_order_relaxed)) {
          for (Thread* worker : s->workers) {
            worker->SetSoftCpuAffinity((step & 1u) ? target_mask : CPU_MASK_ALL);
            worker->SetCpuAffinity((step & 2u) ? boot_or_target : CPU_MASK_ALL);
            s->affinity_updates.fetch_add(1, ktl::memory_order_relaxed);
          }
          ++step;
          Thread::Current::Yield();
        }
        return 0;
      },
      &state, DEFAULT_PRIORITY);
  ASSERT_NONNULL(mutator, "mutator create failed");
  mutator->SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID));
  mutator->Resume();

  constexpr int kUnplugCycles = 20;
  for (int i = 0; i < kUnplugCycles; ++i) {
    Thread::Current::SleepRelative(ZX_MSEC(1));
    ASSERT_OK(mp_unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
              "unplugging core failed");
    ASSERT_OK(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
    ASSERT_OK(hotplug_core(target_cpu), "hotplugging core failed");
    wait_for_cpu_active(target_cpu);
    ASSERT_OK(wait_for_cpu_dpc_runner(target_cpu), "waiting for core's dpc runner failed");
  }

  cleanup_threads.call();
  EXPECT_GT(state.worker_iterations.load(ktl::memory_order_relaxed), 0u);
  EXPECT_GT(state.affinity_updates.load(ktl::memory_order_relaxed), 0u);

  END_TEST;
}

UNITTEST_START_TESTCASE(mp_hotplug_tests)
UNITTEST("test unplug and hotplug cores one by one", mp_hotplug_test)
UNITTEST("test mp_sync_exec while unplugging and hotplugging a core", mp_hotplug_sync_exec_test)
UNITTEST("test unblocking pinned threads while their core is offline",
         mp_hotplug_unblock_pinned_while_offline_test)
UNITTEST("test unblocking unpinned migrating threads while their last core is offline",
         mp_hotplug_unblock_unpinned_migrate_while_offline_test)
UNITTEST("test fair timeline integrity across repeated core unplug migrations",
         mp_hotplug_migrate_fair_timeline_test)
UNITTEST("test soft affinity migration fallback during core unplug",
         mp_hotplug_soft_affinity_migration_fallback_test)
UNITTEST("test concurrent SetCpuAffinity during core unplug migrations",
         mp_hotplug_set_affinity_during_unplug_test)
UNITTEST_END_TESTCASE(mp_hotplug_tests, "hotplug", "Tests for unplugging and hotplugging cores")
