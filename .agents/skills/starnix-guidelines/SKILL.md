---
name: starnix-guidelines
description: >
  Starnix kernel coding guidelines, best practices, and review criteria. Use
  when authoring, modifying, or evaluating Starnix kernel code (//src/starnix)
  for Linux UAPI/ABI fidelity, safe user memory access (MemoryAccessor),
  starnix_sync LockDep ordering, LSM/SELinux security hooks, and resource
  lifecycle management. Intended for use by coding agents, review agents, and
  orchestrators. Do NOT use as a standalone review orchestrator (use /review
  instead).
---

# Starnix Kernel Guidelines & Best Practices

Use these guidelines when authoring, reviewing, or debugging code in the Starnix
kernel (`//src/starnix`), including syscall implementations, the virtual
filesystem (VFS), memory management (`mm`), task lifecycle, and kernel modules.
For general Fuchsia Rust style and API conventions, also follow the
`rust_best_practices` skill (`.agents/skills/rust_best_practices/SKILL.md`).

## 1. Linux UAPI & ABI Fidelity

Starnix runs unmodified Linux binaries (`glibc`, `bionic`, and static binaries).
Exact conformance to Linux ABI and kernel semantics is required.

- **Strict `Errno` Matching**:
  - Verify return values and error codes against Linux man pages (`man 2
    <syscall>`) and upstream Linux kernel sources (`fs/`, `kernel/`, `mm/`,
    `net/`).
  - Construct errors using `error!(E...)` and `errno!(E...)` from
    `starnix_uapi::errors::Errno`.
  - Common error code rules:
    - Return `EFAULT` when user pointers are unmapped, invalid, or out of
      bounds.
    - Return `EINVAL` for invalid flags, sizes, or unaligned pointers only when
      specified by Linux.
    - Return `ESRCH` vs. `ECHILD` for process and thread lookups according to
      POSIX/Linux rules.
    - Return `ENOTSUP` / `EOPNOTSUPP` only when Linux explicitly defines them
      for unsupported file or socket operations.
  - Never expose raw Fuchsia `zx::Status` errors or map them ad-hoc; use
    `from_status_like_fdio!(status)` (from `starnix_uapi`) or an explicit,
    verified status mapping table.
  - When leaving unimplemented Linux flags or behavior as a stub, record it with
    `track_stub!(TODO("https://fxbug.dev/..."), "...")` from `starnix_logging`
    rather than failing silently.
- **Architecture & 32-bit/64-bit Compatibility (`arch32`)**:
  - Distinguish 64-bit (`uapi::*`) vs. 32-bit (`uapi::arch32::*`) UAPI layouts
    (`x86_64`, `aarch64`, `riscv64` vs. 32-bit `arm`).
  - Use `MultiArchUserRef<T64, T32>` and `MappingMultiArchUserRef<T, T64, T32>`
    (from `starnix_uapi::user_address`) with `ArchSpecific::is_arch32()` to
    handle pointer widths, struct alignment, and `time_t` / `timespec` sizes.
  - Use `UserValue<T>` (`starnix_uapi::user_value::UserValue`) to wrap untrusted
    scalar values passed from userspace before validation.
- **Behavioral Edge Cases**:
  - Validate zero-length buffer behavior (many syscalls succeed as a no-op
    returning `0` when `count == 0` without reading or faulting on the user
    pointer).
  - Verify signal interruption and restart behavior: check whether a blocked
    syscall must return `EINTR` or a restart code (`ERESTARTSYS`,
    `ERESTARTNOINTR`, `ERESTART_RESTARTBLOCK`) when `SA_RESTART` is set.

## 2. Safe User Memory Handling

User memory is untrusted, volatile, and can be concurrently modified or unmapped
by other user threads at any time.

- **Zero Direct Dereferencing**:
  - Never cast `UserAddress` or `UserRef<T>` to raw Rust pointers or references.
  - Always read and write user memory through `MemoryAccessor` and
    `MemoryAccessorExt` methods on `CurrentTask` or `Task`
    (`//src/starnix/kernel/core/mm/memory_accessor.rs`):

    ```rust
    let value = current_task.read_object(user_ref)?;
    current_task.write_object(user_ref, &value)?;
    let bytes = current_task.read_memory_to_vec(user_address, length)?;
    let path = current_task.read_path(user_cstring)?;
    ```

  - In VFS `FileOps::read` and `FileOps::write` implementations, transfer data
    through `InputBuffer` and `OutputBuffer`
    (`//src/starnix/kernel/core/vfs/buffers/`) rather than raw `UserAddress`
    pointers.
  - Any struct read from or written to user memory must derive `zerocopy` traits
    (`FromBytes`, `IntoBytes`, `Immutable`, `KnownLayout`).
- **Time-of-Check to Time-of-Use (TOCTOU) Prevention**:
  - Copy user structures or buffers into kernel memory *once* before validating
    fields.
  - Never validate fields in user memory and re-read them later; another user
    thread can mutate the buffer between the check and the use.
- **Bounds Checking & `EFAULT` Propagation**:
  - Use checked arithmetic (`checked_add`, `checked_mul`) on user-provided
    offsets and lengths to prevent integer overflow.
  - Propagate `EFAULT` immediately when `MemoryAccessor` operations fail (unless
    partial transfer semantics require returning the number of bytes already
    transferred).

## 3. Concurrency, Locking & `starnix_sync` Hierarchy

Starnix uses fine-grained locking across multi-threaded Linux workloads. Lock
inversions and holding locks across blocking operations cause kernel lockups.

- **Lock Ordering & `starnix_sync`**:
  - Use `LockDepMutex<T, L>` and `LockDepRwLock<T, L>` from
    `//src/starnix/lib/starnix_sync` with a typed `LockLevel` `L`.
  - Declare every lock level and its acquisition edges (`Parent => Child` or
    `Terminal(Leaf)`) in `//src/starnix/lib/starnix_sync/src/lock_ordering.rs`
    (`lock_ordering!`).
  - When acquiring multiple locks of the same level, use `ordered_lock`,
    `ordered_read_lock`, or `ordered_write_lock` (which acquire locks in
    memory-address order), or `allow_subclass()` when hierarchical nesting of
    the same lock level is sound.
- **Never Hold Kernel Locks Across User Memory Accesses**:
  - Do not hold internal Starnix locks while reading from or writing to user
    memory. User memory accesses can trigger page faults (acquiring
    `MemoryManager` locks or waiting on `userfaultfd`), creating lock-order
    inversions and deadlocks:

    ```rust
    // BAD: Holding an internal lock while writing to user memory
    let state = self.state.lock();
    current_task.write_object(user_ptr, &state.data)?; // Deadlock risk

    // GOOD: Copy data under the lock, drop the guard, then write to user memory
    let data = self.state.lock().data.clone();
    current_task.write_object(user_ptr, &data)?;
    ```

- **Uninterruptible Locks & Blocking Operations**:
  - `CurrentTask::run_in_state` asserts
    `assert_lock_level::<UninterruptibleLock>()` before blocking a task.
  - Never hold any lock ordered after `UninterruptibleLock` across blocking task
    waits (`Waiter::wait`), blocking channel reads, or synchronous FIDL calls to
    external Fuchsia services (such as Fxfs, Netstack, or Scenic).
  - Only locks explicitly ordered *before* `UninterruptibleLock` in
    `lock_ordering.rs` (such as `FileObjectOffset` or `FsNodeAppend`) may be
    held across interruptible waits.
- **RCU (`starnix_rcu` / `fuchsia_rcu`)**:
  - Keep `RcuReadScope` guards (`RcuReadScope::new()`) tightly scoped around
    non-blocking reads (`Idr`, `RcuHashMap`, `FsNode` state). Clone the required
    `Arc` or value and drop the scope before blocking or taking heavy locks.

## 4. Resource Lifecycles & Handle Management

- **Zircon Handles & VMO Mappings**:
  - Verify that error paths do not leak Zircon handles (`zx::Vmo`, `zx::Socket`,
    `zx::EventPair`, `zx::Channel`) or VMAR mappings.
  - Ensure `FileObject` close and release paths (`FileOps::close`, `FdTable`
    teardown) release underlying kernel objects and cancel active wait
    registrations.
- **Task, `ThreadGroup`, & Zombie Management**:
  - Verify that `ThreadGroup` and `Task` termination properly signals parent
    processes (`SIGCHLD`), wakes wait queues (`wait4` / `waitid`), and
    transitions tasks to zombie state until reaped.
  - Ensure single-thread exit (`sys_exit`) releases task-local resources without
    terminating the `ThreadGroup` unless it is the last thread or executing
    `exit_group`.

## 5. Security, Capabilities & LSM / SELinux Hooks

- **Capability Checks**:
  - Gate privileged operations with `security::check_task_capable(current_task,
    CAP_...)` (or `security::is_task_capable_noaudit(current_task, CAP_...)`
    when probing capabilities without emitting an audit log).
  - Do not check raw capability bitmasks on `current_task.real_creds()` directly
    for access control, as doing so bypasses LSM/SELinux policy and auditing.
- **LSM / SELinux Hooks (`//src/starnix/kernel/core/security/`)**:
  - Mediate operations on files, nodes, sockets, tasks, and BPF objects through
    the hook functions in `crate::security` (e.g.,
    `security::fs_node_permission`, `security::file_permission`,
    `security::check_fs_node_create_access`,
    `security::check_task_create_access`,
    `security::check_socket_create_access`, `security::check_file_ioctl_access`,
    and `security::check_bpf_access`).
  - Treat security state types (`security::FsNodeState`,
    `security::FileObjectState`, `security::SocketState`, etc.) as opaque
    outside `//src/starnix/kernel/core/security/` and never invoke
    `selinux_hooks` directly from general kernel or VFS code.

## 6. Performance & Fast-Path Hygiene

- **Allocation Avoidance in Hot Paths**:
  - High-frequency syscalls (`read`, `write`, `readv`, `writev`, `epoll_wait`,
    `poll`, `futex`, `clock_gettime`, `ioctl`) run millions of times per second.
  - Avoid unnecessary heap allocations (`Vec::new()`, `String`, `Box`) on hot
    paths; prefer stack-backed buffers (`SmallVec`, `read_objects_to_smallvec`,
    `read_memory_to_array`, or fixed-size stack arrays).
- **FIDL Round-Trips & Caching**:
  - Minimize synchronous FIDL round-trips to external Fuchsia services on hot
    VFS and socket paths, and preserve VFS node metadata caching where permitted
    by filesystem semantics.

## 7. Testing & Verification Checklist

Every Starnix change should be verified with appropriate automated tests:

1.  **In-Process Unit Tests**: Add `#[::fuchsia::test]` unit tests in
    `//src/starnix/kernel/core` or the relevant module crate and run `fx test
    starnix_core_tests`.
2.  **Syscall & Linux Conformance Tests**: When changing production code in
    `//src/starnix` that adds or modifies userspace-observable Linux syscall or
    kernel behavior (as opposed to internal refactorings, `//src/starnix/lib/*`
    data structures, `//src/starnix/runner`, `//src/starnix/tools`, or features
    covered by other integration suites under `//src/starnix/tests/*`), include
    syscall-level test coverage in `//src/starnix/tests/syscalls` (such as C++
    syscall tests in `//src/starnix/tests/syscalls/cpp` run via `fx test
    starnix_syscalls_cpp_tests`, or Rust syscall tests in
    `//src/starnix/tests/syscalls/rust` run via `fx test
    starnix_syscalls_rust_tests`), and verify relevant gVisor
    (`//src/starnix/tests/gvisor`) or LTP (`//src/starnix/tests/ltp`) test
    suites.
    - **Minimizing `test_helper::IsStarnix()`**: Keep use of
      `test_helper::IsStarnix()` in `//src/starnix/tests/syscalls` to an
      absolute minimum. Because these suites execute the same tests against both
      Linux (`host`, `linux_syscalls_cpp_tests`, and
      `linux_syscalls_rust_tests`) and Starnix to validate that their behavior
      matches, Starnix-exclusive tests or assertions undermine cross-platform
      verification and usually indicate a bug in Starnix or a flawed test
      assumption:
      - Write syscall tests to perform the same actions and assert the same
        Linux-conformant outcomes unconditionally on both Linux and Starnix. Do
        not add `IsStarnix()` branches to assert Starnix-internal implementation
        side effects or current Starnix limitations.
      - When a test fails on Linux, first investigate whether the Starnix
        behavior diverges from Linux or whether the test can be written with
        portable Linux primitives so that it runs identically on both platforms.
      - When Starnix does not yet match Linux behavior due to a known limitation
        or unimplemented feature, write the test to assert the Linux behavior
        and record the failure in the test expectations file
        (`//src/starnix/tests/syscalls/cpp/expectations/*.json5` or
        `//src/starnix/tests/syscalls/rust/expectations/*.json5`) with a
        `TODO(https://fxbug.dev/...)` comment rather than branching on
        `test_helper::IsStarnix()`.
      - When a test depends on privileges, kernel version, or optional system
        features, gate on the specific capability or runtime probe (such as
        `test_helper::HasCapability(...)`, `test_helper::HasSysAdmin()`,
        `test_helper::IsKernelVersionAtLeast(...)`, or `access(..., F_OK)`)
        rather than `test_helper::IsStarnix()` alone; pair a probe with
        `!test_helper::IsStarnix()` only so Starnix fails loudly rather than
        silently skipping a feature it is expected to provide.
      - In the rare cases where `test_helper::IsStarnix()` is unavoidable (such
        as testing a strictly Starnix-specific container feature like `remotefs`
        or `remote_bundle`, or isolating a documented, bug-tracked divergence so
        the rest of a test still runs on both platforms), keep the `IsStarnix()`
        scope as narrow as possible, either perform the same actions and branch
        only on the platform-dependent expectation or perform platform-dependent
        setup to confirm the same result (never both different actions and
        different assertions), and explain why in a comment (with a
        `TODO(https://fxbug.dev/...)` when tracking a bug).
    - **Documenting Unreachable Syscall Coverage (Commit Authoring)**: When
      authoring a commit for a production code change in `//src/starnix` that
      modifies userspace-observable syscall or kernel behavior but cannot be
      covered by syscall-level tests in `//src/starnix/tests/syscalls`, the
      commit message must explicitly explain why syscall-level test coverage is
      not possible (or not appropriate, such as would be the case if it were
      redundant with what gVisor and/or LTP are already testing).
3.  **SELinux / Android Compatibility**: When modifying security hooks, Binder,
    or Android-specific subsystems, verify `//src/starnix/tests/selinux` or
    relevant compatibility tests.
4.  **LockDep & Sanitizer Clean**: Ensure tests pass with
    `detect_lock_dep_cycles` enabled without LockDep ordering panics or
    sanitizer errors.
