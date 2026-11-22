// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2009 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_TIMER_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_TIMER_H_

// Unused here; kept because many files rely on it (and on ktl/span.h) transitively.
#include <string-file.h>
#include <zircon/types.h>

#include <fbl/canary.h>
#include <kernel/cpu.h>
#include <kernel/deadline.h>
#include <kernel/ffi.h>
#include <ktl/array.h>
#include <ktl/bit.h>
#include <ktl/byte.h>

class Timer;

// Timer is implemented in Rust (kernel/timer.rs). The C++ class below only reserves opaque storage
// for the Rust object, so that timers can be embedded in C++ objects and constructed in static
// storage, and forwards to these functions.
extern "C" {
void rust_timer_destroy(const Timer* timer);
void rust_timer_set(Timer* timer, const Deadline* deadline,
                    void (*callback)(Timer*, zx_time_t now, void* arg), void* arg);
bool rust_timer_cancel(Timer* timer);
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

 private:
  // The Rust Timer (kernel/timer.rs) lives in this storage; only Rust interprets it.
  static constexpr size_t kStorageSize = 72;
  static constexpr size_t kStorageAlign = 8;
  using Storage = OpaqueStorage<kStorageSize, kStorageAlign>;

  // The initial state of a Rust Timer is all zero bytes, except for the fields below. Their
  // offsets mirror the Rust #[repr(C)] layout in kernel/timer.rs, and the timer suite's
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

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_TIMER_H_
