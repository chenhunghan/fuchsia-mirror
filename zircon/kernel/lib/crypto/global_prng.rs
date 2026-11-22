// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::crypto::entropy::{self, Collector, HwRngCollector, JitterentropyCollector};
use crate::crypto::entropy_pool::{EntropyPool, mandatory_memset};
use crate::crypto::prng::Prng;
use crate::kernel::scheduler_state::HIGHEST_PRIORITY;
use crate::kernel::thread;
use crate::platform_rs::timer::DurationMono;
use boot_options::BootOptions;
use core::ffi::c_void;
use core::pin::Pin;
use core::ptr::NonNull;
use debug::ltracef;
use kprint::kprintln;
use lazy_init::LazyInit;
use zx_types::ZX_MAX_NAME_LEN;

const LOCAL_TRACE: u32 = 0;

unsafe extern "C" {
    /// Returns a pointer to the entropy pool handed off in place by physboot, or null if no pool
    /// was handed off.
    fn cpp_phys_handoff_entropy_pool() -> *const EntropyPool;

    /// Resets `gPhysHandoff->entropy_pool` in place, running `~EntropyPool()` to shred its storage
    /// with `mandatory_memset`.
    fn cpp_phys_handoff_reset_entropy_pool();
}

/// Global PRNG singleton initialized during early boot.
///
/// TODO(security): Storing `PRNG_INSTANCE` in static memory places the PRNG state in a fairly
/// predictable place. Some aspects of KASLR will help with this, but we may additionally want to
/// remap where this is later.
static PRNG_INSTANCE: LazyInit<Prng> = LazyInit::uninit();

/// Returns a reference to the global PRNG singleton. Panics if the PRNG has not been seeded yet.
pub fn get_instance() -> &'static Prng {
    &PRNG_INSTANCE
}

/// Fills `buffer` with pseudo-random bytes from the global PRNG. Blocks until at least
/// [`Prng::MIN_ENTROPY`] bytes of entropy have been added to this PRNG.
///
/// # Panics
///
/// Panics if called before the global PRNG is initialized in early boot, or if `buffer.len()` is
/// greater than [`Prng::MAX_DRAW_LEN`].
#[inline]
pub fn draw(buffer: &mut [u8]) {
    get_instance().draw(buffer);
}

/// Mixes `buffer` into the global PRNG, which must already have been made thread-safe.
pub fn add_entropy(buffer: &[u8]) {
    let prng = get_instance();
    assert!(prng.is_thread_safe());
    prng.add_entropy(buffer);
}

// Returns true on success, false on failure.
fn seed_from(collector: &dyn Collector) -> bool {
    let mut buf = [0u8; Prng::MIN_ENTROPY];
    let mut remaining = collector.bytes_needed(8 * Prng::MIN_ENTROPY);
    if LOCAL_TRACE != 0 {
        let mut name = [0u8; ZX_MAX_NAME_LEN];
        collector.get_name(&mut name);
        ltracef!(
            "About to collect {} bytes of entropy from '{}'.\n",
            remaining,
            zr::from_utf8_lossy(name.split(|&b| b == 0).next().unwrap_or(&[]))
        );
    }
    while remaining > 0 {
        let len = buf.len().min(remaining);
        let result = collector.draw_entropy(&mut buf[..len]);
        if result == 0 {
            ltracef!(
                "Collected 0 bytes; aborting. There were {} bytes remaining to collect.\n",
                remaining
            );
            return false;
        }

        PRNG_INSTANCE.add_entropy(&buf[..result]);
        mandatory_memset(&mut buf, 0);
        remaining -= result;
    }
    ltracef!("Successfully collected entropy.\n");
    true
}

/// Non-owning view of `gPhysHandoff->entropy_pool` that reads the handed-off entropy bytes
/// directly in place and resets/shreds the handoff storage when dropped, avoiding copying secret
/// bytes through stack temporaries.
struct HandoffEntropyPool {
    pool: NonNull<EntropyPool>,
}

impl HandoffEntropyPool {
    /// Borrows the handoff entropy pool in place if one was provided by physboot.
    ///
    /// # Safety
    ///
    /// Must only be called during single-threaded early boot while `gPhysHandoff` is valid and
    /// with no other live references to `gPhysHandoff->entropy_pool`.
    unsafe fn take() -> Option<Self> {
        // SAFETY: Caller guarantees `gPhysHandoff` is valid and exclusively accessed on the boot
        // CPU.
        NonNull::new(unsafe { cpp_phys_handoff_entropy_pool() }.cast_mut())
            .map(|pool| Self { pool })
    }

    /// Returns the entropy bytes directly from handoff memory.
    fn contents(&self) -> &[u8; Prng::MIN_ENTROPY] {
        // SAFETY: `self.pool` points to the live C++ `crypto::EntropyPool` inside
        // `gPhysHandoff->entropy_pool` (verified layout-identical to `EntropyPool`), which is not
        // mutated or reset until `self` is dropped.
        unsafe { self.pool.as_ref().contents() }
    }
}

impl Drop for HandoffEntropyPool {
    fn drop(&mut self) {
        // SAFETY: `self` was created during single-threaded early boot while `gPhysHandoff` is
        // valid, and any slice borrowed via `contents(&self)` has ended before `drop` runs.
        // Resetting `gPhysHandoff->entropy_pool` runs `~EntropyPool()`, scrubbing the handoff
        // storage in place with `mandatory_memset`.
        unsafe { cpp_phys_handoff_reset_entropy_pool() };
    }
}

// Instantiates the global PRNG (in non-thread-safe mode) and seeds it.
fn early_boot_seed(_level: init::LkInitLevel) {
    // Before doing anything else, test our entropy collector. This is
    // explicitly called here rather than in another init hook to ensure
    // ordering (at level LK_INIT_LEVEL_PLATFORM_EARLY + 1, but before the rest
    // of early_boot_seed).
    entropy::early_boot_test();

    // number of successful entropy sources
    let mut successful = 0u32;

    // SAFETY: This hook runs exactly once on the boot CPU before any other thread exists, so
    // nothing can race with the initialization (`LazyInit` panics if it is ever repeated).
    let Ok(()) =
        unsafe { Pin::static_ref(&PRNG_INSTANCE).init_pin(Prng::init_non_thread_safe(&[])) };
    let prng = get_instance();

    // All validation from zbi item and commandline item is performed on phys, so
    // this instance of entropy pool, is guaranteed to meet the minimum requirements
    // for the current boot options, or we would have panic'd already.
    // SAFETY: `early_boot_seed` runs once on the boot CPU while `gPhysHandoff` is live, and
    // nothing else accesses `gPhysHandoff->entropy_pool`.
    let handoff_pool = unsafe { HandoffEntropyPool::take() };
    assert!(!BootOptions::get().cprng_seed_require_cmdline || handoff_pool.is_some());
    if let Some(pool) = handoff_pool {
        // Dropping `pool` resets and shreds the handoff storage in place.
        prng.add_entropy(pool.contents());
        successful += 1;
    }
    if !BootOptions::get().cprng_disable_hw_rng
        && HwRngCollector::get_instance().is_ok_and(|collector| seed_from(collector))
    {
        successful += 1;
    } else if BootOptions::get().cprng_seed_require_hw_rng {
        panic!("Failed to seed PRNG from required entropy source: hw-rng\n");
    }
    if !BootOptions::get().cprng_disable_jitterentropy
        && JitterentropyCollector::get_instance().is_ok_and(|collector| seed_from(collector))
    {
        successful += 1;
    } else if BootOptions::get().cprng_seed_require_jitterentropy {
        panic!("Failed to seed PRNG from required entropy source: jitterentropy\n");
    }

    if successful == 0 {
        kprintln!(
            "WARNING: System has insufficient randomness.  It is completely \
             unsafe to use this system for any cryptographic applications."
        );
        // TODO(security): *CRITICAL* This is a fallback for systems without RNG
        // hardware that we should remove and attempt to do better.  If this
        // fallback is used, it breaks all cryptography used on the system.
        // *CRITICAL*
        let buf = [0u8; Prng::MIN_ENTROPY];
        prng.add_entropy(&buf);
        #[allow(clippy::needless_return)]
        return;
    } else {
        ltracef!("Successfully collected entropy from {} sources.\n", successful);
    }
}

// Migrate the global PRNG to enter thread-safe mode.
fn become_thread_safe(_level: init::LkInitLevel) {
    get_instance().become_thread_safe();
}

// Collect entropy and add it to the cprng.
fn reseed_prng() {
    let mut successful = 0u32; // number of successful entropy sources
    // Reseed using HW RNG and jitterentropy;
    if !BootOptions::get().cprng_disable_hw_rng
        && HwRngCollector::get_instance().is_ok_and(|collector| seed_from(collector))
    {
        successful += 1;
    } else if BootOptions::get().cprng_reseed_require_hw_rng {
        panic!("Failed to reseed PRNG from required entropy source: hw-rng\n");
    }
    if !BootOptions::get().cprng_disable_jitterentropy
        && JitterentropyCollector::get_instance().is_ok_and(|collector| seed_from(collector))
    {
        successful += 1;
    } else if BootOptions::get().cprng_reseed_require_jitterentropy {
        panic!("Failed to reseed PRNG from required entropy source: jitterentropy\n");
    }

    if successful == 0 {
        PRNG_INSTANCE.self_reseed();
        ltracef!("Reseed PRNG with no new entropy source\n");
    } else {
        ltracef!("Successfully reseed PRNG from {} sources.\n", successful);
    }
}

extern "C" fn reseed_loop(_arg: *mut c_void) -> i32 {
    loop {
        let _ = thread::sleep_relative(DurationMono::from_seconds(30));
        reseed_prng();
    }
}

// Start a thread to reseed PRNG.
fn start_reseed_thread(_level: init::LkInitLevel) {
    // Force a reseed before returning from the init hook.
    // We have no guarantees when the thread will be scheduled and run.
    // TODO(https://fxbug.dev/42163418): Make this synchronous reseed faster by removing
    // JitterEntropy reseed, as we already seeded from it in EarlyBoot.
    reseed_prng();
    // SAFETY: `reseed_loop` ignores its argument and only touches the global PRNG, which lives
    // for the rest of the kernel's lifetime.
    let t = unsafe {
        thread::create_with_priority(
            c"prng-reseed".as_ptr(),
            reseed_loop,
            core::ptr::null_mut(),
            HIGHEST_PRIORITY,
        )
    }
    .expect("Failed to create the prng-reseed thread");
    // SAFETY: `t` was just created and is handed over to the scheduler for good.
    let _ = unsafe { t.detach_and_resume() };
}

// intel hw_rng init hook is at PLATFORM_EARLY+1
// make sure we start after that so we can use it for the early seed.
init::lk_init_hook!(
    global_prng_seed,
    early_boot_seed,
    init::LkInitLevel(init::LK_INIT_LEVEL_PLATFORM_EARLY.0 + 2)
);

init::lk_init_hook!(
    global_prng_thread_safe,
    become_thread_safe,
    init::LkInitLevel(init::LK_INIT_LEVEL_THREADING.0 - 1)
);

// Reseed the CPRNG right before entering userspace.
init::lk_init_hook!(
    global_prng_reseed,
    start_reseed_thread,
    init::LkInitLevel(init::LK_INIT_LEVEL_USER.0 - 1)
);

/// Validate global PRNG singleton.
#[cfg(ktest)]
#[unittest::suite(name = "global_prng")]
mod tests {
    use super::get_instance;
    use unittest::expect_true;

    #[doc = "Identical"]
    #[test]
    fn identical() {
        let prng1 = get_instance();
        let prng2 = get_instance();

        expect_true!(core::ptr::eq(prng1, prng2));
    }
}
