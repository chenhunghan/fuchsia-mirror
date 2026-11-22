// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Bindings for the minimal, kernel-appropriate subset of BoringSSL (SHA-256 and ChaCha20) that
//! is compiled into the kernel from `//zircon/kernel/lib/crypto/boringssl`.
//!
//! These declarations are maintained manually because `//third_party/boringssl/rust:bssl-sys`
//! cannot be used when `is_kernel` is true:
//! - `group("bssl-sys")` pulls in `:bssl-sys.shared` on Fuchsia targets (and defines no kernel
//!   deps when `is_kernel` is true).
//! - `:bssl-sys.lib` links `:rust-wrapper`, whose C wrapper headers reference userspace libc
//!   headers unsupported in the kernel (such as `<stdio.h>` and `<sys/time.h>`).
//! - `bssl-sys/bindgen.rs` is pre-generated offline for `x86_64-unknown-linux-gnu` rather than
//!   kernel ABIs, exposing host glibc types and floating-point (`f64`) FFI signatures (such as
//!   `RAND_add`) that are incompatible with kernel builds.

use object_constants_rs as object_constants;

/// The length of a SHA-256 digest, in bytes (`SHA256_DIGEST_LENGTH`).
pub(super) const SHA256_DIGEST_LENGTH: usize = 32;

/// The length of a ChaCha20 key, in bytes.
pub(super) const CHACHA20_KEY_LENGTH: usize = 32;

/// The length of a ChaCha20 nonce, in bytes.
pub(super) const CHACHA20_NONCE_LENGTH: usize = 12;

unsafe extern "C" {
    fn SHA256_Init(sha: *mut Sha256Ctx) -> core::ffi::c_int;
    fn SHA256_Update(sha: *mut Sha256Ctx, data: *const u8, len: usize) -> core::ffi::c_int;
    fn SHA256_Final(out: *mut u8, sha: *mut Sha256Ctx) -> core::ffi::c_int;

    /// Writes the SHA-256 digest of `len` bytes at `data` to `out`.
    ///
    /// # Safety
    ///
    /// `data` must be valid for reads of `len` bytes and `out` must be valid for writes of
    /// `SHA256_DIGEST_LENGTH` bytes.
    pub(super) fn SHA256(data: *const u8, len: usize, out: *mut u8) -> *mut u8;

    /// Encrypts `in_len` bytes from `in_` with the given key and nonce and writes the result to
    /// `out`. If `in_` and `out` alias, they must be equal. The initial block counter is specified
    /// by `counter`.
    ///
    /// # Safety
    ///
    /// `in_` must be valid for reads and `out` valid for writes of `in_len` bytes, `key` must
    /// point to `CHACHA20_KEY_LENGTH` bytes and `nonce` to `CHACHA20_NONCE_LENGTH` bytes.
    pub(super) fn CRYPTO_chacha_20(
        out: *mut u8,
        in_: *const u8,
        in_len: usize,
        key: *const u8,
        nonce: *const u8,
        counter: u32,
    );
}

/// Storage for an opaque C `SHA256_CTX`, driven through `SHA256_Init` / `SHA256_Update` /
/// `SHA256_Final`.
///
/// Dropping `Sha256Ctx` scrubs the entire context in place via
/// [`explicit_memory::mandatory_memset`], because `SHA256_Final` (`crypto_md32_final`) leaves the
/// intermediate digest state (`ctx->h`) intact. Once initialized with [`Sha256Ctx::init`], a
/// `Sha256Ctx` must remain in place until it goes out of scope so no unwiped stack copy is made.
#[repr(C, align(4))]
pub(super) struct Sha256Ctx(zr::OpaqueBytes<{ object_constants::kSha256CtxSize }>);

zr::static_assert_size_and_align!(
    Sha256Ctx,
    object_constants::kSha256CtxSize,
    object_constants::kSha256CtxAlign,
);

impl Sha256Ctx {
    /// Creates uninitialized context storage; it must be passed to [`Sha256Ctx::init`] before
    /// any other use.
    pub(super) const fn uninit() -> Self {
        Self(zr::OpaqueBytes::uninit())
    }

    /// Initializes the context.
    ///
    /// # Safety
    ///
    /// `ctx` must not be in the middle of another hash computation that is still expected to
    /// complete.
    pub(super) unsafe fn init(ctx: &mut Self) {
        // SAFETY: `ctx` points to valid, exclusively borrowed `SHA256_CTX` storage.
        unsafe { SHA256_Init(ctx) };
    }

    /// Absorbs `data` into the running hash.
    ///
    /// # Safety
    ///
    /// `ctx` must have been initialized with [`Sha256Ctx::init`] and not yet finished.
    pub(super) unsafe fn update(ctx: &mut Self, data: &[u8]) {
        // SAFETY: `ctx` is an initialized context and `data` is valid for reads of its length.
        unsafe { SHA256_Update(ctx, data.as_ptr(), data.len()) };
    }

    /// Finishes the hash, writing the digest into `out`.
    ///
    /// # Safety
    ///
    /// `ctx` must have been initialized with [`Sha256Ctx::init`] and not yet finished.
    pub(super) unsafe fn finish(ctx: &mut Self, out: &mut [u8; SHA256_DIGEST_LENGTH]) {
        // SAFETY: `ctx` is an initialized context and `out` has room for the full digest.
        unsafe { SHA256_Final(out.as_mut_ptr(), ctx) };
    }
}

impl Drop for Sha256Ctx {
    fn drop(&mut self) {
        // SAFETY: `self.0` owns `size_of::<Self>()` bytes of exclusively borrowed `MaybeUninit`
        // storage (`Opaque<[u8; kSha256CtxSize]>`), which is valid for writes whether or not
        // `init` was called.
        unsafe {
            explicit_memory::mandatory_memset(self.0.get().cast::<u8>(), 0, size_of::<Self>());
        }
    }
}
