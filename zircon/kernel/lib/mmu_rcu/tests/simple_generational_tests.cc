// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/mmu_rcu/simple_generational_rcu.h>
#include <lib/unittest/unittest.h>

#include <kernel/event.h>
#include <kernel/mp.h>
#include <ktl/unique_ptr.h>

namespace rcu {
namespace {

bool TestBasicRead() {
  BEGIN_TEST;

  SimpleGenerational rcu;
  {
    AutoSimpleGenerationalReader reader(rcu);
  }

  END_TEST;
}

bool TestSynchronizeNoReaders() {
  BEGIN_TEST;

  SimpleGenerational rcu;
  // Should return immediately without readers.
  rcu.Synchronize();

  END_TEST;
}

bool TestReadThenSynchronize() {
  BEGIN_TEST;

  SimpleGenerational rcu;
  {
    AutoSimpleGenerationalReader reader(rcu);
  }
  rcu.Synchronize();

  END_TEST;
}

bool TestMultipleReaders() {
  BEGIN_TEST;

  SimpleGenerational rcu;
  {
    AutoSimpleGenerationalReader reader1(rcu);
    {
      AutoSimpleGenerationalReader reader2(rcu);
      {
        AutoSimpleGenerationalReader reader3(rcu);
      }
    }
  }
  rcu.Synchronize();

  END_TEST;
}

struct BypassTestShared {
  SimpleGenerational rcu;
  ktl::atomic<bool> reader_in_lock{false};
  ktl::atomic<bool> writer_sync_returned{false};
  ktl::atomic<bool> writer_saw_reader_active{false};
  Event reader_thread_done;
};

int rcu_reader_thread(void* arg) {
  auto* shared = reinterpret_cast<BypassTestShared*>(arg);

  {
    AutoSimpleGenerationalReader reader(shared->rcu);
    shared->reader_in_lock.store(true);

    // Spin for a short fixed amount of time to allow the writer
    // thread to call Synchronize() from another CPU.
    for (int i = 0; i < 1000000; ++i) {
      arch::Yield();
    }

    if (shared->writer_sync_returned.load()) {
      shared->writer_saw_reader_active.store(true);
    }
  }

  shared->reader_thread_done.Signal();
  return 0;
}

// Regression test for https://fxbug.dev/520581131. Verifies that RCU generation 1 read locks
// do not trigger integer sign-extension and cause Synchronize() to bypass active readers.
bool TestGen1SynchronizeBypass() {
  BEGIN_TEST;

  if (__builtin_popcount(mp_get_online_mask()) <= 1) {
    unittest_printf("Skipping test on single CPU system to avoid hang\n");
    return true;
  }

  fbl::AllocChecker ac;
  auto shared = ktl::make_unique<BypassTestShared>(&ac);
  ASSERT_TRUE(ac.check(), "Allocation failed");

  // 1. Set RCU to generation 1.
  shared->rcu.Synchronize();

  // 2. Spawn reader thread.
  Thread* thread =
      Thread::Create("rcu-bypass-reader", rcu_reader_thread, shared.get(), DEFAULT_PRIORITY);
  ASSERT_NONNULL(thread);
  thread->Resume();

  // 3. Busy-spin wait for reader to enter lock. Use arch::Yield() to avoid
  // SleepRelative timer interrupt dependency when CPU interrupts might be disabled.
  while (!shared->reader_in_lock.load()) {
    arch::Yield();
  }

  // 4. Synchronize.
  shared->rcu.Synchronize();
  shared->writer_sync_returned.store(true);

  // 5. Wait for reader thread to finish.
  shared->reader_thread_done.Wait();

  int retcode;
  thread->Join(&retcode, ZX_TIME_INFINITE);

  // If the bug is present, the writer bypassed the reader thread.
  EXPECT_FALSE(shared->writer_saw_reader_active.load(), "Writer bypassed active reader!");

  END_TEST;
}

}  // namespace
}  // namespace rcu

UNITTEST_START_TESTCASE(simple_generational_rcu_tests)
UNITTEST("basic_read", rcu::TestBasicRead)
UNITTEST("synchronize_no_readers", rcu::TestSynchronizeNoReaders)
UNITTEST("read_then_synchronize", rcu::TestReadThenSynchronize)
UNITTEST("multiple_readers", rcu::TestMultipleReaders)
UNITTEST("gen1_synchronize_bypass", rcu::TestGen1SynchronizeBypass)
UNITTEST_END_TESTCASE(simple_generational_rcu_tests, "simple_generational_rcu",
                      "Tests for the simple generational RCU primitive.")
