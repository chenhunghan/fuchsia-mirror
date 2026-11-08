// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_IDLE_POWER_THREAD_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_IDLE_POWER_THREAD_H_

#include <lib/lazy_init/lazy_init.h>
#include <lib/wake-vector.h>
#include <stdint.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <arch/mp_unplug_event.h>
#include <kernel/ffi.h>
#include <kernel/thread.h>
#include <ktl/atomic.h>

class IdlePowerThread;

// TODO(https://fxbug.dev/576429446): Remove transitional C++ FFI declarations once all callers
// are ported to Rust.
extern "C" {

struct IdlePowerThreadTransitionResult {
  zx_status_t status;
  uint8_t starting_state;
};

IdlePowerThreadTransitionResult rust_idle_power_thread_transition_active_to_offline(
    IdlePowerThread& ipt, zx_instant_mono_t timeout_at);
IdlePowerThreadTransitionResult rust_idle_power_thread_transition_offline_to_active(
    IdlePowerThread& ipt, zx_instant_mono_t timeout_at);
int rust_idle_power_thread_run(void* arg);
void rust_idle_power_thread_flush_and_halt(IdlePowerThread& ipt) __NO_RETURN;
void rust_resume_timer_wake_vector_get_diagnostics(
    wake_vector::WakeVector::Diagnostics& diagnostics_out);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_idle_power_thread_init(ffi::Uninitialized<IdlePowerThread>* out);
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_resume_timer_wake_vector_init();
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const wake_vector::WakeEvent* cpp_resume_timer_wake_vector_get_wake_event();

}  // extern "C"

//
// Platform Independent Per-CPU Idle/Power Thread
//
// Manages the idle, suspend, and offline functions of a CPU. This thread is scheduled whenever
// there is no eligible work in the run queues of a CPU or a power state management operation is
// requested for the CPU. The idle/power thread provides the platform-independent run loop for
// handling these functions and delegates to the platform-specific subsystems for the relevant
// operations.
//

class IdlePowerThread final {
 public:
  IdlePowerThread() = default;
  ~IdlePowerThread() = default;

  IdlePowerThread(const IdlePowerThread&) = delete;
  IdlePowerThread& operator=(const IdlePowerThread&) = delete;
  IdlePowerThread(IdlePowerThread&&) = delete;
  IdlePowerThread& operator=(IdlePowerThread&&) = delete;

  // The current or target high-level operational state of the CPU. The depth and type of
  // idle/suspend states depends inputs from other subsystems, such as timer and run queues and
  // system power targets.
  // TODO(https://fxbug.dev/42086705): Rationalize this state with mp active / online states.
  enum class State : uint8_t {
    Active,
    Wakeup,  // Similar to Active with respect to scheduling but signals a wakeup occurred.
    Offline,
    Suspend,
  };

  // Evaluates to true if the snapshotted state indicates the power thread should be scheduled
  // instead of eligible work because it needs to perform a transition or power management function.
  // Provides accessors to the snapshotted state to facilitate efficient asserts in code paths that
  // have preconditions that should be checked.
  class PendingPowerWorkPredicate {
   public:
    explicit constexpr PendingPowerWorkPredicate(State current, State target)
        : current_{current}, target_(target) {}

    constexpr bool is_pending() const {
      return (current_ != State::Active && current_ != State::Wakeup) || current_ != target_;
    }

    explicit constexpr operator bool() const { return is_pending(); }

    constexpr State current() const { return current_; }
    constexpr State target() const { return target_; }

   private:
    const State current_;
    const State target_;
  };

  // Returns true if the power thread should be scheduled instead of eligible work because it needs
  // to perform a transition or power management function.
  PendingPowerWorkPredicate pending_power_work() const {
    const StateMachine state = state_.load(ktl::memory_order_acquire);
    return PendingPowerWorkPredicate(state.current, state.target);
  }

  // The result of a transition request, including the status code of the request and the current
  // state when the request was made.
  struct TransitionResult {
    const zx_status_t status;
    const State starting_state;
  };

  // Requests to transition from Active to Offline.
  //
  // After the transition completes, waits until the platform reports (via platform_get_cpu_state)
  // that the CPU is powered off or stopped.  If the platform does not support querying CPU power
  // state, this wait is skipped.  If the platform never reports that the CPU is powered off, this
  // waits until |timeout_at| (i.e. indefinitely, if |timeout_at| is ZX_TIME_INFINITE), logging a
  // message periodically.
  //
  // Returns:
  //   - {ZX_OK, State::Active} if the transition was successful.
  //   - {ZX_OK, State::Offline} if the current state was already Offline.
  //   - {ZX_ERR_BAD_STATE, current state} if the current state was not Active or Offline.
  //   - {ZX_ERR_TIMED_OUT, State::Active} if the current state was Active and transition timeout
  //     expired.  Note that in this case the request is not withdrawn (once the CPU's idle/power
  //     thread has begun to take it offline, it cannot be stopped), so the CPU may still go Offline
  //     later.
  //   - {ZX_ERR_TIMED_OUT, starting state} if the transition completed, but the timeout expired
  //     before the platform reported that the CPU was powered off.  Note that in this case, the
  //     CPU's state is Offline.
  //   - {error, starting state} if the transition completed, but querying the CPU's power state
  //     failed with |error|.  Note that in this case, the CPU's state is Offline.
  TransitionResult TransitionActiveToOffline(zx_instant_mono_t timeout_at = ZX_TIME_INFINITE)
      TA_EXCL(chainlock_transaction_token) {
    const IdlePowerThreadTransitionResult res =
        rust_idle_power_thread_transition_active_to_offline(*this, timeout_at);
    return {.status = res.status, .starting_state = static_cast<State>(res.starting_state)};
  }

  // Requests to transition from Offline to Active.
  //
  // Returns:
  //   - {ZX_OK, State::Offline} if the transition was successful.
  //   - {ZX_OK, State::Active} if the current state was already Active.
  //   - {ZX_ERR_BAD_STATE, current state} if the current state was not Active or Offline.
  //   - {ZX_ERR_TIMED_OUT, State::Offline} if the current state was Offline and transition timeout
  //     expired.
  TransitionResult TransitionOfflineToActive(zx_instant_mono_t timeout_at = ZX_TIME_INFINITE)
      TA_EXCL(chainlock_transaction_token) {
    const IdlePowerThreadTransitionResult res =
        rust_idle_power_thread_transition_offline_to_active(*this, timeout_at);
    return {.status = res.status, .starting_state = static_cast<State>(res.starting_state)};
  }

  // Implements the run loop executed by the CPU's idle/power thread.
  static constexpr thread_start_routine Run = rust_idle_power_thread_run;

  // Called by mp_unplug_current_cpu() to complete last steps of taking the CPU offline.
  void FlushAndHalt() __NO_RETURN { rust_idle_power_thread_flush_and_halt(*this); }

  // Accessors to the underlying Thread instance.
  Thread& thread() { return thread_; }
  const Thread& thread() const { return thread_; }

 private:
  friend struct IdlePowerThreadLayout;
  friend void cpp_resume_timer_wake_vector_init();
  friend const wake_vector::WakeEvent* cpp_resume_timer_wake_vector_get_wake_event();

  // A bespoke wake vector used by the resume timer handler to wake the system when the resume boot
  // time is reached.
  struct ResumeTimerWakeVector : public wake_vector::WakeVector {
    ResumeTimerWakeVector() : WakeVector{&ResumeTimerWakeVector::wake_event}, wake_event{*this} {
      wake_event.Initialize();
    }
    ~ResumeTimerWakeVector() override { wake_event.Destroy(); }

    void GetDiagnostics(WakeVector::Diagnostics& diagnostics_out) const final {
      rust_resume_timer_wake_vector_get_diagnostics(diagnostics_out);
    }

    wake_vector::WakeEvent wake_event;
  };

  // Use LazyInit to sequence constructing this instance after global ctors initialize the global
  // WakeEvent list. This instance is initialized by an init hook well before it can be used at
  // runtime by TransitionAllActiveToSuspend.
  inline static lazy_init::LazyInit<ResumeTimerWakeVector> resume_timer_wake_vector_;

  struct StateMachine {
    State current{State::Active};
    State target{State::Active};

    constexpr bool operator==(const StateMachine& other) const {
      return current == other.current && target == other.target;
    }
  };

  static constexpr StateMachine kActive{.current = State::Active, .target = State::Active};

  // The layout of these fields must match the Rust `IdlePowerThread`.
  // TODO(https://fxbug.dev/542143859): Explicit initialization of state_ is a workaround for a
  // compiler front-end issue regarding nested structures
  // (https://github.com/llvm/llvm-project/issues/60321).
  ktl::atomic<StateMachine> state_{kActive};
  AutounsignalMpUnplugEvent complete_;
  Thread thread_;
  cpu_num_t this_cpu_{INVALID_CPU};
};

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_IDLE_POWER_THREAD_H_
