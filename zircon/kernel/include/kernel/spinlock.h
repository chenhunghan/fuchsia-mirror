// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_SPINLOCK_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_SPINLOCK_H_

#include <assert.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <zircon/compiler.h>

#include <arch/interrupt.h>
#include <arch/ops.h>
#include <kernel/cpu.h>
#include <kernel/lockdep.h>
#include <kernel/spin_tracing_config.h>
#include <kernel/spin_tracing_storage.h>
#include <ktl/atomic.h>

enum class SpinLockOptions : uint8_t {
  None = 0,

  // Enable integration with the lockup_detector to monitor spinlock critical sections.
  //
  // See //zircon/kernel/lib/lockup_detector/README.md.
  Monitored = (1 << 0),
};

template <SpinLockOptions Options>
class TA_CAP("mutex") SpinLockBase;

using SpinLock = SpinLockBase<SpinLockOptions::None>;

// MonitoredSpinLock is a SpinLock variant that's integrated with the lockup_detector.
//
// When used with |Guard|, the last argument passed to Guard's constructor should be a const char*
// C-string with static lifetime that describes the critical section protected by the guard.
//
// Example usage:
//
// DECLARE_SINGLETON_SPINLOCK_WITH_TYPE(MonitoredSpinLock) gLock;
// ...
// {
//   Guard<MonitoredSpinLock, IrqSave> guard{gLock::Get(), SOURCE_TAG};
//   ...
// }
//
using MonitoredSpinLock = SpinLockBase<SpinLockOptions::Monitored>;

// Rust implementations of SpinLockBase's methods (see kernel/spinlock.rs); the C++ methods of the
// same name below are thin forwarders to these that carry the thread safety annotations. Each
// requires what the C++ method requires of its callers, in particular that interrupts are disabled
// on the calling CPU. The predicates take the lock word, as they do not depend on the lock's
// options.
extern "C" {
void rust_spinlock_acquire(SpinLock& lock);
// |name| must be a non-null C string with static lifetime.
void rust_monitored_spinlock_acquire(MonitoredSpinLock& lock, const char* name) __NONNULL((2));
// Return false when the lock is acquired, and true when the lock is not acquired.
// |name| must be a non-null C string with static lifetime.
bool rust_monitored_spinlock_try_acquire(MonitoredSpinLock& lock, const char* name) __NONNULL((2));
void rust_spinlock_release(SpinLock* lock) __NONNULL((1));
void rust_monitored_spinlock_release(MonitoredSpinLock* lock) __NONNULL((1));
}  // extern "C"

template <SpinLockOptions Options>
class TA_CAP("mutex") alignas(8) SpinLockBase
    : public spin_tracing::LockNameStorage<spin_tracing::LockType::kSpinlock,
                                           kSchedulerLockSpinTracingEnabled> {
 public:
  // kernel/spinlock.rs implements the SpinLock and MonitoredSpinLock instantiations.
  static_assert(Options == SpinLockOptions::None || Options == SpinLockOptions::Monitored,
                "unsupported SpinLockOptions combination");

  constexpr SpinLockBase() = default;

  // Acquire the spinlock.
  //
  // Interrupts must already be disabled.
  void Acquire() TA_ACQ() {
    static_assert(!kIsMonitored, "spinlock is monitored, use Acquire(const char* name) instead");
    rust_spinlock_acquire(*this);
  }
  // See |Acquire| above.
  //
  // |name| is the name of the critical section protected by this spinlock and
  // must have static lifetime.
  void Acquire(const char* name) TA_ACQ() {
    static_assert(kIsMonitored, "spinlock is unmonitored, use Acquire() instead");
    rust_monitored_spinlock_acquire(*this, name);
  }

  // Attempt to acquire the spinlock without waiting.
  //
  // Interrupts must already be disabled.
  //
  // Returns false when the lock is acquired, and true when the lock is not
  // acquired.
  //
  // TryAcquire operations are not permitted to fail spuriously, even on
  // architectures with weak memory ordering.  If a TryAcquire operation fails,
  // it must be because the lock was actually observed to be held by another
  // thread during the attempt.
  bool TryAcquire(const char* name) TA_TRY_ACQ(false) {
    static_assert(kIsMonitored, "spinlock is unmonitored");
    return rust_monitored_spinlock_try_acquire(*this, name);
  }

  // Release the spinlock
  //
  // Interrupts must already be disabled.
  void Release() TA_REL() {
    if constexpr (kIsMonitored) {
      rust_monitored_spinlock_release(this);
    } else {
      rust_spinlock_release(this);
    }
  }

  // Returns true if held by the calling CPU.
  //
  // Interrupts must be disabled before calling this method, otherwise it could return true when it
  // should return false.
  bool IsHeld() const {
    DEBUG_ASSERT(arch_ints_disabled());
    return HolderCpu() == arch_curr_cpu_num();
  }

  // Just like |Acquire|, but saves interrupt state and disables interrupts.
  void AcquireIrqSave(interrupt_saved_state_t& state, const char* name) TA_ACQ() {
    state = arch_interrupt_save();
    Acquire(name);
  }

  // Just like |Release|, but restores interrupt state after unlocking.
  void ReleaseIrqRestore(interrupt_saved_state_t state) TA_REL() {
    Release();
    arch_interrupt_restore(state);
  }

  void AssertHeld() const TA_ASSERT() { DEBUG_ASSERT(IsHeld()); }

  // Returns which cpu currently holds the spin lock, or INVALID_CPU if not held.
  //
  // Note that this predicate does not synchronize other memory operations (i.e.
  // it has relaxed atomic semantics).
  cpu_num_t HolderCpu() const { return spinlock_.load(ktl::memory_order_relaxed) - 1; }

  // SpinLocks cannot be copied or moved.
  SpinLockBase(const SpinLockBase& am) = delete;
  SpinLockBase& operator=(const SpinLockBase& am) = delete;
  SpinLockBase(SpinLockBase&& c) = delete;
  SpinLockBase& operator=(SpinLockBase&& c) = delete;

 private:
  static constexpr bool kIsMonitored = Options == SpinLockOptions::Monitored;

  // The lock word: the number of the holding CPU plus one, so that 0 means unheld. Only the Rust
  // implementation writes it; C++ only performs the relaxed reads in the predicates above.
  ktl::atomic<cpu_num_t> spinlock_;
};

// kernel/spinlock.rs asserts the same layout for its SpinLockBase, which the trampolines above
// receive these objects as.
static_assert(sizeof(ktl::atomic<cpu_num_t>) == sizeof(cpu_num_t));
static_assert(alignof(ktl::atomic<cpu_num_t>) == alignof(cpu_num_t));
static_assert(sizeof(SpinLock) == (kSchedulerLockSpinTracingEnabled ? 16 : 8));
static_assert(alignof(SpinLock) == 8);
static_assert(sizeof(MonitoredSpinLock) == sizeof(SpinLock));
static_assert(alignof(MonitoredSpinLock) == alignof(SpinLock));

// Declares a member of type |spinlock_type| in the struct or class |containing_type| with
// instrumentation for runtime lock validation.
//
// Example usage:
//
// struct MyType {
//     DECLARE_SPINLOCK_WITH_TYPE(MyType, SpinLock [, LockFlags]) lock;
// };
//
#define DECLARE_SPINLOCK_WITH_TYPE(containing_type, spinlock_type, ...) \
  LOCK_DEP_INSTRUMENT(containing_type, spinlock_type, ##__VA_ARGS__)

// Just like |DECLARE_SPINLOCK_WITH_TYPE| except the type SpinLock is implied.
#define DECLARE_SPINLOCK(containing_type, ...) \
  DECLARE_SPINLOCK_WITH_TYPE(containing_type, SpinLock, ##__VA_ARGS__)

// Declares a singleton of type |spinlock_type| with the name |name|.
//
// Example usage:
//
//  DECLARE_SINGLETON_SPINLOCK_WITH_TYPE(MyGlobalLock, SpinLock [, LockFlags]);
//
#define DECLARE_SINGLETON_SPINLOCK_WITH_TYPE(name, spinlock_type, ...) \
  LOCK_DEP_SINGLETON_LOCK(name, spinlock_type, ##__VA_ARGS__)

//
// Configure lockdep flags and wrappers for SpinLock and MonitoredSpinLock.
//

// Configure lockdep to check irq-safety rules.
LOCK_DEP_TRAITS(SpinLock, lockdep::LockFlagsIrqSafe);
LOCK_DEP_TRAITS(MonitoredSpinLock, lockdep::LockFlagsIrqSafe);

// Option tag for acquiring a SpinLock WITHOUT saving irq state.
struct NoIrqSave {};

// Option tag for acquiring a SpinLock WITH saving irq state.
struct IrqSave {};

// Lock policy for acquiring a SpinLock WITHOUT saving irq state.
template <typename LockType>
struct NoIrqSavePolicy {
  // No extra state required when not saving irq state.
  struct State {};

  // The lock list and validation is made atomic by interrupt disable before locking.
  using ValidationGuard = lockdep::NullValidationGuard;

  static void PreValidate(LockType* lock, State*) {}
  static bool Acquire(LockType* lock, State*) TA_ACQ(lock) {
    lock->Acquire();
    return true;
  }
  static void Release(LockType* lock, State*) TA_REL(lock) { lock->Release(); }
  static void AssertHeld(const LockType& lock) TA_ASSERT(lock) { lock.AssertHeld(); }
};

// Configure Guard<SpinLock, NoIrqSave> to use the above policy to acquire and
// release a SpinLock.
LOCK_DEP_POLICY_OPTION(SpinLock, NoIrqSave, NoIrqSavePolicy<SpinLock>);

// Lock policy for acquiring a MonitoredSpinLock WITHOUT saving irq state.
struct NoIrqSaveMonitoredPolicy {
  // No extra state required when not saving irq state.
  struct State {
    State() = delete;
    explicit State(const char* name) : name(name) {}
    const char* const name;
  };

  // The lock list and validation is made atomic by interrupt disable before locking.
  using ValidationGuard = lockdep::NullValidationGuard;

  static void PreValidate(MonitoredSpinLock* lock, State*) {}
  static bool Acquire(MonitoredSpinLock* lock, State* state) TA_ACQ(lock) {
    lock->Acquire(state->name);
    return true;
  }
  static void Release(MonitoredSpinLock* lock, State*) TA_REL(lock) { lock->Release(); }
  static void AssertHeld(const MonitoredSpinLock& lock) TA_ASSERT(lock) { lock.AssertHeld(); }
};

// Configure Guard<MonitoredSpinLock, NoIrqSave> to use the above policy to
// acquire and release a MonitoredSpinLock.
LOCK_DEP_POLICY_OPTION(MonitoredSpinLock, NoIrqSave, NoIrqSaveMonitoredPolicy);

// Lock policy for acquiring a SpinLock WITH saving irq state.
template <typename LockType>
struct IrqSavePolicy {
  // State and flags required to save irq state.
  struct State {
    interrupt_saved_state_t interrupt_state;
  };

  // The lock list and validation is made atomic by interrupt disable in pre-validation.
  using ValidationGuard = lockdep::NullValidationGuard;

  static void PreValidate(LockType* lock, State* state) {
    state->interrupt_state = arch_interrupt_save();
  }

  static bool Acquire(LockType* lock, State*) TA_ACQ(lock) {
    lock->Acquire();
    return true;
  }

  static void Release(LockType* lock, State* state) TA_REL(lock) {
    lock->Release();
    arch_interrupt_restore(state->interrupt_state);
  }

  static void AssertHeld(const LockType& lock) TA_ASSERT(lock) { lock.AssertHeld(); }
};

// Configure Guard<SpinLock, IrqSave> to use the above policy to acquire and
// release a SpinLock.
LOCK_DEP_POLICY_OPTION(SpinLock, IrqSave, IrqSavePolicy<SpinLock>);

// Lock policy for acquiring a MonitoredSpinLock WITH saving irq state.
struct IrqSaveMonitoredPolicy {
  // State and flags required to save irq state.
  struct State {
    State() = delete;
    explicit State(const char* name) : name(name) {}
    interrupt_saved_state_t interrupt_state;
    const char* const name;
  };

  // The lock list and validation is made atomic by interrupt disable in pre-validation.
  using ValidationGuard = lockdep::NullValidationGuard;

  static void PreValidate(MonitoredSpinLock* lock, State* state) {
    state->interrupt_state = arch_interrupt_save();
  }

  static bool Acquire(MonitoredSpinLock* lock, State* state) TA_ACQ(lock) {
    lock->Acquire(state->name);
    return true;
  }

  static void Release(MonitoredSpinLock* lock, State* state) TA_REL(lock) {
    lock->Release();
    arch_interrupt_restore(state->interrupt_state);
  }

  static void AssertHeld(const MonitoredSpinLock& lock) TA_ASSERT(lock) { lock.AssertHeld(); }
};

// Configure Guard<MonitoredSpinLock, IrqSave> to use the above policy to
// acquire and release a MonitoredSpinLock.
LOCK_DEP_POLICY_OPTION(MonitoredSpinLock, IrqSave, IrqSaveMonitoredPolicy);

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_SPINLOCK_H_
