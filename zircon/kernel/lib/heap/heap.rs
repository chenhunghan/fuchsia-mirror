// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel heap bindings and debug console command.

#[path = "cmpctmalloc/mod.rs"]
mod cmpctmalloc;
mod heap_ffi;
mod heap_wrapper;
#[path = "profile/profile.rs"]
mod profile;

pub use heap_wrapper::get_heap_profile;
pub(crate) use heap_wrapper::{heap_get_info as get_info, profile_track_alloc, profile_track_free};

/// Kernel console `heap` command implementation.
#[cfg(console_enabled)]
mod console {
    use super::cmpctmalloc::CmpctDumpOptions;
    use super::heap_wrapper::HEAP_COLLECT_STATS;
    use super::heap_wrapper::console::{dump_stats, heap_dump, heap_test, toggle_heap_trace};
    use crate::console::{CMD_AVAIL_ALWAYS, CMD_FLAG_PANIC, CmdArgs, static_command};
    use core::ffi::c_int;
    use kprint::kprintln;

    /// Console command handler for `k heap`.
    ///
    /// # Safety
    ///
    /// - `argc` must be `>= 1` and `argv` must be non-null and valid for reads of `argc`
    ///   initialized `CmdArgs` entries for the duration of the call.
    /// - The kernel heap must be initialized and `TheHeapLock` must not be held on the current
    ///   thread.
    /// - If `flags` includes `CMD_FLAG_PANIC`, all other CPUs must be halted in a kernel panic
    ///   with no concurrent heap operations.
    unsafe extern "C" fn cmd_heap(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return -1;
        }
        // SAFETY: `argv` is non-null and points to `argc` initialized `CmdArgs` entries from the
        // console dispatcher.
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd_name = args[0].as_str();
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!("\t{:s} info [-v]", cmd_name);
            if HEAP_COLLECT_STATS {
                kprintln!("\t{:s} stats", cmd_name);
            }
            if (flags & CMD_FLAG_PANIC) == 0 {
                kprintln!("\t{:s} trace", cmd_name);
                kprintln!("\t{:s} trim", cmd_name);
                kprintln!("\t{:s} test", cmd_name);
            }
            -1
        };

        if args.len() < 2 {
            return usage();
        }

        let subcmd = args[1].as_str();
        if subcmd == "info" {
            let mut options = if (flags & CMD_FLAG_PANIC) != 0 {
                CmpctDumpOptions::PANIC_TIME
            } else {
                CmpctDumpOptions::NONE
            };

            for arg in &args[2..] {
                let opt = arg.as_str();
                if opt == "-v" {
                    options |= CmpctDumpOptions::VERBOSE;
                } else {
                    kprintln!("unrecognized option (\"{:s}\") for info command", opt);
                    return usage();
                }
            }

            // SAFETY: `PANIC_TIME` is only set when `CMD_FLAG_PANIC` indicates the system is in a
            // single-CPU halted panic state, and `TheHeapLock` is not held here.
            unsafe { heap_dump(options) };
        } else if HEAP_COLLECT_STATS && subcmd == "stats" {
            dump_stats();
        } else if (flags & CMD_FLAG_PANIC) == 0 && subcmd == "test" {
            // SAFETY: `heap_test` is only invoked from the non-panic interactive console when the
            // heap is initialized and `TheHeapLock` is not held.
            unsafe { heap_test() };
        } else if (flags & CMD_FLAG_PANIC) == 0 && subcmd == "trace" {
            let heap_trace = toggle_heap_trace();
            kprintln!("heap trace is now {:s}", if heap_trace { "on" } else { "off" });
        } else {
            kprintln!("unrecognized command");
            return usage();
        }

        0
    }

    static_command!(
        HEAP,
        c"heap".as_ptr(),
        c"heap debug commands".as_ptr(),
        cmd_heap,
        CMD_AVAIL_ALWAYS
    );
}
