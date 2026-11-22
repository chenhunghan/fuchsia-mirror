// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::crypto::entropy::collector::{Collector, CollectorBase};
use crate::ksync_kernel::TrackedMutex;
use boot_options::BootOptions;
use core::convert::Infallible;
use core::ffi::{c_char, c_int, c_uint};
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, Ordering};
use ksync::KMutex;
use lazy_init::LazyInit;
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init};
use zx_status::Status;

/// Size in bytes of the memory block jitterentropy exercises in its memory access loops.
///
/// It can be overridden at build time with
/// `kernel_extra_defines = [ "JITTERENTROPY_MEM_SIZE=<bytes>" ]`, which the kernel build
/// forwards to Rust as the `JITTERENTROPY_MEM_SIZE` environment variable.
///
/// TODO(andrewkrieger): after optimizing jitterentropy parameters
/// (see https://fxbug.dev/42105888), replace `JITTERENTROPY_MEM_SIZE` by the optimal
/// size.
const JITTERENTROPY_MEM_SIZE: usize = match option_env!("JITTERENTROPY_MEM_SIZE") {
    Some(value) => zr::parse_usize(value).expect("JITTERENTROPY_MEM_SIZE invalid"),
    None => 64 * 1024,
};

/// Storage for an opaque jitterentropy `struct rand_data` entropy collector.
#[repr(C, align(8))]
struct RandData(zr::OpaqueBytes<{ object_constants::kJitterentropyRandDataSize }>);

zr::static_assert_size_and_align!(
    RandData,
    object_constants::kJitterentropyRandDataSize,
    object_constants::kJitterentropyRandDataAlign,
);

unsafe extern "C" {
    /// Checks whether the system clock is suitable for jitterentropy; returns 0 on success.
    fn jent_entropy_init() -> c_int;

    /// Initializes `ec` to collect entropy using the `mem_size` bytes at `mem` for its memory
    /// access loops.
    ///
    /// # Safety
    ///
    /// `ec` must point to writable `rand_data` storage, and `mem` must stay valid and exclusively
    /// owned by `ec` for as long as `ec` is used.
    fn jent_entropy_collector_init(
        ec: *mut RandData,
        mem: *mut u8,
        mem_size: usize,
        mem_block_size: c_uint,
        mem_block_count: c_uint,
        mem_loops: c_uint,
        stir: bool,
    );

    /// Fills `len` bytes at `data` with processed entropy, returning the number of bytes written
    /// or a negative error code.
    ///
    /// # Safety
    ///
    /// `ec` must be an initialized collector and `data` valid for writes of `len` bytes.
    fn jent_read_entropy(ec: *mut RandData, data: *mut c_char, len: usize) -> isize;

    /// Statistical test entry point returning the raw timing sample of one folding operation.
    ///
    /// # Safety
    ///
    /// `ec` must be an initialized collector.
    fn jent_lfsr_var_stat(
        ec: *mut RandData,
        lfsr_loops_override: c_uint,
        mem_loops_override: c_uint,
    ) -> u64;
}

static G_COLLECTOR: LazyInit<JitterentropyCollector> = LazyInit::uninit();

/// An implementation of `Collector` that uses jitterentropy as
/// its entropy source. Ultimately, the entropy is derived from variations in
/// CPU timing, when various code blocks are exercised.
///
/// TODO(andrewkrieger): Document jitterentropy better for Zircon, then link to
/// that documentation here.
#[ksync::guarded]
#[pin_data]
pub struct JitterentropyCollector {
    #[pin]
    base: CollectorBase,

    #[guarded_by(lock)]
    ec: RandData,
    #[pin]
    #[guarded_by(lock)]
    mem: [u8; JITTERENTROPY_MEM_SIZE],

    #[mutex]
    lock: KMutex<TrackedMutex>,

    mem_loops: u32,
    lfsr_loops: u32,
    use_raw_samples: bool,
}

/// One-shot initialization states of `G_COLLECTOR`, tracked by `get_instance`.
const STATE_UNINITIALIZED: u32 = 0;
const STATE_INITIALIZING: u32 = 1;
const STATE_INITIALIZED: u32 = 2;
const STATE_UNAVAILABLE: u32 = 3;

impl JitterentropyCollector {
    /// Gets the global `JitterentropyCollector` instance. Returns
    /// `Status::NOT_SUPPORTED` if jitterentropy is not supported (usually because
    /// the system clock is not available or not suitable).
    ///
    /// This function is thread-safe: the first caller performs the one-time
    /// initialization of the `JitterentropyCollector` instance (typically during
    /// boot) while any concurrent callers wait for its outcome, which is then
    /// latched for every later call. The `JitterentropyCollector::draw_entropy`
    /// method is internally guarded by a mutex, so it's safe to call from multiple
    /// threads but it may block.
    pub fn get_instance() -> Result<&'static JitterentropyCollector, Status> {
        static STATE: AtomicU32 = AtomicU32::new(STATE_UNINITIALIZED);

        // Release-acquire ordering guarantees that, once the initializing thread
        // has stored the final state in `STATE` with `Ordering::Release`, any other
        // thread that later loads `STATE` with `Ordering::Acquire` will see the
        // result that the first thread wrote to `G_COLLECTOR`. In particular, any
        // thread that reads `STATE` as `STATE_INITIALIZED` will see `G_COLLECTOR`
        // as having been initialized.
        //
        // The compare-exchange guarantees that only one thread ever runs the
        // initialization code; any thread that loses the race (or arrives while it
        // is in progress) spins until the outcome is published.
        let mut state = STATE.load(Ordering::Acquire);
        if state == STATE_UNINITIALIZED {
            match STATE.compare_exchange(
                STATE_UNINITIALIZED,
                STATE_INITIALIZING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // SAFETY: `jent_entropy_init` has no preconditions.
                    state = if unsafe { jent_entropy_init() } != 0 {
                        // Initialization failed; leave `G_COLLECTOR` unconstructed.
                        STATE_UNAVAILABLE
                    } else {
                        // SAFETY: The compare-exchange above guarantees exactly one caller ever
                        // reaches this point, and everyone else waits for `STATE` to leave
                        // `STATE_INITIALIZING`, so the one-time initialization cannot race with
                        // any other access.
                        let Ok(()) = unsafe {
                            Pin::static_ref(&G_COLLECTOR).init_pin(JitterentropyCollector::init())
                        };
                        G_COLLECTOR.init_entropy_collector();
                        STATE_INITIALIZED
                    };
                    STATE.store(state, Ordering::Release);
                }
                Err(observed) => state = observed,
            }
        }
        while state == STATE_INITIALIZING {
            libarch::r#yield();
            state = STATE.load(Ordering::Acquire);
        }

        if state == STATE_INITIALIZED { Ok(&*G_COLLECTOR) } else { Err(Status::NOT_SUPPORTED) }
    }

    // TODO(https://fxbug.dev/42105889): Test jitterentropy in different environments (especially
    // on different platforms/architectures, and in multi-threaded mode). Ensure
    // entropy estimate is safe enough. Tune as per go/fuchsia-per-device:jitter.

    // Testing with NIST SP800-90B non-iid and restart tests show that, with the
    // default parameters below (bs=64, bc=512, ml=32, ll=1, raw=true), each byte
    // of data contributes approximately 0.5 bit of entropy on astro. A safety
    // factor of 0.1 gives us 0.5 * 0.1 * 1000 = 50 bits of entropy for 1000
    // bytes of random data.
    fn init() -> impl PinInit<Self, Infallible> {
        let options = BootOptions::get();
        // TODO(https://fxbug.dev/42105888): optimize default jitterentropy parameters and
        // update them in //zircon/kernel/lib/boot-options/include/lib/boot-options/options.inc.
        let mem_loops = options.jitterentropy_ml;
        let lfsr_loops = options.jitterentropy_ll;
        let use_raw_samples = options.jitterentropy_raw;

        pin_init!(Self {
            base <- CollectorBase::init(
                b"jitterentropy",
                options.jitterentropy_entropy_per_1000_bytes as usize,
            ),
            ec: RandData(zr::OpaqueBytes::uninit()).into(),
            // `mem` is `JITTERENTROPY_MEM_SIZE` bytes (64 KiB by default), far larger than a
            // kernel stack, so it must be zeroed in place rather than written from a by-value
            // temporary.
            mem <- ksync::kcell_init(pin_init::init_zeroed::<[u8; JITTERENTROPY_MEM_SIZE]>()),
            lock <- KMutex::init(),
            mem_loops,
            lfsr_loops,
            use_raw_samples,
        })
    }

    /// Initializes the jitterentropy state `ec` over the `mem` block, completing the construction
    /// started by `init`. Called exactly once by `get_instance` on the pinned global collector,
    /// before the instance is published to any other caller.
    fn init_entropy_collector(&self) {
        let options = BootOptions::get();
        let bs = options.jitterentropy_bs;
        let bc = options.jitterentropy_bc;

        ksync::lock!(let mut guard = self.lock_lock());
        let token = guard.as_mut().token_mut();
        // SAFETY: `token` proves `self.lock`, which guards both cells, is held. The pointers are
        // taken through this shared reference to the pinned global collector (never through a
        // `&mut` projection), so `ec` may keep `mem` and access it under `self.lock` for as long
        // as the collector exists.
        let (ec, mem) = unsafe { (self.ec.as_mut_ptr(token), self.mem.as_mut_ptr(token)) };
        // SAFETY: `ec` points to the collector's `RandData` storage and `mem` to its
        // `JITTERENTROPY_MEM_SIZE`-byte block; both are only ever accessed while `self.lock` is
        // held, as it is here.
        unsafe {
            jent_entropy_collector_init(
                ec,
                mem.cast::<u8>(),
                JITTERENTROPY_MEM_SIZE,
                bs,
                bc,
                self.mem_loops,
                /*stir=*/ true,
            );
        }
    }
}

impl Collector for JitterentropyCollector {
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
        // TODO(https://fxbug.dev/42105889): Test jitterentropy in multi-CPU environment. Disable
        // interrupts, or otherwise ensure that jitterentropy still performs well in
        // multi-threaded systems.
        ksync::lock!(let mut guard = self.lock_lock());
        // SAFETY: `token` proves `self.lock` is held, and it stays held for every access through
        // `ec` below. Only `ec` is projected (as a raw pointer, not a `&mut`), so the `mem` block
        // it points into is never reborrowed behind its back.
        let ec = unsafe { self.ec.as_mut_ptr(guard.as_mut().token_mut()) };

        if self.use_raw_samples {
            for byte in buf.iter_mut() {
                // SAFETY: `ec` was initialized by `init_entropy_collector`, and both `ec` and the
                // pinned `self.mem` block it points into are only accessed under `self.lock`,
                // which is held.
                *byte = unsafe { jent_lfsr_var_stat(ec, self.lfsr_loops, self.mem_loops) } as u8;
            }
            buf.len()
        } else {
            // SAFETY: `ec` was initialized by `init_entropy_collector`, both `ec` and the pinned
            // `self.mem` block it points into are only accessed under `self.lock`, which is held,
            // and `buf` is valid for writes of its length.
            let err = unsafe { jent_read_entropy(ec, buf.as_mut_ptr().cast(), buf.len()) };
            if err < 0 { 0 } else { err as usize }
        }
    }
}
