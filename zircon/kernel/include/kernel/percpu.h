// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_PERCPU_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_PERCPU_H_

#include <align.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
#include <zircon/compiler.h>

#include <arch/mp_unplug_event.h>
#include <arch/ops.h>
#include <kernel/cpu.h>
#include <kernel/cpu_search_set.h>
#include <kernel/ffi.h>
#include <kernel/scheduler.h>
#include <kernel/spinlock.h>
#include <kernel/stats.h>
#include <kernel/thread.h>
#include <ktl/array.h>
#include <ktl/atomic.h>
#include <ktl/forward.h>
#include <ktl/type_traits.h>
#include <ktl/utility.h>
#include <vm/page_state.h>

// Fwd decl.  Concrete definition is in lib/kconcurrent/chainlock_transaction.h
class ChainLockTransaction;

// Per-CPU line and flush staging state for kernel stdout (`lib/io`).
struct PerCpuStdoutState {
  // Number of per-CPU staging buffers. Deeper nested flushes use the stack.
  static constexpr size_t kNumFlushBuffers = 1;

  // Maximum nested stdout flushes on one CPU before lines are dropped.
  static constexpr size_t kMaxFlushDepth = 8;

  static_assert(kNumFlushBuffers <= kMaxFlushDepth);

  // Line buffer for stdout writes when blocking is disallowed (e.g. IRQ context).
  Thread::Linebuffer irq_linebuffer;

  // Staging buffers for flushing stdout lines without stack allocation.
  // The holder of a slot must not be rescheduled while it holds the slot.
  ktl::array<ktl::array<char, Thread::Linebuffer::kBufferSize>, kNumFlushBuffers> flush_buffers{};

  // Nesting depth of active stdout flushes on this CPU. A flush is only counted
  // while its holder cannot be rescheduled.
  ktl::atomic<size_t> flush_depth{0};
};

// Layout of PerCpuStdoutState, checked against the Rust `PerCpuStdoutState`.
struct PerCpuStdoutStateLayout {
  static constexpr size_t kNumFlushBuffers = PerCpuStdoutState::kNumFlushBuffers;
  static constexpr size_t kMaxFlushDepth = PerCpuStdoutState::kMaxFlushDepth;
  static constexpr size_t kIrqLinebufferOffset = offsetof(PerCpuStdoutState, irq_linebuffer);
  static constexpr size_t kFlushBuffersOffset = offsetof(PerCpuStdoutState, flush_buffers);
  static constexpr size_t kFlushDepthOffset = offsetof(PerCpuStdoutState, flush_depth);
};

struct percpu;

// TODO(https://fxbug.dev/575987745): Remove these transitional FFI declarations once all C++
// callers of `struct percpu` are migrated to Rust.
extern "C" {
extern size_t rust_percpu_processor_count;
extern percpu** rust_percpu_processor_index;
}

struct percpu {
 private:
  static constexpr size_t kTimerQueueSize = 32;
  static constexpr size_t kTimerQueueAlign = 8;

  static constexpr size_t kIdlePowerThreadAlign = alignof(Thread);
  static_assert(kIdlePowerThreadAlign >= alignof(uint16_t));
  static_assert(kIdlePowerThreadAlign >= alignof(AutounsignalMpUnplugEvent));
  static_assert(kIdlePowerThreadAlign >= alignof(cpu_num_t));
  static constexpr size_t kIdlePowerThreadCompleteEnd =
      ROUNDUP(sizeof(uint16_t), alignof(AutounsignalMpUnplugEvent)) +
      sizeof(AutounsignalMpUnplugEvent);
  static constexpr size_t kIdlePowerThreadThreadEnd =
      ROUNDUP(kIdlePowerThreadCompleteEnd, alignof(Thread)) + sizeof(Thread);
  static constexpr size_t kIdlePowerThreadThisCpuEnd =
      ROUNDUP(kIdlePowerThreadThreadEnd, alignof(cpu_num_t)) + sizeof(cpu_num_t);
  static constexpr size_t kIdlePowerThreadSize =
      ROUNDUP(kIdlePowerThreadThisCpuEnd, kIdlePowerThreadAlign);

  static constexpr size_t kDpcRunnerSize = 200;
  static constexpr size_t kDpcRunnerAlign = 8;

  static constexpr size_t kTimerSize = 72;
  static constexpr size_t kTimerAlign = 8;

  static_assert(alignof(Lock<SpinLock>) <= 8);
  static constexpr size_t kStallAccumulatorSize = ROUNDUP(sizeof(Lock<SpinLock>), size_t{8}) + 48;
  static constexpr size_t kStallAccumulatorAlign = 8;

 public:
  percpu() = delete;
  ~percpu() = delete;

  // percpus cannot be moved or copied.
  percpu(const percpu&) = delete;
  percpu(percpu&&) = delete;
  percpu& operator=(const percpu&) = delete;
  percpu& operator=(percpu&&) = delete;

  // Each CPU maintains a per-cpu queue of timers (owned by Rust `PerCpu`).
  [[maybe_unused]] OpaqueStorage<kTimerQueueSize, kTimerQueueAlign> timer_queue;

  // per cpu search set
  CpuSearchSet search_set;

  // per cpu scheduler
  Scheduler scheduler;

#if WITH_LOCK_DEP
  // state for runtime lock validation when in irq context (owned by Rust `PerCpu`).
  [[maybe_unused]] OpaqueStorage<2 * sizeof(uintptr_t), alignof(uintptr_t)> lock_state;
#endif

  // The state of the currently active chain-lock transaction if any, nullptr
  // otherwise.
  //
  // This field and chain_lock_conflict_id are written by this CPU on every
  // chain-lock transaction, while other CPUs take scheduler.queue_lock_ and
  // touch the rest of |scheduler| under it.  Start a new cache line here so
  // that none of |scheduler| shares a line with them.  See AssertLayout.
  __CPU_ALIGN ChainLockTransaction* active_cl_transaction{nullptr};

  // The chainlock conflict ID generator for this CPU.  This field is used by
  // threads to know when to retry a ChainLockTransaction when a conflict is
  // encountered.  See lib/kconcurrent for more details.
  ktl::atomic<uint64_t> chain_lock_conflict_id{1};

  // guest entry/exit statistics
  guest_stats gstats = {};
  // thread/cpu level statistics
  cpu_stats stats = {};

  // per cpu idle/power thread (owned by Rust `PerCpu`).
  [[maybe_unused]] OpaqueStorage<kIdlePowerThreadSize, kIdlePowerThreadAlign> idle_power_thread;

  // kernel counters arena
  int64_t* counters = nullptr;

  // Each cpu maintains a DpcRunner (owned by Rust `PerCpu`).
  [[maybe_unused]] OpaqueStorage<kDpcRunnerSize, kDpcRunnerAlign> dpc_runner;

  // Page state counts are percpu because they change frequently and we don't want to pay for
  // either heavy synchronization, or transferring the counters between CPUs a lot. Using percpu
  // relaxed atomics we minimize the need for synchronization (no locks, interrupt or preemption
  // disabling needed), and avoid cache line bouncing the counters around different CPUs.
  //
  // While it's OK for an observer to temporarily see incorrect values, the counts need to
  // eventually quiesce. It's important that we don't "drop" changes and that the values don't
  // drift over time.
  //
  // When modifying it is not necessary to disable preemption, and can just use
  // percpu::GetCurrent() and then modify the counts. In the unlikely event a CPU migration
  // happens this does not effect correctness, and paying for the rare cache line transfer is
  // preferable to consistently paying to avoid migration.
  //
  // When reading, use `PerCpu::for_each_preempt_disable`. Although it is not possible to guarantee
  // a consistent snapshot of these counters, it should be good enough for diagnostic uses.
  vm_page_counts_t vm_page_counts{};

  // lockup_detector timer (owned by Rust `PerCpu`).
  [[maybe_unused]] OpaqueStorage<kTimerSize, kTimerAlign> lockup_detector_timer;

  // Thread sampling timer (owned by Rust `PerCpu`).
  [[maybe_unused]] OpaqueStorage<kTimerSize, kTimerAlign> sampling_timer;

  // The accumulated memory stall timers for this CPU (owned by Rust `PerCpu`).
  [[maybe_unused]] OpaqueStorage<kStallAccumulatorSize, kStallAccumulatorAlign>
      memory_stall_accumulator;

#ifdef __aarch64__
  // Per-CPU suspend/resume state (owned by Rust `PerCpu`).
  [[maybe_unused]] OpaqueStorage<sizeof(uint64_t), alignof(uint64_t)> resume_state;
#endif

  // Per-CPU line and flush staging buffers for kernel stdout.
  PerCpuStdoutState stdout_state;

  // Returns a reference to the percpu instance for given CPU number.
  static percpu& Get(cpu_num_t cpu_num) {
    DEBUG_ASSERT(cpu_num < processor_count());
    return *ktl::atomic_ref<percpu**>(rust_percpu_processor_index)
                .load(ktl::memory_order_relaxed)[cpu_num];
  }

  // Returns a reference to the percpu instance for the calling CPU.
  static percpu& GetCurrent() { return *arch_get_curr_percpu(); }

  // Returns the number of percpu instances.
  static size_t processor_count() {
    return ktl::atomic_ref<size_t>(rust_percpu_processor_count).load(ktl::memory_order_relaxed);
  }

  // Call |Func| once per CPU with each CPU's percpu struct.
  //
  // |Func| should accept |cpu_num| and |percpu*|.
  template <typename Func>
  static void ForEach(Func&& func) {
    for (cpu_num_t cpu_num = 0; cpu_num < processor_count(); ++cpu_num) {
      ktl::forward<Func>(func)(cpu_num, &Get(cpu_num));
    }
  }

 private:
  // Other CPUs take scheduler.queue_lock_ to wake threads onto this CPU and to
  // steal work from it, while this CPU writes active_cl_transaction and
  // chain_lock_conflict_id on every chain-lock transaction.  If they share a
  // cache line, every cross-CPU wake bounces that line away from this CPU.  A
  // change that moved queue_lock_ onto that line cost FutexWakeOne 15% on x64.
  //
  // queue_lock_ is the first field after rust_state_ (asserted by
  // Scheduler::AssertLayout), so its offset is sizeof(RustSchedulerState), as
  // for Rust's Scheduler::QUEUE_LOCK_OFFSET.
  static void AssertLayout() {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Winvalid-offsetof"
    using QueueLock = ktl::decay_t<decltype(ktl::declval<const Scheduler&>().queue_lock_cap())>;
    constexpr size_t kQueueLockOffset =
        offsetof(percpu, scheduler) + sizeof(Scheduler::RustSchedulerState);
    constexpr size_t kQueueLockLastLine =
        (kQueueLockOffset + sizeof(QueueLock) - 1) / MAX_CACHE_LINE;
    static_assert(kQueueLockLastLine < offsetof(percpu, active_cl_transaction) / MAX_CACHE_LINE);
    static_assert(kQueueLockLastLine < offsetof(percpu, chain_lock_conflict_id) / MAX_CACHE_LINE);
#pragma clang diagnostic pop
  }
} __CPU_ALIGN;

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_PERCPU_H_
