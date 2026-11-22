// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::crypto::entropy::collector::{Collector, CollectorBase};
use crate::hw_rng::{hw_rng_get_entropy, hw_rng_is_registered};
use crate::ksync_kernel::TrackedMutex;
use core::convert::Infallible;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, Ordering};
use ksync::KMutex;
use lazy_init::LazyInit;
use pin_init::{PinInit, pin_data, pin_init};
use zx_status::Status;

static G_COLLECTOR: LazyInit<HwRngCollector> = LazyInit::uninit();

/// One-shot initialization states of `G_COLLECTOR`, tracked by `get_instance`.
const STATE_UNINITIALIZED: u32 = 0;
const STATE_INITIALIZING: u32 = 1;
const STATE_INITIALIZED: u32 = 2;

/// An implementation of `Collector` that uses `hw_rng_get_entropy`
/// as its entropy source. Currently, this is only supported on x86.
#[ksync::guarded]
#[pin_data]
pub struct HwRngCollector {
    #[pin]
    base: CollectorBase,

    #[mutex]
    lock: KMutex<TrackedMutex>,
}

impl HwRngCollector {
    /// Gets the current `HwRngCollector` instance. Returns `Status::NOT_SUPPORTED` if
    /// `hw_rng_get_entropy` is not supported.
    ///
    /// This function is thread-safe, and the `draw_entropy` method of the global
    /// `HwRngCollector` instance is also thread-safe.
    pub fn get_instance() -> Result<&'static HwRngCollector, Status> {
        // Check for a registered hardware RNG on every call. Platform HW RNG drivers
        // may register after the first caller (e.g. EarlyBootSeed on ARM64, where the
        // drivers are registered later in `driver_handoff_late`), so an early call must
        // not latch the collector as unavailable for later reseeds.
        if !hw_rng_is_registered() {
            return Err(Status::NOT_SUPPORTED);
        }

        // Construct the collector lazily, after static constructors have run, so that
        // its mutex is initialized after the lock class metadata it depends on. Only
        // reached once a hardware RNG is registered, so the one-shot initialization
        // always produces a usable instance.
        static STATE: AtomicU32 = AtomicU32::new(STATE_UNINITIALIZED);

        // Release-acquire ordering guarantees that, once the initializing thread
        // has stored `STATE_INITIALIZED` with `Ordering::Release`, any other thread
        // that later loads `STATE` with `Ordering::Acquire` and observes that value
        // also sees the fully constructed `G_COLLECTOR`.
        //
        // The compare-exchange guarantees that only one thread ever runs the
        // initialization code; any thread that loses the race (or arrives while it
        // is in progress) spins until the result is published.
        if STATE.load(Ordering::Acquire) != STATE_INITIALIZED {
            if STATE
                .compare_exchange(
                    STATE_UNINITIALIZED,
                    STATE_INITIALIZING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                // SAFETY: The compare-exchange above guarantees exactly one caller ever
                // reaches this point, and everyone else waits for `STATE` to become
                // `STATE_INITIALIZED`.
                let Ok(()) =
                    unsafe { Pin::static_ref(&G_COLLECTOR).init_pin(HwRngCollector::init()) };
                STATE.store(STATE_INITIALIZED, Ordering::Release);
            } else {
                while STATE.load(Ordering::Acquire) != STATE_INITIALIZED {
                    libarch::r#yield();
                }
            }
        }

        Ok(&*G_COLLECTOR)
    }

    fn init() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            base <- CollectorBase::init(b"hw_rng", /*entropy_per_1000_bytes=*/ 8000),
            lock <- KMutex::init(),
        })
    }
}

impl Collector for HwRngCollector {
    fn base(&self) -> &CollectorBase {
        &self.base
    }

    /// Inherited from `Collector`; see comments there.
    ///
    /// Note that this method internally uses a mutex to prevent multiple
    /// accesses. It is safe to call this method from multiple threads, but it
    /// may block.
    ///
    /// TODO(andrewkrieger): Determine what level of thread safety is needed for
    /// RNG reseeding, and support it more uniformly (e.g. have a thread safety
    /// contract for Collector::draw_entropy, obeyed by all implementations).
    fn draw_entropy(&self, buf: &mut [u8]) -> usize {
        // Especially on systems that have RdRand but not RdSeed, avoid parallel
        // accesses. Per the Intel documentation, properly using RdRand to seed a
        // CPRNG requires careful access patterns, to avoid multiple RNG draws from
        // the same physical seed (see https://fxbug.dev/42105846).
        ksync::lock!(self.lock_lock());

        hw_rng_get_entropy(buf)
    }
}
