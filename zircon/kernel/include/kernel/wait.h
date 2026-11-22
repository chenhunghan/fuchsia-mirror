// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_WAIT_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_WAIT_H_

#include <assert.h>
#include <lib/kconcurrent/chainlock.h>

#include <fbl/canary.h>
#include <fbl/intrusive_wavl_tree.h>
#include <fbl/macros.h>
#include <kernel/scheduler_state.h>
#include <ktl/utility.h>

struct Thread;
class WaitQueueCollection;
class WaitQueueBase;
class WaitQueue;

// When blocking this enum indicates the kind of resource ownership that is being waited for
// that is causing the block.
enum class ResourceOwnership {
  // Blocking is either not for any particular resource, or it is to wait for
  // exclusive access to a resource.
  Normal,
  // Blocking is happening whilst waiting for shared read access to a resource.
  Reader,
};

// Whether a block or a sleep can be interrupted.
enum class Interruptible : bool { No, Yes };

extern "C" {
void rust_wait_queue_base_destroy(WaitQueueBase* wait_queue);
}  // extern "C"

// The collection of threads currently blocked in a wait queue.
class WaitQueueCollection {
 private:
  // fwd decls
  struct BlockedThreadTreeTraits;

 public:
  using PrimaryKey = SchedTime;
  static constexpr PrimaryKey kPrimaryKeyZero = PrimaryKey{0};

  using Key = ktl::pair<PrimaryKey, uintptr_t>;

  // Encapsulation of all the per-thread state for the WaitQueueCollection data structure.
  class ThreadState {
   public:
    ThreadState() = default;

    ~ThreadState() { DEBUG_ASSERT(blocking_wait_queue_ == nullptr); }

    // Disallow copying.
    ThreadState(const ThreadState&) = delete;
    ThreadState& operator=(const ThreadState&) = delete;

   private:
    // WaitQueues, WaitQueueCollections, and their List types, can
    // directly manipulate the contents of the per-thread state, for now.
    friend class Scheduler;
    friend class WaitQueue;
    friend class WaitQueueBase;
    friend class WaitQueueCollection;
    friend struct WaitQueueCollection::BlockedThreadTreeTraits;

    // If blocked, a pointer to the WaitQueue the Thread is on.
    WaitQueueBase* blocking_wait_queue_ = nullptr;

    // Node state for existing in WaitQueueCollection::threads_
    fbl::WAVLTreeNodeState<Thread*> blocked_threads_tree_node_;

    // Primary key used for determining our position in the collection of
    // blocked threads. Pre-computed during insert in order to save a time
    // during insert, rebalance, and search operations.
    PrimaryKey blocked_threads_tree_sort_key_ = kPrimaryKeyZero;

    // The minimum relative deadline of this node's subtree, if any.
    SchedDuration subtree_min_deadline_{SchedDuration::Max()};

    // Return code if woken up abnormally from suspend, sleep, or block.
    zx_status_t blocked_status_ = ZX_OK;

    // Are we allowed to be interrupted on the current thing we're blocked/sleeping on?
    Interruptible interruptible_ = Interruptible::No;

    // Storage used by an OwnedWaitQueue, but held within a thread
    // instance, while that thread is blocked in the wait queue.
    SchedulerState::WaitQueueInheritedSchedulerState inherited_scheduler_state_storage_{};
  };

  constexpr WaitQueueCollection() {}
  ~WaitQueueCollection() { DEBUG_ASSERT(threads_.is_empty()); }

  // Passthru for the underlying container's size method.
  uint32_t Count() const { return static_cast<uint32_t>(threads_.size()); }

  // Peek at the first Thread in the collection.
  Thread* PeekFront() { return threads_.is_empty() ? nullptr : &threads_.front(); }
  const Thread* PeekFront() const { return const_cast<WaitQueueCollection*>(this)->PeekFront(); }

  // Accessor for the underlying thread collection.
  const auto& threads() const { return threads_; }

  // Disallow copying and moving.
  WaitQueueCollection(const WaitQueueCollection&) = delete;
  WaitQueueCollection& operator=(const WaitQueueCollection&) = delete;

  WaitQueueCollection(WaitQueueCollection&&) = delete;
  WaitQueueCollection& operator=(WaitQueueCollection&&) = delete;

 private:
  struct BlockedThreadTreeTraits {
    static Key GetKey(const Thread& thread);
    static bool LessThan(Key a, Key b) { return a < b; }
    static bool EqualTo(Key a, Key b) { return a == b; }
    static fbl::WAVLTreeNodeState<Thread*>& node_state(Thread& thread);
  };

  // The C++ view of the tree of blocked threads. All mutation (`insert`, `remove`) happens in
  // Rust, which maintains the node links and the `subtree_min_deadline_` invariant, so no observer
  // runs on this side; C++ only reads the tree through this type.
  using BlockedThreadTree =
      fbl::WAVLTree<Key, Thread*, BlockedThreadTreeTraits, fbl::DefaultObjectTag,
                    fbl::SizeOrder::Constant, BlockedThreadTreeTraits>;
  BlockedThreadTree threads_;
};

// Base class for wait queue types, providing common functionality.
class WaitQueueBase : public ChainLockable {
 protected:
  explicit constexpr WaitQueueBase(uint32_t magic) : magic_(magic) {}
  ~WaitQueueBase();

  uint32_t magic_;
  WaitQueueCollection collection_ TA_GUARDED(get_lock());
};

// A basic wait queue for blocking and unblocking threads without resource
// ownership semantics.
class WaitQueue : public WaitQueueBase {
 public:
  static constexpr uint32_t kMagic = fbl::magic("wait");

  constexpr WaitQueue() : WaitQueueBase(kMagic) {}

  WaitQueue(WaitQueue&) = delete;
  WaitQueue(WaitQueue&&) = delete;
  WaitQueue& operator=(WaitQueue&) = delete;
  WaitQueue& operator=(WaitQueue&&) = delete;
};

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_WAIT_H_
