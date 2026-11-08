# KSync: Technical Design Document

This document covers the technical architecture, safety invariants, and macro
code generation details of the `ksync` crate.

## 1. Technical Architecture

`ksync` implements a **token-based synchronization pattern** (also known as
the "Ghost Token" pattern). Instead of encapsulating the protected data inside
the mutex or reader-writer lock struct itself (like `std::sync::Mutex<T>`), it
separates the lock state (`KMutex`, `BrwLockPi`) from the actual data storage
(`KCell`).

```
+-------------------------------------------------------------+
|                        Parent Struct                        |
|                                                             |
|   +-------------------+        +------------------------+   |
|   | KMutex/BrwLockPi  |        |  KCell<T, Class>       |   |
|   |  (Raw Lock State) |        |  (UnsafeCell wrapper)  |   |
|   +--------+----------+        +-----------+------------+   |
+------------|-------------------------------|----------------+
             | lock() / read_lock()          | get(token)
             v                               v
    +--------+------------+         +--------+-------+
    |  Lock Guard         |         |  &T / &mut T   |
    |                     |-------->|  (Safe Access) |
    |  - LockToken        |         +----------------+
    +---------------------+
```

### The Core Types

1.  **`LockToken<'a, Class>`**: A zero-sized type (ZST) that serves as
    compile-time proof that the lock for `Class` is currently held by the
    current thread. It has a lifetime `'a` bound to the active lock guard.
2.  **`KMutex<Class, M>`**: The mutual exclusion lock state representation,
    wrapping a raw lock `M: RawLock`. Locking it acquires the raw lock and
    returns a `KMutexGuard` holding the `LockToken`. There is no default `M`:
    the kernel's `TrackedMutex` is not part of this crate (see
    [Raw lock implementations](#raw-lock-implementations)).
3.  **`BrwLockPi<Class, L>`**: The priority-inheriting reader-writer lock state
    representation, wrapping a raw reader-writer lock `L: RawSharedLock` (again
    with no default). Locking it returns a `BrwLockPiReadGuard` or
    `BrwLockPiWriteGuard` holding the `LockToken`.
4.  **`KCell<T, Class>`**: A wrapper around `core::cell::UnsafeCell<T>`. It
    provides token-gated accessors (`get(&self, token)` and `get_mut(&self,
    token)`).

### Guard Types & The Instance-Bound Soundness Model

While the `LockToken` and `KCell` share a compile-time `Class` type parameter
to prevent mixing locks of different types, this type-level association is
insufficient to guarantee memory safety.

To provide safety, the macro generates custom guard structures (e.g.
`MyStructMuGuard`, `MyStructLockReadGuard`, `MyStructLockWriteGuard`) that wrap
standard lock guards and hold a reference to the parent structure instance.
These custom guards expose safe, compiler-checked projection accessors (such as
`.field()` or `.fields_mut()`). This guarantees safe access to guarded
fields without requiring any `unsafe` block or risking cross-instance token
mixing.

### Raw Lock Implementations

`ksync` is split into two parts:

*   The `ksync` crate (`//src/lib/ksync:ksync`) holds the lock traits
    (`RawLock`, `RawSharedLock`, `LockPolicy`, ...), the generic lock types
    (`KMutex`, `BrwLockPi`, `KCell`, ...), the macros, and the userspace raw
    locks used for host builds and tests.
*   The kernel's raw locks (`TrackedMutex`, `TrackedCriticalMutex`,
    `TrackedSpinlock`, `TrackedMonitoredSpinlock`, `RawBrwLockPi`,
    `RawBrwLockNoPi`), their policies, the `KSpinlock` / `KMonitoredSpinlock`
    aliases, and the `declare_singleton_mutex!`,
    `declare_singleton_critical_mutex!`, `declare_singleton_spinlock!` and
    `declare_singleton_monitored_spinlock!` macros live in
    `//zircon/kernel/lib/ksync`. This is a `kernel_rust_mod` that is compiled
    directly into the kernel crate and mounted at `crate::ksync_kernel`. The
    mutexes are implemented directly on the Rust kernel `Mutex` in
    `//zircon/kernel/kernel/mutex.rs`, and the spinlocks on the Rust kernel
    `SpinLock` and `MonitoredSpinLock` in `//zircon/kernel/kernel/spinlock.rs`,
    and re-exported from `crate::ksync_kernel`; `RawBrwLockPi` and
    `RawBrwLockNoPi` wrap the C++ kernel `BrwLockPi` and `BrwLockNoPi`.

As a result, neither the `ksync` crate nor any other non-kernel crate (such as
`fbl` or `region_alloc`) can name the kernel's raw locks. Generic code is
instead parameterized over its raw lock type (`M: RawLock`), and the final
usage site in the kernel crate supplies the concrete raw lock (e.g.
`crate::ksync_kernel::TrackedMutex`). For the same reason there is no default
raw lock type: `KMutex` and `BrwLockPi` fields in a `#[guarded]` struct (e.g.
`KMutex<TrackedSpinlock>` or `BrwLockPi<RawBrwLockPi>`) and
`declare_singleton_lock!` must always name their raw lock type explicitly.

## 2. Safe Exclusive Access

If a thread has unique exclusive access to the `KCell` container itself
(either by holding ownership `self` or an exclusive borrow `&mut self`), it
doesn't need a runtime lock token to access the data safely. The Rust borrow
checker already guarantees compile-time thread exclusivity:

*   **`get_inner_mut(&mut self) -> &mut T`**: Accesses the inner value mutably
    via `UnsafeCell::get_mut()`. This is safe because the `&mut self` borrow
    ensures no other borrows of the cell are active.
*   **`into_inner(self) -> T`**: Consumes the `KCell` container and returns the
    inner value `T` safely via `UnsafeCell::into_inner()`.

## 3. Macro Code Generation

The `#[guarded]` attribute proc-macro parses a struct definition, rewrites its
fields, and generates the lock acquisition methods.

### 3.1 Input Struct

```rust
#[guarded]
pub struct MyStruct {
    #[mutex]
    pub mu: KMutex<TrackedMutex>,

    #[brwlock]
    pub lock: BrwLockPi<RawBrwLockPi>,

    #[guarded_by(mu)]
    pub data1: u32,

    #[guarded_by(lock)]
    pub data2: i32,
}
```

### 3.2 Expanded Code Output (Simplified)

```rust
// 1. Unique Lock Class marker structs generated automatically
pub struct MyStructMuClass;
pub struct MyStructLockClass;

// 2. Struct fields rewritten to KMutex/BrwLockPi and KCell
pub struct MyStruct {
    pub mu: ::ksync::KMutex<MyStructMuClass, TrackedMutex>,
    pub lock: ::ksync::BrwLockPi<MyStructLockClass, RawBrwLockPi>,
    pub data1: ::ksync::KCell<u32, MyStructMuClass>,
    pub data2: ::ksync::KCell<i32, MyStructLockClass>,
}

// 3. Custom projection guard structures (stack-pinned)
#[pin_data(PinnedDrop)]
pub struct MyStructMuGuard<'a> {
    parent: &'a MyStruct,
    #[pin]
    inner: ::ksync::KMutexGuard<'a, MyStructMuClass, TrackedMutex>,
}

#[pin_data(PinnedDrop)]
pub struct MyStructLockReadGuard<'a> {
    parent: &'a MyStruct,
    #[pin]
    inner: ::ksync::BrwLockPiReadGuard<'a, MyStructLockClass, RawBrwLockPi>,
}

#[pin_data(PinnedDrop)]
pub struct MyStructLockWriteGuard<'a> {
    parent: &'a MyStruct,
    #[pin]
    inner: ::ksync::BrwLockPiWriteGuard<'a, MyStructLockClass, RawBrwLockPi>,
}

// 4. Safe projection accessors and field projection structs
pub struct MyStructLockReadFields<'b> {
    pub data2: &'b i32,
    _marker: ::core::marker::PhantomData<&'b ()>,
}

pub struct MyStructLockWriteFields<'b> {
    pub data2: &'b mut i32,
    _marker: ::core::marker::PhantomData<&'b ()>,
}

impl<'a> MyStructMuGuard<'a> {
    pub fn data1(&self) -> &u32 {
        unsafe { self.parent.data1.get(self.inner.token()) }
    }
    pub fn data1_mut(self: Pin<&mut Self>) -> &mut u32 { ... }
    pub fn fields<'b>(&'b self) -> MyStructMuFields<'b> { ... }
    pub fn fields_mut<'b>(self: Pin<&'b mut Self>) -> MyStructMuFieldsMut<'b> { ... }
}

impl<'a> MyStructLockReadGuard<'a> {
    pub fn data2(&self) -> &i32 {
        unsafe { self.parent.data2.get(self.inner.token()) }
    }
    pub fn fields<'b>(&'b self) -> MyStructLockReadFields<'b> { ... }
}

impl<'a> MyStructLockWriteGuard<'a> {
    pub fn data2(&self) -> &i32 {
        unsafe { self.parent.data2.get(self.inner.token()) }
    }
    pub fn data2_mut(self: Pin<&mut Self>) -> &mut i32 { ... }
    pub fn fields<'b>(&'b self) -> MyStructLockReadFields<'b> { ... }
    pub fn fields_mut<'b>(self: Pin<&'b mut Self>) -> MyStructLockWriteFields<'b> { ... }
}

// 5. Lock methods implemented on the parent struct returning PinInit blocks
impl MyStruct {
    #[inline]
    pub fn lock_mu(&self) -> impl pin_init::PinInit<MyStructMuGuard<'_>, ::core::convert::Infallible> {
        pin_init::pin_init!(MyStructMuGuard {
            parent: self,
            inner <- ::ksync::KMutexGuard::new(&self.mu),
        })
    }

    #[inline]
    pub fn read_lock(&self) -> impl pin_init::PinInit<MyStructLockReadGuard<'_>, ::core::convert::Infallible> {
        pin_init::pin_init!(MyStructLockReadGuard {
            parent: self,
            inner <- ::ksync::BrwLockPiReadGuard::new(&self.lock),
        })
    }

    #[inline]
    pub fn write_lock(&self) -> impl pin_init::PinInit<MyStructLockWriteGuard<'_>, ::core::convert::Infallible> {
        pin_init::pin_init!(MyStructLockWriteGuard {
            parent: self,
            inner <- ::ksync::BrwLockPiWriteGuard::new(&self.lock),
        })
    }
}
```


