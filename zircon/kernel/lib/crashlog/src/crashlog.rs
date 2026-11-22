// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The kernel crashlog: rendering of the crash report stored in persistent RAM, the panic buffer
//! that captures panic/assert messages for it, and the stash of the crashlog recovered from the
//! previous boot.

use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char, c_int, c_void};
use core::fmt::{self, Write as _};
use core::pin::Pin;
use core::slice;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::ksync_kernel::TrackedMutex;
use affine::Round;
use bitflags::bitflags;
use boot_options::BootOptions;
use fbl::RefPtr;
use ksync::{KCell, KMutex};
use lazy_init::{LazyInit, NoCheck};
use libc::stdio::{FILE, StringFile};
use pin_init::pin_init;
use zx_types::ZX_KOID_INVALID;

use crate::arch_rs::crashlog::{CrashlogRegs, render_crashlog_registers};
use crate::backtrace::Backtrace;
use crate::debuglog_rs::dlog_render_to_crashlog;
use crate::io::stdout_write_no_persist;
use crate::kernel::thread;
use crate::lockup_detector::{
    lockup_get_critical_section_oops_count, lockup_get_no_heartbeat_oops_count,
};
use crate::object::{ChannelDispatcher, HandleTableArena, RootJobObserver};
use crate::persistent_debuglog_rs::PERSISTENT_RAM_ALLOCATION_GRANULARITY;
use crate::platform_rs::power::{
    ZirconCrashReason, platform_serial_is_suspended, platform_serial_suspended_at,
};
use crate::platform_rs::timer::{
    timer_current_mono_and_boot_ticks, timer_get_boot_ticks_offset, timer_get_mono_ticks_offset,
    timer_get_ticks_to_time_ratio, zx_msec,
};
use crate::top::boot_constants::{kernel_debug_ident, kernel_version_ident};
use crate::vm::pmm_checker::PmmChecker;
use crate::vm::pmm_node::PmmNode;
use crate::vm::vm_object::VmObject;

#[path = "panic_buffer.rs"]
mod panic_buffer;

#[cfg(ktest)]
#[path = "tests.rs"]
mod tests;

use panic_buffer::PANIC_BUFFER;

/// The amount of persistent RAM the crashlog is guaranteed to get, if that much is available.
pub const MIN_CRASHLOG_SIZE: usize =
    zr::parse_usize(env!("MIN_CRASHLOG_SIZE")).expect("MIN_CRASHLOG_SIZE invalid");
// Minimum reserved crashlog size must be a multiple of the persistent RAM allocation granularity.
zr::static_assert!(MIN_CRASHLOG_SIZE.is_multiple_of(PERSISTENT_RAM_ALLOCATION_GRANULARITY));

// This banner is load-bearing server-side as it's used to extract Dlog dump.
// Please notify //src/developer/forensics/OWNERS upon changing.
// LINT.IfChange
pub(crate) const DLOG_DUMP_BEGIN: &[u8] = b"--- BEGIN DLOG DUMP ---";
pub(crate) const DLOG_DUMP_END: &[u8] = b"--- END DLOG DUMP ---";
// LINT.ThenChange(//src/developer/forensics/feedback/reboot_log/reboot_log.cc)

/// Placeholder written inside the debuglog dump section when `kernel.render-dlog-to-crashlog` is
/// false. Lets `mapped_crashlog` distinguish a disabled dump from a real debuglog dump.
pub(crate) const DLOG_DUMP_DISABLED: &[u8] = b"DLOG -> Crashlog disabled";

/// Crashlog state holding architecture-specific exception registers.
struct Crashlog {
    regs: CrashlogRegs,
}

/// Holder for the global [`Crashlog`], synchronized by [`CRASHLOG_STATE`].
#[repr(transparent)]
struct CrashlogCell(UnsafeCell<Crashlog>);

// SAFETY: `CrashlogCell` only exposes its contents through the raw pointers returned by
// `regs_ptr()` and `regs_mut_ptr()`, synchronized via `CRASHLOG_STATE` (`set_crashlog_regs` and
// `is_crashlog_ready`).
unsafe impl Sync for CrashlogCell {}

impl CrashlogCell {
    /// Returns a pointer to the global crashlog's registers.
    fn regs_ptr(&self) -> *const CrashlogRegs {
        // SAFETY: `self.0.get()` is a valid pointer to the `Crashlog`; this only computes a field
        // address without creating a reference.
        unsafe { &raw const (*self.0.get()).regs }
    }

    /// Returns a mutable pointer to the global crashlog's registers.
    fn regs_mut_ptr(&self) -> *mut CrashlogRegs {
        // SAFETY: `self.0.get()` is a valid pointer to the `Crashlog`; this only computes a field
        // address without creating a reference.
        unsafe { &raw mut (*self.0.get()).regs }
    }
}

const CRASHLOG_STATE_EMPTY: u8 = 0;
const CRASHLOG_STATE_WRITING: u8 = 1;
const CRASHLOG_STATE_READY: u8 = 2;

/// One-shot synchronization state (`EMPTY -> WRITING -> READY`) for [`G_CRASHLOG`].
///
/// Multiple CPUs may enter fatal exception or watchdog paths concurrently before secondary CPUs are
/// halted (or a nested fault may occur while dumping the initial fault), so atomic synchronization
/// ensures only the first crashing CPU writes [`G_CRASHLOG`] and readers (`crashlog_to_string`)
/// observe fully initialized register state without data races.
static CRASHLOG_STATE: AtomicU8 = AtomicU8::new(CRASHLOG_STATE_EMPTY);

/// Writes `regs` to the global crashlog and marks it ready.
///
/// # Safety
///
/// The caller must ensure that `regs.iframe` is either null or points to a valid, aligned,
/// initialized `Iframe` that remains live and unmutated for the remainder of the kernel's
/// execution (since `CRASHLOG_STATE_READY` is permanent and [`crashlog_to_string`] may read it at
/// any subsequent point during panic/reboot handling).
#[inline]
pub unsafe fn set_crashlog_regs(regs: CrashlogRegs) {
    // We use Relaxed for the EMPTY -> WRITING claim transition because CRASHLOG_STATE never resets
    // to EMPTY, and publication of G_CRASHLOG is ordered by the subsequent Release store of
    // CRASHLOG_STATE_READY, paired with is_crashlog_ready()'s Acquire load.
    if CRASHLOG_STATE
        .compare_exchange(
            CRASHLOG_STATE_EMPTY,
            CRASHLOG_STATE_WRITING,
            Ordering::Relaxed,
            Ordering::Relaxed,
        )
        .is_ok()
    {
        // SAFETY: `CRASHLOG_STATE` transitions `EMPTY -> WRITING` at most once across the lifetime
        // of the kernel, giving this invocation exclusive write access to `G_CRASHLOG.regs`, while
        // `is_crashlog_ready()` returns `false` until the `Release` store of `READY` below.
        unsafe { G_CRASHLOG.regs_mut_ptr().write(regs) };
        // Release synchronizes with Acquire in is_crashlog_ready to ensure writes to G_CRASHLOG are
        // visible.
        CRASHLOG_STATE.store(CRASHLOG_STATE_READY, Ordering::Release);
    }
}

fn is_crashlog_ready() -> bool {
    CRASHLOG_STATE.load(Ordering::Acquire) == CRASHLOG_STATE_READY
}

/// The global crashlog.
static G_CRASHLOG: CrashlogCell =
    CrashlogCell(UnsafeCell::new(Crashlog { regs: CrashlogRegs::empty() }));

/// Holder for [`STDOUT_PANIC_BUFFER`], which hands out raw mutable [`FILE`] pointers for
/// `fprintf` without forming Rust references to the `FILE`.
#[repr(transparent)]
pub(crate) struct StdoutPanicBuffer(UnsafeCell<FILE>);

impl StdoutPanicBuffer {
    /// Returns a raw mutable pointer to the underlying [`FILE`].
    #[inline]
    pub(crate) const fn as_mut_ptr(&self) -> *mut FILE {
        self.0.get()
    }
}

// SAFETY: The `FILE` is never mutated after construction, its context pointer is null, and its
// write callback only touches the internally-synchronized panic buffer and console.
unsafe impl Sync for StdoutPanicBuffer {}

/// The `FILE` callback for [`STDOUT_PANIC_BUFFER`].
///
/// # Safety
///
/// If `len > 0`, `data` must be non-null and valid for reads of `len` bytes.
unsafe extern "C" fn stdout_panic_buffer_callback(
    _ptr: *mut c_void,
    data: *const c_char,
    len: usize,
) -> c_int {
    let bytes = if len == 0 {
        &[]
    } else {
        // SAFETY: `len > 0`, and the caller guarantees `data` is non-null and valid for reads of
        // `len` bytes.
        unsafe { slice::from_raw_parts(data.cast::<u8>(), len) }
    };
    stdout_panic_buffer_write(bytes)
}

/// Writes `bytes` to both stdout and the global [`PANIC_BUFFER`], exactly as writing to
/// [`STDOUT_PANIC_BUFFER`] does.
pub(crate) fn stdout_panic_buffer_write(bytes: &[u8]) -> c_int {
    PANIC_BUFFER.append(bytes);
    stdout_write_no_persist(bytes);
    bytes.len() as c_int
}

/// A `FILE` that writes to both stdout and the global [`PANIC_BUFFER`], exported to C++ as
/// `stdout_panic_buffer`.
#[unsafe(export_name = "stdout_panic_buffer")]
pub(crate) static STDOUT_PANIC_BUFFER: StdoutPanicBuffer = StdoutPanicBuffer(UnsafeCell::new(
    // SAFETY: `stdout_panic_buffer_callback` ignores its context pointer and only requires that
    // `data` point to `len` readable bytes, which is the `FILE::new` contract. It is a plain
    // function, so it remains valid for the `'static` lifetime of this `FILE`.
    unsafe { FILE::new(Some(stdout_panic_buffer_callback), core::ptr::null_mut()) },
));

/// The stashed recovered crashlog.
// TODO(https://fxbug.dev/565808307): Remove this wrapper once the Rust `VmObject` binding is
// `Send + Sync`.
struct RecoveredCrashlog(Option<RefPtr<VmObject>>);

// SAFETY: `VmObject` is an internally synchronized, reference-counted kernel object that C++
// freely shares across threads; the Rust binding is just not marked `Send`/`Sync`. In addition,
// every access to `RECOVERED_CRASHLOG` goes through `RecoveredCrashlogState::lock`.
unsafe impl Send for RecoveredCrashlog {}

#[ksync::guarded]
struct RecoveredCrashlogState {
    #[mutex]
    lock: KMutex<TrackedMutex>,
    #[guarded_by(lock)]
    crashlog: RecoveredCrashlog,
}

static RECOVERED_CRASHLOG: LazyInit<RecoveredCrashlogState, NoCheck> = LazyInit::uninit();

impl RecoveredCrashlogState {
    #[inline]
    fn get() -> &'static Self {
        // SAFETY: `RECOVERED_CRASHLOG` is initialized at `LK_INIT_LEVEL_EARLIEST` before
        // `crashlog_stash` or `crashlog_get_stashed` can be called.
        unsafe { RECOVERED_CRASHLOG.get_unchecked() }
    }
}

fn recovered_crashlog_lock_init(_level: init::LkInitLevel) {
    // SAFETY: lk init hooks run once during single-threaded early boot before `crashlog_stash` or
    // `crashlog_get_stashed` can be called.
    unsafe {
        let Ok(()) =
            Pin::static_ref(&RECOVERED_CRASHLOG).init_pin(pin_init!(RecoveredCrashlogState {
                lock <- KMutex::init(),
                crashlog: KCell::new(RecoveredCrashlog(None)),
            }));
    }
}
init::lk_init_hook!(
    recovered_crashlog_lock,
    recovered_crashlog_lock_init,
    init::LK_INIT_LEVEL_EARLIEST
);

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct RenderRegion: u32 {
        const BANNER            = 0x01;
        const DEBUG_INFO        = 0x02;
        const CRITICAL_COUNTERS = 0x04;
        const PANIC_BUFFER      = 0x08;
        const DLOG              = 0x10;
        const ROOT_JOB_CRITICAL = 0x20;
    }
}

fn map_reason_to_regions(reason: Option<ZirconCrashReason>) -> RenderRegion {
    match reason {
        Some(ZirconCrashReason::NoCrash) => RenderRegion::empty(),

        Some(ZirconCrashReason::Oom) => {
            RenderRegion::BANNER | RenderRegion::CRITICAL_COUNTERS | RenderRegion::DLOG
        }

        Some(ZirconCrashReason::UserspaceRootJobTermination) => {
            RenderRegion::BANNER
                | RenderRegion::CRITICAL_COUNTERS
                | RenderRegion::DLOG
                | RenderRegion::ROOT_JOB_CRITICAL
        }

        Some(ZirconCrashReason::Panic | ZirconCrashReason::SoftwareWatchdog) => {
            RenderRegion::BANNER
                | RenderRegion::DEBUG_INFO
                | RenderRegion::CRITICAL_COUNTERS
                | RenderRegion::PANIC_BUFFER
                | RenderRegion::DLOG
        }

        _ => RenderRegion::BANNER,
    }
}

/// Formats like C's `%#x`, which (unlike Rust's `{:#x}`) prints zero without the `0x` prefix.
struct CAltHex(u64);

impl fmt::Display for CAltHex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 { f.write_str("0") } else { write!(f, "{:#x}", self.0) }
    }
}

/// Serialize the crashlog to string into `target`. If `reason` is OOM, then a
/// different preamble will be used, and the backtrace will not be included.
pub(crate) fn crashlog_to_string(target: &mut [u8], reason: u32) -> usize {
    let mut outfile = StringFile::new(target);
    let known_reason = ZirconCrashReason::try_from(reason).ok();
    let regions = map_reason_to_regions(known_reason);

    if regions.contains(RenderRegion::BANNER) {
        let reason_str = match known_reason {
            Some(ZirconCrashReason::NoCrash) => "NO CRASH",

            Some(ZirconCrashReason::Oom) => "OOM",

            Some(ZirconCrashReason::Panic) => "KERNEL PANIC",

            Some(ZirconCrashReason::SoftwareWatchdog) => "SW WATCHDOG",

            Some(ZirconCrashReason::UserspaceRootJobTermination) => {
                "USERSPACE ROOT JOB TERMINATION"
            }

            _ => "UNKNOWN",
        };
        let _ = write!(outfile, "ZIRCON REBOOT REASON ({reason_str})\n\n");

        let ticks = timer_current_mono_and_boot_ticks();
        let transform = timer_get_ticks_to_time_ratio();

        let _ = write!(
            outfile,
            "UPTIME (ms)\n{}\n\n",
            transform.scale::<{ Round::DOWN }>(ticks.boot_now) / zx_msec(1)
        );
        let _ = write!(
            outfile,
            "RUNTIME (ms)\n{}\n\n",
            transform.scale::<{ Round::DOWN }>(ticks.mono_now) / zx_msec(1)
        );
        let _ = write!(
            outfile,
            "TIMER INFO (ticks) counter:{} boot: {} mono: {}\n\n",
            // `boot_now = current_ticks + boot_ticks_offset`. The offset is negative.
            CAltHex(ticks.boot_now.wrapping_sub(timer_get_boot_ticks_offset()) as u64),
            CAltHex(timer_get_boot_ticks_offset() as u64),
            CAltHex(timer_get_mono_ticks_offset() as u64)
        );

        // If usermode had suspended the debug UART (or was in the process of
        // suspending it) when this crashlog was generated, none of the kernel's
        // recent output (including any output related to this crashlog) made it out
        // of the serial port.  Make a note of that so that whoever reads this
        // crashlog knows why.
        if platform_serial_is_suspended() {
            let _ = write!(
                outfile,
                "DEBUG UART SUSPENDED (or suspending, since boot time {}ms); \
                 kernel output since then was not written to the serial console\n\n",
                platform_serial_suspended_at().0 / zx_msec(1),
            );
        }

        outfile.write(if regions.contains(RenderRegion::DEBUG_INFO) {
            // Include full symbolizer markup context for DebugInfo.
            kernel_debug_ident()
        } else {
            // Otherwise, the human-readable prefix of the full text.
            kernel_version_ident()
        });
    }

    if regions.contains(RenderRegion::ROOT_JOB_CRITICAL) {
        let proc_koid = RootJobObserver::get_critical_process_koid();
        let proc_name_chars = RootJobObserver::get_critical_process_name();
        let proc_name = CStr::from_bytes_until_nul(&proc_name_chars)
            .map_or(&proc_name_chars[..], CStr::to_bytes);
        if proc_koid != ZX_KOID_INVALID {
            outfile.write(b"ROOT JOB TERMINATED BY CRITICAL PROCESS DEATH: ");
            outfile.write(proc_name);
            let _ = writeln!(outfile, " ({proc_koid})");
        }
    }

    if regions.contains(RenderRegion::DEBUG_INFO) {
        // The presence of "REGISTERS" is load-bearing; we have crashlog tests that
        // that explicitly parse for it.
        // LINT.IfChange
        let _ = writeln!(outfile, "REGISTERS");
        // LINT.ThenChange(
        //     //src/tests/kernel_crashlog/kernel_crashlog_test.go,
        //     //zircon/kernel/lib/crashlog/src/tests.rs
        // )

        // Note: it is the architecture specific implementation which is responsible
        // for determining if the crashlog registers are valid, and what (if
        // anything) to print.
        let regs = if is_crashlog_ready() {
            // SAFETY: `is_crashlog_ready`'s Acquire load synchronizes with the Release store in
            // `set_crashlog_regs`, ensuring `regs_ptr()` points to initialized, immutable data.
            unsafe { G_CRASHLOG.regs_ptr().read() }
        } else {
            CrashlogRegs::empty()
        };
        // SAFETY: `regs.iframe` is null or points to the `Iframe` of the crashing CPU, on the
        // stack of an exception handler that does not return, so it stays live and unwritten.
        unsafe { render_crashlog_registers(&mut outfile, &regs) };

        let _ = writeln!(outfile, "BACKTRACE (up to 16 calls)");
        let mut bt = Backtrace::new();
        thread::current_get_backtrace(&mut bt);
        outfile.with_file(|file| {
            // SAFETY: `file.as_mut_ptr()` points to a valid `FILE` for the duration of the call.
            unsafe { bt.print_without_version(file.as_mut_ptr()) }
        });
        let _ = writeln!(outfile);
    }

    if regions.contains(RenderRegion::CRITICAL_COUNTERS) {
        // Include counters for critical events.
        let _ = writeln!(
            outfile,
            "counters: haf={} paf={} pvf={} lcs={} lhb={} cf={} ",
            HandleTableArena::get_alloc_failed_count(),
            PmmNode::get_alloc_failed_count(),
            PmmChecker::get_validation_failed_count(),
            lockup_get_critical_section_oops_count(),
            lockup_get_no_heartbeat_oops_count(),
            ChannelDispatcher::get_channel_full_count()
        );
    }

    if regions.contains(RenderRegion::PANIC_BUFFER) {
        // Include as much of the contents of the panic buffer as we can, if it is
        // not empty.
        //
        // The panic buffer is one of the last thing we print.  Space is limited so
        // if the panic/assert message was long we may not be able to include the
        // whole thing.  That's OK.  The panic buffer is a "nice to have" and we've
        // already printed the primary diagnostics (register dump and backtrace).
        PANIC_BUFFER.with_contents(|contents| {
            if contents.is_empty() {
                let _ = writeln!(outfile, "panic buffer: empty");
            } else {
                // The contents end at the first NUL, if any.
                let contents =
                    CStr::from_bytes_until_nul(contents).map_or(contents, CStr::to_bytes);
                outfile.write(b"panic buffer: ");
                outfile.write(contents);
                outfile.write(b"\n");
            }
        });
    }

    if regions.contains(RenderRegion::DLOG) {
        // Finally, if we have been configured to do so, render as much of the
        // recent debug log as we can fit into the crashlog memory.
        const FOOTER_LEN: usize = DLOG_DUMP_END.len() + 2;

        outfile.write(b"\n");
        outfile.write(DLOG_DUMP_BEGIN);
        outfile.write(b"\n");
        if BootOptions::get().render_dlog_to_crashlog {
            let available_region = outfile.available_region();
            let payload_len = available_region.len().saturating_sub(FOOTER_LEN);
            let payload_region = &mut available_region[..payload_len];

            let rendered = dlog_render_to_crashlog(payload_region);
            outfile.skip(rendered);
        } else {
            outfile.write(DLOG_DUMP_DISABLED);
            outfile.write(b"\n");
        }
        outfile.write(b"\n");
        outfile.write(DLOG_DUMP_END);
        outfile.write(b"\n");
    }

    outfile.used_region().len()
}

/// Stash the recovered crashlog for later retrieval by the `k crashlog dump` command.
pub(crate) fn crashlog_stash(crashlog: Option<RefPtr<VmObject>>) {
    ksync::lock!(let mut guard = RecoveredCrashlogState::get().lock_lock());
    guard.as_mut().crashlog_mut().0 = crashlog;
}

#[cfg(console_enabled)]
mod console {
    use core::ffi::{CStr, c_int};
    use core::slice;

    use fbl::RefPtr;
    use kprint::{kprint, kprintln};
    use zx_status::Status;

    use super::RecoveredCrashlogState;
    use crate::console::{CMD_AVAIL_ALWAYS, CmdArgs, static_command};
    use crate::vm::vm_object::VmObject;

    /// Returns the previously stashed recovered crashlog, or `None`.
    fn crashlog_get_stashed() -> Option<RefPtr<VmObject>> {
        ksync::lock!(let guard = RecoveredCrashlogState::get().lock_lock());
        guard.crashlog().0.clone()
    }

    fn print_recovered_crashlog() {
        let Some(crashlog) = crashlog_get_stashed() else {
            kprintln!("no recovered crashlog");
            return;
        };

        // Stream the VMO in fixed-size heap chunks so large recovered crashlogs (such as a 1 MiB
        // persistent debuglog) never exceed `cmpctmalloc::HEAP_MAX_ALLOC_SIZE`.
        const CHUNK_SIZE: usize = 4096;
        let crashlog_size = crashlog.size() as usize;
        if crashlog_size == 0 {
            kprintln!("recovered crashlog follows...");
            kprintln!("");
            kprintln!("... end of recovered crashlog");
            return;
        }

        let Ok(mut buffer) = kalloc::Box::<[u8]>::try_new_uninit_slice(CHUNK_SIZE) else {
            kprintln!("error: failed to allocate {:u} for crashlog", CHUNK_SIZE);
            return;
        };

        let mut offset = 0usize;
        let mut printed_header = false;
        while offset < crashlog_size {
            let chunk_len = core::cmp::min(CHUNK_SIZE, crashlog_size - offset);
            let data = match crashlog.read(offset as u64, &mut buffer[..chunk_len]) {
                Ok(data) => data,
                Err(status) => {
                    if printed_header {
                        kprintln!("");
                    }
                    kprintln!(
                        "error: failed to read from recovered crashlog vmo: {:d}",
                        status.into_raw()
                    );
                    return;
                }
            };
            if !printed_header {
                kprintln!("recovered crashlog follows...");
                printed_header = true;
            }
            // The contents end at the first NUL, if any.
            match CStr::from_bytes_until_nul(data) {
                Ok(cstr) => {
                    kprint!("{:s}", cstr.to_bytes());
                    break;
                }
                Err(_) => {
                    kprint!("{:s}", &*data);
                    offset += chunk_len;
                }
            }
        }
        kprintln!("");
        kprintln!("... end of recovered crashlog");
    }

    /// Console command handler for `crashlog`.
    ///
    /// # Safety
    ///
    /// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where each
    /// element's `arg_str` is a valid null-terminated C string.
    unsafe extern "C" fn cmd_crashlog(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INTERNAL.into_raw();
        }
        // SAFETY: `argc > 0` and `argv` is non-null; the caller guarantees `argv` is valid for
        // `argc` elements.
        let argv = unsafe { slice::from_raw_parts(argv, argc as usize) };
        'usage: {
            if argc < 2 {
                kprintln!("not enough arguments");
                break 'usage;
            }

            if argv[1].as_str() == "dump" {
                print_recovered_crashlog();
            } else {
                kprintln!("unknown command");
                break 'usage;
            }

            return zx_types::ZX_OK;
        }
        kprintln!("usage:");
        kprintln!(
            "{:cs} dump                              : dump the recovered crashlog",
            argv[0].arg_str
        );
        Status::INTERNAL.into_raw()
    }

    static_command!(
        CMD_CRASHLOG,
        c"crashlog".as_ptr(),
        c"crashlog".as_ptr(),
        cmd_crashlog,
        CMD_AVAIL_ALWAYS
    );
}
