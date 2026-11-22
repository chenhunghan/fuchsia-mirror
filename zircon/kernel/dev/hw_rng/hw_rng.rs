// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(console_enabled)]
pub mod console;
#[cfg(not(console_enabled))]
use {debug as _, kprint as _, pretty as _, zerocopy as _, zx_status as _};

use core::cell::UnsafeCell;

/// Hardware RNG driver operations.
pub trait HwRngOps: Sync + 'static {
    /// Draws entropy from the hardware RNG into `buf`, returning the number of bytes written.
    ///
    /// A return value other than `buf.len()` indicates failure.
    fn get_entropy(&self, buf: &mut [u8]) -> usize;
}

struct DriverHolder(UnsafeCell<Option<&'static dyn HwRngOps>>);

// SAFETY: `ACTIVE_DRIVER` is written at most once via `hw_rng_register`, whose contract orders that
// write before or after every `hw_rng_*` read, and any registered driver is `Sync + 'static`.
unsafe impl Sync for DriverHolder {}

static ACTIVE_DRIVER: DriverHolder = DriverHolder(UnsafeCell::new(None));

/// Draws entropy from the registered hardware RNG into `buf`.
///
/// The caller must check that the return value equals `buf.len()`; any other value means the
/// operation failed.
#[must_use]
pub fn hw_rng_get_entropy(buf: &mut [u8]) -> usize {
    // SAFETY: `hw_rng_register` requires registration to happen-before or after every `hw_rng_*`
    // call.
    match unsafe { *ACTIVE_DRIVER.0.get() } {
        Some(driver) => driver.get_entropy(buf),
        None => 0,
    }
}

/// Registers a hardware RNG driver with the dispatch layer.
///
/// # Safety
///
/// Must be called at most once during boot initialization, with no concurrent `hw_rng_*` calls, and
/// synchronized so that any earlier `hw_rng_*` call happens-before registration and registration
/// happens-before any later `hw_rng_*` call.
pub unsafe fn hw_rng_register(driver: &'static dyn HwRngOps) {
    // SAFETY: The caller guarantees one-time, synchronized registration with no concurrent
    // `hw_rng_*` calls.
    unsafe {
        let slot = ACTIVE_DRIVER.0.get();
        assert!((*slot).is_none(), "hw_rng driver already registered");
        *slot = Some(driver);
    }
}

/// Returns whether a hardware RNG driver has been registered.
#[must_use]
pub fn hw_rng_is_registered() -> bool {
    // SAFETY: `hw_rng_register` requires registration to happen-before or after every `hw_rng_*`
    // call.
    unsafe { (*ACTIVE_DRIVER.0.get()).is_some() }
}
