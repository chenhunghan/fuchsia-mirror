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

// LINT.IfChange(BtiPageLeakReason)
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
// LINT.ThenChange(
//   //zircon/kernel/dev/iommu/arm_smmu/common/constants.rs:BtiPageLeakReason,
//   //zircon/kernel/dev/iommu/bti_ffi.cc:BtiPageLeakReason,
//   //zircon/kernel/dev/iommu/include/dev/iommu/bti.h:BtiPageLeakReason,
// )

zr::static_assert!(core::mem::size_of::<BtiPageLeakReason>() == 4);
zr::static_assert!(core::mem::align_of::<BtiPageLeakReason>() == 4);

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

    let bti_name = EscapedName::new_or_unknown(bti_name);

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
        let msg = format_args!(
            "Bus Transaction Initiator (ID 0x{bti_id:x}, name \"{bti_name}\") has leaked \
             {total_leaked_pages} pages in {total_leaked_vmos} VMOs. Leak was caused by \
             {leak_cause}. The last handle was closed by process \"{proc_name}\", and thread \
             \"{thread_name}\"\n"
        );
        if reason != BtiPageLeakReason::PmtQuarantined {
            driver_oops!("{msg}");
        } else {
            driver_warn!("{msg}");
        }
    }
}

/// FFI entry point for C++ `iommu::Bti::PrintQuarantineWarning`.
// SAFETY: `rust_bti_print_quarantine_warning` is a unique global symbol matching the `extern "C"`
// declaration in `//zircon/kernel/dev/iommu/bti_ffi.cc`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_bti_print_quarantine_warning(
    bti_id: u64,
    bti_name: &[u8; ZX_MAX_NAME_LEN],
    reason: BtiPageLeakReason,
    total_leaked_pages: u64,
    total_leaked_vmos: usize,
) {
    print_quarantine_warning(bti_id, bti_name, reason, total_leaked_pages, total_leaked_vmos);
}
