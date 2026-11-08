// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2009 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_TIMER_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_TIMER_H_

#include <lib/kconcurrent/chainlock.h>
// Unused here; kept because many files rely on it (and on ktl/span.h) transitively.
#include <string-file.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <fbl/canary.h>
#include <kernel/cpu.h>
#include <kernel/deadline.h>
#include <kernel/ffi.h>
#include <kernel/spinlock.h>
#include <ktl/array.h>
#include <ktl/bit.h>
#include <ktl/byte.h>

class Timer;
class TimerQueue;

// Timer and TimerQueue are implemented in Rust (kernel/timer.rs). The C++ classes below only
// reserve opaque storage for the Rust objects, so that timers can be embedded in C++ objects and
// constructed in static storage, and forward to these functions.
extern "C" {
void rust_timer_destroy(const Timer* timer);
void rust_timer_set(Timer* timer, const Deadline* deadline,
                    void (*callback)(Timer*, zx_time_t now, void* arg), void* arg);
bool rust_timer_cancel(Timer* timer);
zx_status_t rust_timer_trylock_or_cancel(Timer* timer, ChainLock* lock);
bool rust_timer_cancel_pending(const Timer* timer);
zx_duration_t rust_timer_slack_for_test(const Timer* timer);
zx_time_t rust_timer_scheduled_time_for_test(const Timer* timer, zx_clock_t expected_clock_id);
void rust_timer_queue_init(TimerQueue* timer_queue);
void rust_timer_queue_destroy(TimerQueue* timer_queue);
void rust_timer_queue_preempt_reset(TimerQueue* timer_queue, zx_instant_mono_t deadline);
bool rust_timer_queue_preempt_armed(const TimerQueue* timer_queue);
void rust_timer_queue_transition_off_cpu(TimerQueue* timer_queue, TimerQueue* source);
void rust_timer_queue_print_timer_queues(char* buf, size_t len);
void rust_timer_queue_update_platform_timer(TimerQueue* timer_queue);
}  // extern "C"

#ifdef UNITTESTS_ENABLED
extern "C" {
const Timer* cpp_timer_constinit_monotonic_timer_for_test();
const Timer* cpp_timer_constinit_boot_timer_for_test();
}  // extern "C"
#endif

// Rules for Timers:
// - Timer callbacks occur from interrupt context.
// - Timers may be programmed or canceled from interrupt or thread context.
// - Timers may be canceled or reprogrammed from within their callback.
// - Setting and canceling timers is not thread safe and cannot be done concurrently.
// - Timer::cancel() may spin waiting for a pending timer to complete on another cpu.
// - Timers aren't permanently bound to a particular CPU and may fire on a CPU other
//   than the one on which they were set.

class Timer {
 public:
  using Callback = void (*)(Timer*, zx_time_t now, void* arg);

  // Timers need a constexpr constructor, as it is valid to construct them in static storage.
  // TODO(https://fxbug.dev/328306129): The default value for the clock_id parameter should be
  // removed, thus forcing users of the Timer class to explicitly declare the clock they wish
  // to use.
  constexpr explicit Timer(zx_clock_t clock_id = ZX_CLOCK_MONOTONIC)
      : storage_(InitialStorage(clock_id)) {}

  // We ensure that timers are not on a list or an active cpu when destroyed.
  ~Timer() {
    static_assert(sizeof(Timer) == kStorageSize);
    static_assert(alignof(Timer) == kStorageAlign);
    rust_timer_destroy(this);
  }

  // Timers are not moved or copied.
  Timer(const Timer&) = delete;
  Timer(Timer&&) = delete;
  Timer& operator=(const Timer&) = delete;
  Timer& operator=(Timer&&) = delete;

  // Set up a timer that executes once
  //
  // This function specifies a callback function to be run after a specified
  // deadline passes. The function will be called one time.
  //
  // deadline: specifies when the timer should be executed
  // callback: the function to call when the timer expires
  // arg: the argument to pass to the callback
  //
  // The timer function is declared as:
  //   void callback(Timer *, zx_time_t now, void *arg) { ... }
  void Set(const Deadline& deadline, Callback callback, void* arg) {
    rust_timer_set(this, &deadline, callback, arg);
  }

  // Cancel a pending timer
  //
  // Returns true if the timer was canceled before it was
  // scheduled in a cpu and false otherwise or if the timer
  // was not scheduled at all.
  //
  bool Cancel() { return rust_timer_cancel(this); }

  // Equivalent to Set with no slack
  // The deadline parameter should be interpreted differently depending on the clock_id_ field.
  // If clock_id_ is set to ZX_CLOCK_MONOTONIC, deadline is a zx_instant_mono_t.
  // If clock_id_ is set to ZX_CLOCK_BOOT, deadline is a zx_instant_boot_t.
  void SetOneshot(zx_time_t deadline, Callback callback, void* arg) {
    Set(Deadline::no_slack(deadline), callback, arg);
  }

  // Special helper routine to simultaneously try to acquire a spinlock and
  // check for timer cancel, which is needed in a few special cases. Returns
  // ZX_OK if spinlock was acquired, ZX_ERR_TIMED_OUT if timer was canceled.
  zx_status_t TrylockOrCancel(MonitoredSpinLock* lock) TA_TRY_ACQ(false, lock);
  zx_status_t TrylockOrCancel(ChainLock& lock) TA_REQ(chainlock_transaction_token)
      TA_TRY_ACQ(false, lock) TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_timer_trylock_or_cancel(this, &lock);
  }

  // Private accessors for timer tests.
  zx_duration_t slack_for_test() const { return rust_timer_slack_for_test(this); }

  // This function returns a zx_instant_mono_t if the expected_clock_id is ZX_CLOCK_MONOTONIC and a
  // zx_instant_boot_t if the expected_clock_id is ZX_CLOCK_BOOT.
  zx_time_t scheduled_time_for_test(zx_clock_t expected_clock_id) const {
    return rust_timer_scheduled_time_for_test(this, expected_clock_id);
  }

 private:
  // The Rust Timer (kernel/timer.rs) lives in this storage; only Rust interprets it.
  static constexpr size_t kStorageSize = 72;
  static constexpr size_t kStorageAlign = 8;
  using Storage = OpaqueStorage<kStorageSize, kStorageAlign>;

  // The initial state of a Rust Timer is all zero bytes, except for the fields below. Their
  // offsets mirror the Rust #[repr(C)] layout in kernel/timer.rs, and the timer_rust suite's
  // constinit_timer test checks that a constexpr-constructed Timer reads back as expected in Rust.
  static constexpr uint32_t kMagic = fbl::magic("timr");
  static constexpr size_t kMagicOffset = 16;
  static constexpr size_t kClockIdOffset = 56;
  static constexpr size_t kActiveCpuOffset = 60;

  // Writes |value| at |offset| in |storage|, in the native byte order like Rust does.
  static constexpr void StoreU32(Storage& storage, size_t offset, uint32_t value) {
    const auto bytes = ktl::bit_cast<ktl::array<ktl::byte, sizeof(value)>>(value);
    for (size_t i = 0; i < bytes.size(); ++i) {
      storage.bytes[offset + i] = bytes[i];
    }
  }

  static constexpr Storage InitialStorage(zx_clock_t clock_id) {
    static_assert(sizeof(cpu_num_t) == sizeof(uint32_t));
    static_assert(sizeof(zx_clock_t) == sizeof(uint32_t));
    Storage storage{};
    StoreU32(storage, kMagicOffset, kMagic);
    StoreU32(storage, kClockIdOffset, clock_id);
    StoreU32(storage, kActiveCpuOffset, INVALID_CPU);
    return storage;
  }

  [[maybe_unused]] Storage storage_;
};

// Preemption Timers
//
// Each CPU has a dedicated preemption timer that's managed using specialized
// functions (prefixed with timer_preempt_).
//
// Preemption timers are different from general timers. Preemption timers:
//
// - are reset frequently by the scheduler so performance is important
// - should not be migrated off their CPU when the CPU is shutdown
//
// Note: A preemption timer may fire even after it has been canceled.
class TimerQueue {
 public:
  // An empty timer list points back at itself, so the Rust TimerQueue is initialized at runtime.
  TimerQueue() { rust_timer_queue_init(this); }
  ~TimerQueue() {
    static_assert(sizeof(TimerQueue) == kStorageSize);
    static_assert(alignof(TimerQueue) == kStorageAlign);
    rust_timer_queue_destroy(this);
  }

  // TimerQueues are not moved or copied.
  TimerQueue(const TimerQueue&) = delete;
  TimerQueue(TimerQueue&&) = delete;
  TimerQueue& operator=(const TimerQueue&) = delete;
  TimerQueue& operator=(TimerQueue&&) = delete;

  // Set/reset/cancel the preemption timer.
  //
  // When the preemption timer fires, Scheduler::TimerTick is called. Set the
  // deadline to ZX_TIME_INFINITE to cancel the preemption timer.
  // Scheduler::TimerTick may be called spuriously after cancellation.
  void PreemptReset(zx_instant_mono_t deadline) { rust_timer_queue_preempt_reset(this, deadline); }

  // Returns true if the preemption deadline is set and will definitely fire in
  // the future. A false value does not definitively mean the preempt timer will
  // not fire, as a spurious expiration is allowed.
  bool PreemptArmed() const { return rust_timer_queue_preempt_armed(this); }

  // Internal routines used when bringing cpus online/offline

  // Moves |source|'s timers (except its preemption timer) to this TimerQueue.
  void TransitionOffCpu(TimerQueue& source) { rust_timer_queue_transition_off_cpu(this, &source); }

  // Prints the contents of all timer queues into |buf| of length |len| and null
  // terminates |buf|.
  static void PrintTimerQueues(char* buf, size_t len) {
    rust_timer_queue_print_timer_queues(buf, len);
  }

  // UpdatePlatformTimer updates the platform's oneshot timer to the minimum of:
  // 1. The scheduled time of the head of the monotonic timer queue.
  // 2. The scheduled time of the head of the boot timer queue.
  // 3. The preemption timer deadline.
  //
  // This can only be called when interrupts are disabled.
  void UpdatePlatformTimer() { rust_timer_queue_update_platform_timer(this); }

 private:
  // The Rust TimerQueue (kernel/timer.rs) lives in this storage; only Rust interprets it.
  static constexpr size_t kStorageSize = 32;
  static constexpr size_t kStorageAlign = 8;

  [[maybe_unused]] OpaqueStorage<kStorageSize, kStorageAlign> storage_{};
};

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_TIMER_H_
