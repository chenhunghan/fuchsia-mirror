// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::deadline::{Deadline, TimerSlack};
use crate::kernel::thread::ScopedMemoryStall;
use crate::platform_rs::timer::DurationMono;
use crate::vm::page::VmPagePtr;
use crate::vm::pmm_node::PmmOptDelayReuse;
use crate::vm::vm::oom_ktrace_duration;
use crate::vm::{pmm, scanner};
use anonymous_page_request_bindings as bindings;
use core::sync::atomic::{AtomicBool, Ordering};
use debug::dprintf;
use zx_status::Status;
use zx_types::zx_status_t;

/// Atomic token used to ensure that only one thread performs the informational dump so that in the
/// scenario of many threads all reaching the timeout at the same time there is not a rush of spam.
static DUMP_INFO_BEFORE_PANIC_TOKEN: AtomicBool = AtomicBool::new(true);

/// Helper around tracking and performing waits for the PMM to be able to succeed waitable
/// allocations. This is intended to have a similar Wait method as a regular PageRequest to
/// provide a consistent interface. Users are unlikely to want to use this directly, and instead
/// probably want a MultiPageRequest.
/// This class is not thread safe.
#[repr(C)]
#[derive(Default)]
pub struct AnonymousPageRequest {
    active: bool,
    // Safety invariant: when `Some`, points to a valid, unloaned PMM page allocated via
    // `wait_for_single_page_allocation` and owned exclusively by `self`.
    page: Option<VmPagePtr>,
}

// Compile-time layout assertions against the C++ AnonymousPageRequest type via bindgen.
zr::static_assert!(
    core::mem::size_of::<AnonymousPageRequest>()
        == core::mem::size_of::<bindings::AnonymousPageRequest>()
);
zr::static_assert!(
    core::mem::align_of::<AnonymousPageRequest>()
        == core::mem::align_of::<bindings::AnonymousPageRequest>()
);
zr::static_assert!(
    core::mem::offset_of!(AnonymousPageRequest, active)
        == core::mem::offset_of!(bindings::AnonymousPageRequest, active_)
);
zr::static_assert!(
    core::mem::offset_of!(AnonymousPageRequest, page)
        == core::mem::offset_of!(bindings::AnonymousPageRequest, page_)
);

impl Drop for AnonymousPageRequest {
    fn drop(&mut self) {
        if self.has_page() {
            self.free_page();
        }
    }
}

impl AnonymousPageRequest {
    /// Creates a new inactive `AnonymousPageRequest` with no page allocated.
    #[inline]
    pub const fn new() -> Self {
        Self { active: false, page: None }
    }

    /// Make the request active, causing any future Wait calls to block on the PMM.
    /// Caller must make sure no page is allocated (`!has_page()`) before calling `make_active()`.
    #[inline]
    pub fn make_active(&mut self) {
        debug_assert!(!self.has_page());
        self.active = true;
    }

    /// Returns whether the request is active or not.
    #[inline]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Make the request inactive, if it was currently active. As this class is not thread safe, it
    /// assumes there are no parallel calls to Wait that would need to be interrupted.
    #[inline]
    pub fn cancel(&mut self) {
        self.active = false;
    }

    /// On success, a page is allocated.
    pub fn allocate(&mut self, suspendable: bool) -> Result<(), Status> {
        debug_assert!(self.active);
        debug_assert!(!self.has_page());

        oom_ktrace_duration!(1, "AnonymousPageRequest::Allocate");

        // Although the pmm_wait_till_free_pages call will unblock based on bounded kernel action,
        // and not some unbounded user request, the kernel might need to acquire arbitrary locks to
        // achieve this. Therefore blanket require no locks here to ensure no accidental lock
        // dependencies. This can be relaxed in the future if necessary.
        lockdep::assert_no_locks_held();

        // Time spent waiting is regarded as a memory stall.
        let _memory_stall = ScopedMemoryStall::new();

        // Once we return, clear the `active` flag.
        let _defer = zr::defer(|| {
            self.active = false;
        });

        // This should only ever end up waiting momentarily until reclamation catches up. As such if
        // we end up waiting for a long time then this is probably a sign of a bug in reclamation
        // somewhere, so we want to make some noise here.
        const REPORT_WAIT_TIME: DurationMono = DurationMono::from_seconds(5);
        const MAX_WAITS: u32 =
            (DurationMono::from_seconds(30).into_nanos() / REPORT_WAIT_TIME.into_nanos()) as u32;
        zr::static_assert!(MAX_WAITS >= 1);
        // Trigger an informational dump 1 wait prior to triggering a panic. This gives the debug
        // log a chance to get flushed out before the panic.
        const DUMP_INFO_WAITS: u32 = MAX_WAITS - 1;
        let mut waited: u32 = 0;

        loop {
            let page_alloc = pmm::node().wait_for_single_page_allocation(
                Deadline::after_mono(REPORT_WAIT_TIME, TimerSlack::none()),
                suspendable,
            );
            if let Err(err) = page_alloc {
                // System is not making forward progress, lets try again.
                if err == Status::TIMED_OUT {
                    waited += 1;
                    dprintf!(
                        INFO,
                        "WARNING: Waited {} seconds to retry PMM allocations\n",
                        (REPORT_WAIT_TIME * i64::from(waited)).into_seconds()
                    );

                    // If we've reached the threshold to dump information *and* we're the first
                    // thread to do so, i.e. first to get the token, then ask the scanner to do
                    // a pre panic dump. The assumption here is that if we've been blocked this
                    // long then we are most likely going to hit MAX_WAITS and so it's worth
                    // performing a slightly unsafe informational dump for the purposes of
                    // debugging.
                    if waited == DUMP_INFO_WAITS
                        && DUMP_INFO_BEFORE_PANIC_TOKEN.swap(false, Ordering::SeqCst)
                    {
                        // SAFETY: `wait_for_single_page_allocation` just blocked on
                        // `may_allocate_evt` and timed out, so no thread locks or active
                        // `ChainLockTransaction` are held on this CPU, and the global
                        // reclamation/scanner threads are never torn down at runtime.
                        unsafe {
                            scanner::debug_dump_state_before_panic();
                        }
                    }

                    if waited == MAX_WAITS {
                        // If we've been waiting for a while without being able to get more
                        // memory and without declaring OOM, the memory watchdog is probably
                        // wedged and the system is in an unrecoverable state. It would be nice
                        // to get some diagnostics here but we don't want to risk trying to
                        // acquire any locks; just panic.
                        panic!(
                            "Allocation stalled for {} seconds",
                            (REPORT_WAIT_TIME * i64::from(waited)).into_seconds()
                        );
                    }
                    continue;
                }
                // The system reached an allocation allowing event, but we lost the race for the
                // memory resources. We reset the retry count, since the system IS making
                // forward progress.
                if err == Status::SHOULD_WAIT {
                    waited = 0;
                    continue;
                }
                return Err(err);
            }

            self.page = page_alloc.ok();
            return Ok(());
        }
    }

    /// Retrieves a possibly allocated `page`.
    #[inline]
    pub fn take_page(&mut self) -> Option<VmPagePtr> {
        self.page.take()
    }

    /// Returns whether a page is allocated.
    #[inline]
    pub const fn has_page(&self) -> bool {
        self.page.is_some()
    }

    /// Returns a raw pointer to `self` typed for FFI calls taking `AnonymousPageRequest*`.
    #[inline]
    pub fn as_raw(&mut self) -> *mut bindings::AnonymousPageRequest {
        core::ptr::from_mut(self).cast()
    }

    fn free_page(&mut self) {
        let page = self.take_page().unwrap();
        // SAFETY: `self.page` only holds a valid allocated PMM page returned by
        // `wait_for_single_page_allocation`, and `take_page()` transfers ownership to us.
        unsafe { pmm::node().free_page(page, PmmOptDelayReuse::Default) };
    }
}

/// Forwards `AnonymousPageRequest::Allocate`.
// SAFETY: The symbol name is prefixed with `rust_anonymous_page_request_` and does not collide
// with any other symbol in the kernel.
#[unsafe(no_mangle)]
extern "C" fn rust_anonymous_page_request_allocate(
    req: &mut AnonymousPageRequest,
    suspendable: bool,
) -> zx_status_t {
    Status::result_into_raw(req.allocate(suspendable))
}

/// Forwards `AnonymousPageRequest::FreePage`.
// SAFETY: The symbol name is prefixed with `rust_anonymous_page_request_` and does not collide
// with any other symbol in the kernel.
#[unsafe(no_mangle)]
extern "C" fn rust_anonymous_page_request_free_page(req: &mut AnonymousPageRequest) {
    req.free_page();
}

/// Unit tests for `AnonymousPageRequest`.
#[cfg(ktest)]
#[unittest::suite(name = "anonymous_page_request")]
mod tests {
    use super::AnonymousPageRequest;
    use crate::vm::pmm;
    use crate::vm::pmm_node::PmmOptDelayReuse;
    use unittest::{expect_false, expect_ok, expect_true, unwrap_ok};

    /// Tests active state transitions and page allocation/deallocation.
    #[test]
    fn allocate_and_free() {
        let mut req = AnonymousPageRequest::new();
        expect_false!(req.is_active());
        expect_false!(req.has_page());
        expect_true!(req.take_page().is_none());

        req.make_active();
        expect_true!(req.is_active());
        req.cancel();
        expect_false!(req.is_active());

        // Allocate a page and take ownership via `take_page`.
        req.make_active();
        expect_true!(req.is_active());
        unwrap_ok!(req.allocate(true));
        expect_false!(req.is_active());
        expect_true!(req.has_page());
        let page = req.take_page().expect("expected allocated page");
        expect_false!(req.has_page());
        // SAFETY: `page` was allocated by `req.allocate(true)` and ownership was transferred via
        // `take_page()`.
        unsafe { pmm::node().free_page(page, PmmOptDelayReuse::Default) };

        // Allocate a page and let `Drop` free it.
        req.make_active();
        expect_ok!(req.allocate(true));
        expect_true!(req.has_page());
    }
}
