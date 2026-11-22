// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::fmt;
use debug::{driver_oops, driver_warn};
use zx_types::{ZX_MAX_NAME_LEN, zx_koid_t};

use super::escaped_name::EscapedName;
use crate::object::{ProcessDispatcher, RootJobObserver, ThreadDispatcher};

/// Reason why a BTI quarantined or leaked pinned memory pages.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtiPageLeakReason {
    /// The final handle to a BTI was closed while it still had quarantined PMTs.
    /// User-mode no longer has any way to release the quarantine on these PMTs,
    /// and they are now permanently leaked.
    BtiOrphanedWithQuarantinedPmts = 0,
    /// The final handle to a PMT was closed without it having been formally
    /// unpinned, and after its BTI had become orphaned. Once again, user-mode
    /// no longer has any way to release the quarantine on these PMTs, and they
    /// are now permanently leaked.
    PmtQuarantinedWhenBtiOrphaned = 1,
    /// The final handle to a PMT was closed without it having been unpinned.
    PmtQuarantined = 2,
}

zr::static_assert!(core::mem::size_of::<BtiPageLeakReason>() == 4);
zr::static_assert!(core::mem::align_of::<BtiPageLeakReason>() == 4);

/// Suppresses `DRIVER OOPS` log emission during kernel unit tests that deliberately orphan a BTI
/// with quarantined PMTs (since `tefmocheck` treats `"DRIVER OOPS"` in serial logs as a test
/// failure).
#[cfg(ktest)]
static SUPPRESS_QUARANTINE_OOPS_FOR_TEST: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Enables or disables `DRIVER OOPS` log suppression for kernel unit tests.
#[cfg(ktest)]
pub(crate) fn set_suppress_quarantine_oops_for_test(suppress: bool) {
    SUPPRESS_QUARANTINE_OOPS_FOR_TEST.store(suppress, core::sync::atomic::Ordering::Relaxed);
}

/// Maximum number of quarantine warnings buffered during a kernel unit test.
#[cfg(all(ktest, target_arch = "aarch64"))]
const MAX_CAPTURED_QUARANTINE_WARNINGS: usize = 8;

/// Quarantine warning record captured during a kernel unit test.
#[cfg(all(ktest, target_arch = "aarch64"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CapturedQuarantineWarning {
    pub(crate) bti_id: u64,
    pub(crate) bti_name: [u8; ZX_MAX_NAME_LEN],
    pub(crate) reason: BtiPageLeakReason,
    pub(crate) pages: u64,
    pub(crate) vmos: usize,
}

#[cfg(all(ktest, target_arch = "aarch64"))]
#[derive(Clone, Copy)]
struct QuarantineWarningCapture {
    enabled: bool,
    overflowed: bool,
    head: usize,
    len: usize,
    entries: [Option<CapturedQuarantineWarning>; MAX_CAPTURED_QUARANTINE_WARNINGS],
}

#[cfg(all(ktest, target_arch = "aarch64"))]
impl QuarantineWarningCapture {
    const fn new() -> Self {
        Self {
            enabled: false,
            overflowed: false,
            head: 0,
            len: 0,
            entries: [None; MAX_CAPTURED_QUARANTINE_WARNINGS],
        }
    }

    fn reset(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.overflowed = false;
        self.head = 0;
        self.len = 0;
    }
}

#[cfg(all(ktest, target_arch = "aarch64"))]
crate::ksync_kernel::declare_singleton_spinlock!(QuarantineWarningCaptureLock);

#[cfg(all(ktest, target_arch = "aarch64"))]
static QUARANTINE_WARNING_CAPTURE: ksync::KCell<
    QuarantineWarningCapture,
    QuarantineWarningCaptureLock,
> = ksync::KCell::new(QuarantineWarningCapture::new());

#[cfg(all(ktest, target_arch = "aarch64"))]
fn with_quarantine_warning_capture<R>(f: impl FnOnce(&mut QuarantineWarningCapture) -> R) -> R {
    ksync::lock!(let mut guard = QuarantineWarningCaptureLock::lock());
    // SAFETY: `QuarantineWarningCaptureLock` is a singleton spinlock, so `guard`'s mutable token
    // proves exclusive access to `QUARANTINE_WARNING_CAPTURE`.
    let cap = unsafe { QUARANTINE_WARNING_CAPTURE.get_mut(guard.as_mut().token_mut()) };
    f(cap)
}

/// RAII guard that captures `print_quarantine_warning` calls (suppressing `driver_oops!` and
/// `driver_warn!` emission) until dropped.
#[cfg(all(ktest, target_arch = "aarch64"))]
#[must_use = "if unused, quarantine warning capture stops immediately on drop"]
pub(crate) struct QuarantineWarningCaptureGuard(());

#[cfg(all(ktest, target_arch = "aarch64"))]
impl Drop for QuarantineWarningCaptureGuard {
    fn drop(&mut self) {
        with_quarantine_warning_capture(|cap| cap.reset(false));
    }
}

/// Starts capturing quarantine warnings for a kernel unit test and clears any prior entries.
#[cfg(all(ktest, target_arch = "aarch64"))]
pub(crate) fn start_capturing_quarantine_warnings_for_test() -> QuarantineWarningCaptureGuard {
    with_quarantine_warning_capture(|cap| {
        assert!(!cap.enabled, "quarantine warning capture already active");
        cap.reset(true);
    });
    QuarantineWarningCaptureGuard(())
}

/// Pops and returns the oldest captured quarantine warning (if any) since capture started or was
/// last drained.
#[cfg(all(ktest, target_arch = "aarch64"))]
pub(crate) fn take_captured_quarantine_warning_for_test() -> Option<CapturedQuarantineWarning> {
    with_quarantine_warning_capture(|cap| {
        assert!(!cap.overflowed, "quarantine warning capture buffer overflowed");
        if cap.len == 0 {
            return None;
        }
        let item = cap.entries[cap.head].take();
        cap.head = (cap.head + 1) % MAX_CAPTURED_QUARANTINE_WARNINGS;
        cap.len -= 1;
        item
    })
}

enum ThreadName<'a> {
    Name(EscapedName<'a>),
    Koid(zx_koid_t),
}

impl fmt::Display for ThreadName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(name) => fmt::Display::fmt(name, f),
            Self::Koid(koid) => write!(f, "<koid {}>", koid),
        }
    }
}

/// Logs a BTI quarantine warning or `DRIVER OOPS` for `bti_id` and `bti_name`.
#[cold]
pub fn print_quarantine_warning(
    bti_id: u64,
    bti_name: &[u8],
    reason: BtiPageLeakReason,
    total_leaked_pages: u64,
    total_leaked_vmos: usize,
) {
    let mut proc_name_buf = [0u8; ZX_MAX_NAME_LEN];
    let mut thread_name_buf = [0u8; ZX_MAX_NAME_LEN];

    // If we have no current thread dispatcher, then this is a kernel thread. We
    // have no process to report, just report that the action was taken by a
    // kernel thread and leave it at that.
    let (proc_name, thread_name) = match ThreadDispatcher::try_get_current() {
        None => (EscapedName::new(b"<kernel>"), ThreadName::Name(EscapedName::new(b"<kernel>"))),
        Some(thread_disp) => {
            // Get the name of the user mode process and thread which closed the handle
            // to the object which eventually resulted in the leak.
            let status = ProcessDispatcher::get_current().get_name(&mut proc_name_buf);
            debug_assert!(status.is_ok());

            let thread_name = if thread_disp.get_name(&mut thread_name_buf).is_ok() {
                ThreadName::Name(EscapedName::new_or_unknown(&thread_name_buf))
            } else {
                ThreadName::Koid(thread_disp.get_koid())
            };
            (EscapedName::new_or_unknown(&proc_name_buf), thread_name)
        }
    };

    let escaped_bti_name = EscapedName::new_or_unknown(bti_name);

    // Finally, print the message describing the leak, as best we can.
    let leak_cause = match reason {
        BtiPageLeakReason::BtiOrphanedWithQuarantinedPmts => {
            "a BTI having it's final handle closed with a non-empty quarantine list"
        }
        BtiPageLeakReason::PmtQuarantinedWhenBtiOrphaned => {
            "a pinned PMT having its final handle closed after its BTI also had its final handle \
             closed"
        }
        BtiPageLeakReason::PmtQuarantined => {
            "a pinned PMT having its final handle closed without being unpinned first"
        }
    };

    // If we're being torn down for reasons other than the root job being killed
    // over a critical process dying then fire off a DRIVER OOPS to flag the
    // improper handling of pinned pages.
    if !RootJobObserver::get_critical_process_dying() {
        #[cfg(all(ktest, target_arch = "aarch64"))]
        if with_quarantine_warning_capture(|cap| {
            if !cap.enabled {
                return false;
            }
            if cap.len < MAX_CAPTURED_QUARANTINE_WARNINGS {
                let mut name = [0u8; ZX_MAX_NAME_LEN];
                let raw = EscapedName::new(bti_name).as_bytes();
                let copy_len = raw.len().min(ZX_MAX_NAME_LEN - 1);
                name[..copy_len].copy_from_slice(&raw[..copy_len]);
                let idx = (cap.head + cap.len) % MAX_CAPTURED_QUARANTINE_WARNINGS;
                cap.entries[idx] = Some(CapturedQuarantineWarning {
                    bti_id,
                    bti_name: name,
                    reason,
                    pages: total_leaked_pages,
                    vmos: total_leaked_vmos,
                });
                cap.len += 1;
            } else {
                cap.overflowed = true;
            }
            true
        }) {
            return;
        }
        let msg = format_args!(
            "Bus Transaction Initiator (ID 0x{bti_id:x}, name \"{escaped_bti_name}\") has leaked \
             {total_leaked_pages} pages in {total_leaked_vmos} VMOs. Leak was caused by \
             {leak_cause}. The last handle was closed by process \"{proc_name}\", and thread \
             \"{thread_name}\"\n"
        );
        if reason != BtiPageLeakReason::PmtQuarantined {
            #[cfg(ktest)]
            if SUPPRESS_QUARANTINE_OOPS_FOR_TEST.load(core::sync::atomic::Ordering::Relaxed) {
                return;
            }
            driver_oops!("{msg}");
        } else {
            driver_warn!("{msg}");
        }
    }
}
