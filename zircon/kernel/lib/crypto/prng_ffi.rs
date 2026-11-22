// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! `extern "C"` trampolines backing the C++ `crypto::Prng` facade declared in
//! `<lib/crypto/prng.h>` and `crypto::global_prng::GetInstance()` in `<lib/crypto/global_prng.h>`.

use crate::crypto::global_prng;
use crate::crypto::prng::Prng;
use core::mem::MaybeUninit;
use pin_init::PinInit;

/// Constructs a `Prng` seeded with `size` bytes at `data` in the uninitialized `storage`.
///
/// # Safety
///
/// `data` must be valid for reads of `size` bytes (or `size` must be 0), and `storage` must not be
/// moved until it has been destroyed with `rust_crypto_prng_destroy`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_crypto_prng_init(
    storage: &mut MaybeUninit<Prng>,
    data: *const u8,
    size: usize,
    thread_safe: bool,
) {
    assert!(size == 0 || !data.is_null());
    // SAFETY: The caller guarantees `data` is valid for `size` bytes.
    let seed = unsafe { zr::slice_from_raw_parts(data, size) };
    let init = Prng::init_with(seed, thread_safe);
    // SAFETY: `storage` is valid, uninitialized, suitably aligned memory for a `Prng` that the
    // caller promises not to move.
    let Ok(()) = unsafe { init.__pinned_init(storage.as_mut_ptr()) };
}

/// Destroys a `Prng` previously constructed by `rust_crypto_prng_init`.
///
/// # Safety
///
/// `prng` must point to a live `Prng` that is not used again afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_crypto_prng_destroy(prng: *mut Prng) {
    // SAFETY: The caller guarantees `prng` is live and never used again.
    unsafe { core::ptr::drop_in_place(prng) };
}

/// See `Prng::add_entropy`.
///
/// # Safety
///
/// `data` must be valid for reads of `size` bytes (or `size` must be 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_crypto_prng_add_entropy(prng: &Prng, data: *const u8, size: usize) {
    assert!(size == 0 || !data.is_null());
    // SAFETY: The caller guarantees `data` is valid for `size` bytes.
    prng.add_entropy(unsafe { zr::slice_from_raw_parts(data, size) });
}

/// See `Prng::self_reseed`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_crypto_prng_self_reseed(prng: &Prng) {
    prng.self_reseed();
}

/// See `Prng::draw`.
///
/// # Safety
///
/// If `size > 0`, `out` must point to `size` initialized bytes valid for both reads and writes
/// (`Prng::draw` encrypts `out` in place, so `out` is both read and written), with exclusive
/// access for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_crypto_prng_draw(prng: &Prng, out: *mut u8, size: usize) {
    assert!(size == 0 || !out.is_null());
    // SAFETY: If `size > 0`, `out` is non-null and the caller guarantees it points to `size`
    // initialized bytes exclusively valid for both reads and writes during this call.
    prng.draw(unsafe { zr::slice_from_raw_parts_mut(out, size) });
}

/// See `Prng::rand_int`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_crypto_prng_rand_int(prng: &Prng, exclusive_upper_bound: u64) -> u64 {
    prng.rand_int(exclusive_upper_bound)
}

/// See `Prng::become_thread_safe`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_crypto_prng_become_thread_safe(prng: &Prng) {
    prng.become_thread_safe();
}

/// See `Prng::is_thread_safe`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_crypto_prng_is_thread_safe(prng: &Prng) -> bool {
    prng.is_thread_safe()
}

/// See `global_prng::get_instance`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_crypto_global_prng_get_instance() -> &'static Prng {
    global_prng::get_instance()
}
