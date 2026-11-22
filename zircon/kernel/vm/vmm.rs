// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::vm::is_user_accessible;
use crate::arch_rs::{InterruptDisableGuard, blocking_disallowed, ints_disabled};
use crate::kernel::task_runtime_timers::PageFaultTimer;
use crate::kernel::thread::{self, Thread};
use crate::ktrace_rs::{ArgValue, KTrace};
use crate::platform_rs::timer::timer_current_mono_ticks;
use crate::vm::arch_vm_aspace::ArchVmAspace;
use crate::vm::fault::{self, flag};
use crate::vm::vm_aspace::VmAspace;
use core::ptr::NonNull;
use debug::{ltracef, tracef};
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;
const TRACE_PAGE_FAULT: u32 = 0;

// TODO(https://fxbug.dev/42084841): See https://fxbug.dev/42084841 for why we translate results.
/// Helper function for translating [`thread::page_fault`] status in the context of the
/// [`page_fault_handler`].
///
/// Long-term, we should eliminate this helper function.
fn translate_page_fault_status(orig_status: Result<(), Status>, flags: u32) -> Result<(), Status> {
    let Err(err) = orig_status else {
        return Ok(());
    };
    core::hint::cold_path();

    if err == thread::ZX_ERR_INTERNAL_INTR_RETRY {
        // We were asked to suspend.
        //
        // Regardless of whether the fault occurred while in usermode or kernel mode, we lie to the
        // caller.
        //
        // If the fault originated while the CPU was in usermode, the handler will unwind, call
        // `thread::process_pending_signals` and suspend.  All good.
        //
        // If the fault occurred during a user-copy, while the CPU was in kernel mode, the user-copy
        // will resume and the thread will re-fault if the fault has not already been resolved.
        // This is actually a problem because if we end up attempting to wait on the re-fault path,
        // deep down in `WaitQueueBase::block_etc_preamble`, we'll see that the thread is still
        // signaled and return without blocking.  We can end up in an expensive busy-loop,
        // re-faulting over and over.
        //
        // TODO(https://fxbug.dev/443281947): Fix the busy-loop issue described above.  Once that's
        // been fixed or we correctly support suspend during page faults we should debug_assert!
        // here that flags has `flag::USER`.
        return Ok(());
    }

    if err == thread::ZX_ERR_INTERNAL_INTR_KILLED && (flags & flag::USER) != 0 {
        // We were asked to terminate and the fault originated in usermode.
        //
        // Because it was usermode, we know we aren't in a usercopy.  Similar to above, lie to the
        // caller.  The thread will unwind and terminate before returning back to usermode.
        return Ok(());
    }

    orig_status
}

/// Internal routine used by the scheduler to swap MMU contexts.
///
/// `oldspace` and `newaspace` must not refer to the same address space (neither both `None` nor
/// `Some` pointing to the same `VmAspace`).
///
/// # Safety
///
/// The caller must ensure that:
/// - Interrupts are disabled on the calling CPU.
/// - `oldspace` is the user address space currently loaded on the calling CPU (or `None` if no
///   user address space is loaded).
/// - `newaspace`, if `Some`, is an initialized `Type::User` or `Type::LowKernel` `VmAspace` that
///   remains alive (not dropped/freed, so its `ArchVmAspace` is not destroyed) while it is loaded
///   on the calling CPU.
#[inline]
pub unsafe fn context_switch(oldspace: Option<&VmAspace>, newaspace: Option<&VmAspace>) {
    debug_assert!(ints_disabled());

    // SAFETY: Discharged by the caller: interrupts are disabled, `oldspace` is the user address
    // space loaded on this CPU, distinct `VmAspace`s embed distinct `ArchVmAspace`s so the old and
    // new `ArchVmAspace`s do not match, and `newaspace`'s `ArchVmAspace` is an initialized user
    // `ArchVmAspace` (`flags == 0` for `Type::User` and `Type::LowKernel`) that is not destroyed
    // while loaded on this CPU.
    unsafe {
        ArchVmAspace::context_switch(
            oldspace.map(VmAspace::arch_aspace),
            newaspace.map(VmAspace::arch_aspace),
        );
    }
}

/// Page fault handler, called during page fault context, with interrupts enabled.
///
/// Enforce stack-frame isolation here so full-LTO never inlines `page_fault_handler`'s stack frame
/// into top-level `exception_handler`, whose stack frame remains live across nested IRQs.
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU's current `Thread` has been constructed (`ConstructFirstThread` completed)
///   and is in the `THREAD_RUNNING` state; if `addr` is a user-accessible address, the current
///   thread is either a user thread or a kernel thread with an attached initialized, non-unified
///   `Type::User` `VmAspace`.
/// - Interrupts are enabled (`!ints_disabled()`) and blocking is permitted
///   (`!blocking_disallowed()`).
/// - No spinlocks, active `ChainLockTransaction`, conflicting `ArchVmAspace`/`VmObject` locks, or
///   (when `pf_flags & flag::ACCESS == 0`) lockdep-tracked locks are held on the calling CPU.
#[inline(never)]
pub unsafe fn page_fault_handler(addr: zx_types::zx_vaddr_t, pf_flags: u32) -> Result<(), Status> {
    debug_assert!(!ints_disabled());
    debug_assert!(!blocking_disallowed());

    // hardware fault, mark it as such
    let flags = pf_flags | flag::HW_FAULT;

    let current_thread: *mut Thread = thread::current_get();
    // SAFETY: The caller guarantees the calling CPU has a constructed, running current `Thread`,
    // which stays alive while it runs this function.
    let current_thread: &Thread = unsafe { &*current_thread };
    let start_time = timer_current_mono_ticks();
    let trace_start_time = KTrace::timestamp();
    // SAFETY: `current_thread` is the current thread, and `_timer` is dropped on it before this
    // function returns.
    let _timer = unsafe { PageFaultTimer::new(current_thread, start_time) };

    if TRACE_PAGE_FAULT != 0 || LOCAL_TRACE != 0 {
        tracef!(
            "thread {} va {:#x}, flags {:#x} ({})\n",
            current_thread.name(),
            addr,
            flags,
            fault::flags_to_string(flags)
        );
    }

    // Page faults never happen on kernel addresses. Double check this is a valid user address, then
    // continue with the user aspace.
    if !is_user_accessible(addr) {
        core::hint::cold_path();
        ltracef!("PageFault: Invalid virtual address {:#x}\n", addr);
        return Err(Status::NOT_FOUND);
    }

    // page fault it
    // SAFETY: Discharged by this function's `# Safety` contract: `is_user_accessible(addr)`
    // returned `true` (so `!is_kernel_address(addr)`), the calling CPU has a valid running
    // `Thread` (with an attached initialized, non-unified `Type::User` `VmAspace` for user
    // addresses), interrupts are enabled, blocking is allowed, and no spinlocks or active
    // `ChainLockTransaction` are held. Unless `flags` contains `flag::ACCESS`, no locks are held,
    // so blocking on page requests is allowed; if it does, no locks conflicting with
    // `ArchVmAspace`'s internal lock are held.
    let status = translate_page_fault_status(unsafe { thread::page_fault(addr, flags) }, flags);

    crate::ktrace_rs::complete!(
        "kernel:vm",
        "page_fault",
        trace_start_time,
        "vaddr" => ArgValue::Pointer(addr),
        "flags" => fault::flags_to_string(flags)
    );

    status
}

/// Sets `aspace` as the active user address space on the current thread, or unmaps the current user
/// address space if `aspace` is [`None`].
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU has a valid, initialized current [`Thread`]; if called from an interrupt or
///   exception handler, the previous active address space is restored before returning.
/// - The current thread's `Thread::aspace` currently matches the user address space loaded in the
///   calling CPU's hardware MMU (i.e. not mid-context-switch and not out of sync after a raw
///   [`thread::switch_aspace`], [`thread::current_switch_aspace`], or
///   [`thread::current_clear_aspace`] call).
/// - Any currently active address space being replaced remains alive until the hardware MMU switch
///   away from it completes.
/// - If `aspace` is `Some`, it points to an initialized `Type::User` or `Type::LowKernel`
///   `VmAspace` that remains alive (not dropped/freed, so its `ArchVmAspace` is not destroyed)
///   while active on the thread.
/// - If `aspace` is `None`, the current thread can safely run without an attached user address
///   space.
pub unsafe fn set_active_aspace(aspace: Option<NonNull<VmAspace>>) {
    let aspace_ptr: *mut VmAspace = aspace.map_or(core::ptr::null_mut(), NonNull::as_ptr);
    ltracef!("aspace {:p}\n", aspace_ptr);

    let t = thread::current_get();
    debug_assert!(!t.is_null());

    // SAFETY: By this function's `# Safety` contract, `t` is the valid, initialized current thread.
    if core::ptr::eq(aspace_ptr, unsafe { thread::active_aspace(t) }) {
        return;
    }

    let _irqd = InterruptDisableGuard::new();
    // SAFETY: By this function's `# Safety` contract, `t` is the valid, initialized current thread,
    // `aspace` (if `Some`) remains alive while attached to it (or, if `None`, `t` can safely run
    // without an attached user address space), and any replaced address space remains alive until
    // the hardware MMU switch away from it completes below.
    let old = unsafe { thread::switch_aspace(t, aspace) };
    // SAFETY: `old` is null or the aspace that was attached to the current thread until now, which
    // by this function's `# Safety` contract stays alive until the hardware switches away from it
    // in `context_switch` below.
    let old = unsafe { old.as_ref() };
    // SAFETY: By this function's `# Safety` contract, `aspace` (if `Some`) points to a live,
    // initialized `VmAspace` for the duration of `context_switch` below.
    let new = unsafe { aspace.map(|a| a.as_ref()) };
    // SAFETY: Interrupts are disabled by `_irqd`. By this function's `# Safety` contract,
    // `Thread::aspace` matched the hardware MMU context on entry (and any intervening interrupt or
    // exception before `_irqd` that called `set_active_aspace` restored the previous active address
    // space before returning), so `old` is the user address space currently loaded on this CPU and
    // still differs from `new` (`core::ptr::eq` returned `false` above and only the current thread
    // mutates its `aspace`). By this function's `# Safety` contract, `new`, if `Some`, is an
    // initialized `Type::User` or `Type::LowKernel` `VmAspace` that remains alive while loaded.
    unsafe { context_switch(old, new) };
}

/// Kernel console `vmm` command implementation.
#[cfg(console_enabled)]
mod console {
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use crate::kernel::thread;
    use crate::kernel::types::PAddr;
    use crate::platform_rs::timer::InstantMono;
    use crate::vm::arch_vm_aspace::{
        ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE,
    };
    use crate::vm::vm_aspace::{Type, VmAspace};
    use core::ffi::{c_int, c_void};
    use core::pin::Pin;
    use fbl::RefPtr;
    use kprint::kprintln;
    use ksync::{KCell, KMutex};
    use lazy_init::{LazyInit, NoCheck};
    use pin_init::pin_init;
    use zx_status::Status;

    /// Synchronized static storage for the `vmm` console command's active test address space.
    #[ksync::guarded]
    struct TestAspaceStatic {
        #[mutex]
        lock: KMutex<crate::ksync_kernel::TrackedMutex>,
        #[guarded_by(lock)]
        aspace: Option<RefPtr<VmAspace>>,
    }

    /// Active test address space manipulated by `k vmm` subcommands.
    static TEST_ASPACE: LazyInit<TestAspaceStatic, NoCheck> = LazyInit::uninit();

    fn test_aspace_lock_init(_level: init::LkInitLevel) {
        // SAFETY: lk init hooks run once during single-threaded early boot before any console
        // command can access `TEST_ASPACE`.
        unsafe {
            let _ = Pin::static_ref(&TEST_ASPACE).init_pin(pin_init!(TestAspaceStatic {
                lock <- KMutex::init(),
                aspace: KCell::new(None),
            }));
        }
    }
    init::lk_init_hook!(test_aspace_lock, test_aspace_lock_init, init::LK_INIT_LEVEL_EARLIEST);

    impl TestAspaceStatic {
        #[inline]
        fn get() -> &'static Self {
            // SAFETY: `TEST_ASPACE` is initialized at `LK_INIT_LEVEL_EARLIEST` before any console
            // command can access it.
            unsafe { TEST_ASPACE.get_unchecked() }
        }

        /// Returns a cloned reference to the current test address space, initializing it to the
        /// kernel address space if not yet set.
        fn get_or_init(&self) -> RefPtr<VmAspace> {
            ksync::lock!(let mut guard = self.lock_lock());
            guard.as_mut().aspace_mut().get_or_insert_with(VmAspace::kernel_aspace_ref).clone()
        }

        /// Replaces the stored test address space with `val` and returns the previous value so it
        /// can be dropped outside `TestAspaceStatic::lock`.
        fn replace(&self, val: Option<RefPtr<VmAspace>>) -> Option<RefPtr<VmAspace>> {
            ksync::lock!(let mut guard = self.lock_lock());
            core::mem::replace(guard.as_mut().aspace_mut(), val)
        }

        /// Clears and returns the stored test address space if it points to the same `VmAspace` as
        /// `target`.
        fn clear_if_eq(&self, target: &RefPtr<VmAspace>) -> Option<RefPtr<VmAspace>> {
            ksync::lock!(let mut guard = self.lock_lock());
            let slot = guard.as_mut().aspace_mut();
            if slot
                .as_ref()
                .is_some_and(|t| core::ptr::eq(RefPtr::as_ptr(t), RefPtr::as_ptr(target)))
            {
                slot.take()
            } else {
                None
            }
        }
    }

    /// Constructs a `RefPtr<VmAspace>` from a raw address supplied on the console.
    ///
    /// Returns `None` if `addr` is `0` or misaligned.
    ///
    /// # Safety
    ///
    /// If `addr` is non-zero and aligned to `VmAspace`, it must be the exposed address of a
    /// `VmAspace` to which at least one reference is held for the duration of the call.
    unsafe fn aspace_from_addr(addr: u64) -> Option<RefPtr<VmAspace>> {
        let raw_ptr = core::ptr::with_exposed_provenance::<VmAspace>(addr as usize);
        if raw_ptr.is_null() || !raw_ptr.is_aligned() {
            None
        } else {
            // SAFETY: Caller guarantees `raw_ptr` points to a `VmAspace` to which at least one
            // reference is held for the duration of the call.
            Some(unsafe { VmAspace::clone_from_ptr(raw_ptr) })
        }
    }

    /// Console command handler for `k vmm`.
    ///
    /// # Safety
    ///
    /// - `argc` must be `>= 1` and `argv` must be non-null and valid for reads of `argc`
    ///   initialized `CmdArgs` entries for the duration of the call.
    /// - For subcommands that take raw addresses (`alloc_physical`, `free_aspace`,
    ///   `set_test_aspace`), the caller-supplied address arguments must refer to valid physical
    ///   ranges or live `VmAspace` instances respectively, the latter as printed by
    ///   `create_aspace` or `create_test_aspace`.
    /// - For `alloc_physical`, uncached-device is an appropriate cache policy for all mappings of
    ///   the physical range, and creating a read-write kernel mapping of it is sound.
    /// - For `free_region`, the address is in a mapping of the root VMAR whose whole VMO range is
    ///   pinned by a pin the caller owns, and nothing uses that mapping any more.
    unsafe extern "C" fn cmd_vmm(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INTERNAL.into_raw();
        }
        // SAFETY: `argv` is non-null and points to `argc` initialized `CmdArgs` entries from the
        // console dispatcher.
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd_name = args[0].as_str();
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!("{:s} aspaces", cmd_name);
            kprintln!("{:s} kaspace", cmd_name);
            kprintln!("{:s} alloc <size> <align_pow2>", cmd_name);
            kprintln!("{:s} alloc_physical <paddr> <size> <align_pow2>", cmd_name);
            kprintln!("{:s} alloc_contig <size> <align_pow2>", cmd_name);
            kprintln!("{:s} free_region <address>", cmd_name);
            kprintln!("{:s} create_aspace", cmd_name);
            kprintln!("{:s} create_test_aspace", cmd_name);
            kprintln!("{:s} free_aspace <address>", cmd_name);
            kprintln!("{:s} set_test_aspace <address>", cmd_name);
            Status::INTERNAL.into_raw()
        };
        let not_enough_args = || -> c_int {
            kprintln!("not enough arguments");
            usage()
        };

        if args.len() < 2 {
            return not_enough_args();
        }

        let test_aspace = TestAspaceStatic::get().get_or_init();
        let subcmd = args[1].as_str();

        if subcmd == "aspaces" {
            VmAspace::dump_all_aspaces();
        } else if subcmd == "kaspace" {
            VmAspace::kernel_aspace().dump();
        } else if subcmd == "alloc_physical" {
            if args.len() < 4 {
                return not_enough_args();
            }

            let mut ptr = core::ptr::without_provenance_mut::<c_void>(0x99);
            let align = if args.len() >= 5 { args[4].arg_uint as u8 } else { 0u8 };
            let size = args[3].arg_uint as usize;
            // SAFETY: The cache policy in the flags is appropriate for all mappings of the
            // physical range, and creating this mapping is sound.
            let res = unsafe {
                test_aspace.alloc_physical(
                    b"physical test",
                    size,
                    None,
                    align,
                    PAddr(args[2].arg_uint as usize),
                    0,
                    ARCH_MMU_FLAG_UNCACHED_DEVICE
                        | ARCH_MMU_FLAG_PERM_READ
                        | ARCH_MMU_FLAG_PERM_WRITE,
                )
            };
            if let Ok(base) = res
                && size != 0
            {
                ptr = core::ptr::with_exposed_provenance_mut(base);
            }
            let err = Status::result_into_raw(res.map(|_| ()));
            kprintln!("VmAspace::AllocPhysical returns {}, ptr {:p}", err, ptr);
        } else if subcmd == "alloc_contig" {
            if args.len() < 3 {
                return not_enough_args();
            }

            let mut ptr = core::ptr::without_provenance_mut::<c_void>(0x99);
            let align = if args.len() >= 4 { args[3].arg_uint as u8 } else { 0u8 };
            // SAFETY: Creating a mapping of a new contiguous VMO is sound.
            let res = unsafe {
                test_aspace.alloc_contiguous(
                    b"contig test",
                    args[2].arg_uint as usize,
                    None,
                    align,
                    0,
                    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                )
            };
            if let Ok(base) = res {
                ptr = core::ptr::with_exposed_provenance_mut(base);
            }
            let err = Status::result_into_raw(res.map(|_| ()));
            kprintln!("VmAspace::AllocContiguous returns {}, ptr {:p}", err, ptr);
        } else if subcmd == "free_region" {
            if args.len() < 3 {
                return not_enough_args();
            }

            // SAFETY: The caller guarantees that `args[2].arg_uint` is in a mapping of the root
            // VMAR whose whole VMO range is pinned by a pin the caller owns, and that nothing uses
            // that mapping any more.
            let err = Status::result_into_raw(unsafe {
                test_aspace.free_region(args[2].arg_uint as usize)
            });
            kprintln!("VmAspace::FreeRegion returns {}", err);
        } else if subcmd == "create_aspace" {
            let aspace = VmAspace::create(Type::User, b"test");
            kprintln!(
                "VmAspace::Create aspace {:#x}",
                aspace.as_ref().map_or(0, |aspace| RefPtr::as_ptr(aspace).expose_provenance())
            );
        } else if subcmd == "create_test_aspace" {
            let aspace = VmAspace::create(Type::User, b"test");
            kprintln!(
                "VmAspace::Create aspace {:#x}",
                aspace.as_ref().map_or(0, |aspace| RefPtr::as_ptr(aspace).expose_provenance())
            );
            let Some(aspace) = aspace else {
                return Status::NO_MEMORY.into_raw();
            };

            let old_aspace = TestAspaceStatic::get().replace(Some(aspace.clone()));
            // SAFETY: `aspace` is kept alive in `TEST_ASPACE` (and the local `aspace` binding)
            // while attached to the current thread, and `old_aspace` is kept alive until after the
            // address space switch and reschedule sleep complete.
            unsafe { thread::current_switch_aspace(&aspace) };
            // XXX hack to force it to reschedule and thus load the aspace
            let _ = thread::sleep(InstantMono(1));
            drop(old_aspace);
        } else if subcmd == "free_aspace" {
            if args.len() < 3 {
                return not_enough_args();
            }

            // SAFETY: Console debugging command operating on a user-supplied `VmAspace` address.
            let Some(aspace) = (unsafe { aspace_from_addr(args[2].arg_uint) }) else {
                kprintln!("invalid address");
                return usage();
            };
            let old_test_aspace = TestAspaceStatic::get().clear_if_eq(&aspace);
            drop(old_test_aspace);

            // SAFETY: Console commands run on a kernel thread, so the calling CPU has a valid,
            // initialized current thread.
            let is_active = unsafe { thread::current_active_aspace_ptr() }
                .is_some_and(|a| core::ptr::eq(a.as_ptr(), RefPtr::as_ptr(&aspace)));
            if is_active {
                // SAFETY: Detaching the active user address space before destroying it.
                unsafe { thread::current_clear_aspace() };
                let _ = thread::sleep(InstantMono(1)); // hack
            }

            let err = Status::result_into_raw(aspace.destroy());
            kprintln!("VmAspace::Destroy() returns {}", err);
        } else if subcmd == "set_test_aspace" {
            if args.len() < 3 {
                return not_enough_args();
            }

            if args[2].arg_uint == 0 {
                let old_aspace = TestAspaceStatic::get().replace(None);
                // SAFETY: Detaching the test user address space from this console kernel thread;
                // `old_aspace` is kept alive until after the reschedule sleep completes.
                unsafe { thread::current_clear_aspace() };
                // XXX hack to force it to reschedule and thus unload the aspace
                let _ = thread::sleep(InstantMono(1));
                drop(old_aspace);
            } else {
                // SAFETY: Console debugging command operating on a user-supplied `VmAspace`
                // address, kept alive in `TEST_ASPACE` while attached to the current thread.
                let Some(aspace) = (unsafe { aspace_from_addr(args[2].arg_uint) }) else {
                    kprintln!("invalid address");
                    return usage();
                };
                let old_aspace = TestAspaceStatic::get().replace(Some(aspace.clone()));
                // SAFETY: `aspace` is kept alive in `TEST_ASPACE` (and the local `aspace` binding)
                // while attached to the current thread, and `old_aspace` is kept alive until after
                // the address space switch and reschedule sleep complete.
                unsafe { thread::current_switch_aspace(&aspace) };
                // XXX hack to force it to reschedule and thus load the aspace
                let _ = thread::sleep(InstantMono(1));
                drop(old_aspace);
            }
        } else {
            kprintln!("unknown command");
            return usage();
        }

        zx_status::sys::ZX_OK
    }

    static_command!(
        VMM,
        c"vmm".as_ptr(),
        c"virtual memory manager".as_ptr(),
        cmd_vmm,
        CMD_AVAIL_NORMAL
    );
}
