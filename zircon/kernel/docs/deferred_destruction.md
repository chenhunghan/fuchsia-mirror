# Deferred Object Destruction

## Overview

Zircon kernel threads execute on small stacks (typically 8 KiB).
Reference-counted kernel objects (`Dispatcher` and `VmObject` subclasses) can
form deep ownership graphs, such as nested `ChannelDispatcher` messages carrying
channel handles, `JobDispatcher` trees, or long `VmObject` copy-on-write clone
chains. Their destructors can also require non-trivial stack space to acquire
`CriticalMutex` locks, tear down mappings, and return pages to the PMM.

Destroying such graphs recursively at the point where a reference count drops to
zero risks overflowing the kernel stack, and runs destructors deep inside call
stacks where locks may still be held. Previously, `Dispatcher` used a local
`SafeDeleter` list rooted at the first `fbl_recycle()` call while `VmObject`
used a separate mutex-guarded global `VmDeferredDeleter<T>` list.

Unified deferred destruction (`zircon/kernel/kernel/deferred_deleter.rs` and
`zircon/kernel/include/kernel/deferred_deleter.h`) replaces both mechanisms with
a single lock-free, per-thread FIFO scope installed on the stack at kernel entry
points and drained iteratively as the stack unwinds.

## Data Structures

### `DeferredDeleteNode`

`DeferredDeleteNode` is defined in Rust as an 8-byte, 8-byte-aligned
singly-linked node embedded in designated reference-counted base classes
(`Dispatcher` and `VmObject`), with an `OpaqueStorage`-backed C++ wrapper in
`zircon/kernel/include/kernel/deferred_deleter.h` representing its inline
allocation in C++ classes. It contains a single `next` word:

* `UNQUEUED` (`usize::MAX`, low bits `0b111`) when the object is not queued.
* `null` (`0`) when the object is the tail of a `DeferredDeleteScope`.
* A tagged pointer to the next queued object otherwise.

Replacing `Dispatcher::deleter_ll_` and `VmDeferredDeleter<T>` with
`DeferredDeleteNode` preserves `sizeof(Dispatcher)` and `VmObject` subclass FFI
layout offsets without extra memory overhead. Debug assertions verify that nodes
are never double-enqueued or destroyed while still linked.

### Pointer Tagging (`DeferredDeleteKind`)

Because participating heap objects are at least 8-byte aligned, the low two bits
of each queued object pointer encode `DeferredDeleteKind` (`Dispatcher = 0`,
`VmObject = 1`, and `TestObject = 2` in kernel unit tests). Low-bit tagging
allows heterogeneous objects to share one queue and dispatch node lookup and
destruction without storing a vtable or function pointer in
`DeferredDeleteNode`. Because bit 2 is always `0` on valid tagged pointers,
`UNQUEUED` (`usize::MAX`) cannot collide with any valid queue entry.

### `DeferredDeleteScope` and Thread-Local Discovery

`DeferredDeleteScope` is defined in Rust as a 16-byte stack-allocated queue
header (installed via `MaybeUninit<DeferredDeleteScope>` paired with
`DeferredDeleteGuard` or `enter_raw` / `leave_raw` in Rust, and via the
`OpaqueStorage`-backed `DeferredDeleteScope` wrapper in C++):

* `head`: pointer to the first tagged object entry, or `null` when empty.
* `tail`: pointer to `head` when empty, or to the tail node's `next` field when
  non-empty, enabling O(1) FIFO tail append. When a scope is nested inside an
  already-active outer scope (or detached), `tail` is `null`.

The outermost active scope on a thread is registered in
`Thread::recursive_object_deletion_list`, providing O(1) thread-local lookup
with no locks, atomic operations, or cross-CPU contention. While active and
during `Drop`, scope storage is manipulated strictly through raw pointers and
`UnsafeCell` without holding live `&` or `&mut` Rust references
(`DeferredDeleteGuard<'a>` binds the storage lifetime via
`PhantomData<&'a mut MaybeUninit<DeferredDeleteScope>>`), ensuring that
transitive destructors can soundly append to the scope via the thread-local
pointer under Stacked Borrows and Tree Borrows.

## Lifecycle

### 1. Scope Installation and Nesting

Entry-point scopes are installed where threads enter the kernel or process
asynchronous work items:

* Syscalls (`do_syscall_pre` / `do_syscall_post` in
  `zircon/kernel/lib/syscalls/syscalls.rs`).
* Hardware page faults (`vmm_page_fault_handler` in `zircon/kernel/vm/vmm.cc`).
* User exceptions (`dispatch_user_exception` in
  `zircon/kernel/object/exception.rs`).
* DPC worker callbacks (`DpcQueue::do_work` in `zircon/kernel/kernel/dpc.rs`).
* Pending thread-signal handlers (`process_pending_signals` /
  `process_pending_signals_slow` in `zircon/kernel/kernel/thread.rs` around
  `ProcessDispatcher::kill`, `current_do_suspend`, and
  `current_do_sample_stack`).

If an outer `DeferredDeleteScope` is already installed on the current thread
(for example, a user-copy page fault during a syscall), nested scopes leave
`tail` set to `null` and act as no-ops on entry and exit. Deferred objects
therefore remain queued until execution unwinds to the outermost entry point,
after inner locks have been released.

### 2. Enqueue at Reference Count Zero

When the last reference to a `Dispatcher` or `VmObject` is dropped from either
C++ (`fbl::RefPtr<T>`, `KernelHandle<T>`) or Rust (`fbl::RefPtr<T>`),
`fbl_recycle()` invokes `DeferredDeleter::EnqueueDispatcher` or
`DeferredDeleter::EnqueueVmObject`. All C++ `Dispatcher` subclasses (verified at
compile time by `DISPATCHER_VERIFY_OFFSET`) and `VmObject` subclasses
(`VmObjectPaged`, `VmObjectPhysical`) inherit `fbl::Recyclable<Derived>` and
forward `fbl_recycle()` to the base class so typed `RefPtr<Derived>` drops
always route through deferred recycling.

Because only the `1 -> 0` reference-count transition enqueues an object, the
caller holds unique ownership of the object and its `DeferredDeleteNode`.
Appending to the current thread's scope tail is O(1), allocation-free, and
lock-free.

### 3. Missing-Scope Synchronous Fallback

Code running outside an entry-point scope (such as early boot initialization
after `thread::init_early`, standalone kernel threads, or `k ut` unit tests
that check destructor side effects immediately after dropping a `RefPtr`) may
drop reference-counted objects when `Thread::recursive_object_deletion_list` is
`null`.

In that case, `enqueue_without_scope` installs a temporary `DeferredDeleteScope`
on the current stack frame, enqueues the object, and drains the scope
immediately before returning. Callers outside entry points thus retain
synchronous destruction semantics while still tearing down arbitrarily deep
object graphs iteratively in O(1) stack space.

### 4. Iterative Drain and Loop Detection

When the outermost scope unwinds, `leave_raw` detaches immediately if `head` is
`null`. When `head` is non-null, the cold `drain_installed` path pops entries
from `head` one by one while keeping the scope registered on `Thread`:

1. Unlink the head node and reset its `next` field to `UNQUEUED` (updating
   `tail` back to `&head` if the queue becomes empty).
2. Dispatch destruction (`delete disp` or `delete vmo`) based on the pointer's
   `DeferredDeleteKind` tag.
3. Any child or peer references dropped by that destructor append to `tail` and
   are processed by subsequent iterations of the same flat loop.

`drain_installed` counts destroyed objects in a single drain pass and asserts
`destroyed_count <= MAX_DEFERRED_DELETIONS` (`16 * 1024 * 1024`) in debug builds
to catch unbounded re-queuing bugs while permitting legitimate teardown of large
handle tables and VMO trees.

### 5. Non-Unwinding Stack Transitions

Paths that abandon or reset the current kernel stack without unwinding enclosing
frames must explicitly drain any queued objects (with interrupts enabled when
queued objects may be present) and detach any active scope:

* `thread::current_exit` (`zircon/kernel/kernel/thread.rs`) calls
  `drain_and_detach_current()`, and `ThreadDispatcher::StartRoutine`
  (`zircon/kernel/object/thread_dispatcher.cc`) calls
  `DeferredDeleter::DrainAndDetachCurrent()` before disabling interrupts and
  terminating or entering user mode (on empty kernel-only bootstrap exit paths
  such as `secondary_cpu_entry`, `drain_and_detach_current()` is a no-op even
  with interrupts disabled).
* `restricted_enter` calls `drain_current()` before disabling interrupts; if a
  thread signal is pending, it re-enables interrupts and returns normally
  through `do_syscall` with the syscall's scope still attached. Otherwise, once
  committed to entering restricted mode, it calls `detach_empty_current()`
  before resetting the kernel stack.

## Execution Context and Locking Invariants

* **Thread context required**: Enqueuing, scope installation/exit, and draining
  all require an initialized `Thread` in blockable thread context
  (`!blocking_disallowed()`). Interrupt handlers and other non-blockable
  contexts (`mp_sync_exec`, `SeqLock` write guards, VCPU execution) must never
  drop the final reference to a deferred-delete object or manipulate a scope.
* **Enqueue locking**: When a `DeferredDeleteScope` is already installed on the
  current thread, enqueuing only touches thread-local pointers and is safe while
  holding spinlocks, `ChainLock`s, or mutexes, or while interrupts are disabled.
* **Drain locking**: Draining queued objects (including the missing-scope
  synchronous fallback, `drain_current()`, and non-empty
  `drain_and_detach_current()` / `DrainAndDetachCurrent()`) requires no
  spinlocks or active `ChainLockTransaction`, no locks held that could conflict
  with `Dispatcher` or `VmObject` destructors, and interrupts enabled once the
  scheduler is active on the current CPU (`!ints_disabled() ||
  !scheduler::peek_is_active(curr_cpu_num())`, permitting pre-scheduler early
  boot initialization hooks). In `do_syscall_post`, draining runs before
  `disable_ints()`.
