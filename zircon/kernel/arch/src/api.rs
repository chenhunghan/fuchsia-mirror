// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Cross-architecture API definitions, types, and signature validation.

use core::sync::atomic::AtomicU32;

use zx_status::Status;
use zx_types::zx_restricted_state_t;

use super::{GeneralRegsSource, UserEntryState};
use crate::kernel::spin_tracing::EncodedLockId;
use crate::kernel::thread::Thread;
use crate::kernel::types::cpu_num_t;

// Function pointer type aliases for standard architecture functions.
pub type ArchEarlyInitFn = unsafe fn();
pub type ArchPrevmInitFn = unsafe fn();
pub type ArchInitFn = unsafe fn();
pub type ArchLateInitPercpuFn = unsafe fn();
pub type ArchEnterIdleStateFn = unsafe extern "C" fn();

pub type ArchPrepareUspaceFn<Iframe> = unsafe extern "C" fn(&UserEntryState, *mut Iframe);
pub type ArchEnterUspaceFn<Iframe> = unsafe extern "C" fn(*const Iframe) -> !;

pub type ArchThreadInitializeFn = unsafe extern "C" fn(*mut core::ffi::c_void, usize);
pub type ArchThreadConstructFirstFn = unsafe extern "C" fn(*mut core::ffi::c_void);
pub type ArchContextSwitchFn = unsafe fn(*mut Thread, *mut Thread);
pub type ArchSaveUserStateFn = unsafe extern "C" fn(*mut core::ffi::c_void);
pub type ArchRestoreUserStateFn = unsafe extern "C" fn(*mut core::ffi::c_void);
pub type ArchDumpThreadFn = unsafe fn(*const Thread);
pub type ArchThreadGetBlockedFpFn = unsafe fn(*const Thread) -> usize;
pub type ArchSetSuspendedGeneralRegsFn =
    unsafe extern "C" fn(*mut core::ffi::c_void, GeneralRegsSource, *mut core::ffi::c_void);
pub type ArchResetSuspendedGeneralRegsFn = unsafe extern "C" fn(*mut core::ffi::c_void);
pub type ArchGetInstructionPointerFn =
    unsafe fn(GeneralRegsSource, *const core::ffi::c_void) -> usize;
pub type ArchSetReturnInstructionPointerFn =
    unsafe fn(GeneralRegsSource, *mut core::ffi::c_void, usize) -> Result<(), Status>;

// Per-CPU state and spinlock function pointer type aliases (see kernel/spinlock.rs).
pub type ArchCurrCpuNumFn = fn() -> cpu_num_t;
pub type ArchGetRestrictedFlagFn = unsafe fn() -> bool;
pub type ArchSpinLockNonInstrumentedFn = unsafe fn(&AtomicU32, cpu_num_t);
pub type ArchSpinLockTraceInstrumentedFn = unsafe fn(&AtomicU32, cpu_num_t, EncodedLockId);
pub type ArchSpinTrylockFn = unsafe fn(&AtomicU32, cpu_num_t) -> bool;
pub type ArchSpinUnlockFn = unsafe fn(*const AtomicU32);

// Restricted mode function pointer type aliases.
pub type ArchValidateStatePreRestrictedEntryFn = fn(&zx_restricted_state_t) -> Result<(), Status>;
pub type ArchSaveStatePreRestrictedEntryFn<NormalState> = fn(&mut NormalState);
pub type ArchEnterRestrictedFn = fn(&zx_restricted_state_t) -> !;
pub type ArchSaveRestrictedSyscallStateFn<SyscallRegs> =
    fn(&mut zx_restricted_state_t, &SyscallRegs);
pub type ArchSaveRestrictedIframeStateFn<Iframe> = fn(&mut zx_restricted_state_t, &Iframe);
pub type ArchSaveRestrictedExceptionStateFn = unsafe fn(&mut zx_restricted_state_t);
pub type ArchRedirectRestrictedExceptionToNormalFn<NormalState> =
    unsafe fn(&NormalState, usize, usize, u64);
pub type ArchEnterFullFn<NormalState> = fn(&NormalState, usize, usize, u64) -> !;
pub type ArchDumpFn = fn(&zx_restricted_state_t);

/// Compile-time assertion macro that validates that the target architecture module
/// implements all standard architecture functions with identical signatures.
#[macro_export]
macro_rules! assert_arch_signatures {
    ($arch:ident) => {
        const _: () = {
            use $crate::arch_rs::api::*;

            let _: ArchEarlyInitFn = $arch::arch_early_init;
            let _: ArchPrevmInitFn = $arch::arch_prevm_init;
            let _: ArchInitFn = $arch::arch_init;
            let _: ArchLateInitPercpuFn = $arch::arch_late_init_percpu;
            let _: ArchEnterIdleStateFn = $arch::arch_enter_idle_state;
            let _: ArchPrepareUspaceFn<$arch::Iframe> = $arch::arch_prepare_uspace;
            let _: ArchEnterUspaceFn<$arch::Iframe> = $arch::arch_enter_uspace;
            let _: ArchThreadInitializeFn = $arch::arch_thread_initialize;
            let _: ArchThreadConstructFirstFn = $arch::arch_thread_construct_first;
            let _: ArchContextSwitchFn = $arch::arch_context_switch;
            let _: ArchSaveUserStateFn = $arch::arch_save_user_state;
            let _: ArchRestoreUserStateFn = $arch::arch_restore_user_state;
            let _: ArchDumpThreadFn = $arch::arch_dump_thread;
            let _: ArchThreadGetBlockedFpFn = $arch::arch_thread_get_blocked_fp;
            let _: ArchSetSuspendedGeneralRegsFn = $arch::arch_set_suspended_general_regs;
            let _: ArchResetSuspendedGeneralRegsFn = $arch::arch_reset_suspended_general_regs;
            let _: ArchGetInstructionPointerFn = $arch::arch_get_instruction_pointer;
            let _: ArchSetReturnInstructionPointerFn = $arch::arch_set_return_instruction_pointer;

            // Per-CPU state and spinlock signatures
            let _: ArchCurrCpuNumFn = $arch::arch_curr_cpu_num;
            let _: ArchGetRestrictedFlagFn = $arch::arch_get_restricted_flag;
            let _: ArchSpinLockNonInstrumentedFn = $arch::arch_spin_lock_non_instrumented;
            let _: ArchSpinLockTraceInstrumentedFn = $arch::arch_spin_lock_trace_instrumented;
            let _: ArchSpinTrylockFn = $arch::arch_spin_trylock;
            let _: ArchSpinUnlockFn = $arch::arch_spin_unlock;

            // Restricted mode signatures
            let _: ArchValidateStatePreRestrictedEntryFn =
                $arch::validate_state_pre_restricted_entry;
            let _: ArchSaveStatePreRestrictedEntryFn<$arch::ArchSavedNormalState> =
                $arch::save_state_pre_restricted_entry;
            let _: ArchEnterRestrictedFn = $arch::enter_restricted;
            let _: ArchSaveRestrictedSyscallStateFn<$arch::SyscallRegs> =
                $arch::save_restricted_syscall_state;
            let _: ArchSaveRestrictedIframeStateFn<$arch::Iframe> =
                $arch::save_restricted_iframe_state;
            let _: ArchSaveRestrictedExceptionStateFn = $arch::save_restricted_exception_state;
            let _: ArchRedirectRestrictedExceptionToNormalFn<$arch::ArchSavedNormalState> =
                $arch::redirect_restricted_exception_to_normal;
            let _: ArchEnterFullFn<$arch::ArchSavedNormalState> = $arch::enter_full;
            let _: ArchDumpFn = $arch::dump;
        };
    };
}
