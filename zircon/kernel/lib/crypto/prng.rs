// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::crypto::boringssl::{CHACHA20_KEY_LENGTH, CHACHA20_NONCE_LENGTH, CRYPTO_chacha_20};
use crate::crypto::entropy_pool::EntropyPool;
use crate::kernel::event::Event;
use crate::ksync_kernel::{TrackedMutex, TrackedSpinlock};
use core::convert::Infallible;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use explicit_memory::ZeroOnDrop;
use ksync::{KCell, KMutex};
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop, stack_pin_init};

// The first 12 bytes of the in-memory nonce are handed to ChaCha20 directly, which only yields
// the low 96 bits of the counter on little-endian targets.
zr::static_assert!(cfg!(target_endian = "little"));
zr::static_assert!(size_of::<EntropyPool>() == CHACHA20_KEY_LENGTH);

/// This exposes a (optionally-)thread-safe cryptographically secure PRNG.
/// This PRNG must be seeded with at least 256 bits of "real" entropy before
/// being used for cryptographic applications.
#[ksync::guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Prng {
    // ChaCha20 key (`pool`) and nonce as described in RFC 7539.
    #[pin]
    #[guarded_by(pool_lock)]
    pool: EntropyPool,
    #[guarded_by(pool_lock)]
    nonce: u128,

    // Synchronizes calls to `add_entropy`.
    #[mutex]
    add_entropy_lock: KMutex<TrackedMutex>,

    // Controls access to `pool` and `nonce`.
    #[mutex]
    pool_lock: KMutex<TrackedSpinlock>,

    // Event used to signal when calls to `draw` may proceed. It is only
    // initialized (signaled once enough entropy has accumulated) when
    // `become_thread_safe` is called.
    #[pin]
    ready: Event,

    // Number of bytes of entropy added so far.
    accumulated: AtomicUsize,

    is_thread_safe: AtomicBool,
}

// The C++ facade reserves exactly this much storage for the Rust implementation.
zr::static_assert!(size_of::<Prng>() == object_constants::kPrngSize);
zr::static_assert!(align_of::<Prng>() == object_constants::kPrngAlign);

impl Prng {
    /// The minimum amount of entropy (in bytes) the generator requires before
    /// `draw` will return data.
    pub const MIN_ENTROPY: usize = 32;

    /// The maximum amount of entropy (in bytes) that can be submitted to
    /// `add_entropy`.  Anything above this will panic.
    pub const MAX_ENTROPY: usize = 1 << 30;

    /// The maximum amount of pseudorandom data (in bytes) that can be drawn in
    /// one call to `draw`. This the limit imposed by the maximum number of bytes
    /// that can be generated with a single key/nonce pair. Each request to `draw`
    /// uses a different key/nonce pair.  Anything above this will panic.
    pub const MAX_DRAW_LEN: usize = 1 << 38;

    const NONCE_OVERFLOW: u128 = 1u128 << (8 * CHACHA20_NONCE_LENGTH);

    /// Construct a thread-safe instance of the PRNG with the input pool, shredding `pool` in
    /// place. The pool must provide at least 256 bits of entropy.
    pub fn init_from_pool(pool: Pin<&mut EntropyPool>) -> impl PinInit<Self, Infallible> + '_ {
        let accumulated = pool.contents().len();
        pin_init!(Self {
            pool <- KCell::pin_init(EntropyPool::init_move_from(pool)),
            nonce: KCell::new(0),
            add_entropy_lock <- KMutex::init(),
            pool_lock <- KMutex::init(),
            ready <- Event::init(accumulated >= Self::MIN_ENTROPY),
            accumulated: AtomicUsize::new(accumulated),
            is_thread_safe: AtomicBool::new(true),
        })
    }

    /// Construct a thread-safe instance of the PRNG with the byte array at
    /// `data` as the initial seed.  `data.len()` is the length of `data` in bytes.
    pub fn init(data: &[u8]) -> impl PinInit<Self, Infallible> + '_ {
        Self::init_with(data, /*thread_safe=*/ true)
    }

    /// Construct a non-thread-safe instance of the PRNG with the byte array at
    /// `data` as the initial seed.  `data.len()` is the length of `data` in bytes.
    pub fn init_non_thread_safe(data: &[u8]) -> impl PinInit<Self, Infallible> + '_ {
        Self::init_with(data, /*thread_safe=*/ false)
    }

    /// Construct a thread-safe or non-thread-safe instance of the PRNG with the byte array at
    /// `data` as the initial seed.  `data.len()` is the length of `data` in bytes.
    pub(super) fn init_with(data: &[u8], thread_safe: bool) -> impl PinInit<Self, Infallible> + '_ {
        pin_init!(Self {
            pool <- KCell::pin_init(EntropyPool::init()),
            nonce: KCell::new(0),
            add_entropy_lock <- KMutex::init(),
            pool_lock <- KMutex::init(),
            ready <- Event::init_unsignaled(),
            accumulated: AtomicUsize::new(0),
            is_thread_safe: AtomicBool::new(false),
        })
        .pin_chain(move |this| {
            this.add_entropy(data);
            if thread_safe {
                this.become_thread_safe();
            }
            Ok(())
        })
    }

    /// Re-seed the PRNG by mixing-in new entropy. `data.len()` MUST NOT be greater than
    /// `MAX_ENTROPY`. If `data` is empty, only hash of the current key is used to re-seed.
    pub fn add_entropy(&self, data: &[u8]) {
        assert!(data.len() <= Self::MAX_ENTROPY);

        // Concurrent calls to `add_entropy` must run sequentially.
        ksync::lock!(self.lock_add_entropy_lock());
        stack_pin_init!(let pool = EntropyPool::init());
        {
            ksync::lock!(let pool_guard = self.lock_pool_lock());
            pool.as_mut().clone_from(pool_guard.fields().pool);
        }

        pool.as_mut().add(data);

        {
            ksync::lock!(let mut pool_guard = self.lock_pool_lock());
            pool_guard.as_mut().fields_mut().pool.move_from(pool.as_mut());
        }

        // Increment how much entropy has been added, and signal if we have enough.
        let total_entropy = self.accumulated.fetch_add(data.len(), Ordering::SeqCst) + data.len();
        if self.is_thread_safe() && total_entropy >= Self::MIN_ENTROPY {
            self.ready.signal();
        }
    }

    /// Re-seed the PRNG by hash of the current key. This does not mix in new
    /// entropy.
    pub fn self_reseed(&self) {
        // `add_entropy` with empty input effectively reseeds with hash of current key.
        self.add_entropy(&[]);
    }

    /// Get pseudo-random output filling `out`.  Blocks until at least
    /// `MIN_ENTROPY` bytes of entropy have been added to this PRNG.  `out.len()` MUST
    /// NOT be greater than `MAX_DRAW_LEN`.  Identical PRNGs are only guaranteed to
    /// produce identical output when given identical inputs.
    pub fn draw(&self, out: &mut [u8]) {
        assert!(out.len() <= Self::MAX_DRAW_LEN);
        // Wait if other threads should add entropy.
        if self.is_thread_safe() && self.accumulated.load(Ordering::SeqCst) < Self::MIN_ENTROPY {
            let _ = self.ready.wait_infinite();
        }
        // Save these on the stack, but guarantee we clean them up
        stack_pin_init!(let pool = EntropyPool::init());
        let mut nonce = ZeroOnDrop::new(0u128);
        {
            ksync::lock!(let mut pool_guard = self.lock_pool_lock());
            let fields = pool_guard.as_mut().fields_mut();
            *fields.nonce += 1;
            *nonce = *fields.nonce;
            pool.as_mut().clone_from(&fields.pool);
        }
        assert!(*nonce < Self::NONCE_OVERFLOW);

        // We randomize `out` by encrypting it with a key that is never exposed to
        // the caller, and a 96-bit nonce that changes on each call.  We don't zero
        // `out` because the encrypted output meets the criteria of the PRNG
        // regardless of its original contents.  We reset the counter to 0 on each
        // request; it can't overflow because of the limit on the overall size.
        let out_len = out.len();
        let out_ptr = out.as_mut_ptr();
        // SAFETY: `out_ptr` is valid for reads and writes of `out_len` bytes (in-place encryption
        // is permitted when `in` and `out` are equal, and both pointers derive from `out_ptr`),
        // the key is the 32-byte pool contents, and `&*nonce` points to `size_of::<u128>()`
        // initialized bytes whose first 12 bytes are read as the little-endian 96-bit nonce.
        unsafe {
            CRYPTO_chacha_20(
                out_ptr,
                out_ptr.cast_const(),
                out_len,
                pool.contents().as_ptr(),
                core::ptr::from_ref(&*nonce).cast::<u8>(),
                0,
            );
        }
    }

    /// Return an integer in the range [0, exclusive_upper_bound) chosen
    /// uniformly at random.  This is a wrapper for `draw`, and so has the same
    /// caveats.
    pub fn rand_int(&self, exclusive_upper_bound: u64) -> u64 {
        assert!(exclusive_upper_bound != 0);

        let log2 = u64::BITS - (exclusive_upper_bound - 1).leading_zeros();
        let mask = if log2 != u64::BITS { (1u64 << log2) - 1 } else { u64::MAX };
        debug_assert!(exclusive_upper_bound - 1 <= mask);

        // This loop should terminate very fast, since the probability that the
        // drawn value is >= exclusive_upper_bound is less than 0.5.  This is the
        // classic discard out-of-range values approach.
        loop {
            let mut v = [0u8; size_of::<u64>()];
            self.draw(&mut v);
            let v = u64::from_ne_bytes(v) & mask;
            if v < exclusive_upper_bound {
                return v;
            }
        }
    }

    /// Transitions the PRNG to thread-safe mode.  This asserts that the
    /// instance is not yet thread-safe.
    ///
    /// It is safe to call this function from the PRNG's initializer provided
    /// `ready` and `accumulated` are initialized.
    pub fn become_thread_safe(&self) {
        assert!(!self.is_thread_safe());
        if self.accumulated.load(Ordering::SeqCst) >= Self::MIN_ENTROPY {
            self.ready.signal();
        }
        self.is_thread_safe.store(true, Ordering::SeqCst);
    }

    /// Inspect if this PRNG is thread-safe.
    pub fn is_thread_safe(&self) -> bool {
        // Safe to read `is_thread_safe`; it is read-only in a threaded context.
        self.is_thread_safe.load(Ordering::SeqCst)
    }
}

#[pinned_drop]
impl PinnedDrop for Prng {
    fn drop(self: Pin<&mut Self>) {
        // `self` is exclusively owned here, so `nonce` is scrubbed without taking `pool_lock`.
        let this = self.project();
        let nonce_ptr = core::ptr::from_mut(this.nonce.get_inner_mut()).cast::<u8>();
        // SAFETY: `nonce_ptr` points to `size_of::<u128>()` exclusively borrowed, initialized
        // bytes, and all-zeroes is a valid `u128`.
        unsafe { explicit_memory::mandatory_memset(nonce_ptr, 0, size_of::<u128>()) };
    }
}

/// Test pseudo-random number generator implementation.
#[cfg(ktest)]
#[unittest::suite(name = "prng")]
mod tests {
    use super::Prng;
    use crate::crypto::entropy_pool::EntropyPool;
    use crate::kernel::thread;
    use crate::platform_rs::timer::{DurationMono, InstantMono};
    use core::ffi::c_void;
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_false, expect_lt, expect_ok, expect_true, unwrap_ok};

    const fn seed<const N: usize>(prefix: &[u8]) -> [u8; N] {
        let mut out = [0u8; N];
        let mut i = 0;
        while i < prefix.len() {
            out[i] = prefix[i];
            i += 1;
        }
        out
    }

    #[doc = "Instantiate"]
    #[test]
    fn instantiate() {
        {
            stack_pin_init!(let _prng = Prng::init(b""));
        }
    }

    #[doc = "NonThreadSafeMode"]
    #[test]
    fn non_thread_safe_prng_same_behavior() {
        const SEED1_SIZE: usize = 32;
        const SEED1: [u8; SEED1_SIZE] = seed(b"123");
        const SEED2_SIZE: usize = 32;
        const SEED2: [u8; SEED2_SIZE] = seed(b"abc");
        const DRAW_SIZE: usize = 13;

        stack_pin_init!(let prng1 = Prng::init_non_thread_safe(&SEED1));
        stack_pin_init!(let prng2 = Prng::init(&SEED1));

        expect_false!(prng1.is_thread_safe(), "unexpected PRNG state");
        expect_true!(prng2.is_thread_safe(), "unexpected PRNG state");

        let mut out1 = [0u8; DRAW_SIZE];
        let mut out2 = [0u8; DRAW_SIZE];
        prng1.draw(&mut out1);
        prng2.draw(&mut out2);
        expect_true!(out1 == out2, "inconsistent prng");

        // Verify they stay in sync after adding entropy
        prng1.add_entropy(&SEED2);
        prng2.add_entropy(&SEED2);

        prng1.draw(&mut out1);
        prng2.draw(&mut out2);
        expect_true!(out1 == out2, "inconsistent prng");

        // Verify they stay in sync after the non-thread-safe one transitions
        // to being thread-safe.
        prng1.become_thread_safe();
        expect_true!(prng1.is_thread_safe(), "unexpected PRNG state");

        prng1.draw(&mut out1);
        prng2.draw(&mut out2);
        expect_true!(out1 == out2, "inconsistent prng");
    }

    #[doc = "Reseed"]
    #[test]
    fn reseed() {
        const SEED1_SIZE: usize = 32;
        const SEED1: [u8; SEED1_SIZE] = seed(b"123");
        const SEED2_SIZE: usize = 32;
        const SEED2: [u8; SEED2_SIZE] = seed(b"abc");
        const DRAW_SIZE: usize = 13;

        stack_pin_init!(let prng1 = Prng::init(&SEED1));
        stack_pin_init!(let prng2 = Prng::init(&SEED1));
        stack_pin_init!(let prng3 = Prng::init(&SEED1));

        let mut out1 = [0u8; DRAW_SIZE];
        let mut out2 = [0u8; DRAW_SIZE];
        let mut out3 = [0u8; DRAW_SIZE];
        prng1.draw(&mut out1);
        prng2.draw(&mut out2);
        prng3.draw(&mut out3);
        expect_true!(out1 == out2, "inconsistent prng");
        expect_true!(out1 == out3, "inconsistent prng");

        // Verify state changed after reseeding.
        prng2.add_entropy(&SEED2);
        prng3.self_reseed();

        prng1.draw(&mut out1);
        prng2.draw(&mut out2);
        prng3.draw(&mut out3);
        expect_true!(out1 != out2, "same output after reseeding");
        expect_true!(out1 != out3, "same output after reseeding");
        expect_true!(out3 != out2, "same output after reseeding");
    }

    #[doc = "Test Output"]
    #[test]
    fn prng_output() {
        const SEED1_SIZE: usize = 32;
        const SEED1: [u8; SEED1_SIZE] = seed(b"abc");
        const DRAW_SIZE: usize = 13;

        stack_pin_init!(let prng1 = Prng::init(&SEED1));
        let mut out1 = [0u8; DRAW_SIZE];
        prng1.draw(&mut out1);

        stack_pin_init!(let prng2 = Prng::init(&SEED1));
        let mut out2 = [0u8; DRAW_SIZE];
        prng2.draw(&mut out2);

        expect_true!(out1 == out2, "inconsistent prng");

        // Draw from prng1 again. Check that the output is different this time.
        // There is no theoritical guarantee that the output is different, but
        // DRAW_SIZE is large enough that the probability of this happening is
        // negligible. Also this test is fully deterministic for one given PRNG
        // implementation.
        prng1.draw(&mut out1);

        expect_true!(out1 != out2, "prng output is constant");

        // We can expect the same output from prng2.
        prng2.draw(&mut out2);

        expect_true!(out1 == out2, "inconsistent prng");

        // Now verify that different seeds produce different outputs.
        const SEED2: [u8; 33] = seed(b"blah");
        stack_pin_init!(let prng3 = Prng::init(&SEED2));
        let mut out3 = [0u8; DRAW_SIZE];
        prng3.draw(&mut out3);

        const SEED3: [u8; 33] = seed(b"bleh");
        stack_pin_init!(let prng4 = Prng::init(&SEED3));
        let mut out4 = [0u8; DRAW_SIZE];
        prng4.draw(&mut out4);

        expect_true!(out3 != out4, "prng output is constant");
    }

    extern "C" fn cprng_drawer_thread(prng: *mut c_void) -> i32 {
        let mut buf = [0u8; 16];
        // SAFETY: `prng` points to a live `Prng` that is joined before it goes out of scope.
        let prng = unsafe { &*prng.cast::<Prng>() };
        prng.draw(&mut buf);
        0
    }

    fn create_drawer(prng: &Prng) -> Result<thread::ThreadPtr, zx_status::Status> {
        let arg = core::ptr::from_ref(prng).cast_mut().cast::<c_void>();
        // SAFETY: `cprng_drawer_thread` only reads through `arg`, which the caller keeps alive
        // until the thread has been joined.
        unsafe {
            thread::create_with_priority(
                c"cprng drawer thread".as_ptr(),
                cprng_drawer_thread,
                arg,
                thread::DEFAULT_PRIORITY,
            )
        }
    }

    /// If not enough entropy has been added to the CPRNG, it should block.
    #[test]
    fn prng_blocks() {
        let fake_entropy = [0u8; Prng::MIN_ENTROPY];

        stack_pin_init!(let prng = Prng::init_non_thread_safe(&[]));
        prng.become_thread_safe();

        let drawer = unwrap_ok!(create_drawer(&prng));
        // SAFETY: `drawer` was just created and has not been joined.
        unsafe { drawer.resume() };

        let mut wait_duration = DurationMono::from_micros(1);
        loop {
            // The drawer thread should be blocked waiting for the prng to have enough entropy.
            // SAFETY: `drawer` is live until it is joined below.
            if unsafe { drawer.is_blocked() } {
                break;
            }
            let _ = thread::sleep_relative(wait_duration);
            wait_duration = wait_duration * 2;
        }

        prng.add_entropy(&fake_entropy);
        // After this the thread has to eventually finish.

        // SAFETY: `drawer` has not been joined yet.
        let res = unsafe { drawer.join(InstantMono::INFINITE) };
        expect_ok!(res.map(|_retcode| ()));
    }

    /// Adding entropy before becoming thread safe should count towards the cprng unblocking.
    #[test]
    fn prng_doesnt_block_if_entropy_is_added_early() {
        let fake_entropy = [0u8; Prng::MIN_ENTROPY];

        stack_pin_init!(let prng = Prng::init_non_thread_safe(&[]));
        prng.add_entropy(&fake_entropy);
        prng.become_thread_safe();
        let drawer = unwrap_ok!(create_drawer(&prng));

        // SAFETY: `drawer` was just created and has not been joined.
        unsafe { drawer.resume() };
        // SAFETY: `drawer` has not been joined yet.
        let res = unsafe { drawer.join(InstantMono::INFINITE) };
        expect_ok!(res.map(|_retcode| ()));
    }

    #[doc = "Test RandInt"]
    #[test]
    fn prng_randint() {
        const SEED_SIZE: usize = 32;
        const SEED: [u8; SEED_SIZE] = seed(b"abc");

        stack_pin_init!(let prng = Prng::init(&SEED));

        // Technically could fall out of the log2 loop below, but let's be explicit
        // about this case.
        for _ in 0..100 {
            expect_eq!(prng.rand_int(1), 0u64, "RandInt(1) must equal 0");
        }

        for log2 in 1..64 {
            for _ in 0..100 {
                let bound = 1u64 << log2;
                expect_lt!(prng.rand_int(bound), bound, "RandInt(2^i) must be less than 2^i");
            }
        }

        let mut high_bit = false;
        for _ in 0..100 {
            high_bit |= (prng.rand_int(u64::MAX) & (1u64 << 63)) != 0;
        }
        expect_true!(high_bit, "RandInt(UINT64_MAX) should have high bit set sometimes");
    }

    #[doc = "Initialize through EntropyPool"]
    #[test]
    fn prng_from_entropy_pool_matches_prng_from_contents() {
        const DRAW_SIZE: usize = 13;
        const SEED_SIZE: usize = 32;
        const SEED: [u8; SEED_SIZE] = seed(b"abc");

        stack_pin_init!(let pool = EntropyPool::init());
        pool.as_mut().add(&SEED);

        stack_pin_init!(let prng = Prng::init(&SEED));
        stack_pin_init!(let prng_from_pool = Prng::init_from_pool(pool));

        let mut out1 = [0u8; DRAW_SIZE];
        let mut out2 = [0u8; DRAW_SIZE];

        // Drawing from both should be the same.
        prng.draw(&mut out1);
        prng_from_pool.draw(&mut out2);

        expect_true!(out1 == out2);
    }
}
