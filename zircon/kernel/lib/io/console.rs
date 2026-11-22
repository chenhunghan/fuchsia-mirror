// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int, c_void};
use core::hint::cold_path;
use core::mem::{align_of, offset_of, size_of};
use core::sync::atomic::{Ordering, compiler_fence};

use libc::stdio::FILE;

use crate::arch_rs::blocking_disallowed;
use crate::arch_rs::ops::num_spinlocks_held;
use crate::arch_rs::vm::is_kernel_address;
use crate::counters::define_kcounter;
use crate::debuglog_rs::{DEBUGLOG_INFO, PersistLog, dlog_bypass, dlog_write};
use crate::kconcurrent::ChainLockTransaction;
use crate::kernel::auto_preempt_disabler::AutoPreemptDisabler;
use crate::kernel::percpu::{PerCpu, PerCpuStdoutState};
use crate::kernel::thread::{Linebuffer, Thread, current_get};
use crate::ksync_kernel::declare_singleton_monitored_spinlock;
use crate::persistent_debuglog_rs::persistent_dlog_write;
use crate::platform_rs::debug::platform_dputs_irq;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SkipPersistedDebuglog {
    No = 0,
    Yes,
}

const MAX_FLUSH_DEPTH: usize = PerCpuStdoutState::MAX_FLUSH_DEPTH;

define_kcounter!(STDOUT_DROPPED_LINES, "io.stdout.dropped_lines", Sum);

// TODO(https://fxbug.dev/575806167): Rust singleton spinlocks have no spin-tracing lock name, so
// spin traces do not show "dputc_spin_lock".
declare_singleton_monitored_spinlock!(DputcSpinLock);

/// Tracks active flush nesting on this CPU and claims a per-CPU staging buffer
/// when available.
///
/// Only a holder that cannot be rescheduled uses the per-CPU state. That is the
/// case when blocking is disallowed, or when interrupts are disabled and a
/// spinlock is held, which defers any reschedule until after this scope.
/// Otherwise [`ScopedFlushBuffer::new`] disables preemption.
///
/// The exception is a thread with interrupts disabled, no spinlock held, and an
/// active [`ChainLockTransaction`]. Reenabling preemption there could flush a
/// pending preemption inside the transaction, so it claims nothing. It falls
/// back to the stack, and its flushes do not count toward [`MAX_FLUSH_DEPTH`].
struct ScopedFlushBuffer {
    state: Option<&'static PerCpuStdoutState>,
    depth: usize,
    preempt_disabler: AutoPreemptDisabler,
}

impl ScopedFlushBuffer {
    fn new() -> Self {
        let mut preempt_disabler = AutoPreemptDisabler::new_deferred();
        if !blocking_disallowed() {
            let ints_disabled = crate::arch_rs::ints_disabled();
            if !ints_disabled || num_spinlocks_held() == 0 {
                if ints_disabled && ChainLockTransaction::active().is_some() {
                    cold_path();
                    return Self { state: None, depth: 0, preempt_disabler };
                }
                preempt_disabler.disable();
            }
        }
        compiler_fence(Ordering::SeqCst);
        let state = &PerCpu::get_current().stdout_state;
        let depth = state.flush_depth();
        if depth < MAX_FLUSH_DEPTH {
            // SAFETY: `state` is the current CPU's `PerCpuStdoutState`, we cannot be rescheduled
            // until `Drop` restores `depth` (either blocking is disallowed, interrupts are
            // disabled with a spinlock held, or `preempt_disabler` disabled preemption above), and
            // `depth` was just observed on this CPU so no active flush uses a slot at `depth` or
            // above.
            unsafe { state.set_flush_depth(depth + 1) };
        } else {
            cold_path();
        }
        Self { state: Some(state), depth, preempt_disabler }
    }

    /// Returns true if this flush is nested too deeply and its line must be dropped.
    fn too_deep(&self) -> bool {
        self.depth >= MAX_FLUSH_DEPTH
    }

    fn data(&mut self) -> Option<&mut [u8; Linebuffer::BUFFER_SIZE]> {
        if let Some(state) = self.state
            && self.depth < state.flush_buffers.len()
        {
            // SAFETY: `self.depth < state.flush_buffers.len() <= MAX_FLUSH_DEPTH`, so `new()`
            // incremented `flush_depth` past `self.depth` and `Drop` has not yet decremented it.
            // No other CPU accesses this CPU's `flush_buffers`, we cannot be rescheduled while
            // `self` is alive, any nested flush on this CPU sees `flush_depth > self.depth`, and
            // `&mut self` prevents concurrent re-borrowing through `self`.
            Some(unsafe { &mut *state.flush_buffers[self.depth].get() })
        } else {
            cold_path();
            None
        }
    }
}

impl Drop for ScopedFlushBuffer {
    fn drop(&mut self) {
        if let Some(state) = self.state
            && self.depth < MAX_FLUSH_DEPTH
        {
            // SAFETY: `state` is still the current CPU's `PerCpuStdoutState` because we have not
            // been rescheduled since `new()`, our own staging buffer borrow at `self.depth` has
            // ended, and any nested flushes on this CPU at `> self.depth` have already completed.
            unsafe { state.set_flush_depth(self.depth) };
        } else {
            cold_path();
        }
        self.preempt_disabler.enable();
    }
}

/// Back door to directly write to the kernel serial port.
///
/// Safe to call prior to `call_constructors()` (`ConstructorsCalled() == false`), in which case
/// output is silently dropped.
pub fn serial_write(str: &[u8]) {
    if !crate::top::main::ConstructorsCalled() {
        cold_path();
        return;
    }
    ksync::lock!(let _guard = DputcSpinLock::get().lock_with(ksync::source_tag!()));

    // Write out the serial port.
    platform_dputs_irq(str);
}

fn stdout_write(str: &[u8], skip_pdlog: SkipPersistedDebuglog) {
    if !crate::top::main::ConstructorsCalled() {
        cold_path();
        return;
    }
    let persist =
        if skip_pdlog == SkipPersistedDebuglog::No { PersistLog::Yes } else { PersistLog::No };
    if !dlog_bypass() && dlog_write(DEBUGLOG_INFO, 0, str, persist).is_ok() {
        return;
    }
    if persist == PersistLog::Yes {
        persistent_dlog_write(str);
    }
    serial_write(str);
}

/// Flushes `lb` through a stack buffer when per-CPU staging buffers are in use.
///
/// # Safety
///
/// The caller must ensure that:
///
/// - `lb` is non-null, properly aligned for [`Linebuffer`], and valid for both reads and writes.
/// - `(*lb).pos` and `(*lb).buffer[..len]` are initialized, and `len <= Linebuffer::BUFFER_SIZE`.
/// - This execution context has exclusive access to `*lb` with no live references to `*lb` until
///   `(*lb).pos = 0` is stored prior to calling `stdout_write`.
#[cold]
#[inline(never)]
unsafe fn flush_linebuffer_stack(
    lb: *mut Linebuffer,
    len: usize,
    skip_pdlog: SkipPersistedDebuglog,
) {
    let mut buf = [0u8; Linebuffer::BUFFER_SIZE];
    // SAFETY: By this function's `# Safety` preconditions, `lb` is non-null, aligned, and valid
    // for reads and writes, `(*lb).buffer[..len]` is initialized with
    // `len <= Linebuffer::BUFFER_SIZE`, and this execution context has exclusive access with no
    // live references until `(*lb).pos = 0` is stored. `buf` is a freshly allocated local array of
    // `Linebuffer::BUFFER_SIZE` bytes, so it is valid for writes of `len` bytes and disjoint from
    // `*lb`. Storing `(*lb).pos = 0` before calling `stdout_write` completes all accesses to `*lb`
    // before any re-entrant write can occur.
    unsafe {
        core::ptr::copy_nonoverlapping(
            (&raw const (*lb).buffer).cast::<u8>(),
            buf.as_mut_ptr(),
            len,
        );
        (*lb).pos = 0;
    }

    stdout_write(&buf[..len], skip_pdlog);
}

/// Flushes the first `len` bytes of `lb` to stdout and resets its position.
///
/// # Safety
///
/// The caller must ensure that:
///
/// - `lb` is non-null, properly aligned for [`Linebuffer`], and valid for both reads and writes.
/// - `(*lb).pos` and `(*lb).buffer[..len]` are initialized, and `len <= Linebuffer::BUFFER_SIZE`.
/// - This execution context has exclusive access to `*lb` with no live references to `*lb` until
///   `(*lb).pos = 0` is stored prior to calling `stdout_write`.
/// - Per-CPU, thread, and console state is initialized (`ConstructorsCalled()` is true).
unsafe fn flush_linebuffer(lb: *mut Linebuffer, len: usize, skip_pdlog: SkipPersistedDebuglog) {
    let mut flush_buf = ScopedFlushBuffer::new();
    if let Some(data) = flush_buf.data() {
        // SAFETY: By this function's `# Safety` preconditions, `lb` is non-null, aligned, and
        // valid for reads and writes, `(*lb).buffer[..len]` is initialized with
        // `len <= Linebuffer::BUFFER_SIZE`, and this execution context has exclusive access with
        // no live references until `(*lb).pos = 0` is stored. `data` is a unique mutable reference
        // to a separate per-CPU staging buffer of `Linebuffer::BUFFER_SIZE` bytes, so it is valid
        // for writes of `len` bytes and disjoint from `*lb`. Storing `(*lb).pos = 0` before
        // calling `stdout_write` completes all accesses to `*lb` before any re-entrant write can
        // occur.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (&raw const (*lb).buffer).cast::<u8>(),
                data.as_mut_ptr(),
                len,
            );
            (*lb).pos = 0;
        }

        stdout_write(&data[..len], skip_pdlog);
        return;
    }

    if !flush_buf.too_deep() {
        // SAFETY: All `# Safety` preconditions of `flush_linebuffer_stack` are directly
        // discharged by this function's `# Safety` preconditions, and `*lb` has not been modified
        // above.
        unsafe { flush_linebuffer_stack(lb, len, skip_pdlog) };
    } else {
        cold_path();
        STDOUT_DROPPED_LINES.add(1);
        // SAFETY: By this function's `# Safety` preconditions, `lb` is non-null, aligned, valid
        // for writes, and exclusively accessed by this execution context with no live references.
        unsafe {
            (*lb).pos = 0;
        }
    }
}

fn stdout_write_buffered(mut str: &[u8], skip_pdlog: SkipPersistedDebuglog) {
    if !crate::top::main::ConstructorsCalled() {
        cold_path();
        return;
    }

    let t = current_get();

    if t.is_null() {
        cold_path();
        stdout_write(str, skip_pdlog);
        return;
    }

    // Use a per-CPU buffer when blocking is disallowed so interrupt handlers do
    // not touch the interrupted thread's buffer.
    let lb = if blocking_disallowed() {
        PerCpu::get_current().stdout_state.irq_linebuffer.get()
    } else {
        Thread::linebuffer_ptr(t)
    };

    // Don't continue if the line buffer pointer is corrupted.
    if !is_kernel_address(lb.wrapping_byte_add(offset_of!(Linebuffer, buffer)).addr()) {
        cold_path();
        stdout_write(b"<linebuffer corruption>\n", skip_pdlog);
        return;
    }

    // Premise for the raw accesses to `*lb` below (and for the callees' safety requirements): `lb`
    // is either the running thread's `Linebuffer` while blocking is allowed (the thread is live
    // while it runs, and only it writes its own linebuffer) or this CPU's `irq_linebuffer` while
    // blocking is disallowed (so we cannot migrate). Interrupt-context writers only use the
    // per-CPU `irq_linebuffer`, never the interrupted thread's buffer, so no other context
    // accesses `*lb` concurrently, and no Rust reference to `*lb` exists. The `is_kernel_address`
    // check above is only a best-effort corruption heuristic.
    while !str.is_empty() {
        // SAFETY: `lb` is valid and accessed only by this execution context, per the premise
        // above.
        let mut pos = unsafe { (*lb).pos };
        // Don't continue if the buffer position is corrupted.
        if pos >= Linebuffer::BUFFER_SIZE {
            cold_path();
            stdout_write(b"<linebuffer corruption>\n", skip_pdlog);
            return;
        }

        let remaining = Linebuffer::BUFFER_SIZE - pos;
        let substring = &str[..str.len().min(remaining)];
        let newline_pos = substring.iter().position(|&b| b == b'\n');

        let size;
        let inject;
        let flush;
        if let Some(newline_pos) = newline_pos {
            // A newline that fits in our remaining buffer.
            size = newline_pos + 1;
            inject = false;
            flush = true;
        } else if substring.len() == remaining {
            // We fill the buffer, injecting a newline.
            size = remaining - 1;
            inject = true;
            flush = true;
        } else {
            // We only add to the buffer.
            size = substring.len();
            inject = false;
            flush = false;
        }

        // SAFETY: `lb` is valid and accessed only by this execution context, per the premise
        // above, so projecting to its `buffer` field is in bounds.
        let buf_ptr = unsafe { (&raw mut (*lb).buffer).cast::<u8>() };
        // SAFETY: `buf_ptr` points to `Linebuffer::BUFFER_SIZE` writable bytes and
        // `pos + size <= Linebuffer::BUFFER_SIZE`, so `buf_ptr.add(pos)` is valid for writes of
        // `size` bytes. `substring` is a shared `&[u8]` slice of `size` initialized readable bytes
        // (`size <= substring.len()`). `substring` cannot overlap `*lb`: `*lb` is private
        // `Linebuffer` storage accessed only via raw pointers with no live references (and
        // `flush_linebuffer` stages its contents into a separate buffer before calling
        // `stdout_write`), so by `&[u8]`'s immutability invariant `substring` cannot alias the
        // writable destination `*lb`.
        unsafe { core::ptr::copy_nonoverlapping(substring.as_ptr(), buf_ptr.add(pos), size) };
        str = &str[size..];
        pos += size;

        if inject {
            // SAFETY: `inject` implies `size == remaining - 1`, so `pos < Linebuffer::BUFFER_SIZE`
            // and the write is in bounds of `*lb`'s buffer.
            unsafe { *buf_ptr.add(pos) = b'\n' };
            pos += 1;
        }
        if flush {
            // SAFETY: `lb` is non-null, aligned, valid for reads and writes, and exclusively
            // accessed by this execution context with no live references per the premise above;
            // `(*lb).pos` and `(*lb).buffer[..pos]` are initialized with
            // `pos <= Linebuffer::BUFFER_SIZE`; and `ConstructorsCalled()` was checked true on
            // entry so per-CPU, thread, and console state is initialized.
            unsafe { flush_linebuffer(lb, pos, skip_pdlog) };
        } else {
            // SAFETY: `lb` is valid and accessed only by this execution context, per the premise
            // above.
            unsafe { (*lb).pos = pos };
        }
    }
}

/// Writes `str` directly to stdout without per-line buffering.
///
/// Safe to call prior to `call_constructors()` (`ConstructorsCalled() == false`), in which case
/// output is silently dropped.
pub fn stdout_write_unbuffered(str: &[u8]) {
    stdout_write(str, SkipPersistedDebuglog::No);
}

/// Writes `str` to buffered stdout while skipping the persistent debuglog.
///
/// Safe to call prior to `call_constructors()` (`ConstructorsCalled() == false`), in which case
/// output is silently dropped.
pub fn stdout_write_no_persist(str: &[u8]) {
    stdout_write_buffered(str, SkipPersistedDebuglog::Yes);
}

/// Holder for the global [`STDOUT`] (`gStdout`) placed in writable `.data` so C `<stdio.h>`
/// callers (and kernel console tests that temporarily swap `*stdout()`) can use raw mutable
/// `FILE` pointers without forming Rust references.
#[repr(transparent)]
struct GlobalFile(
    // Safety invariant: the `FILE` always satisfies `FILE::new`'s contract with a callback that is
    // sound to call concurrently from any CPU or context (including interrupt context and before
    // `call_constructors()`). It is mutated only by `unsafe` code (through `libc::stdio::stdout()`
    // or from C/C++), which must ensure there is no concurrent reader and that the replacement
    // value also upholds this invariant.
    UnsafeCell<FILE>,
);

// SAFETY: By the safety invariant on `GlobalFile`'s field, the `FILE`'s callback is thread-safe and
// any mutation of the `FILE` is done by `unsafe` code that excludes concurrent readers. Shared
// access only reads the `FILE` through raw pointers without forming Rust references.
unsafe impl Sync for GlobalFile {}

zr::static_assert!(size_of::<GlobalFile>() == size_of::<FILE>());
zr::static_assert!(align_of::<GlobalFile>() == align_of::<FILE>());

/// `FILE` write callback for [`STDOUT`] (`gStdout`).
///
/// # Safety
///
/// The caller must ensure that if `len > 0`, `str` is non-null, valid for reads of `len`
/// initialized bytes within a single allocation, `len <= isize::MAX as usize`, and the memory is
/// not mutated for the duration of this call.
unsafe extern "C" fn stdout_callback(_ptr: *mut c_void, str: *const c_char, len: usize) -> c_int {
    // SAFETY: By this function's `# Safety` precondition, if `len > 0`, `str` (cast to `*const u8`,
    // which has alignment 1) is non-null, valid for reads of `len` initialized bytes within a
    // single allocation with `len <= isize::MAX as usize`, and not mutated for the duration of
    // this call, satisfying `zr::slice_from_raw_parts`.
    let bytes = unsafe { zr::slice_from_raw_parts(str.cast::<u8>(), len) };
    stdout_write_buffered(bytes, SkipPersistedDebuglog::No);
    bytes.len() as c_int
}

/// This is what `printf` calls. Really this could and should be const.
/// But all the stdio function signatures require non-const `FILE*`.
///
/// Statically initialized, so `gStdout` has a live callback before global constructors run; writes
/// prior to `call_constructors()` (`ConstructorsCalled() == false`) are safe no-ops whose output
/// is dropped.
// SAFETY: `gStdout` is uniquely defined in the kernel binary and matches `extern FILE gStdout` in
// `<stdio.h>` (`GlobalFile` is `#[repr(transparent)]` over `UnsafeCell<FILE>`). `libc::stdio` also
// declares this symbol as `static mut gStdout: FILE`, which is layout-compatible, and writes
// through that declaration are permitted because the definition is wrapped in `UnsafeCell` (and so
// placed in writable `.data`).
#[unsafe(export_name = "gStdout")]
static STDOUT: GlobalFile = GlobalFile(UnsafeCell::new(
    // SAFETY: `stdout_callback` ignores its context pointer, only requires `str` to point to `len`
    // initialized, immutable readable bytes within a single allocation (`len <= isize::MAX`) when
    // `len > 0`, and safely no-ops before `ConstructorsCalled()`, matching `FILE::new`'s contract
    // for the `'static` lifetime of `STDOUT`.
    unsafe { FILE::new(Some(stdout_callback), core::ptr::null_mut()) },
));
