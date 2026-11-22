// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/kconcurrent/chainlock.h>
#include <lib/object-constants.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <kernel/scheduler_state.h>
#include <kernel/thread.h>

extern "C" {

// LINT.IfChange(thread_state)
// Keep `enum thread_state` in sync with Rust's `ThreadStateKind`.
static_assert(sizeof(thread_state) == 1);
static_assert(THREAD_INITIAL == 0);
static_assert(THREAD_READY == 1);
static_assert(THREAD_RUNNING == 2);
static_assert(THREAD_BLOCKED == 3);
static_assert(THREAD_BLOCKED_READ_LOCK == 4);
static_assert(THREAD_SLEEPING == 5);
static_assert(THREAD_SUSPENDED == 6);
static_assert(THREAD_DEATH == 7);
// LINT.ThenChange(//zircon/kernel/kernel/scheduler_state.rs:ThreadStateKind)

// LINT.IfChange(ThreadStallState)
// Keep `enum class ThreadStallState` in sync with Rust's `ThreadStallState`. It has no fixed
// underlying type, so it is an `int`, which the Rust mirror matches with #[repr(i32)].
static_assert(sizeof(ThreadStallState) == 4);
static_assert(static_cast<int>(ThreadStallState::IgnoredKernelOnly) == 0);
static_assert(static_cast<int>(ThreadStallState::Inactive) == 1);
static_assert(static_cast<int>(ThreadStallState::Progressing) == 2);
static_assert(static_cast<int>(ThreadStallState::Stalling) == 3);
// LINT.ThenChange(//zircon/kernel/kernel/thread.rs:ThreadStallState)

// LINT.IfChange(ThreadMigrateStage)
static_assert(sizeof(Thread::MigrateStage) == 1);
static_assert(static_cast<int>(Thread::MigrateStage::Save) == 0);
static_assert(static_cast<int>(Thread::MigrateStage::Restore) == 1);
static_assert(static_cast<int>(Thread::MigrateStage::Exiting) == 2);
// LINT.ThenChange(//zircon/kernel/kernel/thread.rs:ThreadMigrateStage)
// NOLINTBEGIN(bugprone-signed-bitwise)
// LINT.IfChange(thread_flags_and_signals)
// Keep THREAD_FLAG_* and THREAD_SIGNAL_* in sync with Rust's constants.
static_assert(THREAD_FLAG_DETACHED == 1 << 0);
static_assert(THREAD_FLAG_FREE_STRUCT == 1 << 1);
static_assert(THREAD_FLAG_IDLE == 1 << 2);
static_assert(THREAD_FLAG_VCPU == 1 << 3);
static_assert(THREAD_FLAG_RESTRICTED_KICK_PENDING == 1 << 4);

static_assert(THREAD_SIGNAL_KILL == 1 << 0);
static_assert(THREAD_SIGNAL_SUSPEND == 1 << 1);
static_assert(THREAD_SIGNAL_POLICY_EXCEPTION == 1 << 2);
static_assert(THREAD_SIGNAL_RESTRICTED_KICK == 1 << 3);
static_assert(THREAD_SIGNAL_SAMPLE_STACK == 1 << 4);
static_assert(THREAD_SIGNAL_CHECK_RSEQ == 1 << 5);
static_assert(THREAD_SIGNAL_POLICY_KILL == 1 << 6);
// LINT.ThenChange(
//   //zircon/kernel/include/kernel/thread.h:thread_flags_and_signals,
//   //zircon/kernel/kernel/thread.rs:thread_flags_and_signals,
// )
// NOLINTEND(bugprone-signed-bitwise)

// Rust `ChainLock` is a 16-byte pair of atomics (lock token and contention state) with 8-byte
// alignment; the layout must match because Rust accesses it directly.
static_assert(sizeof(ChainLock) == 16);
static_assert(alignof(ChainLock) == 8);

static_assert(sizeof(SchedulerState::WaitQueueInheritedSchedulerState) == 64,
              "WaitQueueInheritedSchedulerState size mismatch");
static_assert(alignof(SchedulerState::WaitQueueInheritedSchedulerState) == 8,
              "WaitQueueInheritedSchedulerState alignment mismatch");
static_assert(offsetof(SchedulerState::WaitQueueInheritedSchedulerState, ipvs) == 0,
              "WaitQueueInheritedSchedulerState ipvs offset mismatch");
static_assert(offsetof(SchedulerState::WaitQueueInheritedSchedulerState, start_time) == 32,
              "WaitQueueInheritedSchedulerState start_time offset mismatch");
static_assert(offsetof(SchedulerState::WaitQueueInheritedSchedulerState, finish_time) == 40,
              "WaitQueueInheritedSchedulerState finish_time offset mismatch");
static_assert(offsetof(SchedulerState::WaitQueueInheritedSchedulerState, time_slice_ns) == 48,
              "WaitQueueInheritedSchedulerState time_slice_ns offset mismatch");
static_assert(offsetof(SchedulerState::WaitQueueInheritedSchedulerState, time_slice_used_ns) == 56,
              "WaitQueueInheritedSchedulerState time_slice_used_ns offset mismatch");

Thread* cpp_thread_current_get();

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_current_get() { return Thread::Current::Get(); }

static_assert(sizeof(SchedulerState::BaseProfile) == kSchedulerStateBaseProfileSize);
static_assert(alignof(SchedulerState::BaseProfile) == kSchedulerStateBaseProfileAlign);

}  // extern "C"
