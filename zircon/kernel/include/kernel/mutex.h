// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
// Copyright (c) 2012 Shantanu Gupta
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_MUTEX_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_MUTEX_H_

#include <lib/kconcurrent/chainlock.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/time.h>

#include <fbl/macros.h>
#include <kernel/ffi.h>
#include <kernel/lock_trace_config.h>
#include <kernel/lock_validation_guard.h>
#include <kernel/lockdep.h>
#include <kernel/spin_tracing_config.h>

class CriticalMutex;
class Mutex;

// Rust implementations of the Mutex and CriticalMutex methods, in //zircon/kernel/kernel/mutex.rs.
// The methods below forward to these. Only Mutex and CriticalMutex methods may call them; see the
// `# Safety` sections in mutex.rs for their preconditions.
extern "C" {
void rust_mutex_init(Mutex* mutex);
void rust_mutex_set_lock_class_id(Mutex* mutex, lockdep::LockClassId lcid);
void rust_mutex_destroy(Mutex* mutex);
void rust_mutex_acquire(Mutex* mutex, zx_duration_t spin_max_duration)
    TA_EXCL(chainlock_transaction_token);
void rust_mutex_release(Mutex* mutex) TA_EXCL(chainlock_transaction_token);
bool rust_mutex_is_held(const Mutex* mutex);
void rust_mutex_assert_held(const Mutex* mutex);

void rust_critical_mutex_init(CriticalMutex* mutex);
void rust_critical_mutex_set_lock_class_id(CriticalMutex* mutex, lockdep::LockClassId lcid);
void rust_critical_mutex_destroy(CriticalMutex* mutex);
[[nodiscard]] bool rust_critical_mutex_acquire(CriticalMutex* mutex,
                                               zx_duration_t spin_max_duration)
    TA_EXCL(chainlock_transaction_token);
void rust_critical_mutex_release(CriticalMutex* mutex, bool should_clear)
    TA_EXCL(chainlock_transaction_token);
bool rust_critical_mutex_is_held(const CriticalMutex* mutex);
void rust_critical_mutex_assert_held(const CriticalMutex* mutex);
bool rust_critical_mutex_is_contested(const CriticalMutex* mutex);
}

// Kernel mutex support.
//
// This is a facade over the Rust `Mutex` in //zircon/kernel/kernel/mutex.rs, which is stored in
// place in `storage_`. All methods, including the constructors and destructor, forward to Rust.
class TA_CAP("mutex") Mutex {
 public:
  Mutex() { rust_mutex_init(this); }

  ~Mutex() { rust_mutex_destroy(this); }

  // No moving or copying allowed.
  DISALLOW_COPY_ASSIGN_AND_MOVE(Mutex);

  // The maximum duration to spin before falling back to blocking.
  // TODO(https://fxbug.dev/42109976): Decide how to make this configurable per device/platform
  // and describe how to optimize this value.
  static constexpr zx_duration_t SPIN_MAX_DURATION = ZX_USEC(150);

  // A constant for users of AutoExpiringPreemptDisabler to select a value
  // consistent with the defaults for Mutex/CriticalMutex. Using this constant
  // also improves readablity, providing a link to the motivating use case for
  // timeslice extension.
  static constexpr zx_duration_t DEFAULT_TIMESLICE_EXTENSION = SPIN_MAX_DURATION;

  // Acquire the mutex.
  void Acquire(zx_duration_t spin_max_duration = SPIN_MAX_DURATION) TA_ACQ() {
    rust_mutex_acquire(this, spin_max_duration);
  }

  // Release the mutex. Must be held by the current thread.
  void Release() TA_REL() { rust_mutex_release(this); }

  // does the current thread hold the mutex?
  bool IsHeld() const { return rust_mutex_is_held(this); }

  // Panic unless the given lock is held.
  //
  // Can be used when thread safety analysis can't prove you are holding
  // a lock. The asserts may be optimized away in release builds.
  void AssertHeld() const TA_ASSERT() { rust_mutex_assert_held(this); }

  // Records the lock class name of |lcid| for mutex name tracing. Called by lockdep::Lock<Mutex>
  // when it is constructed, before the mutex is shared.
  void SetLockClassId(lockdep::LockClassId lcid) {
    if constexpr (kNameTracingEnabled) {
      rust_mutex_set_lock_class_id(this, lcid);
    }
  }

 private:
  friend class CriticalMutex;

  // Whether the Rust `Mutex` has lock name storage (see `MUTEX_NAME_TRACING_ENABLED`).
  static constexpr bool kNameTracingEnabled =
      kSchedulerLockSpinTracingEnabled || kLockNameTracingEnabled;

  // The size and alignment of the Rust `Mutex`, which asserts them against mutex_bindings.
  static constexpr size_t kStorageSize = kNameTracingEnabled ? 32 : 24;
  static constexpr size_t kStorageAlign = 8;

  [[maybe_unused]] OpaqueStorage<kStorageSize, kStorageAlign> storage_;
};

// CriticalMutex is a mutex variant that uses a thread timeslice extension to
// disable preemption for a limited time during the critical section.
//
// Acquiring a CriticalMutex sets a timeslice extension that's cleared when the
// mutex is released or the extension expires, whichever comes first.
//
// This variant is useful to avoid a form of thrash where a thread holding a
// mutex is preempted by another thread that subsequently blocks while trying to
// acquire the already held mutex.  CriticalMutex is designed to mitigate this
// type of thrash by preventing the holder from being preempted, up to some
// maximum amount of time.  By limiting maximum amount of time the holder is
// non-preemptible, we can limit the degree to which other, logically higher
// priority tasks are delayed from executing.
//
// Good candidates for CriticalMutex are global or widely shared locks that
// typically, but not necessarily always, have very short critical sections
// (tens of microseconds or less) and high contention under load.
//
// CriticalMutex differs from SpinLock in the following ways:
// * Threads contending a CriticalMutex will block after the spin interval is
//   exceeded, avoiding extended monopolization of multiple CPUs.
// * Threads may block while holding a CriticalMutex, simplifying maintaining
//   invariants in slow paths.
// * Interrupts may remain enabled while holding a CriticalMutex, avoiding
//   undesirable IRQ latency.
//
// Like Mutex, this is a facade over the Rust `CriticalMutex` in //zircon/kernel/kernel/mutex.rs,
// which is stored in place in `storage_`.
class TA_CAP("mutex") CriticalMutex {
 public:
  CriticalMutex() { rust_critical_mutex_init(this); }
  ~CriticalMutex() { rust_critical_mutex_destroy(this); }

  // No moving or copying allowed.
  DISALLOW_COPY_ASSIGN_AND_MOVE(CriticalMutex);

  // Acquire the mutex. Returns whether or not a time slice extension was set, and must be
  // subsequently cleared on Release. More simply, the return value must be passed into the Release
  // method.
  enum class ShouldClear : bool { No = false, Yes = true };
  [[nodiscard]] ShouldClear Acquire(zx_duration_t spin_max_duration = Mutex::SPIN_MAX_DURATION)
      TA_ACQ() {
    return static_cast<ShouldClear>(rust_critical_mutex_acquire(this, spin_max_duration));
  }

  // Release the mutex. Must be held by the current thread. The |should_clear| parameter is the
  // return value of the matching |Acquire| call.
  void Release(ShouldClear should_clear) TA_REL() {
    rust_critical_mutex_release(this, should_clear == ShouldClear::Yes);
  }

  // See |Mutex::IsHeld|.
  bool IsHeld() const { return rust_critical_mutex_is_held(this); }

  // See |Mutex::AssertHeld|.
  void AssertHeld() const TA_ASSERT() { rust_critical_mutex_assert_held(this); }

  // See |Mutex::IsContested|.
  bool IsContested() const { return rust_critical_mutex_is_contested(this); }

  // See |Mutex::SetLockClassId|.
  void SetLockClassId(lockdep::LockClassId lcid) {
    if constexpr (Mutex::kNameTracingEnabled) {
      rust_critical_mutex_set_lock_class_id(this, lcid);
    }
  }

 private:
  // The Rust `CriticalMutex` is a transparent wrapper around a Rust `Mutex`.
  [[maybe_unused]] OpaqueStorage<Mutex::kStorageSize, Mutex::kStorageAlign> storage_;
};

// Lock policy for kernel mutexes
//
struct MutexPolicy {
  struct State {
    const zx_duration_t spin_max_duration{Mutex::SPIN_MAX_DURATION};
  };

  // Protects the thread local lock list and validation.
  using ValidationGuard = LockValidationGuard;

  // No special actions are needed during pre-validation.
  template <typename LockType>
  static void PreValidate(LockType*, State*) {}

  // Basic acquire and release operations.
  template <typename LockType>
  static bool Acquire(LockType* lock, State* state) TA_ACQ(lock) {
    lock->Acquire(state->spin_max_duration);
    return true;
  }

  template <typename LockType>
  static void Release(LockType* lock, State*) TA_REL(lock) {
    lock->Release();
  }

  // Runtime lock assertions.
  template <typename LockType>
  static void AssertHeld(const LockType& lock) TA_ASSERT(lock) {
    lock.AssertHeld();
  }
};

// Lock policy for critical kernel mutexes.
//
struct CriticalMutexPolicy {
  struct State {
    const uint32_t spin_max_duration{Mutex::SPIN_MAX_DURATION};
    CriticalMutex::ShouldClear should_clear = CriticalMutex::ShouldClear::No;
  };

  // Protects the thread local lock list and validation.
  using ValidationGuard = LockValidationGuard;

  // No special actions are needed during pre-validation.
  template <typename LockType>
  static void PreValidate(LockType*, State*) {}

  // Basic acquire and release operations.
  template <typename LockType>
  static bool Acquire(LockType* lock, State* state) TA_ACQ(lock) {
    state->should_clear = lock->Acquire(state->spin_max_duration);
    return true;
  }

  template <typename LockType>
  static void Release(LockType* lock, State* state) TA_REL(lock) {
    lock->Release(state->should_clear);
  }

  // Runtime lock assertions.
  template <typename LockType>
  static void AssertHeld(const LockType& lock) TA_ASSERT(lock) {
    lock.AssertHeld();
  }
};

// Configure the lockdep::Guard for kernel mutexes to use MutexPolicy.
LOCK_DEP_POLICY(Mutex, MutexPolicy);
LOCK_DEP_POLICY(CriticalMutex, CriticalMutexPolicy);

// Declares a Mutex member of the struct or class |containing_type|.
//
// Example usage:
//
// struct MyType {
//     DECLARE_MUTEX(MyType [, LockFlags]) lock;
// };
//
#define DECLARE_MUTEX(containing_type, ...) \
  LOCK_DEP_INSTRUMENT(containing_type, ::Mutex, ##__VA_ARGS__)

#define DECLARE_CRITICAL_MUTEX(containing_type, ...) \
  LOCK_DEP_INSTRUMENT(containing_type, ::CriticalMutex, ##__VA_ARGS__)

// By default, singleton mutexes in the kernel use Mutex in order to avoid
// a useless global dtor
//
// Example usage:
//
//  DECLARE_SINGLETON_MUTEX(MyGlobalLock [, LockFlags]);
//
#define DECLARE_SINGLETON_MUTEX(name, ...) LOCK_DEP_SINGLETON_LOCK(name, ::Mutex, ##__VA_ARGS__)

#define DECLARE_SINGLETON_CRITICAL_MUTEX(name, ...) \
  LOCK_DEP_SINGLETON_LOCK(name, ::CriticalMutex, ##__VA_ARGS__)

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_MUTEX_H_
