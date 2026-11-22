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
#include <ktl/utility.h>

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
                                                      : (kSchedulerLockSpinTracingEnabled ? 16 : 8);

static_assert(sizeof(SystemSpinlockType) == kExpectedSpinlockSize,
              "Rust TrackedSpinlock size mismatch with C++ size");

constexpr size_t kExpectedSpinlockAlign = 8;
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

extern "C" {

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
          // `cpp_lock_dep_acquire` and `cpp_lock_dep_release` select `ThreadLockState` (`percpu`
          // vs `Thread`) from `flags`, whereas `cpp_lock_dep_move` (`AcquiredLockEntry::Replace`)
          // reads `ValidatorLockClassState::Get(id_)->flags()`. Enforce agreement on IRQ safety so
          // moves always operate on the same intrusive `acquired_locks_` list.
          ZX_ASSERT(lockdep::ValidatorLockClassState::IsIrqSafe(entry->id()) ==
                    ((flags & lockdep::LockFlagsIrqSafe) != 0));
          lockdep::ThreadLockState::Get(flags)->Acquire(entry);
        }
      }
    }
  }
#endif
}

FFI_ALWAYS_INLINE void cpp_lock_dep_move(void* src_entry_storage, void* dst_entry_storage) {
#if WITH_LOCK_DEP
  if constexpr (lockdep::kLockValidationEnabled) {
    if (src_entry_storage != nullptr && dst_entry_storage != nullptr &&
        src_entry_storage != dst_entry_storage) {
      LockValidationGuard guard;
      auto* src = static_cast<lockdep::AcquiredLockEntry*>(src_entry_storage);
      new (dst_entry_storage) lockdep::AcquiredLockEntry(ktl::move(*src));
      src->~AcquiredLockEntry();
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
