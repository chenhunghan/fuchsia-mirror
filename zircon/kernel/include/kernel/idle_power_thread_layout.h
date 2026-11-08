// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_IDLE_POWER_THREAD_LAYOUT_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_IDLE_POWER_THREAD_LAYOUT_H_

#include <stddef.h>
#include <stdint.h>

#include <arch/mp_unplug_event.h>
#include <kernel/idle_power_thread.h>
#include <ktl/atomic.h>

// Layout contract between the C++ IdlePowerThread and its Rust mirror in
// //zircon/kernel/kernel/idle_power_thread.rs. bindgen turns this into the
// idle_power_thread_bindings crate, where Rust asserts its own layout and field
// offsets against it at compile time.
//
// IdlePowerThread embeds non-standard-layout members (AutounsignalMpUnplugEvent
// and Thread), so offsetof on it is only conditionally supported; in our
// compiler and ABI the layout is deterministic, hence the -Winvalid-offsetof
// suppression.
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Winvalid-offsetof"

struct IdlePowerThreadLayout {
  static_assert(sizeof(IdlePowerThread::State) == sizeof(uint8_t));
  static_assert(static_cast<uint8_t>(IdlePowerThread::State::Active) == 0);
  static_assert(static_cast<uint8_t>(IdlePowerThread::State::Wakeup) == 1);
  static_assert(static_cast<uint8_t>(IdlePowerThread::State::Offline) == 2);
  static_assert(static_cast<uint8_t>(IdlePowerThread::State::Suspend) == 3);
  static_assert(offsetof(IdlePowerThread::StateMachine, current) == 0);
  static_assert(offsetof(IdlePowerThread::StateMachine, target) == 1);
  static_assert(ktl::atomic<IdlePowerThread::StateMachine>::is_always_lock_free);
  static_assert(sizeof(ktl::atomic<IdlePowerThread::StateMachine>) == sizeof(uint16_t));
  static_assert(alignof(ktl::atomic<IdlePowerThread::StateMachine>) == alignof(uint16_t));
  static_assert(sizeof(AutounsignalMpUnplugEvent) == sizeof(MpUnplugEvent));
  static_assert(alignof(AutounsignalMpUnplugEvent) == alignof(MpUnplugEvent));

  static constexpr size_t kSize = sizeof(IdlePowerThread);
  static constexpr size_t kAlign = alignof(IdlePowerThread);
  static constexpr size_t kCompleteSize = sizeof(AutounsignalMpUnplugEvent);
  static constexpr size_t kCompleteAlign = alignof(AutounsignalMpUnplugEvent);

  static constexpr size_t kStateOffset = offsetof(IdlePowerThread, state_);
  static constexpr size_t kCompleteOffset = offsetof(IdlePowerThread, complete_);
  static constexpr size_t kThreadOffset = offsetof(IdlePowerThread, thread_);
  static constexpr size_t kThisCpuOffset = offsetof(IdlePowerThread, this_cpu_);
};

#pragma GCC diagnostic pop

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_IDLE_POWER_THREAD_LAYOUT_H_
