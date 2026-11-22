// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::crashlog::{DLOG_DUMP_BEGIN, DLOG_DUMP_DISABLED, DLOG_DUMP_END};
use crate::kernel::deadline::{Deadline, SlackMode, TimerSlack};
use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
use crate::ksync_kernel::TrackedSpinlock;
use crate::persistent_debuglog_rs::persistent_dlog_get_recovered_log;
use crate::platform_rs::crashlog::Interface;
use crate::platform_rs::power::ZirconCrashReason;
use crate::platform_rs::timer::{
    DurationMono, DurationUnknown, current_boot_time, current_mono_time, zx_msec, zx_sec,
};
use core::cmp::{max, min};
use core::convert::Infallible;
use core::ffi::{c_char, c_int, c_void};
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::{from_mut, from_ref, null, null_mut, slice_from_raw_parts_mut};
use core::sync::atomic::{AtomicBool, Ordering};
use kprint::{kformat, kprint};
use ksync::{KMutex, guarded};
use libc::stdio::FILE;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zbi::HwRebootReason;
use zr::{slice_from_raw_parts, static_assert_size_and_align};
use zx_status::Status;
use zx_types::{zx_instant_boot_t, zx_instant_mono_t, zx_status_t};

const RAM_CRASHLOG_V1_SIZE: usize = 72;

/// Returns the byte offset of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Whether `haystack` contains `needle` anywhere.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

/// Writes the recovered persistent debuglog `dlog`, if non-empty, through `write`.
///
/// When `use_dlog_dump_markers` is true, wraps `dlog` in `DLOG_DUMP_BEGIN` and `DLOG_DUMP_END` so
/// userspace feedback extracts it as the previous boot's kernel log. This preserves recent kernel
/// logs in snapshots after a warm reset with no crashlog payload (such as a hardware watchdog
/// reset or power-button reset).
fn write_recovered_dlog(write: &mut impl FnMut(&[u8]), dlog: &[u8], use_dlog_dump_markers: bool) {
    if dlog.is_empty() {
        return;
    }
    let mut fmt_buf = [0u8; 80];
    write(kformat!(&mut fmt_buf, "Recovered {} bytes from the persistent debug log\n", dlog.len()));
    if use_dlog_dump_markers {
        write(DLOG_DUMP_BEGIN);
        write(b"\n");
    } else {
        write(b"=================== BEGIN ===================\n");
    }
    write(dlog);
    if !dlog.ends_with(b"\n") {
        write(b"\n");
    }
    if use_dlog_dump_markers {
        write(DLOG_DUMP_END);
        write(b"\n");
    } else {
        write(b"=================== END ===================\n");
    }
}

/// Writes the crashlog `payload` and recovered persistent debuglog `dlog` through `write`.
///
/// Wraps `dlog` in `DLOG_DUMP_BEGIN` and `DLOG_DUMP_END` unless `payload` already contains a real
/// debuglog dump. If `payload` contains the `DLOG_DUMP_DISABLED` placeholder (the default when
/// `kernel.render-dlog-to-crashlog` is false) and `dlog` is non-empty, strips that placeholder so
/// userspace extracts `dlog` instead.
fn write_payload_and_recovered_dlog(write: &mut impl FnMut(&[u8]), payload: &[u8], dlog: &[u8]) {
    let dlog_dump_span = find(payload, DLOG_DUMP_BEGIN).map(|begin| {
        let end = find(&payload[begin..], DLOG_DUMP_END).map_or(payload.len(), |rel_end| {
            let end = begin + rel_end + DLOG_DUMP_END.len();
            if payload.get(end) == Some(&b'\n') { end + 1 } else { end }
        });
        (begin, end)
    });
    let payload_has_disabled_dlog = dlog_dump_span
        .is_some_and(|(begin, end)| contains(&payload[begin..end], DLOG_DUMP_DISABLED));

    if let Some((begin, end)) = dlog_dump_span
        && payload_has_disabled_dlog
        && !dlog.is_empty()
    {
        if begin > 0 {
            write(&payload[..begin]);
        }
        if end < payload.len() {
            write(&payload[end..]);
        }
    } else if !payload.is_empty() {
        write(payload);
    }

    let payload_has_dlog_dump = dlog_dump_span.is_some() && !payload_has_disabled_dlog;
    write_recovered_dlog(write, dlog, !payload_has_dlog_dump);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RecoveredRamCrashlog {
    uptime: zx_instant_boot_t,
    runtime: zx_instant_mono_t,
    reason: u32,
    payload_valid: bool,
    payload: *const u8,
    payload_len: u32,
}

static_assert_size_and_align!(RecoveredRamCrashlog, 40, 8);

#[allow(clippy::derivable_impls)]
impl Default for RecoveredRamCrashlog {
    fn default() -> Self {
        Self {
            uptime: 0,
            runtime: 0,
            reason: 0,
            payload_valid: false,
            payload: null(),
            payload_len: 0,
        }
    }
}

unsafe extern "C" {
    fn ram_crashlog_stow(
        buf: *mut c_void,
        blen: usize,
        payload: *const c_void,
        plen: u32,
        sw_reason: ZirconCrashReason,
        uptime_nvram: zx_instant_boot_t,
        runtime_nvram: zx_instant_mono_t,
    ) -> zx_status_t;

    fn ram_crashlog_recover(
        buf: *const c_void,
        blen: usize,
        log_out: *mut RecoveredRamCrashlog,
    ) -> zx_status_t;

    fn cpp_boot_constants_hw_reboot_reason() -> u32;
}

/// # Safety
///
/// Always safe to call; only inspects `len`.
unsafe extern "C" fn null_file_write(_ptr: *mut c_void, _str: *const c_char, len: usize) -> c_int {
    debug_assert!(len <= c_int::MAX as usize);
    len as c_int
}

// SAFETY: `null_file_write` ignores `_ptr` and `_str` and only uses `len`.
const NULL_FILE: FILE = unsafe { FILE::new(Some(null_file_write), null_mut()) };

/// RAM-mapped crashlog implementation.
#[guarded]
#[pin_data(PinnedDrop)]
pub struct MappedCrashlog {
    crashlog_buffer: *mut [u8],
    render_target: *mut [u8],

    recovered_log: RecoveredRamCrashlog,
    log_recovery_result: Result<(), Status>,

    #[mutex]
    uptime_updater_lock: KMutex<TrackedSpinlock>,
    #[pin]
    #[guarded_by(uptime_updater_lock)]
    uptime_updater_timer: Timer,
    #[guarded_by(uptime_updater_lock)]
    uptime_updater_enabled: bool,

    /// Make sure we print the crashlog status to the debuglog only once, no matter
    /// how many times [`Self::recover`] is called.
    status_printed_to_debuglog: AtomicBool,
    #[pin]
    _pin: PhantomPinned,
}

// SAFETY: `crashlog_buffer`, `render_target`, and `recovered_log.payload` point to persistent RAM
// wired for the lifetime of the kernel, and mutable state is synchronized by
// `uptime_updater_lock` or `status_printed_to_debuglog`.
unsafe impl Send for MappedCrashlog {}
// SAFETY: See `Send` safety comment above.
unsafe impl Sync for MappedCrashlog {}

impl MappedCrashlog {
    /// Creates an in-place initializer for a [`MappedCrashlog`] backed by `range`.
    pub fn new(range: &'static mut [u8]) -> impl PinInit<Self, Infallible> {
        let crashlog_buffer: *mut [u8] =
            if range.is_empty() || !range.as_ptr().cast::<u64>().is_aligned() {
                slice_from_raw_parts_mut(null_mut(), 0)
            } else {
                from_mut(range)
            };
        let render_target: *mut [u8] = if crashlog_buffer.len() > RAM_CRASHLOG_V1_SIZE {
            // SAFETY: `crashlog_buffer` has length `> RAM_CRASHLOG_V1_SIZE`, so advancing by
            // `RAM_CRASHLOG_V1_SIZE` stays within the same allocated object.
            unsafe {
                slice_from_raw_parts_mut(
                    crashlog_buffer.cast::<u8>().add(RAM_CRASHLOG_V1_SIZE),
                    crashlog_buffer.len() - RAM_CRASHLOG_V1_SIZE,
                )
            }
        } else {
            slice_from_raw_parts_mut(null_mut(), 0)
        };

        let mut recovered_log = RecoveredRamCrashlog::default();
        let mut log_recovery_result = Err(Status::INTERNAL);
        if !crashlog_buffer.is_empty() {
            // Go ahead and "recover" the log right now. All this will do is verify the
            // various CRCs and extract the results if everything checks out. We don't
            // want to do this more than once.
            // SAFETY: `crashlog_buffer` is non-empty, 8-byte aligned, and valid for reads of
            // `crashlog_buffer.len()` bytes, and `recovered_log` is valid for writes.
            log_recovery_result = unsafe {
                Status::ok(ram_crashlog_recover(
                    crashlog_buffer.cast::<c_void>(),
                    crashlog_buffer.len(),
                    &mut recovered_log,
                ))
            };
        }

        pin_init!(Self {
            crashlog_buffer,
            render_target,
            recovered_log,
            log_recovery_result,
            uptime_updater_lock <- KMutex::init(),
            uptime_updater_timer <- ksync::kcell_init(Timer::init(ZX_CLOCK_MONOTONIC)),
            uptime_updater_enabled: false.into(),
            status_printed_to_debuglog: AtomicBool::new(false),
            _pin: PhantomPinned,
        })
    }

    fn should_print_crashlog_status(&self) -> bool {
        // `Relaxed` ordering is sufficient because this flag only gates one-time diagnostic
        // logging and does not synchronize other memory accesses.
        self.status_printed_to_debuglog
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// # Safety
    ///
    /// `arg` must be a valid pointer to a live [`MappedCrashlog`].
    unsafe extern "C" fn uptime_updater_callback(
        _timer: *mut Timer,
        _now: zx_instant_mono_t,
        arg: *mut c_void,
    ) {
        // SAFETY: `arg` was passed as `self` in `update_uptime_locked` and points to a
        // static-lifetime `MappedCrashlog`.
        let thiz = unsafe { &*arg.cast::<Self>() };
        ksync::lock!(let mut guard = thiz.lock_uptime_updater_lock());
        thiz.update_uptime_locked(guard.as_mut());
    }

    fn update_uptime_locked(&self, mut guard: Pin<&mut MappedCrashlogUptimeUpdaterLockGuard<'_>>) {
        if *guard.uptime_updater_enabled() {
            const DEFAULT_UPDATE_INTERVAL: DurationMono = DurationMono(zx_sec(1));

            // SAFETY: `self.crashlog_buffer` is either empty or points to valid persistent RAM for
            // `self.crashlog_buffer.len()` bytes.
            unsafe {
                ram_crashlog_stow(
                    self.crashlog_buffer.cast::<c_void>(),
                    self.crashlog_buffer.len(),
                    null(),
                    0,
                    ZirconCrashReason::Unknown,
                    current_boot_time().0,
                    current_mono_time().0,
                );
            }

            let next_update_time = Deadline::after_mono(
                DEFAULT_UPDATE_INTERVAL,
                TimerSlack::new(DurationUnknown(DEFAULT_UPDATE_INTERVAL.0 / 2), SlackMode::Center),
            );
            // SAFETY: `self` is pinned in static storage for the life of the kernel,
            // `Self::uptime_updater_callback` is a valid callback, and `uptime_updater_lock`
            // serializes all `set_deadline` and `cancel` operations on `uptime_updater_timer`.
            unsafe {
                guard.as_mut().uptime_updater_timer_mut().as_ref().set_deadline(
                    &next_update_time,
                    Self::uptime_updater_callback,
                    from_ref(self).cast_mut().cast::<c_void>(),
                );
            }
        }
    }
}

// SAFETY: `self.render_target` is either empty or points into `self.crashlog_buffer` (`'static`
// persistent RAM), which is valid for writes and unaliased once `enable_crashlog_uptime_updates`
// is disabled during platform halt.
unsafe impl Interface for MappedCrashlog {
    fn get_render_target(&self) -> *mut [u8] {
        self.render_target
    }

    unsafe fn finalize(&self, reason: ZirconCrashReason, mut amt: usize) {
        // Whatever the user tells us, the amt of the crashlog render target which was
        // filled cannot exceed the amount that we originally reported, nor can it be
        // larger than what a u32 can hold.
        amt = min(amt, self.render_target.len());
        amt = min(amt, u32::MAX as usize);

        // The RAM crashlog library will gracefully handle a nullptr or 0 length here;
        // no need to explicitly check that they are valid.
        // SAFETY: `self.crashlog_buffer` and `self.render_target` are either empty or point to
        // valid, 8-byte aligned persistent RAM regions for their respective lengths.
        unsafe {
            ram_crashlog_stow(
                self.crashlog_buffer.cast::<c_void>(),
                self.crashlog_buffer.len(),
                self.render_target.cast::<c_void>(),
                amt as u32,
                reason,
                current_boot_time().0,
                current_mono_time().0,
            );
        }
    }

    unsafe fn recover(&self, tgt: Option<&FILE>) -> usize {
        // If the user didn't supply a target FILE to render to, use the NULL_FILE
        // instead so that we compute a proper length for them as we go.
        let tgt: &FILE = tgt.unwrap_or(&NULL_FILE);

        // Create a string representation of the HW reboot reason.
        // SAFETY: `kBootConstants` is initialized by physboot before kernel entry and remains
        // read-only for the life of the kernel.
        let hw_reason = HwRebootReason::from_raw(unsafe { cpp_boot_constants_hw_reboot_reason() });
        let mut str_hw_reason_buf = [0u8; 16];
        let str_hw_reason: &[u8] = match hw_reason {
            HwRebootReason::UNDEFINED => b"UNKNOWN",
            HwRebootReason::COLD => b"COLD BOOT",
            HwRebootReason::WARM => b"WARM BOOT",
            HwRebootReason::BROWNOUT => b"BROWNOUT",
            HwRebootReason::WATCHDOG => b"HW WATCHDOG",
            HwRebootReason::USER_HARD_RESET => b"USER HARD RESET",
            _ => kformat!(&mut str_hw_reason_buf, "0x{:08x}", hw_reason.as_raw()),
        };

        if let Err(status) = self.log_recovery_result
            && self.should_print_crashlog_status()
        {
            kprint!(
                "Crashlog: Failed to recover crashlog.  Result {}, HW Reboot Reason \"{:s}\"\n",
                status.into_raw(),
                str_hw_reason
            );
        }

        // Persistent debuglog recovery runs independently of the crashlog header, so a recovered
        // persistent debuglog may exist even when no valid crashlog header is present.
        let dlog = persistent_dlog_get_recovered_log().unwrap_or(&[]);

        // If we failed to recover any crashlog or persistent dlog and there is no HW reboot
        // reason, simply report the size as 0.
        if self.log_recovery_result.is_err()
            && hw_reason == HwRebootReason::UNDEFINED
            && dlog.is_empty()
        {
            return 0;
        }

        // From then on, we always return something, starting with the HW reboot reason.
        let mut fmt_buf = [0u8; 80];
        let mut written: usize = 0;
        let mut write = |bytes: &[u8]| {
            written = written.wrapping_add(max(tgt.write(bytes), 0) as usize);
        };
        write(kformat!(&mut fmt_buf, "HW REBOOT REASON ({:s})\n\n", str_hw_reason));

        if self.log_recovery_result.is_err() {
            write(
                b"WARNING - Could not recover crashlog from RAM. \
Only HW reboot reason is available.\n",
            );
            write_recovered_dlog(&mut write, dlog, true);
            return written;
        }

        // OK, we have a log. Currently, the log is expected to be nothing but text,
        // so we need to take the structured information we have access to and put it
        // into string form. This includes:
        //
        // 1) The uptime estimate
        // 2) The runtime estimate
        // 3) The "software" reboot reason.
        // 4) The payload damage indicator (only if there was potential damage to the
        //    payload)
        //
        // The first few lines of text need to be structured so that they can be
        // understood by the crash-log harvester up in userland. Right now, this is
        // just a loose convention. Someday, it would be good to pass this data in a
        // much more structured form.
        let rlog = &self.recovered_log;
        let str_reason: &[u8] = match ZirconCrashReason::try_from(rlog.reason) {
            Ok(ZirconCrashReason::Unknown) => b"UNKNOWN",
            Ok(ZirconCrashReason::Oom) => b"OOM",
            Ok(ZirconCrashReason::Panic) => b"KERNEL PANIC",
            Ok(ZirconCrashReason::SoftwareWatchdog) => b"SW WATCHDOG",
            Ok(ZirconCrashReason::NoCrash) => b"NO CRASH",
            Ok(ZirconCrashReason::UserspaceRootJobTermination) => b"USERSPACE ROOT JOB TERMINATION",
            Ok(ZirconCrashReason::Invalid) | Err(_) => b"<null>",
        };

        if self.should_print_crashlog_status() {
            // Provide some basic details about the crashlog we recovered in the kernel
            // log. This can assist in debugging failure in CI/CQ where we might have
            // access to serial logs, but nothing else.
            let uptime_msec = rlog.uptime / zx_msec(1);
            if ZirconCrashReason::try_from(rlog.reason) == Ok(ZirconCrashReason::NoCrash) {
                kprint!(
                    "Crashlog: Clean reboot. Uptime ({}.{:03} sec) HW Reason \"{:s}\"\n",
                    uptime_msec / 1000,
                    uptime_msec % 1000,
                    str_hw_reason
                );
            } else {
                kprint!(
                    "Crashlog: Uptime ({}.{:03} sec) SW Reason \"{:s}\" HW Reason \"{:s}\" \
Payload {:s} PLen {}\n",
                    uptime_msec / 1000,
                    uptime_msec % 1000,
                    str_reason,
                    str_hw_reason,
                    if rlog.payload_valid { "valid" } else { "invalid" },
                    rlog.payload_len
                );
            }
        }

        // Second line must give the Zircon reboot reason, and be followed by two newlines.
        write(kformat!(&mut fmt_buf, "ZIRCON REBOOT REASON ({:s})\n\n", str_reason));

        // Uptime estimate comes next with a newline between the tag and the actual number
        write(kformat!(&mut fmt_buf, "UPTIME (ms)\n{}\n", rlog.uptime / zx_msec(1)));

        // Runtime estimate comes next with a newline between the tag and the actual number.
        write(kformat!(&mut fmt_buf, "RUNTIME (ms)\n{}\n", rlog.runtime / zx_msec(1)));

        if !rlog.payload_valid {
            write(
                b"WARNING - The following crashlog payload failed length/CRC sanity checks \
and may contain errors!\n",
            );
        }

        let payload: &[u8] = if !rlog.payload.is_null() && rlog.payload_len != 0 {
            // SAFETY: `rlog.payload` is a non-null pointer into `self.crashlog_buffer` validated
            // by `ram_crashlog_recover` for `rlog.payload_len` bytes, and `recover` is called
            // before `enable_crashlog_uptime_updates(true)` or `finalize` modifies the buffer.
            unsafe { slice_from_raw_parts(rlog.payload, rlog.payload_len as usize) }
        } else {
            &[]
        };
        write_payload_and_recovered_dlog(&mut write, payload, dlog);

        // Report the total length.
        written
    }

    fn enable_crashlog_uptime_updates(&self, enabled: bool) {
        ksync::lock!(let mut guard = self.lock_uptime_updater_lock());

        if *guard.uptime_updater_enabled() != enabled {
            *guard.as_mut().uptime_updater_enabled_mut() = enabled;
            if *guard.uptime_updater_enabled() {
                self.update_uptime_locked(guard.as_mut());
            } else {
                guard.uptime_updater_timer().cancel();
            }
        }
    }
}

#[pinned_drop]
impl PinnedDrop for MappedCrashlog {
    fn drop(self: Pin<&mut Self>) {
        // In case `self` is ever destroyed, ensure the timer has been canceled first.
        self.enable_crashlog_uptime_updates(false);
    }
}

/// MappedCrashlog tests.
#[cfg(ktest)]
#[unittest::suite(name = "mapped_crashlog")]
mod tests {
    use core::mem::size_of;
    use kalloc::Box;
    use libc::stdio::StringFile;
    use pin_init::stack_pin_init;
    use unittest::{
        assert_false, assert_gt, assert_ok, assert_true, expect_eq, expect_true, unwrap_some,
    };

    const CRASHLOG_BUFFER_SIZE: usize = 256;
    const TEST_PAYLOAD: &[u8] = b"test panic payload\n";

    struct TestBuf {
        buf: [u8; 512],
        len: usize,
    }

    impl TestBuf {
        fn new() -> Self {
            Self { buf: [0; 512], len: 0 }
        }

        fn push(&mut self, bytes: &[u8]) {
            self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
        }

        fn as_bytes(&self) -> &[u8] {
            &self.buf[..self.len]
        }
    }

    /// Verify recovered crashlog length matches written bytes without trailing NULs.
    #[test]
    fn recover_length_and_payload_test() {
        let mut storage = unwrap_some!(
            Box::<[u64]>::try_new_zeroed_slice(CRASHLOG_BUFFER_SIZE / size_of::<u64>()).ok()
        );
        // SAFETY: `storage` is a valid, exclusively owned buffer of `CRASHLOG_BUFFER_SIZE` bytes,
        // and `TEST_PAYLOAD` is valid for reads of `TEST_PAYLOAD.len()` bytes.
        let status = unsafe {
            ram_crashlog_stow(
                storage.as_mut_ptr().cast::<c_void>(),
                CRASHLOG_BUFFER_SIZE,
                TEST_PAYLOAD.as_ptr().cast::<c_void>(),
                TEST_PAYLOAD.len() as u32,
                ZirconCrashReason::Panic,
                0,
                0,
            )
        };
        assert_ok!(Status::ok(status));

        let raw_words = Box::into_raw(storage);
        // SAFETY: `raw_words` comes from `Box::into_raw` with `CRASHLOG_BUFFER_SIZE` initialized,
        // 8-byte-aligned bytes and is intentionally leaked to satisfy the `'static` lifetime of
        // `MappedCrashlog::new`.
        let crashlog_range =
            unsafe { &mut *slice_from_raw_parts_mut(raw_words.cast::<u8>(), CRASHLOG_BUFFER_SIZE) };
        stack_pin_init!(let crashlog = MappedCrashlog::new(crashlog_range));

        // First pass: measure the recovered crashlog length with `tgt = None`, as
        // `crashlog_to_vmo` does when sizing `/boot/log/last-panic.txt`.
        // SAFETY: `recover` is called before `enable_crashlog_uptime_updates(true)` or `finalize`.
        let measured_len = unsafe { crashlog.recover(None) };
        assert_gt!(measured_len, TEST_PAYLOAD.len());

        // Allocate an output buffer large enough to hold the entire crashlog even if the bug
        // inflated `measured_len` by `TEST_PAYLOAD.len()`, plus one byte reserved by `StringFile`
        // for a trailing NUL terminator.
        let mut output_buffer = unwrap_some!(
            Box::<[u8]>::try_new_zeroed_slice(measured_len + TEST_PAYLOAD.len() + 1).ok()
        );
        let mut string_file = StringFile::new(&mut output_buffer);
        let written_len = string_file.with_file(|scoped_file| {
            // SAFETY: `scoped_file.as_mut_ptr()` points to a valid `FILE` for the duration of the
            // closure, and `recover` preconditions hold as above.
            unsafe { crashlog.recover(Some(&*scoped_file.as_mut_ptr())) }
        });

        expect_eq!(measured_len, written_len);
        expect_eq!(written_len, string_file.as_bytes().len());
        expect_true!(contains(string_file.as_bytes(), b"ZIRCON REBOOT REASON (KERNEL PANIC)\n\n"));
        expect_true!(contains(string_file.as_bytes(), TEST_PAYLOAD));
    }

    /// Tests that a recovered persistent debuglog replaces a disabled-dlog placeholder.
    #[test]
    fn disabled_dlog_stub_replaced_by_recovered_dlog_test() {
        const PAYLOAD: &[u8] = b"panic buffer: empty\n\n\
--- BEGIN DLOG DUMP ---\n\
DLOG -> Crashlog disabled\n\n\
--- END DLOG DUMP ---\n";
        const DLOG: &[u8] = b"==== WEDGE DUMP ====\nline 2\n";

        let mut out = TestBuf::new();
        write_payload_and_recovered_dlog(&mut |b| out.push(b), PAYLOAD, DLOG);
        let bytes = out.as_bytes();

        assert_false!(contains(bytes, DLOG_DUMP_DISABLED));
        let first_begin = find(bytes, DLOG_DUMP_BEGIN);
        assert_true!(first_begin.is_some());
        if let Some(pos) = first_begin {
            assert_true!(find(&bytes[pos + DLOG_DUMP_BEGIN.len()..], DLOG_DUMP_BEGIN).is_none());
        }
        assert_true!(
            bytes
                == b"panic buffer: empty\n\n\
Recovered 28 bytes from the persistent debug log\n\
--- BEGIN DLOG DUMP ---\n\
==== WEDGE DUMP ====\n\
line 2\n\
--- END DLOG DUMP ---\n"
        );
    }

    /// Tests that a disabled-dlog placeholder stays intact when no log was recovered.
    #[test]
    fn disabled_dlog_stub_preserved_when_recovered_dlog_empty_test() {
        const PAYLOAD: &[u8] = b"panic buffer: empty\n\n\
--- BEGIN DLOG DUMP ---\n\
DLOG -> Crashlog disabled\n\n\
--- END DLOG DUMP ---\n";

        let mut out = TestBuf::new();
        write_payload_and_recovered_dlog(&mut |b| out.push(b), PAYLOAD, b"");
        assert_true!(out.as_bytes() == PAYLOAD);
    }

    /// Tests that a real debuglog dump stays intact alongside a recovered persistent log.
    #[test]
    fn real_dlog_dump_preserved_with_recovered_dlog_test() {
        const PAYLOAD: &[u8] = b"panic buffer: empty\n\n\
--- BEGIN DLOG DUMP ---\n\
crashlog dlog\n\
--- END DLOG DUMP ---\n";
        const DLOG: &[u8] = b"persistent dlog\n";

        let mut out = TestBuf::new();
        write_payload_and_recovered_dlog(&mut |b| out.push(b), PAYLOAD, DLOG);
        assert_true!(
            out.as_bytes()
                == b"panic buffer: empty\n\n\
--- BEGIN DLOG DUMP ---\n\
crashlog dlog\n\
--- END DLOG DUMP ---\n\
Recovered 16 bytes from the persistent debug log\n\
=================== BEGIN ===================\n\
persistent dlog\n\
=================== END ===================\n"
        );
    }

    /// Tests that a recovered persistent log uses DLOG DUMP markers on an empty payload.
    #[test]
    fn empty_payload_with_recovered_dlog_test() {
        const DLOG: &[u8] = b"watchdog bite";

        let mut out = TestBuf::new();
        write_payload_and_recovered_dlog(&mut |b| out.push(b), b"", DLOG);
        assert_true!(
            out.as_bytes()
                == b"Recovered 13 bytes from the persistent debug log\n\
--- BEGIN DLOG DUMP ---\n\
watchdog bite\n\
--- END DLOG DUMP ---\n"
        );
    }
}
