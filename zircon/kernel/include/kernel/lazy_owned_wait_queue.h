// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_LAZY_OWNED_WAIT_QUEUE_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_LAZY_OWNED_WAIT_QUEUE_H_

#include <zircon/compiler.h>

#include <fbl/macros.h>
#include <kernel/ffi.h>
#include <kernel/spinlock.h>
#include <ktl/type_traits.h>

class OwnedWaitQueue;
class OwnedWaitQueuePool;

extern "C" Lock<SpinLock>* rust_owned_wait_queue_pool_lock_get();

// Global lock for OwnedWaitQueuePool.
//
// If this lock will be held at the same time as OwnedWaitQueue::get_lock(), then
// OwnedWaitQueue's lock must be acquired first.
struct OwnedWaitQueuePoolLock {
  static Lock<SpinLock>* Get() TA_RET_CAP(rust_owned_wait_queue_pool_lock_get()) {
    return rust_owned_wait_queue_pool_lock_get();
  }
};

// Global pool accessor policy.
struct GlobalOwqPoolAccess {};

// Local pool accessor policy for tests.
//
// The Rust implementation treats |pool_| as a reference, so the pool must outlive every
// LocalLazyOwnedWaitQueue that uses it.
struct LocalOwqPoolAccess {
  constexpr explicit LocalOwqPoolAccess(OwnedWaitQueuePool& pool) : pool_(&pool) {}
  OwnedWaitQueuePool* const pool_;
};

template <typename PoolAccess = GlobalOwqPoolAccess>
class LazyOwnedWaitQueueImpl;

using LazyOwnedWaitQueue = LazyOwnedWaitQueueImpl<GlobalOwqPoolAccess>;
using LocalLazyOwnedWaitQueue = LazyOwnedWaitQueueImpl<LocalOwqPoolAccess>;

// The implementation of LazyOwnedWaitQueueImpl lives in Rust
// (zircon/kernel/kernel/lazy_owned_wait_queue.rs).
extern "C" {
void rust_lazy_owned_wait_queue_destroy(LazyOwnedWaitQueue* lazy);
OwnedWaitQueue* rust_lazy_owned_wait_queue_get(LazyOwnedWaitQueue* lazy);
void rust_lazy_owned_wait_queue_release_locked(LazyOwnedWaitQueue* lazy)
    TA_REQ(OwnedWaitQueuePoolLock::Get());
OwnedWaitQueue* rust_lazy_owned_wait_queue_detach(LazyOwnedWaitQueue* lazy);
OwnedWaitQueue* rust_lazy_owned_wait_queue_peek(const LazyOwnedWaitQueue* lazy);

void rust_local_lazy_owned_wait_queue_destroy(LocalLazyOwnedWaitQueue* lazy);
OwnedWaitQueue* rust_local_lazy_owned_wait_queue_get(LocalLazyOwnedWaitQueue* lazy);
void rust_local_lazy_owned_wait_queue_release_locked(LocalLazyOwnedWaitQueue* lazy)
    TA_REQ(OwnedWaitQueuePoolLock::Get());
OwnedWaitQueue* rust_local_lazy_owned_wait_queue_detach(LocalLazyOwnedWaitQueue* lazy);
OwnedWaitQueue* rust_local_lazy_owned_wait_queue_peek(const LocalLazyOwnedWaitQueue* lazy);
}  // extern "C"

namespace internal {

// The Rust FFI entry points implementing each instantiation of LazyOwnedWaitQueueImpl.
template <typename PoolAccess>
struct LazyOwnedWaitQueueFfi;

template <>
struct LazyOwnedWaitQueueFfi<GlobalOwqPoolAccess> {
  static constexpr auto Destroy = rust_lazy_owned_wait_queue_destroy;
  static constexpr auto Get = rust_lazy_owned_wait_queue_get;
  static constexpr auto ReleaseLocked = rust_lazy_owned_wait_queue_release_locked;
  static constexpr auto Detach = rust_lazy_owned_wait_queue_detach;
  static constexpr auto Peek = rust_lazy_owned_wait_queue_peek;
};

template <>
struct LazyOwnedWaitQueueFfi<LocalOwqPoolAccess> {
  static constexpr auto Destroy = rust_local_lazy_owned_wait_queue_destroy;
  static constexpr auto Get = rust_local_lazy_owned_wait_queue_get;
  static constexpr auto ReleaseLocked = rust_local_lazy_owned_wait_queue_release_locked;
  static constexpr auto Detach = rust_local_lazy_owned_wait_queue_detach;
  static constexpr auto Peek = rust_local_lazy_owned_wait_queue_peek;
};

}  // namespace internal

// A wrapper class for OwnedWaitQueue that lazily borrows a queue from a pool
// when necessary.
template <typename PoolAccess>
class LazyOwnedWaitQueueImpl : private PoolAccess {
  static constexpr bool kIsGlobal = ktl::is_same_v<PoolAccess, GlobalOwqPoolAccess>;
  static_assert(kIsGlobal || ktl::is_same_v<PoolAccess, LocalOwqPoolAccess>,
                "LazyOwnedWaitQueueImpl is only implemented for the global and local pools.");
  // The Rust destructor does not destroy the |PoolAccess| base, which is destroyed here instead.
  static_assert(ktl::is_trivially_destructible_v<PoolAccess>);

  using Ffi = internal::LazyOwnedWaitQueueFfi<PoolAccess>;

 public:
  // An all-zero |storage_| is the empty state of the Rust LazyOwnedWaitQueue, so that a
  // LazyOwnedWaitQueue (and so a Mutex) can be constant-initialized.
  constexpr LazyOwnedWaitQueueImpl()
    requires kIsGlobal
  = default;

  // Limit this constructor to use by the LocalLazyOwnedWaitQueue that is used in tests.
  //
  // The Rust implementation reads |pool| from the |PoolAccess| base, which precedes |storage_|.
  constexpr explicit LazyOwnedWaitQueueImpl(OwnedWaitQueuePool& pool)
    requires(!kIsGlobal)
      : PoolAccess(pool) {}

  DISALLOW_COPY_ASSIGN_AND_MOVE(LazyOwnedWaitQueueImpl);

  ~LazyOwnedWaitQueueImpl() { Ffi::Destroy(this); }

  // Returns a pointer to an OwnedWaitQueue, borrowing from the pool if necessary.
  //
  // Callers should only call this when they are preparing to use the returned queue to block.
  // Callers are responsible for coordinating amongst each other so that the last caller on each
  // LazyOwnedWaitQueue to *unblock* after calling this calls Detach() or ReleaseLocked() to
  // recycle the queue to the pool.
  //
  // The returned queue may be detached and recycled by another thread at any time, so callers
  // must acquire its lock and then check that Peek() still returns it before using it.
  //
  // Thread-safe. Will not block but may acquire the pool spinlock.
  OwnedWaitQueue* Get() TA_EXCL(OwnedWaitQueuePoolLock::Get()) { return Ffi::Get(this); }

  // Releases the underlying OwnedWaitQueue back to the pool if it exists.
  //
  // In addition to the requirements of Detach(), the queue (if any) must have no owner.
  //
  // Will not block.
  void ReleaseLocked() TA_REQ(OwnedWaitQueuePoolLock::Get()) { Ffi::ReleaseLocked(this); }

  // Detaches the underlying queue and returns it. Caller takes ownership and is responsible
  // for calling OwnedWaitQueuePool::RecycleLocked() on the returned queue when done.
  //
  // No thread may be blocked in the queue, and no other thread may hold the queue's lock having
  // observed via Peek() that it is attached, since callers of Get() rely on a queue that they have
  // locked and observed attached staying attached for as long as they hold its lock. Callers
  // ensure this by holding the queue's lock across the detach.
  //
  // Will not block.
  [[nodiscard]] OwnedWaitQueue* Detach() { return Ffi::Detach(this); }

  // Returns the current pointer without allocating. Will neither block nor acquire the pool
  // spinlock. Thread-safe.
  OwnedWaitQueue* Peek() const { return Ffi::Peek(this); }

 private:
  // Storage for the Rust `AtomicPtr<OwnedWaitQueue>`, which is null when no queue is attached.
  [[maybe_unused]] OpaqueStorage<sizeof(void*), alignof(void*)> storage_{};
};

// These must match the sizes asserted against the Rust types in
// zircon/kernel/kernel/lazy_owned_wait_queue.rs.
static_assert(sizeof(LazyOwnedWaitQueue) == sizeof(void*),
              "LazyOwnedWaitQueue must be exactly pointer-sized.");
static_assert(alignof(LazyOwnedWaitQueue) == alignof(void*));
static_assert(sizeof(LocalLazyOwnedWaitQueue) == 2 * sizeof(void*));
static_assert(alignof(LocalLazyOwnedWaitQueue) == alignof(void*));

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_LAZY_OWNED_WAIT_QUEUE_H_
