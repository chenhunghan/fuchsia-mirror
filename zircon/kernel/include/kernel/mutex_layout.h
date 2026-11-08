// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_MUTEX_LAYOUT_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_MUTEX_LAYOUT_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/time.h>

#include <kernel/cpu.h>
#include <kernel/lazy_owned_wait_queue.h>
#include <kernel/lock_trace_config.h>
#include <kernel/mutex.h>
#include <kernel/spin_tracing_config.h>
#include <kernel/spin_tracing_storage.h>
#include <ktl/atomic.h>
#include <ktl/type_traits.h>

// Layout contract between the C++ Mutex/CriticalMutex and their Rust mirrors in
// //zircon/kernel/kernel/mutex.rs. bindgen turns this into the mutex_bindings crate, where Rust
// asserts its own layout and constants against it at compile time.
//
// bindgen does not apply the empty base optimization to Mutex's spin_tracing::LockNameStorage
// base (a class template specialized on a non-type bool parameter), so the layout and field
// offsets are exported as constants rather than taken from a bindgen-generated Mutex struct.
//
// Mutex is not a standard-layout class (it has a base class with non-static data members when
// scheduler lock spin tracing or lock name tracing is enabled), so offsetof on it is only
// conditionally supported; in our compiler and ABI the layout is deterministic, hence the
// -Winvalid-offsetof suppression.
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Winvalid-offsetof"

struct MutexLayout {
  static_assert(sizeof(ktl::atomic<cpu_num_t>) == sizeof(uint32_t));
  static_assert(alignof(ktl::atomic<cpu_num_t>) == alignof(uint32_t));
  static_assert(sizeof(ktl::atomic<uintptr_t>) == sizeof(uintptr_t));
  static_assert(alignof(ktl::atomic<uintptr_t>) == alignof(uintptr_t));
  static_assert(sizeof(LazyOwnedWaitQueue) == sizeof(void*));
  static_assert(alignof(LazyOwnedWaitQueue) == alignof(void*));

  using NameStorage =
      spin_tracing::LockNameStorage<spin_tracing::LockType::kMutex,
                                    kSchedulerLockSpinTracingEnabled | kLockNameTracingEnabled>;
  static_assert(ktl::is_base_of_v<NameStorage, Mutex>);

  static constexpr size_t kSize = sizeof(Mutex);
  static constexpr size_t kAlign = alignof(Mutex);
  static constexpr size_t kCriticalMutexSize = sizeof(CriticalMutex);
  static constexpr size_t kCriticalMutexAlign = alignof(CriticalMutex);

  // An empty base class occupies no storage in Mutex (empty base optimization), even though
  // sizeof reports 1 for it.
  static constexpr size_t kNameStorageSize = ktl::is_empty_v<NameStorage> ? 0 : sizeof(NameStorage);

  static constexpr size_t kMagicOffset = offsetof(Mutex, magic_);
  static constexpr size_t kMaybeAcquiredOnCpuOffset = offsetof(Mutex, maybe_acquired_on_cpu_);
  static constexpr size_t kValOffset = offsetof(Mutex, val_);
  static constexpr size_t kWaitOffset = offsetof(Mutex, wait_);

  static constexpr uint32_t kMagic = Mutex::MAGIC;
  static constexpr uintptr_t kStateFree = Mutex::STATE_FREE;
  static constexpr uintptr_t kStateFlagContested = Mutex::STATE_FLAG_CONTESTED;
  static constexpr zx_duration_t kSpinMaxDuration = Mutex::SPIN_MAX_DURATION;
  static constexpr zx_duration_t kDefaultTimesliceExtension = Mutex::DEFAULT_TIMESLICE_EXTENSION;
};

#pragma GCC diagnostic pop

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_MUTEX_LAYOUT_H_
