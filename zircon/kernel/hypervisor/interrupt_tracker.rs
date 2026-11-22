// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hypervisor virtual interrupt bitmap and tracker (`<hypervisor/interrupt_tracker.h>`).

use crate::ksync_kernel::TrackedSpinlock;
use bitmap::{Bitmap, FixedStorage, RawBitmapGeneric};
use core::convert::Infallible;
use core::mem::{align_of, size_of};
use ksync::{KCell, KMutex};
use pin_init::{Init, PinInit, init, pin_init};
use zr::static_assert;
use zx_status::Status;

use super::ktrace::{VcpuBlockOp, VcpuMeta, ktrace_vcpu};
use super::state_invalidator::Invalidate;
use crate::kernel::deadline::Deadline;
use crate::kernel::event::AutounsignalEvent;
use crate::platform_rs::timer::InstantMono;

/// Default interrupt vector count for [`InterruptBitmap`] and [`InterruptTracker`].
const DEFAULT_INTERRUPT_VECTORS: usize = 256;

/// Returns the number of `usize` words needed to store `n` interrupt bits.
const fn bitmap_words(n: usize) -> usize {
    n.div_ceil(usize::BITS as usize)
}

/// Bitmap of `N` interrupt vectors, backed by `WORDS` words of inline storage
/// (`WORDS == bitmap_words(N)`).
#[derive(Debug)]
#[repr(C)]
struct InterruptBitmap<
    const N: usize = DEFAULT_INTERRUPT_VECTORS,
    const WORDS: usize = { bitmap_words(DEFAULT_INTERRUPT_VECTORS) },
> {
    bitmap: RawBitmapGeneric<FixedStorage<WORDS>>,
}

static_assert!(size_of::<InterruptBitmap>() == 40);
static_assert!(align_of::<InterruptBitmap>() == 8);

impl<const N: usize, const WORDS: usize> InterruptBitmap<N, WORDS> {
    /// Creates an in-place initializer for a cleared `InterruptBitmap<N, WORDS>`.
    fn init() -> impl Init<Self, Infallible> {
        const {
            assert!(N <= u32::MAX as usize, "N must fit in u32");
            assert!(WORDS == bitmap_words(N), "WORDS must equal N.div_ceil(usize::BITS)");
        }
        init!(Self { bitmap <- RawBitmapGeneric::<FixedStorage<WORDS>>::init_fixed(N) })
    }

    /// Creates a new cleared `InterruptBitmap<N, WORDS>`.
    #[cfg(ktest)]
    fn new() -> Self {
        const {
            assert!(N <= u32::MAX as usize, "N must fit in u32");
            assert!(WORDS == bitmap_words(N), "WORDS must equal N.div_ceil(usize::BITS)");
        }
        let mut bitmap = RawBitmapGeneric::default();
        let result = bitmap.reset(N);
        // `bitmap` uses static storage, so `reset` cannot fail.
        debug_assert_eq!(result, Ok(()));
        Self { bitmap }
    }

    /// Returns whether `vector` (where `vector < N`) is set in the bitmap.
    #[inline]
    fn get(&self, vector: u32) -> bool {
        self.bitmap.get_one(vector as usize)
    }

    /// Sets `vector` in the bitmap.
    #[inline]
    fn set(&mut self, vector: u32) {
        // TODO(https://fxbug.dev/572853263): Slice `RawBitmapGeneric` storage directly and add
        // `#[inline]` single-word `set_one` / `clear_one` specializations (also used in `try_pop`
        // and `pop`) to inline single-bit updates without iterator spills.
        let _ = self.bitmap.set_one(vector as usize);
    }

    /// Clears all vectors in `[min, max)`.
    #[inline]
    fn clear(&mut self, min: u32, max: u32) {
        let _ = self.bitmap.clear(min as usize, max as usize);
    }

    /// Scans for the highest-priority pending interrupt vector.
    #[inline]
    fn scan(&self) -> Option<u32> {
        #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
        let bitoff = self.bitmap.scan(0, N, false);
        #[cfg(target_arch = "x86_64")]
        let bitoff = self.bitmap.reverse_scan(0, N, false);
        #[cfg(not(any(target_arch = "aarch64", target_arch = "riscv64", target_arch = "x86_64")))]
        compile_error!("Unsupported architecture for InterruptBitmap::scan");
        bitoff.map(|off| off as u32)
    }
}

/// `N` is the maximum number of interrupts to be tracked.
// Note: `#[ksync::guarded]` expands to `#[pin_init::pin_data]`, so `#[pin]` on `event` makes
// `InterruptTracker` `!Unpin` and structurally pins `event`.
#[ksync::guarded]
#[repr(C)]
pub struct InterruptTracker<
    const N: usize = DEFAULT_INTERRUPT_VECTORS,
    const WORDS: usize = { bitmap_words(DEFAULT_INTERRUPT_VECTORS) },
> {
    #[pin]
    event: AutounsignalEvent,
    #[mutex]
    lock: KMutex<TrackedSpinlock>,
    #[guarded_by(lock)]
    bitmap: InterruptBitmap<N, WORDS>,
}

static_assert!(
    size_of::<InterruptTracker>()
        == size_of::<AutounsignalEvent>()
            + size_of::<TrackedSpinlock>().next_multiple_of(8)
            + size_of::<InterruptBitmap>()
);
static_assert!(align_of::<InterruptTracker>() == 8);

impl<const N: usize, const WORDS: usize> InterruptTracker<N, WORDS> {
    /// Creates an in-place pinned initializer for `InterruptTracker<N, WORDS>`.
    pub fn init() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            event <- AutounsignalEvent::init_unsignaled(),
            lock <- KMutex::init(),
            bitmap <- KCell::init(InterruptBitmap::<N, WORDS>::init()),
        })
    }

    /// Returns whether there are pending interrupts.
    #[inline]
    pub fn pending(&self) -> bool {
        ksync::lock!(let guard = self.lock_lock());
        guard.bitmap().scan().is_some()
    }

    /// Clears all vectors in the range `[min, max)`.
    #[inline]
    pub fn clear(&self, min: u32, max: u32) {
        ksync::lock!(let mut guard = self.lock_lock());
        guard.as_mut().bitmap_mut().clear(min, max);
    }

    /// Pops the specified vector, if it is pending.
    #[inline]
    pub fn try_pop(&self, vector: u32) -> bool {
        // TODO(https://fxbug.dev/572853263): Combine the `X86_INT_NMI` check and priority `pop`
        // under one spinlock acquisition in `local_apic_maybe_interrupt`
        // (`arch/x86/hypervisor/vcpu.rs`).
        if (vector as usize) >= N {
            core::hint::cold_path();
            return false;
        }
        ksync::lock!(let mut guard = self.lock_lock());
        if guard.bitmap().get(vector) {
            guard.as_mut().bitmap_mut().clear(vector, vector + 1);
            return true;
        }
        false
    }

    /// Pops the highest priority interrupt.
    #[inline]
    pub fn pop(&self) -> Option<u32> {
        ksync::lock!(let mut guard = self.lock_lock());
        if let Some(vector) = guard.bitmap().scan() {
            guard.as_mut().bitmap_mut().clear(vector, vector + 1);
            return Some(vector);
        }
        None
    }

    /// Tracks the given interrupt.
    #[inline]
    pub fn track(&self, vector: u32) -> Result<(), Status> {
        if (vector as usize) >= N {
            core::hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }
        ksync::lock!(let mut guard = self.lock_lock());
        guard.as_mut().bitmap_mut().set(vector);
        Ok(())
    }

    /// Tracks the compile-time validated interrupt `VECTOR`.
    #[inline]
    pub fn track_const<const VECTOR: u32>(&self) {
        const {
            assert!((VECTOR as usize) < N, "Vector out of bounds");
        }
        let _ = self.track(VECTOR);
    }

    /// Tracks the given interrupt, and signals any waiters.
    #[inline]
    pub fn interrupt(&self, vector: u32) -> Result<(), Status> {
        let result = self.track(vector);
        if result.is_ok() {
            self.event.signal();
        }
        result
    }

    /// Tracks the compile-time validated interrupt `VECTOR`, and signals any waiters.
    #[inline]
    pub fn interrupt_const<const VECTOR: u32>(&self) {
        const {
            assert!((VECTOR as usize) < N, "Vector out of bounds");
        }
        let _ = self.interrupt(VECTOR);
    }

    /// Cancels a wait for an interrupt.
    ///
    /// We signal [`Status::INTERRUPTED_RETRY`] (`ZX_ERR_INTERNAL_INTR_RETRY`), so that if the
    /// status is propagated to the syscall-layer, we will retry the syscall.
    #[inline]
    pub fn cancel(&self) {
        self.event.signal_result(Err(Status::INTERRUPTED_RETRY));
    }

    /// Waits for an interrupt, optionally invalidating `invalidator` before blocking.
    #[inline]
    pub fn wait(
        &self,
        deadline: InstantMono,
        invalidator: Option<&mut dyn Invalidate>,
    ) -> Result<(), Status> {
        if let Some(inv) = invalidator {
            inv.invalidate();
        }
        ktrace_vcpu(VcpuBlockOp::Block, VcpuMeta::Interrupt);
        let _defer = zr::defer(|| ktrace_vcpu(VcpuBlockOp::Unblock, VcpuMeta::Interrupt));
        loop {
            match self.event.wait(&Deadline::no_slack(deadline.into())) {
                Ok(()) => {}
                // If the event timed out, return `Ok(())` to resume the VCPU.
                Err(Status::TIMED_OUT) => return Ok(()),
                // Otherwise, return the status.
                Err(status) => {
                    core::hint::cold_path();
                    return Err(status);
                }
            }
            if self.pending() {
                break;
            }
        }
        Ok(())
    }
}

/// Unit tests for `InterruptBitmap` and `InterruptTracker`.
#[cfg(ktest)]
#[unittest::suite(name = "interrupt_tracker_rust")]
mod tests {
    // Note: `#[unittest::suite]` automatically injects `use super::*;`.
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_err, expect_false, expect_ok, expect_true, unwrap_some};

    /// Verifies `InterruptBitmap` set, get, clear, and scan operations.
    #[test]
    fn interrupt_bitmap() {
        let mut bitmap = InterruptBitmap::<8, 1>::new();

        expect_false!(bitmap.get(0));
        expect_false!(bitmap.get(1));
        expect_true!(bitmap.scan().is_none());

        // Index 0.
        bitmap.set(0);
        expect_true!(bitmap.get(0));
        expect_false!(bitmap.get(1));
        expect_eq!(unwrap_some!(bitmap.scan()), 0);

        bitmap.clear(0, 1);
        expect_false!(bitmap.get(0));
        expect_false!(bitmap.get(1));
        expect_true!(bitmap.scan().is_none());

        // Index 1.
        bitmap.set(1);
        expect_false!(bitmap.get(0));
        expect_true!(bitmap.get(1));
        expect_eq!(unwrap_some!(bitmap.scan()), 1);

        bitmap.clear(1, 2);
        expect_false!(bitmap.get(0));
        expect_false!(bitmap.get(1));
        expect_true!(bitmap.scan().is_none());

        // Clear
        bitmap.set(0);
        bitmap.set(1);
        bitmap.set(2);
        bitmap.set(3);
        bitmap.clear(1, 3);
        expect_true!(bitmap.get(0));
        expect_false!(bitmap.get(1));
        expect_false!(bitmap.get(2));
        expect_true!(bitmap.get(3));
    }

    /// Verifies `InterruptTracker` track, pending, and pop operations.
    #[test]
    fn interrupt_tracker_track_and_pop() {
        stack_pin_init!(let tracker = InterruptTracker::<8, 1>::init());
        expect_false!(tracker.pending());
        expect_true!(tracker.pop().is_none());
        expect_err!(tracker.track(8), Status::OUT_OF_RANGE);

        expect_ok!(tracker.track(2));
        expect_ok!(tracker.track(5));
        expect_true!(tracker.pending());
        expect_true!(tracker.try_pop(2));
        expect_false!(tracker.try_pop(2));
        expect_false!(tracker.try_pop(8));

        // Re-track 2 to verify priority pop ordering with multiple items.
        expect_ok!(tracker.track(2));
        #[cfg(target_arch = "x86_64")]
        {
            expect_eq!(unwrap_some!(tracker.pop()), 5);
            expect_eq!(unwrap_some!(tracker.pop()), 2);
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            expect_eq!(unwrap_some!(tracker.pop()), 2);
            expect_eq!(unwrap_some!(tracker.pop()), 5);
        }
        expect_false!(tracker.pending());
    }
}
