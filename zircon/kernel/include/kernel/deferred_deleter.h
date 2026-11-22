// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_DEFERRED_DELETER_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_DEFERRED_DELETER_H_

#include <stddef.h>
#include <stdint.h>

#include <kernel/ffi.h>

// DeferredDeleteNode and DeferredDeleteScope are implemented in Rust
// (zircon/kernel/kernel/deferred_deleter.rs). The C++ classes below only reserve OpaqueStorage for
// their allocations when embedded in C++ objects or placed on the stack in C++ functions,
// forwarding lifecycle operations to these entry points.
extern "C" {

void rust_deferred_delete_node_init(void* storage);
void rust_deferred_delete_node_destroy(void* storage);
void rust_deferred_delete_scope_enter(void* storage);
void rust_deferred_delete_scope_leave(void* storage);
void rust_deferred_delete_drain_and_detach_current();

}  // extern "C"

// Singly-linked list node embedded in reference-counted kernel objects (such as Dispatcher and
// VmObject) whose destruction is deferred until the kernel stack unwinds to a kernel entry point.
//
// Invariant: Must be unqueued (not currently linked in any DeferredDeleteScope, with no live scope
// pointers referencing storage_) when destroyed.
class DeferredDeleteNode {
 public:
  DeferredDeleteNode() { rust_deferred_delete_node_init(&storage_); }
  ~DeferredDeleteNode() { rust_deferred_delete_node_destroy(&storage_); }

  DeferredDeleteNode(const DeferredDeleteNode&) = delete;
  DeferredDeleteNode& operator=(const DeferredDeleteNode&) = delete;
  DeferredDeleteNode(DeferredDeleteNode&&) = delete;
  DeferredDeleteNode& operator=(DeferredDeleteNode&&) = delete;

 private:
  static constexpr size_t kStorageSize = sizeof(uintptr_t);
  static constexpr size_t kStorageAlign = alignof(uintptr_t);

  OpaqueStorage<kStorageSize, kStorageAlign> storage_;
};

static_assert(sizeof(DeferredDeleteNode) == sizeof(uintptr_t));
static_assert(alignof(DeferredDeleteNode) == alignof(uintptr_t));

// Stack-allocated deferred-destruction scope installed at kernel entry points.
//
// When the outermost scope on the current thread unwinds, any objects queued for deferred
// destruction (including objects transitively dropped by their destructors) are iteratively
// destroyed in FIFO order. Nested scopes are no-ops so destruction always occurs at the outermost
// entry-point frame.
//
// Invariants:
// - Must be constructed on the stack of an initialized Thread in blockable thread context
//   (!arch_blocking_disallowed()), and must remain live, unmodified, and unaliased until its
//   destructor completes (unless its destructor is skipped on a non-returning stack transition
//   after calling DeferredDeleter::DrainAndDetachCurrent()).
// - Must be destroyed on the same thread in blockable thread context (!arch_blocking_disallowed()),
//   unless abandoned on a non-returning stack transition after calling
//   DeferredDeleter::DrainAndDetachCurrent(). Whenever queued objects may be present, interrupts
//   must be enabled (once the scheduler is active on the current CPU) and no spinlocks,
//   ChainLockTransactions, or locks conflicting with queued object destructors may be held.
// - Transitional invariant (until SafeDeleter in zircon/kernel/object/dispatcher.cc is removed):
//   SafeDeleter must not be active on the current thread when entering or leaving a scope, and no
//   Dispatcher may be recycled via SafeDeleter while a DeferredDeleteScope is active.
class DeferredDeleteScope {
 public:
  DeferredDeleteScope() { rust_deferred_delete_scope_enter(&storage_); }
  ~DeferredDeleteScope() { rust_deferred_delete_scope_leave(&storage_); }

  DeferredDeleteScope(const DeferredDeleteScope&) = delete;
  DeferredDeleteScope& operator=(const DeferredDeleteScope&) = delete;
  DeferredDeleteScope(DeferredDeleteScope&&) = delete;
  DeferredDeleteScope& operator=(DeferredDeleteScope&&) = delete;

 private:
  static constexpr size_t kStorageSize = 2 * sizeof(uintptr_t);
  static constexpr size_t kStorageAlign = alignof(uintptr_t);

  OpaqueStorage<kStorageSize, kStorageAlign> storage_;
};

static_assert(sizeof(DeferredDeleteScope) == 2 * sizeof(uintptr_t));
static_assert(alignof(DeferredDeleteScope) == alignof(uintptr_t));

// Entry points for draining the current thread's deferred destruction queue at non-unwinding
// thread transitions.
//
// Callers must run on an initialized Thread in blockable thread context
// (!arch_blocking_disallowed()), and SafeDeleter must not be active on the current thread.
class DeferredDeleter {
 public:
  // Drains any queued deferred deletions on the current thread and detaches any active stack
  // scope. Must be called in blockable thread context before non-returning transitions that
  // abandon or reset the current kernel stack (such as ThreadDispatcher::StartRoutine). Whenever
  // queued objects may be present, interrupts must be enabled (once the scheduler is active on the
  // current CPU) and no spinlocks, ChainLockTransactions, or locks that conflict with queued
  // object destructors may be held (interrupts may remain disabled on empty kernel-thread exit
  // paths where no objects are queued).
  static void DrainAndDetachCurrent() { rust_deferred_delete_drain_and_detach_current(); }
};

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_DEFERRED_DELETER_H_
