// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "bindings.h"

#include <stddef.h>
#include <zircon/assert.h>

#include <new>

#include <kernel/ffi.h>
#include <kernel/lock_validation_guard.h>
#include <kernel/mutex.h>
#include <kernel/spinlock.h>

#ifdef WITH_LOCK_DEP
constexpr bool kWithLockDep = true;
#else
constexpr bool kWithLockDep = false;
#endif

constexpr size_t kExpectedMutexSize =
    kWithLockDep ? ((kSchedulerLockSpinTracingEnabled || kLockNameTracingEnabled) ? 40 : 32)
                 : ((kSchedulerLockSpinTracingEnabled || kLockNameTracingEnabled) ? 32 : 24);

#if WITH_LOCK_DEP
using SystemLockType = lockdep::LockDep<void, Mutex>;
#else
using SystemLockType = Mutex;
#endif
static_assert(sizeof(SystemLockType) == kExpectedMutexSize,
              "Rust KMutex size mismatch with C++ size");
static_assert(alignof(SystemLockType) == 8, "Mutex alignment must be exactly 8 bytes.");

#if WITH_LOCK_DEP
using SystemSpinlockType = lockdep::LockDep<void, SpinLock>;
#else
using SystemSpinlockType = SpinLock;
#endif

constexpr size_t kExpectedSpinlockSize = kWithLockDep ? (kSchedulerLockSpinTracingEnabled ? 24 : 16)
                                                      : (kSchedulerLockSpinTracingEnabled ? 16 : 4);

static_assert(sizeof(SystemSpinlockType) == kExpectedSpinlockSize,
              "Rust TrackedSpinlock size mismatch with C++ size");

constexpr size_t kExpectedSpinlockAlign =
    (kWithLockDep || kSchedulerLockSpinTracingEnabled) ? 8 : 4;
static_assert(alignof(SystemSpinlockType) == kExpectedSpinlockAlign,
              "Rust TrackedSpinlock alignment mismatch with C++ alignment");

#if WITH_LOCK_DEP
using SystemMonitoredSpinlockType = lockdep::LockDep<void, MonitoredSpinLock>;
#else
using SystemMonitoredSpinlockType = MonitoredSpinLock;
#endif

static_assert(sizeof(SystemMonitoredSpinlockType) == kExpectedSpinlockSize,
              "Rust TrackedMonitoredSpinlock size mismatch with C++ size");
static_assert(alignof(SystemMonitoredSpinlockType) == kExpectedSpinlockAlign,
              "Rust TrackedMonitoredSpinlock alignment mismatch with C++ alignment");

static_assert(sizeof(lockdep::AcquiredLockEntry) == 40, "AcquiredLockEntry size mismatch");
static_assert(alignof(lockdep::AcquiredLockEntry) == 8,
              "AcquiredLockEntry alignment must be exactly 8 bytes.");

#if WITH_LOCK_DEP
using SystemBrwLockType = lockdep::LockDep<void, BrwLockPi, 0, lockdep::LockFlagsMultiAcquire>;
using SystemBrwLockNoPiType =
    lockdep::LockDep<void, BrwLockNoPi, 0, lockdep::LockFlagsMultiAcquire>;
#else
using SystemBrwLockType = BrwLockPi;
using SystemBrwLockNoPiType = BrwLockNoPi;
#endif

constexpr size_t kExpectedBrwLockNoPiSize =
    ((kSchedulerLockSpinTracingEnabled || kLockNameTracingEnabled) ? 80 : 72) +
    (kWithLockDep ? 8 : 0);
constexpr size_t kExpectedBrwLockNoPiAlign = 8;

#if defined(__riscv)
constexpr size_t kExpectedBrwLockSize = kExpectedBrwLockNoPiSize;
constexpr size_t kExpectedBrwLockAlign = kExpectedBrwLockNoPiAlign;
#else
constexpr size_t kExpectedBrwLockSize = kWithLockDep ? 144 : 128;
constexpr size_t kExpectedBrwLockAlign = 16;
#endif

static_assert(sizeof(SystemBrwLockType) == kExpectedBrwLockSize, "SystemBrwLockType size mismatch");
static_assert(alignof(SystemBrwLockType) == kExpectedBrwLockAlign,
              "SystemBrwLockType alignment mismatch");
static_assert(sizeof(SystemBrwLockNoPiType) == kExpectedBrwLockNoPiSize,
              "SystemBrwLockNoPiType size mismatch");
static_assert(alignof(SystemBrwLockNoPiType) == kExpectedBrwLockNoPiAlign,
              "SystemBrwLockNoPiType alignment mismatch");

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_brwlock_pi_init(BrwLockPi* lock, const void* class_id) {
  new (lock) BrwLockPi();
  lock->SetLockClassId(reinterpret_cast<lockdep::LockClassId>(class_id));
}

FFI_ALWAYS_INLINE void cpp_brwlock_pi_destroy(BrwLockPi* lock) { lock->~BrwLockPi(); }

FFI_ALWAYS_INLINE void cpp_brwlock_pi_acquire_read(BrwLockPi* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->ReadAcquire();
}

FFI_ALWAYS_INLINE void cpp_brwlock_pi_release_read(BrwLockPi* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->ReadRelease();
}

FFI_ALWAYS_INLINE void cpp_brwlock_pi_acquire_write(BrwLockPi* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->WriteAcquire();
}

FFI_ALWAYS_INLINE void cpp_brwlock_pi_release_write(BrwLockPi* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->WriteRelease();
}

FFI_ALWAYS_INLINE void cpp_brwlock_no_pi_init(BrwLockNoPi* lock, const void* class_id) {
  new (lock) BrwLockNoPi();
  lock->SetLockClassId(reinterpret_cast<lockdep::LockClassId>(class_id));
}

FFI_ALWAYS_INLINE void cpp_brwlock_no_pi_destroy(BrwLockNoPi* lock) { lock->~BrwLockNoPi(); }

FFI_ALWAYS_INLINE void cpp_brwlock_no_pi_acquire_read(BrwLockNoPi* lock)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->ReadAcquire();
}

FFI_ALWAYS_INLINE void cpp_brwlock_no_pi_release_read(BrwLockNoPi* lock)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->ReadRelease();
}

FFI_ALWAYS_INLINE void cpp_brwlock_no_pi_acquire_write(BrwLockNoPi* lock)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->WriteAcquire();
}

FFI_ALWAYS_INLINE void cpp_brwlock_no_pi_release_write(BrwLockNoPi* lock)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->WriteRelease();
}

FFI_ALWAYS_INLINE void cpp_lock_dep_acquire(void* lock, const void* class_id, void* entry_storage,
                                            lockdep::LockFlags flags, uintptr_t order,
                                            bool assert_nestable) {
#if WITH_LOCK_DEP
  if constexpr (lockdep::kLockValidationEnabled) {
    if (entry_storage != nullptr) {
      LockValidationGuard guard;
      auto* entry = new (entry_storage)
          lockdep::AcquiredLockEntry(lock, reinterpret_cast<lockdep::LockClassId>(class_id), order);
      if (class_id != nullptr) {
        // `assert_nestable` is set by an ordered acquisition (Rust `OrderedLockPolicy`) and
        // mirrors the C++ `Guard(AssertOrderedLock, ...)` constructor. It is deliberately
        // independent of `order` so that a non-nestable class may still be acquired with a
        // nonzero order (e.g. an address-ordered multi-acquisition, see the TODO on Rust
        // `KMutexGuardMultiple`).
        ZX_DEBUG_ASSERT(!assert_nestable ||
                        lockdep::ValidatorLockClassState::IsNestable(entry->id()));
        if (!lockdep::ValidatorLockClassState::IsTrackingDisabled(entry->id())) {
          lockdep::ThreadLockState::Get(flags)->Acquire(entry);
        }
      }
    }
  }
#endif
}

FFI_ALWAYS_INLINE void cpp_lock_dep_release(void* entry_storage, lockdep::LockFlags flags) {
#if WITH_LOCK_DEP
  if constexpr (lockdep::kLockValidationEnabled) {
    if (entry_storage != nullptr) {
      LockValidationGuard guard;
      auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
      if (entry->id() != lockdep::kInvalidLockClassId &&
          !lockdep::ValidatorLockClassState::IsTrackingDisabled(entry->id())) {
        lockdep::ThreadLockState::Get(flags)->Release(entry);
      }
      entry->~AcquiredLockEntry();
    }
  }
#endif
}

FFI_ALWAYS_INLINE void cpp_lock_validate_release(void* entry_storage, lockdep::LockFlags flags) {
#if WITH_LOCK_DEP
  if constexpr (lockdep::kLockValidationEnabled) {
    if (entry_storage != nullptr) {
      LockValidationGuard guard;
      auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
      if (entry->id() != lockdep::kInvalidLockClassId &&
          !lockdep::ValidatorLockClassState::IsTrackingDisabled(entry->id())) {
        lockdep::ThreadLockState::Get(flags)->Release(entry);
      }
    }
  }
#endif
}

FFI_ALWAYS_INLINE void cpp_lock_validate_acquire(void* entry_storage, lockdep::LockFlags flags) {
#if WITH_LOCK_DEP
  if constexpr (lockdep::kLockValidationEnabled) {
    if (entry_storage != nullptr) {
      LockValidationGuard guard;
      auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
      if (entry->id() != lockdep::kInvalidLockClassId &&
          !lockdep::ValidatorLockClassState::IsTrackingDisabled(entry->id())) {
        lockdep::ThreadLockState::Get(flags)->Acquire(entry);
      }
    }
  }
#endif
}

}  // extern "C"
