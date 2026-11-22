// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use debug::{dprintf, ltracef};
use init::{LkInitFlags, LkInitLevel, LkInitStruct};

const LOCAL_TRACE: u32 = 0;

/// Returns `(start_addr, count)` for the linker-collected `.data.rel.ro.lk_init` table.
fn lk_init_bounds() -> (usize, usize) {
    let start_addr: usize;
    let stop_addr: usize;
    // SAFETY: Reads the linker-defined hidden section boundary addresses `__start_lk_init` and
    // `__stop_lk_init` without dereferencing memory or modifying registers/stack.
    unsafe {
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!(
            "adrp {start}, __start_lk_init",
            "add {start}, {start}, :lo12:__start_lk_init",
            "adrp {stop}, __stop_lk_init",
            "add {stop}, {stop}, :lo12:__stop_lk_init",
            start = out(reg) start_addr,
            stop = out(reg) stop_addr,
            options(nomem, nostack, preserves_flags),
        );
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!(
            "lla {start}, __start_lk_init",
            "lla {stop}, __stop_lk_init",
            start = out(reg) start_addr,
            stop = out(reg) stop_addr,
            options(nomem, nostack, preserves_flags),
        );
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!(
            "lea __start_lk_init(%rip), {start}",
            "lea __stop_lk_init(%rip), {stop}",
            start = out(reg) start_addr,
            stop = out(reg) stop_addr,
            options(att_syntax, nomem, nostack, preserves_flags),
        );
    }
    let count = (stop_addr - start_addr) / size_of::<LkInitStruct>();
    (start_addr, count)
}

/// Runs registered kernel initialization hooks matching `required_flag` in level order from
/// `start_level` through `stop_level` (inclusive).
///
/// # Safety
///
/// Must only be called in the intended kernel boot/CPU-bringup sequence with all preconditions of
/// the hooks in `[start_level, stop_level]` satisfied.
// SAFETY: Exported symbol matches `extern "C" void lk_init_level(...)` in `<lk/init.h>` and has no
// other definition.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_init_level(
    required_flag: LkInitFlags,
    start_level: LkInitLevel,
    stop_level: LkInitLevel,
) {
    ltracef!(
        "flags {:#x}, start_level {:#x}, stop_level {:#x}\n",
        required_flag as u32,
        start_level.0,
        stop_level.0
    );
    assert!(start_level.0 > 0);
    let mut last_called_level = LkInitLevel(start_level.0 - 1);
    let mut last: Option<*const LkInitStruct> = None;

    let (start_addr, init_count) = lk_init_bounds();

    loop {
        // Search for the lowest uncalled hook to call.
        ltracef!(
            "last {:p}, last_called_level {:#x}\n",
            last.unwrap_or_default(),
            last_called_level.0
        );

        let mut found: Option<&LkInitStruct> = None;
        let mut seen_last = false;

        for i in 0..init_count {
            let entry_addr = start_addr + i * size_of::<LkInitStruct>();
            // SAFETY: Each `LkInitStruct` entry in `.data.rel.ro.lk_init` is an initialized,
            // read-only `'static` registration whose address is materialized individually rather
            // than forming a cross-allocation slice.
            let ptr = unsafe { &*core::ptr::with_exposed_provenance::<LkInitStruct>(entry_addr) };
            ltracef!(
                "looking at {:p} {:?} level {:#x}, flags {:#x}, seen_last {}\n",
                ptr,
                // SAFETY: `ptr.name` is a static NUL-terminated C string from `lk_init_hook!`.
                unsafe { core::ffi::CStr::from_ptr(ptr.name) },
                ptr.level.0,
                ptr.flags as u32,
                seen_last
            );
            let ptr_addr = core::ptr::from_ref(ptr);
            if Some(ptr_addr) == last {
                seen_last = true;
            }

            // Reject the easy ones.
            if ((ptr.flags as u32) & (required_flag as u32)) == 0 {
                continue;
            }
            if ptr.level > stop_level {
                continue;
            }
            if ptr.level < last_called_level {
                continue;
            }
            if let Some(found_ref) = found
                && found_ref.level <= ptr.level
            {
                continue;
            }

            // Keep the lowest one we haven't called yet.
            if ptr.level >= start_level && ptr.level > last_called_level {
                found = Some(ptr);
                continue;
            }

            // If we're at the same level as the last one we called and we've
            // already passed over it this time around, we can mark this one
            // and early terminate the loop.
            if ptr.level == last_called_level && Some(ptr_addr) != last && seen_last {
                found = Some(ptr);
                break;
            }
        }

        let Some(found_ref) = found else {
            break;
        };

        dprintf!(
            INFO,
            "INIT: cpu {}, calling hook {:?} {:?} at level {:#x}, flags {:#x}\n",
            crate::arch_rs::curr_cpu_num(),
            found_ref.hook,
            // SAFETY: `found_ref.name` is a static NUL-terminated C string from `lk_init_hook!`.
            unsafe { core::ffi::CStr::from_ptr(found_ref.name) },
            found_ref.level.0,
            found_ref.flags as u32
        );
        (found_ref.hook)(found_ref.level);

        last_called_level = found_ref.level;
        last = Some(core::ptr::from_ref(found_ref));
    }
}

/// Runs primary CPU initialization hooks from `start_level` through `stop_level`.
///
/// # Safety
///
/// Must only be called on the primary CPU in ascending boot-level order with all preconditions of
/// the hooks in `[start_level, stop_level]` satisfied.
#[inline]
pub unsafe fn lk_primary_cpu_init_level(start_level: LkInitLevel, stop_level: LkInitLevel) {
    // SAFETY: Caller upholds the primary-CPU boot ordering invariants for
    // `[start_level, stop_level]`.
    unsafe { lk_init_level(LkInitFlags::PrimaryCpu, start_level, stop_level) };
}
