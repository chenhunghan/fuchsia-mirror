// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014-2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Secondary CPU start-up: each secondary CPU's bootstrap thread and stack, and the first Rust
//! code the CPU runs.

use core::ffi::c_void;
use core::mem::{MaybeUninit, align_of, offset_of, size_of};
use core::ptr::with_exposed_provenance_mut;

use init::{LK_INIT_LEVEL_EARLIEST, LK_INIT_LEVEL_THREADING, LkInitFlags, LkInitLevel};
use zx_status::Status;
use zx_types::{ZX_TLS_STACK_GUARD_OFFSET, ZX_TLS_UNSAFE_SP_OFFSET};

use super::arch::arm64_cpu_early_init;
use super::feature::{arm64_feature_current_is_first_in_cluster, arm64_feature_debug};
use super::mp::{arch_curr_cpu_num, arch_mp_init_percpu};
use super::thread::thread_arch;
use crate::kernel::mp::SMP_MAX_CPUS;
use crate::kernel::thread::{Thread, current_get, secondary_cpu_init_early};
use crate::kernel::types::{PAddr, cpu_num_t};
use crate::top::init::lk_init_level;
use crate::top::main::lk_secondary_cpu_entry;
use crate::vm::kstack::KernelStack;
use crate::vm::vm::kernel_physical_address_of;

unsafe extern "C" {
    /// The cold start-up routine for secondary CPUs when starting from an OFF state (defined in
    /// `secondary.S`). Only its address is used from Rust.
    fn arm64_secondary_start(stack: *mut c_void) -> !;

    /// The start-up routine for CPUs resuming from a PSCI CPU_SUSPEND or SYSTEM_SUSPEND powerdown
    /// state (defined in `secondary.S`). Only its address is used from Rust.
    fn arm64_secondary_resume_start(resume_context: *mut c_void) -> !;
}

/// Returns the physical address of `arm64_secondary_start` (`secondary.S`).
///
/// # Safety-usable invariant
///
/// The returned physical address is the entry point of `arm64_secondary_start` in the kernel image.
pub fn arm64_secondary_start_paddr() -> PAddr {
    // SAFETY: `arm64_secondary_start` is code in the kernel image, inside the physically
    // contiguous `[__executable_start, __data_end)` range that `kernel_physical_address_of`
    // requires.
    unsafe { kernel_physical_address_of((arm64_secondary_start as *const ()).addr()) }
}

/// Returns the physical address of `arm64_secondary_resume_start` (`secondary.S`).
///
/// # Safety-usable invariant
///
/// The returned physical address is the entry point of `arm64_secondary_resume_start` in the
/// kernel image.
pub fn arm64_secondary_resume_start_paddr() -> PAddr {
    // SAFETY: `arm64_secondary_resume_start` is code in the kernel image, inside the physically
    // contiguous `[__executable_start, __data_end)` range that `kernel_physical_address_of`
    // requires.
    unsafe { kernel_physical_address_of((arm64_secondary_resume_start as *const ()).addr()) }
}

/// one for each secondary CPU, indexed by (cpu_num - 1).
/// Zero-initialized BSS storage; each `Thread` is constructed in place by
/// [`secondary_cpu_init_early`].
///
/// Rust reaches the slots only through place expressions and raw pointers, never a reference: a
/// slot holds no `Thread` until its CPU first starts, and each start constructs one again in place.
static mut INIT_THREAD: [MaybeUninit<Thread>; SMP_MAX_CPUS - 1] =
    [const { MaybeUninit::zeroed() }; SMP_MAX_CPUS - 1];

/// The stack each secondary CPU starts on, indexed by (cpu_num - 1), kept until the next create or
/// free for the CPU tears it down.
///
/// Held outside [`INIT_THREAD`], because [`secondary_cpu_init_early`] empties the `Thread`'s own
/// stack without tearing it down. Only [`arm64_create_secondary_stack`] and
/// [`arm64_free_secondary_stack`] reach the elements, through raw pointers.
//
// SAFETY: Each field of `KernelStack` is a `Mapping`, which holds only an `Option<RefPtr<_>>`, and
// `RefPtr` is a `#[repr(transparent)]` `NonNull`, so all zero bytes are `None` in each: an empty
// stack, the same as `KernelStack::default()`.
static mut INIT_STACK: [KernelStack; SMP_MAX_CPUS - 1] = unsafe { core::mem::zeroed() };

/// Structure used to pass information to a newly booted secondary cpu.
///
/// SP will be set to the bottom of this structure.
#[repr(C)]
struct Arm64SpInfo {
    /// SCS pointer points to array of addresses.
    shadow_call_sp: *mut usize,
    /// Pad so that its size is a multiple of 16.
    pad: u64,

    /// Stack-protector guard. With `unsafe_sp`, it serves temporarily as the fake `arch_thread`
    /// in the thread pointer, so that safe-stack and stack-protector code can work early. The
    /// thread pointer (TPIDR_EL1) points just past `Arm64SpInfo`.
    stack_guard: usize,
    /// Never actually used in the kernel.
    unsafe_sp: *mut c_void,
}

zr::static_assert!(size_of::<Arm64SpInfo>() == 32, "check arm64_secondary_start assembly");
zr::static_assert!(
    offset_of!(Arm64SpInfo, shadow_call_sp) == 0,
    "check arm64_secondary_start assembly"
);
zr::static_assert!(size_of::<Arm64SpInfo>().is_multiple_of(16));
zr::static_assert!(align_of::<Arm64SpInfo>() == 8);

/// Returns the offset from the thread pointer of the [`Arm64SpInfo`] field at `offset`.
const fn tp_offset(offset: usize) -> isize {
    offset as isize - size_of::<Arm64SpInfo>() as isize
}
zr::static_assert!(tp_offset(offset_of!(Arm64SpInfo, stack_guard)) == ZX_TLS_STACK_GUARD_OFFSET);
zr::static_assert!(tp_offset(offset_of!(Arm64SpInfo, unsafe_sp)) == ZX_TLS_UNSAFE_SP_OFFSET);

/// Allocates a stack for the secondary cpu with bootstrap data placed on it.
/// Ready to be passed to the cpu when starting it.
/// On success, returns a virtual address near the top of the stack just below
/// the bootstrap payload.
///
/// Frees the stack of the CPU's previous start first, if it has one.
///
/// # Safety
///
/// `cpu_num` must be in `1..SMP_MAX_CPUS`, since this indexes the per-CPU stacks with
/// `cpu_num - 1`. CPU `cpu_num` must be fully stopped: the bootstrap thread of its previous start
/// has exited and switched off the stack this frees. The caller must serialize this with every
/// other create and free for `cpu_num`, and the calling thread's `Thread` must be initialized.
pub unsafe fn arm64_create_secondary_stack(cpu_num: cpu_num_t) -> Result<usize, Status> {
    // Allocate a stack for the init thread of this particular secondary cpu.
    debug_assert!(cpu_num > 0 && (cpu_num as usize) < SMP_MAX_CPUS, "cpu_num: {}", cpu_num);
    // SAFETY: Taking the address of an element of `INIT_STACK` reads nothing and forms no
    // reference.
    let stack = unsafe { &raw mut INIT_STACK[cpu_num as usize - 1] };
    // SAFETY: `stack` is a valid `KernelStack`, empty from its zero bytes at boot until this
    // function first initializes it. Only this function and `arm64_free_secondary_stack` access
    // it, and the caller serializes those for `cpu_num`.
    let stack = unsafe { &mut *stack };
    // Free the previous start's stack. CPU `cpu_num` is fully stopped (see `# Safety`), so that
    // start's bootstrap thread has exited and is off the stack.
    stack.teardown()?;
    stack.init()?;

    // Get the stack pointers.
    let sp = stack.top();
    debug_assert!(sp.is_multiple_of(16));
    debug_assert!(stack.shadow_call_base() != 0);
    // The shadow call stack grows up.
    let shadow_call_sp = with_exposed_provenance_mut::<usize>(stack.shadow_call_base());

    // Place the secondary bootstrap structure at the top of the stack.
    let mut cpu = with_exposed_provenance_mut::<Arm64SpInfo>(sp);
    cpu = cpu.wrapping_sub(1);

    // Store the necessary cpu boot info.
    // SAFETY: `sp` is the page-aligned top of the stack just mapped, so `cpu` is the aligned,
    // writable top `Arm64SpInfo` of it, and nothing else uses the stack until CPU `cpu_num` starts
    // on it. The calling thread's `Thread` is initialized (see `# Safety`), so `current_get`
    // returns it.
    unsafe {
        (*cpu).stack_guard = (*thread_arch(current_get())).stack_guard;
        (*cpu).shadow_call_sp = shadow_call_sp;
    }

    Ok(cpu.expose_provenance())
}

/// Frees a stack created by [`arm64_create_secondary_stack`].
///
/// # Safety
///
/// `cpu_num` must be in `1..SMP_MAX_CPUS`, and CPU `cpu_num` must not be running on the stack
/// or start on it later. The caller must serialize this with every other create and free for
/// `cpu_num`.
pub unsafe fn arm64_free_secondary_stack(cpu_num: cpu_num_t) -> Result<(), Status> {
    debug_assert!(cpu_num > 0 && (cpu_num as usize) < SMP_MAX_CPUS);
    // SAFETY: Taking the address of an element of `INIT_STACK` reads nothing and forms no
    // reference.
    let stack = unsafe { &raw mut INIT_STACK[cpu_num as usize - 1] };
    // SAFETY: `stack` is a valid `KernelStack`, as in `arm64_create_secondary_stack`. Only that
    // function and this one access it, and the caller serializes those for `cpu_num`.
    unsafe { (*stack).teardown() }
}

/// called from assembly.
///
/// # Safety
///
/// Only `arm64_secondary_start` calls this, on a secondary CPU that `platform_start_cpu` started
/// with a stack from [`arm64_create_secondary_stack`]: interrupts disabled, SP at the bootstrap
/// structure at the top of that stack, TPIDR_EL1 just past it, and x18 at its `shadow_call_sp`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_secondary_entry() {
    // SAFETY: This CPU is starting. `topology_cpu_init` registered its mpid before first starting
    // it, the boot CPU made its own call in `arch_early_init` before that, and nothing on this CPU
    // has used per-CPU state through x20 since `arm64_secondary_start`.
    unsafe { arm64_cpu_early_init() };

    let cpu = arch_curr_cpu_num();
    // SAFETY: Taking the address of an element of `INIT_THREAD` reads nothing and forms no
    // reference.
    let thread = unsafe { &raw mut INIT_THREAD[cpu as usize - 1] }.cast::<Thread>();
    // SAFETY:
    // - Interrupts are disabled on entry (see `# Safety`), and `arm64_cpu_early_init` does not
    //   unmask them.
    // - TPIDR_EL1 still points just past this start's `Arm64SpInfo`, the fake `arch_thread`:
    //   `arm64_secondary_start` set it, and `arm64_cpu_early_init` does not write it.
    // - `thread` is this CPU's own `INIT_THREAD` slot, so it is non-null, aligned and writable
    //   storage for a `Thread`, which the static keeps pinned and live. No other CPU accesses the
    //   slot while this CPU runs. It is not in `THREAD_LIST`: before the CPU's first start the slot
    //   holds no `Thread`, and on a restart `arch_mp_cpu_hotplug` requires the CPU to be fully
    //   stopped, so the thread of its previous start has exited. `construct_first_thread` made
    //   that thread detached, and a detached thread's exit erases it from the list.
    // - The boot CPU's `lk_main` ran `thread::init_early` (`thread_lists_init`) and `kernel_init`
    //   (`PerCpu::initialize_secondaries_begin`) before `platform_init` first started a secondary
    //   CPU.
    // - Nothing on this CPU since `arm64_secondary_start` takes `Thread::get_list_lock()` or opens
    //   a `ChainLockTransaction`.
    // The CPU runs on the stack that `arm64_create_secondary_stack` set up for this start, which
    // `INIT_STACK` holds, not `thread`.
    unsafe { secondary_cpu_init_early(thread) };
    // Run early secondary cpu init routines up to the threading level.
    // SAFETY: This is the start-up sequence that the `SecondaryCpus` hooks below
    // `LK_INIT_LEVEL_THREADING` are written for: this CPU's first thread is installed, and the CPU
    // does not run threads yet.
    unsafe {
        lk_init_level(
            LkInitFlags::SecondaryCpus,
            LK_INIT_LEVEL_EARLIEST,
            LkInitLevel(LK_INIT_LEVEL_THREADING.0 - 1),
        )
    };

    arch_mp_init_percpu();

    let full_dump = arm64_feature_current_is_first_in_cluster();
    // SAFETY: `arm64_cpu_early_init` above ran `arm64_feature_init` on this CPU, and with
    // interrupts disabled and no threads running yet, this code stays on this CPU.
    unsafe { arm64_feature_debug(full_dump) };

    // SAFETY: This CPU runs on its bootstrap stack with interrupts disabled, once for each time it
    // starts, and the boot CPU's `arch_init` ran `lk_init_secondary_cpus` before starting any
    // secondary CPU.
    unsafe { lk_secondary_cpu_entry() };
}
