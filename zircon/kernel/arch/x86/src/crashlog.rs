// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Architectural crashlog register formatting for x86_64.

use core::fmt::Write;

use super::Iframe;

/// Hardware crashlog registers capturing CPU exception state on x86_64.
#[derive(Clone, Copy, Debug)]
pub struct CrashlogRegs {
    /// Pointer to the faulting interrupt frame (`null` if unavailable).
    pub iframe: *const Iframe,
    /// On x86/x64, the CR2 register is important for diagnosing kernel crashes,
    /// but is not included in [`Iframe`].
    pub cr2: u64,
    /// `IA32_FS_BASE` MSR value at crash time.
    pub fsbase: u64,
    /// `IA32_GS_BASE` MSR value at crash time.
    pub gsbase: u64,
    /// `IA32_KERNEL_GS_BASE` (`swapgs` target) MSR value at crash time.
    pub swapgs_gsbase: u64,
}

impl CrashlogRegs {
    /// Returns an empty [`CrashlogRegs`] with a null `iframe` and zeroed registers.
    pub const fn empty() -> Self {
        Self { iframe: core::ptr::null(), cr2: 0, fsbase: 0, gsbase: 0, swapgs_gsbase: 0 }
    }
}

impl Default for CrashlogRegs {
    fn default() -> Self {
        Self::empty()
    }
}

#[cold]
#[inline(never)]
fn write_hex_reg(w: &mut impl Write, name: &str, val: u64) -> core::fmt::Result {
    if val == 0 {
        writeln!(w, "{:>13}:                  0", name)
    } else {
        writeln!(w, "{:>13}: {:#18x}", name, val)
    }
}

/// Writes the formatted register fields of a non-null [`CrashlogRegs::iframe`] into `target`.
///
/// # Safety
///
/// The caller must ensure that `regs.iframe` is non-null, properly aligned for [`Iframe`], points
/// to a live (not freed) [`Iframe`] object, and remains valid for shared reads (no concurrent
/// mutation) for the duration of the call.
#[cold]
#[sanitize(address = "off")]
unsafe fn render_crashlog_registers_inner(
    target: &mut impl Write,
    regs: &CrashlogRegs,
) -> core::fmt::Result {
    // SAFETY: Caller guarantees `regs.iframe` is non-null, aligned, initialized, live, and valid
    // for shared reads for the duration of the call; `#[sanitize(address = "off")]` suppresses
    // KASAN checks if the stack frame is poisoned, and `write_hex_reg` takes `u64` by value so no
    // reference into the frame escapes to sanitized code.
    let f = unsafe { &*regs.iframe };
    write_hex_reg(target, "CS", f.cs)?;
    write_hex_reg(target, "RIP", f.ip)?;
    write_hex_reg(target, "EFL", f.flags)?;
    write_hex_reg(target, "CR2", regs.cr2)?;
    write_hex_reg(target, "RAX", f.rax)?;
    write_hex_reg(target, "RBX", f.rbx)?;
    write_hex_reg(target, "RCX", f.rcx)?;
    write_hex_reg(target, "RDX", f.rdx)?;
    write_hex_reg(target, "RSI", f.rsi)?;
    write_hex_reg(target, "RDI", f.rdi)?;
    write_hex_reg(target, "RBP", f.rbp)?;
    write_hex_reg(target, "RSP", f.user_sp)?;
    write_hex_reg(target, "R8", f.r8)?;
    write_hex_reg(target, "R9", f.r9)?;
    write_hex_reg(target, "R10", f.r10)?;
    write_hex_reg(target, "R11", f.r11)?;
    write_hex_reg(target, "R12", f.r12)?;
    write_hex_reg(target, "R13", f.r13)?;
    write_hex_reg(target, "R14", f.r14)?;
    write_hex_reg(target, "R15", f.r15)?;
    write_hex_reg(target, "vector", f.vector)?;
    write_hex_reg(target, "errc", f.err_code)?;
    write_hex_reg(target, "fsbase", regs.fsbase)?;
    write_hex_reg(target, "gsbase", regs.gsbase)?;
    write_hex_reg(target, "swapgs gsbase", regs.swapgs_gsbase)?;
    writeln!(target)
}

/// Render the architecture-specific details of an [`Iframe`] in a fashion suitable for a crashlog
/// into `target`.
///
/// # Safety
///
/// The caller must ensure that if `regs.iframe` is non-null, it is properly aligned for [`Iframe`],
/// points to a live (not freed) [`Iframe`] object, and remains valid for shared reads
/// (no concurrent mutation) for the duration of the call.
#[cold]
#[sanitize(address = "off")]
pub unsafe fn render_crashlog_registers(target: &mut impl Write, regs: &CrashlogRegs) {
    if regs.iframe.is_null() {
        let _ = writeln!(target, "missing");
        return;
    }
    // SAFETY: Null was ruled out above, and the caller guarantees a non-null `regs.iframe` is
    // aligned, initialized, live, and valid for shared reads for the duration of the call.
    let _ = unsafe { render_crashlog_registers_inner(target, regs) };
}
