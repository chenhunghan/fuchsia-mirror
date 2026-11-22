// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Rust facade for the x86_64 global `CpuContextExchange` (`g_cpu_context_exchange`).

use core::mem::offset_of;

use zx_status::Status;
use zx_types::{zx_duration_mono_t, zx_status_t};

use super::Backtrace;
use crate::arch_rs::x86::Iframe;
use crate::kernel::types::cpu_num_t;

unsafe extern "C" {
    fn cpp_global_cpu_context_exchange_request_context(
        target_cpu: cpu_num_t,
        timeout: zx_duration_mono_t,
        context: *mut CpuContext,
    ) -> zx_status_t;
    fn cpp_global_cpu_context_exchange_handle_request(fp: usize, frame: *const Iframe);
}

/// `CpuContext` contains the execution state of a CPU (`<lib/backtrace/cpu_context_exchange.h>`).
#[repr(C)]
#[derive(Default)]
pub struct CpuContext {
    pub frame: Iframe,
    pub backtrace: Backtrace,
}

// `backtrace_ffi.cc` asserts the same layout for the C++ `CpuContext`.
zr::static_assert!(core::mem::size_of::<CpuContext>() == 448);
zr::static_assert!(core::mem::align_of::<CpuContext>() == 8);
zr::static_assert!(offset_of!(CpuContext, frame) == 0);
zr::static_assert!(offset_of!(CpuContext, backtrace) == 176);

/// Rust handle to the global x86_64 `CpuContextExchange` (`g_cpu_context_exchange`).
pub struct GlobalCpuContextExchange(());

impl GlobalCpuContextExchange {
    /// Synchronously requests `target_cpu`'s execution context, spinning until `target_cpu`
    /// handles the request or `timeout` has elapsed (`CpuContextExchange::RequestContext`).
    ///
    /// # Safety
    ///
    /// Interrupts must be disabled on the calling CPU, and `target_cpu` must be a valid CPU
    /// number (`is_valid_cpu_num(target_cpu)`).
    pub unsafe fn request_context(
        &self,
        target_cpu: cpu_num_t,
        timeout: zx_duration_mono_t,
    ) -> Result<CpuContext, Status> {
        let mut context = CpuContext::default();
        // SAFETY: `context` is a valid, exclusively borrowed `CpuContext`, and the caller
        // guarantees interrupts are disabled and `target_cpu` is valid.
        Status::ok(unsafe {
            cpp_global_cpu_context_exchange_request_context(target_cpu, timeout, &mut context)
        })?;
        Ok(context)
    }

    /// Synchronously replies to a pending cross-CPU context request from interrupt/NMI context
    /// (`CpuContextExchange::HandleRequest`).
    ///
    /// This method is a no-op if there is no active request targeting the calling CPU, and is safe
    /// for use in interrupt/NMI context without acquiring spinlocks.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - CPL is 0, interrupts are disabled on the calling CPU, and `%gs.base` points to the
    ///   calling CPU's `x86_percpu`.
    /// - `%gs:current_thread` is either null or points to a valid, constructed `Thread` whose
    ///   `KernelStack` metadata is not concurrently modified; whenever `fp` falls within
    ///   `[stack.base(), stack.top())`, `fp` is a valid, initialized frame pointer on the thread's
    ///   stack with no conflicting live `&mut` stack borrows.
    #[inline]
    pub unsafe fn handle_request(&self, fp: usize, frame: &Iframe) {
        // SAFETY: Discharged by this method's `# Safety` contract: interrupts are disabled with
        // `%gs.base` initialized, `fp` satisfies `GetBacktrace`'s stack preconditions, and `frame`
        // references a valid, initialized `Iframe`.
        unsafe { cpp_global_cpu_context_exchange_handle_request(fp, frame) };
    }
}

/// Global x86_64 `CpuContextExchange` instance (`g_cpu_context_exchange` in
/// `<lib/backtrace/global_cpu_context_exchange.h>`).
pub static GLOBAL_CPU_CONTEXT_EXCHANGE: GlobalCpuContextExchange = GlobalCpuContextExchange(());
