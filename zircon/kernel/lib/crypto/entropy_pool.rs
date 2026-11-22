// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::crypto::boringssl::{SHA256, SHA256_DIGEST_LENGTH, Sha256Ctx};
use core::convert::Infallible;
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init_from_closure, pinned_drop};

/// The minimum amount of entropy (in bytes) that a generator requires before
/// safely drawing from it.
pub const MIN_ENTROPY_BYTES: u64 = 32;

/// Maximum allowed size for any collected entropy to be added into the pool.
pub const MAX_ENTROPY_SIZE: u64 = 1 << 30;

/// Overwrites `dst` with `value` in a way that the compiler is not allowed to optimize away,
/// even if `dst` is never read again. This is suitable for scrubbing buffers containing sensitive
/// data.
pub(super) fn mandatory_memset(dst: &mut [u8], value: u8) {
    let len = dst.len();
    // SAFETY: `dst` is an exclusively borrowed, initialized slice of `len` bytes.
    unsafe { explicit_memory::mandatory_memset(dst.as_mut_ptr(), value.into(), len) };
}

/// Represents a collection of entropy from multiple sources.
///
/// `EntropyPool` is `!Unpin` and must always be constructed, mutated, and transferred in place so
/// secret pool bytes are never bitwise-copied through unwiped stack slots.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct EntropyPool {
    contents: [u8; EntropyPool::CONTENT_SIZE],
    #[pin]
    _pin: PhantomData<PhantomPinned>,
}

zr::static_assert_size_and_align!(
    EntropyPool,
    object_constants::kEntropyPoolSize,
    object_constants::kEntropyPoolAlign,
);

impl EntropyPool {
    /// Maximum allowed size for any collected entropy to be added into the pool.
    pub const MAX_ENTROPY_SIZE: u64 = 1 << 30;

    /// Shred value to use for `mandatory_memset`.
    pub const SHRED_VALUE: u8 = 0b11100110;

    // Matches `SHA256_DIGEST_LENGTH`.
    const CONTENT_SIZE: usize = 32;

    /// Returns an in-place initializer for an empty (zeroed) pool.
    pub fn init() -> impl PinInit<Self, Infallible> {
        // SAFETY: Zeroes `contents` in place without constructing a stack temporary and writes the
        // zero-sized `_pin` field.
        unsafe {
            pin_init_from_closure(|slot: *mut Self| {
                let contents_ptr = (&raw mut (*slot).contents).cast::<u8>();
                core::ptr::write_bytes(contents_ptr, 0, Self::CONTENT_SIZE);
                (&raw mut (*slot)._pin).write(PhantomData);
                Ok(())
            })
        }
    }

    /// Returns an in-place initializer that copies the current state of `self` directly into the
    /// destination slot.
    pub fn init_clone_from(&self) -> impl PinInit<Self, Infallible> + '_ {
        // SAFETY: Copies `self.contents` directly into `(*slot).contents` without intermediate
        // stack buffers and writes the zero-sized `_pin` field.
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                let dst = (&raw mut (*slot).contents).cast::<u8>();
                core::ptr::copy_nonoverlapping(self.contents.as_ptr(), dst, Self::CONTENT_SIZE);
                (&raw mut (*slot)._pin).write(PhantomData);
                Ok(())
            })
        }
    }

    /// Returns an in-place initializer that moves the contents of `other` into the destination
    /// slot, shredding `other` in place afterwards.
    pub fn init_move_from(other: Pin<&mut Self>) -> impl PinInit<Self, Infallible> + '_ {
        // SAFETY: Copies `other.contents` directly into `(*slot).contents` (which cannot alias the
        // live `Pin<&mut Self>`), writes `_pin`, and scrubs `other.contents` in place.
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                let other_this = other.project();
                let dst = (&raw mut (*slot).contents).cast::<u8>();
                core::ptr::copy_nonoverlapping(
                    other_this.contents.as_ptr(),
                    dst,
                    Self::CONTENT_SIZE,
                );
                (&raw mut (*slot)._pin).write(PhantomData);
                mandatory_memset(other_this.contents, Self::SHRED_VALUE);
                Ok(())
            })
        }
    }

    /// Adds `entropy` into the pool, collecting `entropy.len()` bits of entropy.
    pub fn add(self: Pin<&mut Self>, entropy: &[u8]) {
        assert!(entropy.len() as u64 <= Self::MAX_ENTROPY_SIZE);

        let this = self.project();
        let mut ctx = Sha256Ctx::uninit();
        // SAFETY: `ctx` is valid storage for a `SHA256_CTX`, and `entropy` and `contents` are
        // valid for reads of their lengths. `SHA256_Final` writes exactly
        // `SHA256_DIGEST_LENGTH` bytes into `contents`, which is that long.
        unsafe {
            Sha256Ctx::init(&mut ctx);
            Sha256Ctx::update(&mut ctx, entropy);
            Sha256Ctx::update(&mut ctx, this.contents);
            Sha256Ctx::finish(&mut ctx, this.contents);
        }
    }

    /// Adds the entropy of the digested `source` to the pool.
    /// Returns the size in bytes of the digest of `source`.
    pub fn add_from_digest(self: Pin<&mut Self>, source: &[u8]) -> usize {
        let mut digest = [0u8; SHA256_DIGEST_LENGTH];
        // SAFETY: `source` is valid for reads of its length and `digest` has room for the
        // `SHA256_DIGEST_LENGTH` bytes that `SHA256` writes.
        unsafe { SHA256(source.as_ptr(), source.len(), digest.as_mut_ptr()) };
        self.add(&digest);
        mandatory_memset(&mut digest, Self::SHRED_VALUE);
        SHA256_DIGEST_LENGTH
    }

    /// Copies the current state of `other` into `self` in place.
    pub fn clone_from(self: Pin<&mut Self>, other: &Self) {
        self.project().contents.copy_from_slice(&other.contents);
    }

    /// Moves the contents of `other` into `self`, shredding `other` in place afterwards.
    pub fn move_from(self: Pin<&mut Self>, other: Pin<&mut Self>) {
        let this = self.project();
        let other_this = other.project();
        this.contents.copy_from_slice(other_this.contents);
        mandatory_memset(other_this.contents, Self::SHRED_VALUE);
    }

    /// Returns a view into a buffer where entropy can be drawn from.
    pub const fn contents(&self) -> &[u8; Self::CONTENT_SIZE] {
        &self.contents
    }
}

#[pinned_drop]
impl PinnedDrop for EntropyPool {
    fn drop(self: Pin<&mut Self>) {
        const _: () = assert!(
            EntropyPool::CONTENT_SIZE == SHA256_DIGEST_LENGTH,
            "EntropyPool::contents size must match SHA256 digest length."
        );
        mandatory_memset(self.project().contents, Self::SHRED_VALUE);
    }
}

/// Validate security properties of entropy pool.
#[cfg(ktest)]
#[unittest::suite(name = "crypto_entropy_pool")]
mod tests {
    use super::EntropyPool;
    use core::mem::MaybeUninit;
    use core::pin::Pin;
    use pin_init::{PinInit, stack_pin_init};
    use unittest::{assert_true, expect_false, expect_true};

    const ZEROED_CONTENTS: [u8; 32] = [0; 32];
    const SHREDDED_CONTENTS: [u8; 32] = [EntropyPool::SHRED_VALUE; 32];

    fn storage_bytes(storage: &MaybeUninit<EntropyPool>) -> [u8; 32] {
        // SAFETY: `EntropyPool` is `repr(C)` and exactly 32 bytes, and the storage is always
        // initialized (either live or shredded) when this is called.
        unsafe { core::ptr::read(storage.as_ptr().cast::<[u8; 32]>()) }
    }

    #[doc = "Default constructor has zeroed contents."]
    #[test]
    fn default_constructor_is_zeroed() {
        stack_pin_init!(let pool = EntropyPool::init());
        assert_true!(*pool.contents() == ZEROED_CONTENTS);
    }

    #[doc = "AddEntropy updates contents."]
    #[test]
    fn add_entropy_updates_the_pool() {
        stack_pin_init!(let pool = EntropyPool::init());
        let entropy: [u8; 15] = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0];
        assert_true!(*pool.contents() == ZEROED_CONTENTS);

        pool.as_mut().add(&entropy);
        expect_false!(*pool.contents() == ZEROED_CONTENTS);
    }

    #[doc = "AddEntropyFromDigest updates contents."]
    #[test]
    fn add_entropy_from_digest_updates_the_pool() {
        stack_pin_init!(let pool = EntropyPool::init());
        stack_pin_init!(let eq_pool = EntropyPool::init());
        let source: [u8; 15] = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0];
        assert_true!(*pool.contents() == ZEROED_CONTENTS);

        pool.as_mut().add_from_digest(&source);
        eq_pool.as_mut().add_from_digest(&source);
        expect_false!(*pool.contents() == ZEROED_CONTENTS);
        expect_true!(pool.contents() == eq_pool.contents());
    }

    #[doc = "AddEntropyFromDifferentDigests generates different contents."]
    #[test]
    fn add_entropy_from_different_digests() {
        stack_pin_init!(let pool = EntropyPool::init());
        stack_pin_init!(let pool_2 = EntropyPool::init());
        stack_pin_init!(let pool_3 = EntropyPool::init());
        stack_pin_init!(let pool_4 = EntropyPool::init());

        let source: [u8; 15] = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0];
        let source_2: [u8; 15] = [2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];

        pool.as_mut().add_from_digest(&source);
        pool_2.as_mut().add_from_digest(&source_2);
        expect_true!(pool.contents() != pool_2.contents());

        pool_3.as_mut().add_from_digest(&source[..4]);
        pool_4.as_mut().add_from_digest(&source[..5]);
        expect_true!(pool_3.contents() != pool_4.contents());
    }

    #[doc = "Clone generates copy."]
    #[test]
    fn clone_creates_copy() {
        stack_pin_init!(let pool = EntropyPool::init());

        let entropy: [u8; 15] = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0];
        pool.as_mut().add(&entropy);
        stack_pin_init!(let pool_clone = EntropyPool::init_clone_from(&pool));

        assert_true!(pool.contents() == pool_clone.contents());
    }

    #[doc = "Destructor shreds contents."]
    #[test]
    fn destructor_cleans_up_contents() {
        let mut storage = MaybeUninit::<EntropyPool>::uninit();
        // SAFETY: `storage` is valid, uninitialized, pinned stack storage for `EntropyPool`.
        let Ok(()) = unsafe { EntropyPool::init().__pinned_init(storage.as_mut_ptr()) };

        // SAFETY: `storage` holds a live pool that is dropped in place once here.
        unsafe { storage.assume_init_drop() };

        assert_true!(storage_bytes(&storage) == SHREDDED_CONTENTS);
    }

    #[doc = "Move shreds contents."]
    #[test]
    fn move_cleans_up_contents() {
        let mut storage = MaybeUninit::<EntropyPool>::uninit();

        {
            // SAFETY: `storage` is valid, uninitialized, pinned stack storage for `EntropyPool`.
            let Ok(()) = unsafe { EntropyPool::init().__pinned_init(storage.as_mut_ptr()) };
            // SAFETY: `storage` holds a live pool that never moves.
            let pool = unsafe { Pin::new_unchecked(storage.assume_init_mut()) };
            stack_pin_init!(let _new_pool = EntropyPool::init_move_from(pool));
        }

        assert_true!(storage_bytes(&storage) == SHREDDED_CONTENTS);

        {
            // SAFETY: `storage` is valid, uninitialized, pinned stack storage for `EntropyPool`.
            let Ok(()) = unsafe { EntropyPool::init().__pinned_init(storage.as_mut_ptr()) };
            // SAFETY: `storage` was just re-initialized with a live pool that never moves.
            let pool = unsafe { Pin::new_unchecked(storage.assume_init_mut()) };
            stack_pin_init!(let new_pool = EntropyPool::init());
            new_pool.move_from(pool);
        }

        assert_true!(storage_bytes(&storage) == SHREDDED_CONTENTS);
    }
}
